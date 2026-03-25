use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use reed_solomon_erasure::galois_8::ReedSolomon;

const DATA_SHARDS: usize = 2;
const PARITY_SHARDS: usize = 1;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;
/// Fixed 12-byte video header:
/// [4B block_seq][1B shard_idx][1B total_shards][1B data_shards][1B pad]
/// [2B shard0_len][2B shard1_len]
const VIDEO_HDR_LEN: usize = 12;
const SHARD0_LEN_OFFSET: usize = 8;
const SHARD1_LEN_OFFSET: usize = 10;
const DATA_START: usize = 12; // shard payload starts after header

/// How long to wait for the next video payload before checking stall timeouts.
const RECV_TIMEOUT: Duration = Duration::from_millis(50);

/// If no FEC block completes within this window, drop the stalled block.
const STALL_TIMEOUT: Duration = Duration::from_millis(50);

struct RsBlock {
    shards: Vec<Option<Vec<u8>>>,
    shard_sizes: Vec<usize>,
    received: usize,
    actual_data_shards: usize,
    /// Original data shard lengths before FEC padding, broadcast by the sender
    /// in every shard's header. Used to truncate reconstructed shards so that
    /// zero-fill padding from RS reconstruction does NOT enter the H.264 stream.
    shard0_len: usize,
    shard1_len: usize,
}

/// Video receiver that processes FEC-encoded video payloads
/// from a crossbeam channel (fed by the raw socket RX dispatcher).
pub struct VideoReceiver {
    tx: crossbeam_channel::Sender<Vec<u8>>,
    rx: crossbeam_channel::Receiver<Vec<u8>>,
}

impl VideoReceiver {
    pub fn new(
        tx: crossbeam_channel::Sender<Vec<u8>>,
        rx: crossbeam_channel::Receiver<Vec<u8>>,
    ) -> Self {
        info!(
            "Video receiver (RS {}+{}) ready (L2 payload channel)",
            DATA_SHARDS, PARITY_SHARDS
        );
        Self { tx, rx }
    }

    /// Check whether the current block has stalled and flush/discard it if so.
    /// Returns true if a stall was detected and handled.
    fn check_stall(
        blocks: &mut HashMap<u32, RsBlock>,
        next_block: &mut Option<u32>,
        last_decode_time: &mut Instant,
        nal_buf: &mut Vec<u8>,
        nal_started: &mut bool,
    ) -> bool {
        if let Some(cur) = *next_block {
            if last_decode_time.elapsed() > STALL_TIMEOUT {
                if let Some(block) = blocks.get(&cur) {
                    warn!(
                        "RS: block {} stalled (had {}/{} shards), dropping",
                        cur, block.received, DATA_SHARDS
                    );
                } else {
                    warn!("RS: block {} stalled (no shards received), dropping", cur);
                }
                if *nal_started {
                    nal_buf.clear();
                    *nal_started = false;
                }
                blocks.remove(&cur);
                *next_block = Some(cur.wrapping_add(1));
                *last_decode_time = Instant::now();
                return true;
            }
        }
        false
    }

    pub fn run(&self) {
        let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS)
            .expect("Failed to create Reed-Solomon decoder");

        let mut blocks: HashMap<u32, RsBlock> = HashMap::new();
        let mut next_block: Option<u32> = None;
        let mut last_decode_time = Instant::now();
        let mut block_count: u64 = 0;
        let mut fec_recovered: u64 = 0;
        let mut fec_dropped: u64 = 0;

        let mut nal_buf: Vec<u8> = Vec::new();
        let mut nal_started = false;

        loop {
            // Use recv_timeout so stall detection fires even when no packets arrive.
            // This is critical: without it, a full block loss causes a permanent stall
            // because the stall check below was previously only reached after recv().
            let payload = match self.rx.recv_timeout(RECV_TIMEOUT) {
                Ok(p) => p,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    // No packet arrived — check if the current block has stalled.
                    Self::check_stall(
                        &mut blocks,
                        &mut next_block,
                        &mut last_decode_time,
                        &mut nal_buf,
                        &mut nal_started,
                    );
                    continue;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    info!("Video payload channel closed");
                    return;
                }
            };

            // Video packet header: 12 bytes fixed
            // [4B block_seq][1B shard_idx][1B total_shards][1B data_shards]
            // [1B pad][2B shard0_len][2B shard1_len]
            if payload.len() < VIDEO_HDR_LEN {
                continue;
            }

            let block_seq = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let shard_index = payload[4] as usize;
            let total_shards = payload[5] as usize;
            let actual_data_shards = payload[6] as usize;
            let shard0_len =
                u16::from_le_bytes([payload[SHARD0_LEN_OFFSET], payload[SHARD0_LEN_OFFSET + 1]])
                    as usize;
            let shard1_len =
                u16::from_le_bytes([payload[SHARD1_LEN_OFFSET], payload[SHARD1_LEN_OFFSET + 1]])
                    as usize;

            if total_shards != TOTAL_SHARDS || shard_index >= TOTAL_SHARDS {
                warn!(
                    "RS: invalid shard idx={} total={}",
                    shard_index, total_shards
                );
                continue;
            }

            // Extract shard data: everything after the 12-byte video header.
            // Data shards may be shorter than the FEC max (variable-size shards).
            // The parity shard is always max_size. The receiver pads smaller
            // shards during reconstruct_rs_block and truncates after recovery.
            let payload_start = DATA_START;
            if payload_start >= payload.len() {
                continue;
            }
            let shard_data = payload[payload_start..].to_vec();

            if next_block.is_none() {
                next_block = Some(block_seq);
            } else if let Some(current) = next_block {
                let gap = current.wrapping_sub(block_seq);
                if gap > 1000 {
                    info!(
                        "RS: camera restarted, seq reset {} -> {}",
                        current, block_seq
                    );
                    next_block = Some(block_seq);
                    blocks.clear();
                    nal_buf.clear();
                    nal_started = false;
                }
            }

            if let Some(current) = next_block {
                blocks.retain(|&k, _| {
                    let age = current.wrapping_sub(k);
                    age == 0 || (k.wrapping_sub(current) < 4 && age < 8)
                });
            }

            let block = blocks.entry(block_seq).or_insert_with(|| RsBlock {
                shards: vec![None; TOTAL_SHARDS],
                shard_sizes: vec![0; TOTAL_SHARDS],
                received: 0,
                actual_data_shards,
                shard0_len,
                shard1_len,
            });

            if block.shards[shard_index].is_none() {
                block.received += 1;
                block.shard_sizes[shard_index] = shard_data.len();
            }
            block.shards[shard_index] = Some(shard_data);

            if let Some(current_seq) = next_block {
                if block_seq == current_seq {
                    let block = blocks.get(&block_seq).unwrap();
                    if block.received >= DATA_SHARDS {
                        let reconstructed =
                            reconstruct_rs_block(&rs, block, block.actual_data_shards);
                        if let Some(data_shards) = reconstructed {
                            fec_recovered += 1;
                            if fec_recovered % 120 == 0 {
                                info!(
                                    "RS: recovered={} dropped={} blocks",
                                    fec_recovered, fec_dropped
                                );
                            }
                            for (idx, shard_data) in data_shards
                                .iter()
                                .take(block.actual_data_shards)
                                .enumerate()
                            {
                                // Truncate reconstructed shard to its original length.
                                // This removes the zero-fill padding that RS reconstruction
                                // injects, which would corrupt the H.264 bitstream if left in.
                                let orig_len = if idx == 0 {
                                    block.shard0_len
                                } else {
                                    block.shard1_len
                                };
                                let trimmed = if orig_len > 0 && orig_len <= shard_data.len() {
                                    &shard_data[..orig_len]
                                } else {
                                    shard_data
                                };
                                if trimmed.is_empty() {
                                    continue;
                                }
                                // Fragment index is u16 LE (2 bytes) to support large NALUs
                                // that need more than 256 fragments.
                                if trimmed.len() < 2 {
                                    continue;
                                }
                                let frag_index = u16::from_le_bytes([trimmed[0], trimmed[1]]);
                                let frag_payload = &trimmed[2..];

                                if frag_index == 0 {
                                    if nal_started && !nal_buf.is_empty() {
                                        let nal_data = std::mem::take(&mut nal_buf);
                                        if let Err(e) = self.tx.try_send(nal_data) {
                                            if block_count % 60 == 0 {
                                                warn!("Video frame channel full, dropping: {}", e);
                                            }
                                        }
                                    }
                                    nal_buf.clear();
                                    nal_buf.extend_from_slice(&[0, 0, 0, 1]); // Annex-B start code
                                    nal_buf.extend_from_slice(frag_payload);
                                    nal_started = true;
                                } else if nal_started {
                                    nal_buf.extend_from_slice(frag_payload);
                                }
                            }
                            // Count one completed block (not one per shard)
                            block_count += 1;
                            for (idx, shard_data) in data_shards
                                .iter()
                                .take(block.actual_data_shards)
                                .enumerate()
                            {
                                // Truncate reconstructed shard to its original length.
                                // This removes the zero-fill padding that RS reconstruction
                                // injects, which would corrupt the H.264 bitstream if left in.
                                let orig_len = if idx == 0 {
                                    block.shard0_len
                                } else {
                                    block.shard1_len
                                };
                                let trimmed = if orig_len > 0 && orig_len <= shard_data.len() {
                                    &shard_data[..orig_len]
                                } else {
                                    shard_data
                                };
                                if trimmed.is_empty() {
                                    continue;
                                }
                                // Fragment index is u16 LE (2 bytes) to support large NALUs
                                // that need more than 256 fragments.
                                if trimmed.len() < 2 {
                                    continue;
                                }
                                let frag_index = u16::from_le_bytes([trimmed[0], trimmed[1]]);
                                let frag_payload = &trimmed[2..];

                                if frag_index == 0 {
                                    if nal_started && !nal_buf.is_empty() {
                                        let nal_data = std::mem::take(&mut nal_buf);
                                        if let Err(e) = self.tx.try_send(nal_data) {
                                            if block_count % 60 == 0 {
                                                warn!("Video frame channel full, dropping: {}", e);
                                            }
                                        }
                                        block_count += 1;
                                    }
                                    nal_buf.clear();
                                    nal_buf.extend_from_slice(&[0, 0, 0, 1]); // Annex-B start code
                                    nal_buf.extend_from_slice(frag_payload);
                                    nal_started = true;
                                } else if nal_started {
                                    nal_buf.extend_from_slice(frag_payload);
                                }
                            }
                        } else {
                            fec_dropped += 1;
                            warn!(
                                "RS: block {} unrecoverable (had {} shards), dropping",
                                block_seq, block.received
                            );
                            if nal_started {
                                nal_buf.clear();
                                nal_started = false;
                            }
                        }

                        blocks.remove(&block_seq);
                        next_block = Some(current_seq.wrapping_add(1));
                        last_decode_time = Instant::now();
                    }
                }

                // Stall timeout check for blocks that received some shards but not enough.
                Self::check_stall(
                    &mut blocks,
                    &mut next_block,
                    &mut last_decode_time,
                    &mut nal_buf,
                    &mut nal_started,
                );
            }
        }
    }
}

fn reconstruct_rs_block(
    rs: &ReedSolomon,
    block: &RsBlock,
    actual_data_shards: usize,
) -> Option<Vec<Vec<u8>>> {
    let max_size = block.shard_sizes.iter().max().copied().unwrap_or(0);
    if max_size == 0 {
        return None;
    }

    let mut shard_refs: Vec<Option<Vec<u8>>> = Vec::with_capacity(TOTAL_SHARDS);
    for i in 0..TOTAL_SHARDS {
        if let Some(ref data) = block.shards[i] {
            let mut padded = vec![0u8; max_size];
            padded[..data.len()].copy_from_slice(data);
            shard_refs.push(Some(padded));
        } else {
            shard_refs.push(None);
        }
    }

    if let Err(e) = rs.reconstruct(&mut shard_refs) {
        warn!("RS reconstruct failed: {:?}", e);
        return None;
    }

    let mut result = Vec::with_capacity(actual_data_shards);
    for i in 0..actual_data_shards {
        result.push(shard_refs[i].clone().unwrap_or_default());
    }
    Some(result)
}

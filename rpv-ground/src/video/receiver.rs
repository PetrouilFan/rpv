use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use reed_solomon_erasure::galois_8::ReedSolomon;

const DATA_SHARDS: usize = 2;
const PARITY_SHARDS: usize = 1;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;
/// Video header layout (8 + 2*DATA_SHARDS bytes):
///   [4B block_seq][1B shard_idx][1B total_shards][1B data_shards][1B pad]
///   [2B * DATA_SHARDS shard_len_array]
const VIDEO_HDR_FIXED: usize = 8;
const VIDEO_HDR_LEN: usize = VIDEO_HDR_FIXED + DATA_SHARDS * 2;
const DATA_START: usize = VIDEO_HDR_LEN;

/// If no FEC block completes within this window, drop the stalled block.
const STALL_TIMEOUT: Duration = Duration::from_millis(500);

struct RsBlock {
    shards: Vec<Option<Vec<u8>>>,
    shard_sizes: Vec<usize>,
    received: usize,
    actual_data_shards: usize,
    /// Original data shard lengths before FEC padding, broadcast by the sender.
    shard_lens: [usize; DATA_SHARDS],
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
        let mut _block_count: u64 = 0;
        let mut fec_recovered: u64 = 0;
        let mut fec_dropped: u64 = 0;
        let mut payload_count: u64 = 0;

        let mut nal_buf: Vec<u8> = Vec::new();
        let mut nal_started = false;

        info!("VideoReceiver loop starting");

        loop {
            // Fast path: drain buffered packets immediately without blocking
            let payload = match self.rx.try_recv() {
                Ok(p) => p,
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    // No data buffered — check stall, then block until data arrives
                    Self::check_stall(
                        &mut blocks,
                        &mut next_block,
                        &mut last_decode_time,
                        &mut nal_buf,
                        &mut nal_started,
                    );
                    match self.rx.recv() {
                        Ok(p) => p,
                        Err(_) => {
                            info!("Video payload channel closed");
                            return;
                        }
                    }
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    info!("Video payload channel closed");
                    return;
                }
            };

            if payload.len() < VIDEO_HDR_LEN {
                continue;
            }
            payload_count += 1;
            if payload_count % 500 == 0 {
                info!(
                    "VideoReceiver: received {} payloads, blocks={}, next={:?}",
                    payload_count,
                    blocks.len(),
                    next_block
                );
            }

            let block_seq = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let shard_index = payload[4] as usize;
            let total_shards = payload[5] as usize;
            let actual_data_shards = (payload[6] as usize).min(DATA_SHARDS);

            // Parse [u16; DATA_SHARDS] shard length array
            let mut shard_lens = [0usize; DATA_SHARDS];
            for i in 0..DATA_SHARDS {
                let off = VIDEO_HDR_FIXED + i * 2;
                shard_lens[i] = u16::from_le_bytes([payload[off], payload[off + 1]]) as usize;
            }

            if total_shards != TOTAL_SHARDS || shard_index >= TOTAL_SHARDS {
                warn!(
                    "RS: invalid shard idx={} total={}",
                    shard_index, total_shards
                );
                continue;
            }

            if DATA_START >= payload.len() {
                continue;
            }
            let shard_data = payload[DATA_START..].to_vec();

            if next_block.is_none() {
                next_block = Some(block_seq);
            }

            // Prune old blocks: only keep blocks within a small window ahead of expected
            if let Some(current) = next_block {
                blocks.retain(|&k, _| {
                    let age = current.wrapping_sub(k);
                    let ahead = k.wrapping_sub(current);
                    // Keep: current block, blocks slightly behind, or blocks within 16 ahead
                    age == 0 || age < 100 || ahead < 16
                });
            }

            let block = blocks.entry(block_seq).or_insert_with(|| RsBlock {
                shards: vec![None; TOTAL_SHARDS],
                shard_sizes: vec![0; TOTAL_SHARDS],
                received: 0,
                actual_data_shards,
                shard_lens,
            });

            if block.shards[shard_index].is_none() {
                block.received += 1;
                block.shard_sizes[shard_index] = shard_data.len();
            }
            block.shards[shard_index] = Some(shard_data);

            if let Some(current_seq) = next_block {
                // Process current block and any consecutive completed blocks
                // Need at least DATA_SHARDS total shards (data + parity) for RS recovery
                let mut seq = current_seq;
                loop {
                    let should_process = blocks
                        .get(&seq)
                        .map_or(false, |b| b.received >= DATA_SHARDS);
                    if !should_process {
                        break;
                    }
                    let block = blocks.remove(&seq).unwrap();
                    let reconstructed = reconstruct_rs_block(&rs, &block, block.actual_data_shards);
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
                            let orig_len = block.shard_lens.get(idx).copied().unwrap_or(0);
                            let trimmed = if orig_len > 0 && orig_len <= shard_data.len() {
                                &shard_data[..orig_len]
                            } else {
                                shard_data
                            };
                            if trimmed.is_empty() {
                                continue;
                            }
                            if trimmed.len() < 2 {
                                continue;
                            }
                            let frag_index = u16::from_le_bytes([trimmed[0], trimmed[1]]);
                            let frag_payload = &trimmed[2..];

                            if frag_index == 0 {
                                if nal_started && !nal_buf.is_empty() {
                                    let nal_data = std::mem::take(&mut nal_buf);
                                    if let Err(e) = self.tx.send(nal_data) {
                                        warn!("Video frame channel closed: {}", e);
                                    }
                                }
                                nal_buf.clear();
                                nal_buf.extend_from_slice(&[0, 0, 0, 1]);
                                nal_buf.extend_from_slice(frag_payload);
                                nal_started = true;
                            } else if nal_started {
                                nal_buf.extend_from_slice(frag_payload);
                            }
                        }
                        _block_count += 1;
                    } else {
                        fec_dropped += 1;
                        warn!(
                            "RS: block {} unrecoverable (had {} shards), dropping",
                            seq, block.received
                        );
                        if nal_started {
                            nal_buf.clear();
                            nal_started = false;
                        }
                    }

                    seq = seq.wrapping_add(1);
                    last_decode_time = Instant::now();
                }
                next_block = Some(seq);
            }

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

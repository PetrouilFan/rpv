use std::collections::HashMap;
use std::time::Instant;
use tracing::{info, warn};

use reed_solomon_erasure::galois_8::ReedSolomon;

const DATA_SHARDS: usize = 2;
const PARITY_SHARDS: usize = 1;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;
const VIDEO_HDR_FIXED: usize = 10; // block_seq(4) + shard_idx(1) + total(1) + data(1) + pad(1) + shard_len(2)
const VIDEO_HDR_LEN: usize = VIDEO_HDR_FIXED + TOTAL_SHARDS * 2; // + shard_sizes array

struct RsBlock {
    shards: Vec<Option<Vec<u8>>>,
    shard_sizes: Vec<usize>,
    received: usize,
    actual_data_shards: usize,
    original_sizes: Vec<usize>,
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

    pub fn run(&self) {
        let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS)
            .expect("Failed to create Reed-Solomon decoder");

        let mut blocks: HashMap<u32, RsBlock> = HashMap::new();
        let mut next_block: Option<u32> = None;
        let mut last_decode_time = Instant::now();
        let mut block_count: u64 = 0;

        let mut nal_buf: Vec<u8> = Vec::new();
        let mut nal_started = false;

        loop {
            let payload = match self.rx.recv() {
                Ok(p) => p,
                Err(_) => {
                    info!("Video payload channel closed");
                    return;
                }
            };

            // Video packet header: fixed(10) + shard_sizes(TOTAL_SHARDS*2)
            if payload.len() < VIDEO_HDR_LEN {
                continue;
            }

            let block_seq = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let shard_index = payload[4] as usize;
            let total_shards = payload[5] as usize;
            let actual_data_shards = payload[6] as usize;
            let shard_len = u16::from_le_bytes([payload[8], payload[9]]) as usize;

            // Read original shard sizes (before FEC padding) from header
            let mut orig_sizes = Vec::with_capacity(TOTAL_SHARDS);
            for j in 0..TOTAL_SHARDS {
                let off = VIDEO_HDR_FIXED + j * 2;
                let sz = u16::from_le_bytes([payload[off], payload[off + 1]]) as usize;
                orig_sizes.push(sz);
            }

            if total_shards != TOTAL_SHARDS || shard_index >= TOTAL_SHARDS {
                warn!(
                    "RS: invalid shard idx={} total={}",
                    shard_index, total_shards
                );
                continue;
            }

            let payload_start = VIDEO_HDR_LEN;
            let payload_end = payload_start + shard_len;
            if payload_end > payload.len() {
                continue;
            }
            let shard_data = payload[payload_start..payload_end].to_vec();

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
                original_sizes: orig_sizes.clone(),
            });

            if block.shards[shard_index].is_none() {
                block.received += 1;
                block.shard_sizes[shard_index] = shard_len;
            }
            block.shards[shard_index] = Some(shard_data);

            if let Some(current_seq) = next_block {
                if block_seq == current_seq {
                    let block = blocks.get(&block_seq).unwrap();
                    if block.received >= DATA_SHARDS {
                        let reconstructed =
                            reconstruct_rs_block(&rs, block, block.actual_data_shards);
                        if let Some(data_shards) = reconstructed {
                            for (idx, shard_data) in data_shards
                                .iter()
                                .take(block.actual_data_shards)
                                .enumerate()
                            {
                                // Trim FEC padding using original size from header
                                let orig_len = block.original_sizes.get(idx).copied().unwrap_or(0);
                                let trimmed = if orig_len > 0 && orig_len <= shard_data.len() {
                                    &shard_data[..orig_len]
                                } else {
                                    shard_data
                                };
                                if trimmed.is_empty() {
                                    continue;
                                }
                                let frag_index = trimmed[0];
                                let frag_payload = &trimmed[1..];

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

                // Stall timeout check: runs regardless of whether we received
                // a packet matching current_seq. Handles the case where an entire
                // FEC block is lost in RF and no matching packets arrive.
                if let Some(cur) = next_block {
                    if let Some(block) = blocks.get(&cur) {
                        if last_decode_time.elapsed().as_millis() > 50 {
                            warn!(
                                "RS: block {} stalled (had {}/{} shards), dropping",
                                cur, block.received, DATA_SHARDS
                            );
                            if nal_started {
                                nal_buf.clear();
                                nal_started = false;
                            }
                            blocks.remove(&cur);
                            next_block = Some(cur.wrapping_add(1));
                            last_decode_time = Instant::now();
                        }
                    }
                }
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

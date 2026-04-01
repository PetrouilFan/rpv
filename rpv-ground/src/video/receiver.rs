use std::time::{Duration, Instant};
use tracing::{info, warn};

use reed_solomon_erasure::galois_8::ReedSolomon;

const DATA_SHARDS: usize = 1;
const PARITY_SHARDS: usize = 1;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;
/// #13: Ring buffer size for FEC blocks — O(1) lookups via seq % RING_SIZE
const RING_SIZE: usize = 256;
/// Only need 1 shard (data shard) — parity shard allows recovery of lost data
/// Video header layout (8 + 2*DATA_SHARDS bytes):
///   [4B block_seq][1B shard_idx][1B total_shards][1B data_shards][1B pad]
///   [2B * DATA_SHARDS shard_len_array]
const VIDEO_HDR_FIXED: usize = 8;
const VIDEO_HDR_LEN: usize = VIDEO_HDR_FIXED + DATA_SHARDS * 2;
const DATA_START: usize = VIDEO_HDR_LEN;

/// If no FEC block completes within this window, drop the stalled block.
const STALL_TIMEOUT: Duration = Duration::from_millis(1000);

struct RsBlock {
    block_seq: u32,
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
        blocks: &mut [Option<RsBlock>; RING_SIZE],
        next_block: &mut Option<u32>,
        last_decode_time: &mut Instant,
        nal_buf: &mut Vec<u8>,
        nal_started: &mut bool,
    ) -> bool {
        if let Some(cur) = *next_block {
            if last_decode_time.elapsed() > STALL_TIMEOUT {
                let idx = (cur as usize) % RING_SIZE;
                if let Some(ref block) = blocks[idx] {
                    warn!(
                        "RS: block {} stalled (had {}/{} shards), dropping",
                        cur, block.received, DATA_SHARDS
                    );
                } else {
                    if cur % 100 == 0 {
                        warn!("RS: block {} stalled (no shards received), dropping", cur);
                    }
                }
                if *nal_started {
                    nal_buf.clear();
                    *nal_started = false;
                }
                blocks[idx] = None;
                // Jump to nearest available block in ring buffer
                let mut next_seq = cur.wrapping_add(1);
                for offset in 1..RING_SIZE {
                    let check_seq = cur.wrapping_add(offset as u32);
                    let check_idx = (check_seq as usize) % RING_SIZE;
                    if blocks[check_idx].is_some() {
                        next_seq = check_seq;
                        break;
                    }
                }
                *next_block = Some(next_seq);
                *last_decode_time = Instant::now();
                return true;
            }
        }
        false
    }

    pub fn run(&self) {
        let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS)
            .expect("Failed to create Reed-Solomon decoder");

        // #13: Fixed-size ring buffer — O(1) zero-allocation lookups via seq % RING_SIZE
        let mut blocks: [Option<RsBlock>; RING_SIZE] = std::array::from_fn(|_| None);
        // #15: Ring buffer for processed sequence tracking — O(1) bitmap lookup
        // Use Option<u32> so block_seq=0 isn't confused with empty slot
        let mut processed_ring: [Option<u32>; RING_SIZE] = [None; RING_SIZE];
        let mut next_block: Option<u32> = None;
        let mut last_decode_time = Instant::now();
        let mut _block_count: u64 = 0;
        let mut fec_recovered: u64 = 0;
        let mut fec_dropped: u64 = 0;
        let mut payload_count: u64 = 0;
        let mut nal_buf: Vec<u8> = Vec::with_capacity(65536);

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
            if payload_count % 1000 == 0 {
                info!(
                    "VideoReceiver: {} payloads, blocks={}, next={:?}",
                    payload_count,
                    blocks.len(),
                    next_block
                );
            }

            let block_seq = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let shard_index = payload[4] as usize;
            let total_shards = payload[5] as usize;
            let actual_data_shards = (payload[6] as usize).min(DATA_SHARDS);

            // Parse [u16; DATA_SHARDS] shard length array from this shard's header.
            // Every shard (data + parity) carries the full shard_lens array.
            let mut parsed_shard_lens = [0usize; DATA_SHARDS];
            for i in 0..DATA_SHARDS {
                let off = VIDEO_HDR_FIXED + i * 2;
                parsed_shard_lens[i] =
                    u16::from_le_bytes([payload[off], payload[off + 1]]) as usize;
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

            // #15: O(1) bitmap lookup — seq % RING_SIZE maps to slot
            let dedup_idx = (block_seq as usize) % RING_SIZE;
            if processed_ring[dedup_idx] == Some(block_seq) {
                continue;
            }

            if next_block.is_none() {
                next_block = Some(block_seq);
            }

            // #13: O(1) ring buffer lookup — no HashMap allocation or hashing
            let idx = (block_seq as usize) % RING_SIZE;

            // Clear stale slot if it contains a different block_seq
            if let Some(ref existing) = blocks[idx] {
                if existing.block_seq != block_seq {
                    blocks[idx] = None;
                }
            }

            // #13: Direct array access instead of HashMap entry API
            if blocks[idx].is_none() {
                blocks[idx] = Some(RsBlock {
                    block_seq,
                    shards: vec![None; TOTAL_SHARDS],
                    shard_sizes: vec![0; TOTAL_SHARDS],
                    received: 0,
                    actual_data_shards,
                    shard_lens: parsed_shard_lens,
                });
            }
            let block = blocks[idx].as_mut().unwrap();

            if block.shards[shard_index].is_none() {
                block.received += 1;
                block.shard_sizes[shard_index] = shard_data.len();
            }
            block.shards[shard_index] = Some(shard_data);

            // #13: Scan ring buffer for blocks with enough shards
            let mut ready_slots: Vec<usize> = Vec::new();
            for slot in 0..RING_SIZE {
                if let Some(ref block) = blocks[slot] {
                    if block.received >= DATA_SHARDS {
                        ready_slots.push(slot);
                    }
                }
            }
            for slot in ready_slots {
                let block = blocks[slot].take().unwrap();
                let reconstructed = reconstruct_rs_block(&rs, &block, block.actual_data_shards);
                if let Some(data_shards) = reconstructed {
                    fec_recovered += 1;
                    if fec_recovered % 10 == 0 {
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
                        if trimmed.is_empty() || trimmed.len() < 2 {
                            continue;
                        }

                        // Fragment header: first byte is fragment type
                        let frag_type = trimmed[0];
                        let frag_data = &trimmed[1..];

                        match frag_type {
                            0x00 => {
                                // Complete NAL — send directly
                                if let Err(e) = self.tx.send(frag_data.to_vec()) {
                                    warn!("Video frame channel closed: {}", e);
                                }
                            }
                            0x01 => {
                                // First fragment — reset buffer
                                nal_buf.clear();
                                nal_buf.extend_from_slice(frag_data);
                            }
                            0x02 => {
                                // Continuation — append
                                nal_buf.extend_from_slice(frag_data);
                            }
                            0x03 => {
                                // Last fragment — append and send
                                nal_buf.extend_from_slice(frag_data);
                                if let Err(e) = self.tx.send(nal_buf.clone()) {
                                    warn!("Video frame channel closed: {}", e);
                                }
                                nal_buf.clear();
                            }
                            _ => {
                                // Unknown fragment type — skip
                            }
                        }
                    }
                    _block_count += 1;
                    // #15: Mark as processed in O(1) bitmap
                    let dedup_idx = (block_seq as usize) % RING_SIZE;
                    processed_ring[dedup_idx] = Some(block_seq);
                    if let Some(nb) = next_block {
                        if block_seq >= nb {
                            next_block = Some(block_seq.wrapping_add(1));
                        }
                    }
                    last_decode_time = Instant::now();
                } else {
                    fec_dropped += 1;
                    // #15: Mark as processed in O(1) bitmap
                    let dedup_idx = (block_seq as usize) % RING_SIZE;
                    processed_ring[dedup_idx] = Some(block_seq);
                    if nal_started {
                        nal_buf.clear();
                        nal_started = false;
                    }
                    if let Some(nb) = next_block {
                        if block_seq >= nb {
                            next_block = Some(block_seq.wrapping_add(1));
                        }
                    }
                }
            }

            Self::check_stall(
                &mut blocks,
                &mut next_block,
                &mut last_decode_time,
                &mut nal_buf,
                &mut nal_started,
            );

            // #15: Ring buffer handles cleanup automatically — no periodic retain needed
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

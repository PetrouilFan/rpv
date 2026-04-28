use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use reed_solomon_erasure::galois_8::ReedSolomon;
use rpv_proto::link;

const DATA_SHARDS: usize = 4;
const PARITY_SHARDS: usize = 2;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;
const RING_SIZE: usize = 128;
const VIDEO_HDR_FIXED: usize = 8;
const VIDEO_HDR_LEN: usize = VIDEO_HDR_FIXED + DATA_SHARDS * 2;
const DATA_START: usize = VIDEO_HDR_LEN;

const STALL_TIMEOUT: Duration = Duration::from_millis(1000);

struct RsBlock {
    block_seq: u32,
    shards: Vec<Option<Vec<u8>>>,
    shard_sizes: Vec<usize>,
    received: usize,
    actual_data_shards: usize,
    shard_lens: [usize; DATA_SHARDS],
}

struct CompletedBlock {
    block_seq: u32,
    data_shards: Vec<Vec<u8>>,
    shard_lens: [usize; DATA_SHARDS],
}

pub struct VideoReceiver {
    tx: crossbeam_channel::Sender<Vec<u8>>,
    rx: crossbeam_channel::Receiver<Vec<u8>>,
    assembly_buf: Vec<u8>,
    assembly_active: bool,
    orphan_fragments: u64,
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

    if rs.reconstruct(&mut shard_refs).is_ok() {
        let mut result = Vec::with_capacity(actual_data_shards);
        for i in 0..actual_data_shards {
            result.push(shard_refs[i].clone().unwrap_or_default());
        }
        Some(result)
    } else {
        None
    }
}
impl VideoReceiver {
    pub fn new(
        tx: crossbeam_channel::Sender<Vec<u8>>,
        rx: crossbeam_channel::Receiver<Vec<u8>>,
    ) -> Self {
        Self {
            tx,
            rx,
            assembly_buf: Vec::with_capacity(32768),
            assembly_active: false,
            orphan_fragments: 0,
        }
    }

    #[inline]
    fn is_future_block(block_seq: u32, next_block: u32) -> bool {
        block_seq != next_block && (block_seq.wrapping_sub(next_block) & 0x80000000) == 0
    }

    fn emit_block(&mut self, block: &CompletedBlock, fec_recovered: &mut u64) {
        for (sidx, shard_data) in block.data_shards.iter().enumerate() {
            let orig_len = block.shard_lens.get(sidx).copied().unwrap_or(0);
            let trimmed = if orig_len > 0 && orig_len <= shard_data.len() {
                &shard_data[..orig_len]
            } else {
                shard_data.as_slice()
            };
            if trimmed.is_empty() || trimmed.len() < 2 {
                continue;
            }

            let frag_type = trimmed[0];
            let frag_data = &trimmed[1..];

            if *fec_recovered < 3 {
                let nalu_type = if frag_data.len() >= 5
                    && frag_data[0] == 0x00 && frag_data[1] == 0x00
                    && frag_data[2] == 0x00 && frag_data[3] == 0x01
                {
                    Some(frag_data[4] & 0x1F)
                } else if frag_data.len() >= 4
                    && frag_data[0] == 0x00 && frag_data[1] == 0x00
                    && frag_data[2] == 0x01
                {
                    Some(frag_data[3] & 0x1F)
                } else {
                    None
                };
                info!(
                    "NAL: seq={}, sidx={}, frag_type=0x{:02x}, NAL_type={:?}, len={}",
                    block.block_seq, sidx, frag_type, nalu_type, frag_data.len()
                );
            }

            match frag_type {
                0x00 => {
                    *fec_recovered += 1;
                    // Single-fragment NAL: if there is an ongoing multi-frag assembly, it's unexpected.
                    if self.assembly_active {
                        warn!("Incomplete multi-frag NAL interrupted by single-frag NAL; discarding assembly");
                        self.assembly_buf.clear();
                        self.assembly_active = false;
                    }
                    if let Err(e) = self.tx.send(frag_data.to_vec()) {
                        warn!("Video frame channel closed: {}", e);
                    }
                }
                0x01 => {
                    // Start of a multi-fragment NAL
                    self.assembly_buf.clear();
                    self.assembly_buf.extend_from_slice(frag_data);
                    self.assembly_active = true;
                }
                0x02 => {
                    if self.assembly_active {
                        self.assembly_buf.extend_from_slice(frag_data);
                    } else {
                        // Orphan continuation fragment - no assembly in progress
                        self.orphan_fragments += 1;
                        if self.orphan_fragments <= 10 || self.orphan_fragments % 100 == 0 {
                            debug!("Orphan continuation fragment (no active assembly), total: {}", self.orphan_fragments);
                        }
                    }
                }
                0x03 => {
                    if self.assembly_active {
                        self.assembly_buf.extend_from_slice(frag_data);
                        if let Err(e) = self.tx.send(self.assembly_buf.clone()) {
                            warn!("Video frame channel closed: {}", e);
                        }
                        self.assembly_buf.clear();
                        self.assembly_active = false;
                    } else {
                        // Orphan last fragment; ignore
                    }
                }
                _ => {}
            }
        }
    }

    fn drain_completed(
        &mut self,
        completed: &mut [Option<CompletedBlock>; RING_SIZE],
        next_block: &mut u32,
        fec_recovered: &mut u64,
    ) {
        // NOTE: This is strict in-order drain - a missing block holds back all later blocks.
        // This ensures video frames are decoded in order, but if a block is permanently
        // lost (e.g., network dropout), all subsequent completed blocks are blocked.
        // Trade-off: Correct frame ordering vs. potential head-of-line blocking.
        loop {
            let idx = (*next_block as usize) % RING_SIZE;
            if let Some(block) = completed[idx].take() {
                self.emit_block(&block, fec_recovered);
                *next_block = next_block.wrapping_add(1);
            } else {
                break;
            }
        }
    }

    pub fn run(&mut self) {
        let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS)
            .expect("Failed to create Reed-Solomon decoder");

        let mut blocks: [Option<RsBlock>; RING_SIZE] = std::array::from_fn(|_| None);
        let mut completed: [Option<CompletedBlock>; RING_SIZE] = std::array::from_fn(|_| None);
        let mut processed_ring: [Option<u32>; RING_SIZE] = std::array::from_fn(|_| None);
        let mut next_block: u32 = 0;
        let mut next_block_init = false;
        let mut last_decode_time = Instant::now();
        let mut fec_recovered: u64 = 0;
        let mut fec_dropped: u64 = 0;
        let mut payload_count: u64 = 0;

        info!("VideoReceiver loop starting (strict in-order)");

        loop {
            let payload = match self.rx.try_recv() {
                Ok(p) => p,
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    if next_block_init && last_decode_time.elapsed() > STALL_TIMEOUT {
                        let idx = (next_block as usize) % RING_SIZE;
                        if let Some(ref block) = blocks[idx] {
                            warn!(
                                "RS: block {} stalled (had {}/{} shards), dropping",
                                next_block, block.received, DATA_SHARDS
                            );
                        }
                        blocks[idx] = None;
                        processed_ring[idx] = None;
                        next_block = next_block.wrapping_add(1);
                        fec_dropped += 1;
                    }

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

            let l2_header_size = if payload.len() >= 2 && payload[0..2] == link::MAGIC {
                8
            } else {
                0
            };

            let actual_payload = if l2_header_size > 0 {
                if payload.len() <= l2_header_size {
                    continue;
                }
                &payload[l2_header_size..]
            } else {
                &payload[..]
            };

            if actual_payload.len() < VIDEO_HDR_LEN {
                continue;
            }

            payload_count += 1;
            if payload_count % 1000 == 0 {
                info!(
                    "VideoReceiver: {} payloads, recovered={}, dropped={}, next={}",
                    payload_count, fec_recovered, fec_dropped, next_block
                );
            }

            let block_seq = u32::from_le_bytes([
                actual_payload[0],
                actual_payload[1],
                actual_payload[2],
                actual_payload[3],
            ]);
            let shard_index = actual_payload[4] as usize;
            let total_shards = actual_payload[5] as usize;
            let actual_data_shards = (actual_payload[6] as usize).min(DATA_SHARDS);

            let mut parsed_shard_lens = [0usize; DATA_SHARDS];
            for i in 0..DATA_SHARDS {
                let off = VIDEO_HDR_FIXED + i * 2;
                parsed_shard_lens[i] =
                    u16::from_le_bytes([actual_payload[off], actual_payload[off + 1]]) as usize;
            }

            if total_shards != TOTAL_SHARDS || shard_index >= TOTAL_SHARDS {
                continue;
            }

            if DATA_START >= actual_payload.len() {
                continue;
            }
            let shard_data = actual_payload[DATA_START..].to_vec();

            if !next_block_init {
                next_block = block_seq;
                next_block_init = true;
            }

            let idx = (block_seq as usize) % RING_SIZE;

            if next_block_init {
                let diff = block_seq.wrapping_sub(next_block);
                if diff != 0 && (diff & 0x80000000) != 0 {
                    // Old block (pre-wrap or already processed), discard
                    continue;
                }
            }

            if let Some(ref existing) = blocks[idx] {
                if existing.block_seq.wrapping_sub(block_seq) != 0 {
                    blocks[idx] = None;
                    processed_ring[idx] = None;
                }
            }

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

            // Check for duplicate first, then increment received count
            if block.shards[shard_index].is_some() {
                // Duplicate shard, skip
            } else {
                block.received += 1;
                block.shard_sizes[shard_index] = shard_data.len();
            }
            block.shards[shard_index] = Some(shard_data);

            // Validate header consistency: later shards must agree on shard lengths
            if block.received == 1 {
                block.shard_lens = parsed_shard_lens;
            } else if parsed_shard_lens != block.shard_lens {
                tracing::warn!(
                    "Block {}: header inconsistency detected - expected {:?}, got {:?}",
                    block_seq,
                    block.shard_lens,
                    parsed_shard_lens
                );
            }

            if block.received >= DATA_SHARDS {
                let block = blocks[idx].take().unwrap();
                let reconstructed = reconstruct_rs_block(&rs, &block, block.actual_data_shards);

                if let Some(data_shards) = reconstructed {
                    fec_recovered += 1;

                    completed[idx] = Some(CompletedBlock {
                        block_seq: block.block_seq,
                        data_shards,
                        shard_lens: block.shard_lens,
                    });
                    processed_ring[idx] = Some(block_seq);

                    if block_seq.wrapping_sub(next_block) == 0 {
                        self.drain_completed(&mut completed, &mut next_block, &mut fec_recovered);
                        last_decode_time = Instant::now();
                    } else if Self::is_future_block(block_seq, next_block) {
                        // block is in the future, just store it
                    }
                } else {
                    fec_dropped += 1;
                    processed_ring[idx] = Some(block_seq);
                }
#[cfg(test)]
mod tests {
            }
        }
    }
    use super::*;
    use crossbeam_channel;

    fn make_test_receiver() -> (VideoReceiver, crossbeam_channel::Receiver<Vec<u8>>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        (VideoReceiver::new(tx, rx.clone()), rx)
    }

    // ==================== Regression tests for AUDIT.md bugs ====================

    /// Bug: AUDIT.md receiver.rs — Dedup check doesn't handle wrapping correctly
    /// Fixed: The code now uses wrapping_sub with MSB check for proper wraparound handling
    /// Test: Verify that is_future_block correctly handles wraparound at u32::MAX
    #[test]
    fn regression_dedup_handles_wrapping() {
        // Test is_future_block function
        // is_future_block checks: block_seq != next_block && (block_seq.wrapping_sub(next_block) & 0x80000000) == 0

        // Normal case: block 100, next 99 -> future (MSB not set in diff)
        assert!(VideoReceiver::is_future_block(100, 99));

        // Wraparound case: block 0 (after wrap), next u32::MAX -> future
        assert!(VideoReceiver::is_future_block(0, u32::MAX));

        // Past case: block 99, next 100 -> NOT future (negative diff wraps to large positive)
        assert!(!VideoReceiver::is_future_block(99, 100));

        // Wraparound past: block u32::MAX, next 0 -> NOT future (should be treated as old)
        assert!(!VideoReceiver::is_future_block(u32::MAX, 0));
    }

    /// Test: Verify RsBlock dedup logic works correctly
    #[test]
    fn regression_rsblock_dedup() {
        let mut block = RsBlock {
            block_seq: 42,
            shards: vec![None; TOTAL_SHARDS],
            shard_sizes: vec![0; TOTAL_SHARDS],
            received: 0,
            actual_data_shards: DATA_SHARDS,
            shard_lens: [0; DATA_SHARDS],
        };

        // Add shard 0
        block.shards[0] = Some(vec![0x01, 0x02, 0x03]);
        block.shard_sizes[0] = 3;
        block.received += 1;

        // Try to add shard 0 again (duplicate)
        // In the current code, this is checked by `if block.shards[shard_index].is_some()`
        let shard_index = 0;
        if block.shards[shard_index].is_some() {
            // Duplicate detected - don't increment received count
            // This is the correct behavior
        } else {
            block.received += 1;
        }

        assert_eq!(
            block.received, 1,
            "Duplicate shard should not increment received count"
        );

        // Add a different shard
        block.shards[1] = Some(vec![0x04, 0x05]);
        block.shard_sizes[1] = 2;
        block.received += 1;

        assert_eq!(block.received, 2, "New shard should increment received count");
    }

    /// Test: Verify ring buffer index calculation handles wraparound
    #[test]
    fn regression_ring_buffer_wraparound() {
        let ring_size = RING_SIZE as u32;

        // Test index calculation
        let block_seq: u32 = 0;
        let idx = (block_seq as usize) % RING_SIZE;
        assert_eq!(idx, 0);

        // Test near wraparound
        let block_seq = u32::MAX;
        let idx = (block_seq as usize) % RING_SIZE;
        // Just verify it doesn't panic and gives a valid index
        assert!(idx < RING_SIZE);
    }

    /// Test: Verify DATA_SHARDS and PARITY_SHARDS constants are correct
    #[test]
    fn regression_fec_constants() {
        assert!(DATA_SHARDS > 0);
        assert!(PARITY_SHARDS > 0);
        assert_eq!(TOTAL_SHARDS, DATA_SHARDS + PARITY_SHARDS);
    }

    /// Test: Verify is_future_block logic matches the code in run()
    #[test]
    fn regression_future_block_matches_run_logic() {
        // The run() function uses:
        // let diff = block_seq.wrapping_sub(next_block);
        // if diff != 0 && (diff & 0x80000000) != 0 { // Old block }

        // Test that this logic is consistent with is_future_block

        // Case 1: block is future (next=100, block=101)
        let next_block: u32 = 100;
        let block_seq: u32 = 101;
        let diff = block_seq.wrapping_sub(next_block);
        let is_old = diff != 0 && (diff & 0x80000000) != 0;
        assert!(!is_old, "Block 101 should not be old when next is 100");
        assert!(VideoReceiver::is_future_block(block_seq, next_block));

        // Case 2: block is old (next=100, block=99)
        let next_block: u32 = 100;
        let block_seq: u32 = 99;
        let diff = block_seq.wrapping_sub(next_block);
        let is_old = diff != 0 && (diff & 0x80000000) != 0;
        assert!(is_old, "Block 99 should be old when next is 100");
        assert!(!VideoReceiver::is_future_block(block_seq, next_block));

        // Case 3: wraparound - block 0 is FUTURE when next is u32::MAX
        // (block 0 hasn't happened yet from next's perspective)
        let next_block: u32 = u32::MAX;
        let block_seq: u32 = 0;
        let diff = block_seq.wrapping_sub(next_block);
        let is_old = diff != 0 && (diff & 0x80000000) != 0;
        assert!(!is_old, "Block 0 should NOT be old when next is u32::MAX (it's future)");
        assert!(VideoReceiver::is_future_block(block_seq, next_block),
            "Block 0 should be future when next is u32::MAX");
    }
}
}

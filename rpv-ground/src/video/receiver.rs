use std::time::{Duration, Instant};
use tracing::{info, warn};
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
}

impl VideoReceiver {
    pub fn new(
        tx: crossbeam_channel::Sender<Vec<u8>>,
        rx: crossbeam_channel::Receiver<Vec<u8>>,
    ) -> Self {
        info!(
            "Video receiver (RS {}+{}) ready (strict in-order release)",
            DATA_SHARDS, PARITY_SHARDS
        );
        Self { tx, rx }
    }

    #[inline]
    fn is_future_block(block_seq: u32, next_block: u32) -> bool {
        block_seq != next_block && (block_seq.wrapping_sub(next_block) & 0x80000000) == 0
    }

    fn emit_block(&self, block: &CompletedBlock, fec_recovered: &mut u64) {
        let mut nal_buf: Vec<u8> = Vec::with_capacity(32768);
        let mut nal_started = false;

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
                    if let Err(e) = self.tx.send(frag_data.to_vec()) {
                        warn!("Video frame channel closed: {}", e);
                    }
                }
                0x01 => {
                    nal_buf.clear();
                    nal_buf.extend_from_slice(frag_data);
                    nal_started = true;
                }
                0x02 => {
                    if nal_started {
                        nal_buf.extend_from_slice(frag_data);
                    }
                }
                0x03 => {
                    if nal_started {
                        nal_buf.extend_from_slice(frag_data);
                        if let Err(e) = self.tx.send(nal_buf.clone()) {
                            warn!("Video frame channel closed: {}", e);
                        }
                        nal_buf.clear();
                        nal_started = false;
                    }
                }
                _ => {}
            }
        }
    }

    fn drain_completed(
        &self,
        completed: &mut [Option<CompletedBlock>; RING_SIZE],
        next_block: &mut u32,
        fec_recovered: &mut u64,
    ) {
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

    pub fn run(&self) {
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

            if let Some(ref existing) = blocks[idx] {
                if existing.block_seq != block_seq {
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

            if block.shards[shard_index].is_none() {
                block.received += 1;
                block.shard_sizes[shard_index] = shard_data.len();
            }
            block.shards[shard_index] = Some(shard_data);

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

                    if block_seq == next_block {
                        self.drain_completed(&mut completed, &mut next_block, &mut fec_recovered);
                        last_decode_time = Instant::now();
                    } else if Self::is_future_block(block_seq, next_block) {
                        // block is in the future, just store it
                    }
                } else {
                    fec_dropped += 1;
                    processed_ring[idx] = Some(block_seq);
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
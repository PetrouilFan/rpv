use std::collections::HashMap;
use std::os::unix::io::AsRawFd;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{info, warn};

use reed_solomon_erasure::galois_8::ReedSolomon;

const DATA_SHARDS: usize = 2;
const PARITY_SHARDS: usize = 1;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;

#[derive(Clone)]
pub struct VideoFrame {
    pub data: Vec<u8>,
    pub send_ts_us: Option<u64>,
    pub recv_time: Instant,
}

struct RsBlock {
    shards: Vec<Option<Vec<u8>>>,
    shard_sizes: Vec<usize>,
    received: usize,
    first_recv: Instant,
}

pub struct VideoReceiver {
    tx: mpsc::Sender<VideoFrame>,
    port: u16,
    cam_ip: std::sync::Arc<std::sync::Mutex<Option<std::net::IpAddr>>>,
}

impl VideoReceiver {
    pub async fn new(
        port: u16,
        tx: mpsc::Sender<VideoFrame>,
        cam_ip: std::sync::Arc<std::sync::Mutex<Option<std::net::IpAddr>>>,
    ) -> std::io::Result<Self> {
        info!(
            "Video receiver (RS {}+{}) ready on port {}",
            DATA_SHARDS,
            PARITY_SHARDS,
            port
        );
        Ok(Self { tx, port, cam_ip })
    }

    pub async fn run(&self) {
        let bind_addr = format!("0.0.0.0:{}", self.port);
        let socket = match UdpSocket::bind(&bind_addr).await {
            Ok(s) => {
                let fd = s.as_raw_fd();
                let rcvbuf: libc::c_int = 4 * 1024 * 1024;
                unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_RCVBUF,
                        &rcvbuf as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    );
                }
                s
            }
            Err(e) => {
                warn!("Failed to bind video socket on {}: {}", bind_addr, e);
                return;
            }
        };
        info!("Video receiver listening on {}", bind_addr);

        let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS)
            .expect("Failed to create Reed-Solomon decoder");

        let mut buf = vec![0u8; 65536];
        let mut blocks: HashMap<u32, RsBlock> = HashMap::new();
        let mut next_block: Option<u32> = None;
        let mut last_decode_time = Instant::now();
        let mut block_count: u64 = 0;

        // FU-A reassembly state
        let mut nal_buf: Vec<u8> = Vec::new();
        let mut nal_started = false;

        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    // Enforce source IP filter
                    {
                        let guard = self.cam_ip.lock().unwrap();
                        if let Some(allowed_ip) = *guard {
                            if allowed_ip != src.ip() {
                                continue;
                            }
                        }
                    }
                    let recv_time = Instant::now();

                    // Header: [4B seq][1B shard_index][1B total_shards][1B data_shards][1B pad][2B shard_len] = 10 bytes
                    if len < 10 {
                        continue;
                    }

                    let block_seq = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                    let shard_index = buf[4] as usize;
                    let total_shards = buf[5] as usize;
                    let _data_shards = buf[6] as usize;  // actual data shards (may be < DATA_SHARDS for partial)
                    let shard_len = u16::from_le_bytes([buf[8], buf[9]]) as usize;

                    if total_shards != TOTAL_SHARDS || shard_index >= TOTAL_SHARDS {
                        warn!(
                            "RS: invalid shard idx={} total={}",
                            shard_index, total_shards
                        );
                        continue;
                    }

                    let payload_start = 10;
                    let payload_end = payload_start + shard_len;
                    if payload_end > len {
                        continue;
                    }
                    let payload = buf[payload_start..payload_end].to_vec();

                    // Initialize next_block if needed
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

                    // Clean up old blocks
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
                        first_recv: recv_time,
                    });

                    if block.shards[shard_index].is_none() {
                        block.received += 1;
                        block.shard_sizes[shard_index] = shard_len;
                    }
                    block.shards[shard_index] = Some(payload);

                    // Try to reconstruct and send immediately
                    if let Some(current_seq) = next_block {
                        if block_seq == current_seq {
                            let block = blocks.get(&block_seq).unwrap();
                            if block.received >= DATA_SHARDS {
                                // Enough shards received, attempt RS reconstruction
                                let reconstructed = reconstruct_rs_block(&rs, block);
                                if let Some(data_shards) = reconstructed {
                                    // Process shards: FU-A reassembly
                                    for shard_data in &data_shards {
                                        if shard_data.is_empty() {
                                            continue;
                                        }
                                        let frag_index = shard_data[0];
                                        let frag_payload = &shard_data[1..];

                                        if frag_index == 0 {
                                            // Start of a new NALU
                                            if nal_started && !nal_buf.is_empty() {
                                                // Flush previous NALU
                                                let frame = VideoFrame {
                                                    data: std::mem::take(&mut nal_buf),
                                                    send_ts_us: None,
                                                    recv_time: block.first_recv,
                                                };
                                                if let Err(e) = self.tx.try_send(frame) {
                                                    if block_count % 60 == 0 {
                                                        warn!(
                                                            "Video frame channel full, dropping: {}",
                                                            e
                                                        );
                                                    }
                                                }
                                                block_count += 1;
                                            }
                                            nal_buf.clear();
                                            nal_buf.extend_from_slice(&[0, 0, 0, 1]); // Annex-B start code
                                            nal_buf.extend_from_slice(frag_payload);
                                            nal_started = true;
                                        } else if nal_started {
                                            // Continuation fragment
                                            nal_buf.extend_from_slice(frag_payload);
                                        }
                                        // If !nal_started and frag_index != 0, discard orphan fragment
                                    }
                                } else {
                                    // FEC reconstruction failed - drop entire NALU
                                    if nal_started {
                                        nal_buf.clear();
                                        nal_started = false;
                                    }
                                }

                                blocks.remove(&block_seq);
                                next_block = Some(current_seq.wrapping_add(1));
                                last_decode_time = Instant::now();
                            } else if last_decode_time.elapsed().as_millis() > 50 {
                                // Stall timeout - drop block
                                warn!(
                                    "RS: block {} stalled (had {}/{} shards), dropping",
                                    current_seq, block.received, DATA_SHARDS
                                );
                                // Drop entire NALU if we were accumulating
                                if nal_started {
                                    nal_buf.clear();
                                    nal_started = false;
                                }
                                blocks.remove(&block_seq);
                                next_block = Some(current_seq.wrapping_add(1));
                                last_decode_time = Instant::now();
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Video receive error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }
        }
    }
}

fn reconstruct_rs_block(
    rs: &ReedSolomon,
    block: &RsBlock,
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

    // Return only the data shards (first DATA_SHARDS)
    let mut result = Vec::with_capacity(DATA_SHARDS);
    for i in 0..DATA_SHARDS {
        result.push(shard_refs[i].clone().unwrap_or_default());
    }
    Some(result)
}

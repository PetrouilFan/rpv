use std::collections::HashMap;
use std::os::unix::io::AsRawFd;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{info, warn, error};

use reed_solomon_erasure::ReedSolomon;

const DATA_SHARDS: usize = 4;
const PARITY_SHARDS: usize = 2;

#[derive(Clone)]
pub struct VideoFrame {
    pub data: Vec<u8>,
    pub send_ts_us: Option<u64>,
    pub recv_time: Instant,
}

struct FecGroup {
    shards: Vec<Option<Vec<u8>>>,
    shard_size: usize,
    received: usize,
    first_recv: Instant,
}

pub struct VideoReceiver {
    tx: mpsc::Sender<VideoFrame>,
    port: u16,
}

impl VideoReceiver {
    pub async fn new(port: u16, tx: mpsc::Sender<VideoFrame>) -> std::io::Result<Self> {
        info!("Video receiver (FEC {}+{}) ready on port {}", DATA_SHARDS, PARITY_SHARDS, port);
        Ok(Self { tx, port })
    }

    pub async fn run(&self) {
        let bind_addr = format!("0.0.0.0:{}", self.port);
        let socket = match UdpSocket::bind(&bind_addr).await {
            Ok(s) => {
                // Set 4MB receive buffer to prevent kernel-side drops
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
        let mut fec_groups: HashMap<u32, FecGroup> = HashMap::new();
        let mut next_seq: Option<u32> = None;
        let mut last_decode_time = Instant::now();
        let mut latencies: Vec<u64> = Vec::new();
        let mut frame_count: u64 = 0;

        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, _addr)) => {
                    let recv_time = Instant::now();

                    if len < 8 {
                        let frame = VideoFrame { data: buf[..len].to_vec(), send_ts_us: None, recv_time };
                        let _ = self.tx.try_send(frame);
                        continue;
                    }

                    // Parse header: [4B seq][1B shard_index][1B total_shards][2B shard_len]
                    let seq = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                    let shard_index = buf[4] as usize;
                    let total_shards = buf[5] as usize;
                    let shard_len = u16::from_le_bytes([buf[6], buf[7]]) as usize;

                    let header_size = 8;
                    let payload_available = len - header_size;

                    // Validate total_shards matches expected FEC configuration
                    if total_shards != DATA_SHARDS + PARITY_SHARDS {
                        warn!("FEC: unexpected total_shards={} (expected {}), dropping packet",
                            total_shards, DATA_SHARDS + PARITY_SHARDS);
                        continue;
                    }

                    if shard_index >= total_shards || shard_len == 0 || shard_len > payload_available {
                        warn!("FEC: invalid shard idx={} total={} len={} avail={} pkt_len={}",
                            shard_index, total_shards, shard_len, payload_available, len);
                        continue;
                    }

                    let shard_data = buf[header_size..header_size + shard_len].to_vec();

                    // Track sequence group
                    if next_seq.is_none() {
                        next_seq = Some(seq);
                    } else if let Some(current) = next_seq {
                        // Detect camera restart: if seq is much lower than expected, reset
                        let gap = current.wrapping_sub(seq);
                        if gap > 1000 {
                            info!("FEC: camera restarted, seq reset {} -> {}", current, seq);
                            next_seq = Some(seq);
                            fec_groups.clear();
                        }
                    }

                    // Clean up old groups (keep current and a few future)
                    fec_groups.retain(|&k, _| {
                        k.wrapping_sub(next_seq.unwrap_or(k)) < 8
                    });

                    let group = fec_groups.entry(seq).or_insert_with(|| FecGroup {
                        shards: vec![None; total_shards],
                        shard_size: shard_len,
                        received: 0,
                        first_recv: Instant::now(),
                    });

                    if group.shards[shard_index].is_none() {
                        group.received += 1;
                    }
                    group.shards[shard_index] = Some(shard_data);
                    group.shard_size = shard_len;

                    // Check if we can decode this group
                    if let Some(current_seq) = next_seq {
                        if seq == current_seq {
                            let group = fec_groups.get(&seq).unwrap();
                            if group.received >= DATA_SHARDS {
                                let group_clone = FecGroup {
                                    shards: group.shards.clone(),
                                    shard_size: group.shard_size,
                                    received: group.received,
                                    first_recv: group.first_recv,
                                };
                                decode_and_send(&rs, &group_clone, &mut fec_groups, &mut next_seq, &mut last_decode_time, &self.tx, &mut latencies, &mut frame_count);
                            }
                        }
                    }

                    // FEC stall recovery: skip stalled group after 200ms
                    if let Some(current_seq) = next_seq {
                        if last_decode_time.elapsed().as_millis() > 200 && fec_groups.contains_key(&current_seq) {
                            let group = &fec_groups[&current_seq];
                            if group.received < DATA_SHARDS {
                                warn!("FEC: seq {} stalled for >200ms, skipping (had {}/{} shards)",
                                    current_seq, group.received, DATA_SHARDS);
                                fec_groups.remove(&current_seq);
                                next_seq = Some(current_seq.wrapping_add(1));
                                last_decode_time = Instant::now();
                            } else {
                                // Force decode: have enough shards but missed normal trigger
                                let group_clone = FecGroup {
                                    shards: group.shards.clone(),
                                    shard_size: group.shard_size,
                                    received: group.received,
                                    first_recv: group.first_recv,
                                };
                                decode_and_send(&rs, &group_clone, &mut fec_groups, &mut next_seq, &mut last_decode_time, &self.tx, &mut latencies, &mut frame_count);
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Video receive error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }
    }
}

fn decode_and_send(
    rs: &reed_solomon_erasure::galois_8::ReedSolomon,
    group: &FecGroup,
    fec_groups: &mut HashMap<u32, FecGroup>,
    next_seq: &mut Option<u32>,
    last_decode_time: &mut Instant,
    tx: &mpsc::Sender<VideoFrame>,
    latencies: &mut Vec<u64>,
    frame_count: &mut u64,
) {
    let current_seq = next_seq.unwrap();
    let fec_start = Instant::now();

    let decoded = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode_fec_group(rs, group))) {
        Ok(d) => d,
        Err(_) => {
            error!("FEC: decode_fec_group panicked for seq {}", current_seq);
            fec_groups.remove(&current_seq);
            *next_seq = Some(current_seq.wrapping_add(1));
            *last_decode_time = Instant::now();
            return;
        }
    };

    let fec_us = fec_start.elapsed().as_micros() as u64;
    let group_delay_us = group.first_recv.elapsed().as_micros() as u64;

    if decoded.is_empty() {
        warn!("FEC: decode_fec_group returned empty for seq {}", current_seq);
        fec_groups.remove(&current_seq);
        *next_seq = Some(current_seq.wrapping_add(1));
        *last_decode_time = Instant::now();
        return;
    }

    for (i, chunk) in decoded.iter().enumerate() {
        let send_ts_us = if i == 0 && chunk.len() >= 8 {
            Some(u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3],
                chunk[4], chunk[5], chunk[6], chunk[7],
            ]))
        } else {
            None
        };

        let frame = VideoFrame {
            data: if send_ts_us.is_some() { chunk[8..].to_vec() } else { chunk.clone() },
            send_ts_us,
            recv_time: group.first_recv,
        };

        if let Some(send_ts) = send_ts_us {
            let now_us = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64;
            let e2e_us = now_us.saturating_sub(send_ts);
            latencies.push(e2e_us);
            *frame_count += 1;

            if *frame_count % 30 == 1 {
                let avg = latencies.iter().sum::<u64>() / latencies.len() as u64;
                let min = *latencies.iter().min().unwrap_or(&0);
                let max = *latencies.iter().max().unwrap_or(&0);
                let p50 = {
                    let mut sorted = latencies.clone();
                    sorted.sort();
                    sorted[sorted.len() / 2]
                };
                info!(
                    "LATENCY stats (n={}): avg={:.1}ms min={:.1}ms p50={:.1}ms max={:.1}ms | fec={}us group={}us",
                    latencies.len(),
                    avg as f64 / 1000.0,
                    min as f64 / 1000.0,
                    p50 as f64 / 1000.0,
                    max as f64 / 1000.0,
                    fec_us,
                    group_delay_us,
                );
                latencies.clear();
            }
        }

        if let Err(e) = tx.try_send(frame) {
            warn!("FEC: video frame channel full, dropping frame: {}", e);
        }
    }

    fec_groups.remove(&current_seq);
    *next_seq = Some(current_seq.wrapping_add(1));
    *last_decode_time = Instant::now();
}

fn decode_fec_group(
    rs: &reed_solomon_erasure::galois_8::ReedSolomon,
    group: &FecGroup,
) -> Vec<Vec<u8>> {
    let mut shards: Vec<Option<Vec<u8>>> = group.shards.iter()
        .map(|s| s.as_ref().map(|v| v.clone()))
        .collect();

    let missing_count = shards.iter().filter(|s| s.is_none()).count();

    if missing_count > 0 {
        if missing_count <= PARITY_SHARDS {
            info!("FEC: reconstructing {} missing shards", missing_count);
        } else {
            warn!("FEC: {} missing shards exceeds parity ({}), dropping group", missing_count, PARITY_SHARDS);
            return Vec::new();
        }

        if rs.reconstruct(&mut shards).is_err() {
            warn!("FEC: reconstruction failed, dropping group");
            return Vec::new();
        }
    }

    let mut result = Vec::with_capacity(DATA_SHARDS);
    for shard in shards.iter().take(DATA_SHARDS) {
        if let Some(data) = shard {
            result.push(data.clone());
        }
    }
    result
}

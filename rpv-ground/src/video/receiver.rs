use std::collections::HashMap;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{info, warn};

use reed_solomon_erasure::ReedSolomon;

const DATA_SHARDS: usize = 4;
const PARITY_SHARDS: usize = 2;

#[derive(Clone)]
pub struct VideoFrame {
    pub data: Vec<u8>,
}

struct FecGroup {
    shards: Vec<Option<Vec<u8>>>,
    shard_size: usize,
    received: usize,
}

pub struct VideoReceiver {
    tx: mpsc::UnboundedSender<VideoFrame>,
    port: u16,
}

impl VideoReceiver {
    pub async fn new(port: u16, tx: mpsc::UnboundedSender<VideoFrame>) -> std::io::Result<Self> {
        info!("Video receiver (FEC 4+2) ready on port {}", port);
        Ok(Self { tx, port })
    }

    pub async fn run(&self) {
        let bind_addr = format!("0.0.0.0:{}", self.port);
        let socket = match UdpSocket::bind(&bind_addr).await {
            Ok(s) => s,
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

        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, _addr)) => {
                    if len < 8 {
                        // Too short for header, send raw
                        let frame = VideoFrame { data: buf[..len].to_vec() };
                        let _ = self.tx.send(frame);
                        continue;
                    }

                    // Parse header: [4B seq][1B shard_index][1B total_shards][2B shard_len]
                    let seq = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                    let shard_index = buf[4] as usize;
                    let total_shards = buf[5] as usize;
                    let shard_len = u16::from_le_bytes([buf[6], buf[7]]) as usize;

                    let header_size = 8;
                    let payload_available = len - header_size;

                    if shard_index >= total_shards || shard_len == 0 || shard_len > payload_available {
                        // Malformed header, treat as raw data
                        let frame = VideoFrame { data: buf[..len].to_vec() };
                        let _ = self.tx.send(frame);
                        continue;
                    }

                    let shard_data = buf[header_size..header_size + shard_len].to_vec();

                    // Track sequence group
                    if next_seq.is_none() {
                        next_seq = Some(seq);
                    }

                    // Clean up old groups (keep current and a few future)
                    fec_groups.retain(|&k, _| {
                        k.wrapping_sub(next_seq.unwrap_or(k)) < 8
                    });

            let group = fec_groups.entry(seq).or_insert_with(|| FecGroup {
                shards: vec![None; total_shards],
                shard_size: shard_len,
                received: 0,
            });

                    if group.shards[shard_index].is_none() {
                        group.received += 1;
                    }
                    group.shards[shard_index] = Some(shard_data);
                    group.shard_size = shard_len;

                    // Check if we can decode this group
                    if let Some(current_seq) = next_seq {
                        if seq == current_seq && group.received >= DATA_SHARDS {
                            let decoded = decode_fec_group(&rs, group);
                            for chunk in decoded {
                                let frame = VideoFrame { data: chunk };
                                let _ = self.tx.send(frame);
                            }
                            fec_groups.remove(&current_seq);
                            next_seq = Some(current_seq.wrapping_add(1));
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

fn decode_fec_group(
    rs: &reed_solomon_erasure::galois_8::ReedSolomon,
    group: &FecGroup,
) -> Vec<Vec<u8>> {
    let _shard_size = group.shard_size;

    // Build Option<Vec<u8>> shards for reconstruction
    let mut shards: Vec<Option<Vec<u8>>> = group.shards.iter()
        .map(|s| s.as_ref().map(|v| v.clone()))
        .collect();

    // Count missing shards
    let missing_count = shards.iter().filter(|s| s.is_none()).count();

    if missing_count > 0 {
        warn!("FEC: missing {} shards, attempting reconstruction", missing_count);

        if rs.reconstruct(&mut shards).is_ok() {
            info!("FEC: successfully reconstructed {} missing shards", missing_count);
        } else {
            warn!("FEC: reconstruction failed, dropping group");
            return Vec::new();
        }
    }

    // Extract data shards (first DATA_SHARDS)
    let mut result = Vec::with_capacity(DATA_SHARDS);
    for shard in shards.iter().take(DATA_SHARDS) {
        if let Some(data) = shard {
            result.push(data.clone());
        }
    }
    result
}

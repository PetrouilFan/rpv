use std::io::{BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use reed_solomon_erasure::ReedSolomon;

use crate::link;
use crate::rawsock::RawSocket;

const DATA_SHARDS: usize = 2;
const PARITY_SHARDS: usize = 1;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;
const MAX_NAL_BUF: usize = 512 * 1024;

/// Maximum shard payload: L2 header (8) + video packet header (10) + shard data
/// Must stay under ~1400 bytes for safe 802.11 frames.
const MAX_SHARD_PAYLOAD: usize = link::MAX_PAYLOAD - 10; // 10 bytes for video packet header

/// Run the video capture and streaming loop.
///
/// * `running` — shared shutdown flag
/// * `socket` — shared raw AF_PACKET socket (already bound to wlan in monitor mode)
/// * `drone_id` — L2 header drone ID for filtering
/// * `bitrate` — rpicam-vid bitrate (e.g. 3_000_000)
/// * `intra` — keyframe interval (e.g. 10)
pub fn run(
    running: Arc<AtomicBool>,
    socket: Arc<RawSocket>,
    drone_id: u8,
    bitrate: u32,
    intra: u32,
) {
    tracing::info!(
        "Video sender ready (FEC {}+{}, L2 broadcast)",
        DATA_SHARDS,
        PARITY_SHARDS
    );

    let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS)
        .expect("Failed to create Reed-Solomon encoder");

    let mut l2_seq: u32 = 0;

    while running.load(Ordering::SeqCst) {
        tracing::info!(
            "Starting rpicam-vid (bitrate={}, intra={})...",
            bitrate,
            intra
        );

        let bitrate_s = bitrate.to_string();
        let intra_s = intra.to_string();
        let child = Command::new("rpicam-vid")
            .args(&[
                "--width",
                "960",
                "--height",
                "540",
                "--framerate",
                "30",
                "--codec",
                "h264",
                "--profile",
                "baseline",
                "--level",
                "4.1",
                "--bitrate",
                &bitrate_s,
                "--low-latency",
                "--flush",
                "--inline",
                "--intra",
                &intra_s,
                "--nopreview",
                "-t",
                "0",
                "-o",
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to start rpicam-vid: {}", e);
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        let stdout = child.stdout.take().expect("No stdout");
        let mut reader = BufReader::new(stdout);

        let stderr = child.stderr.take();
        thread::spawn(move || {
            if let Some(mut stderr) = stderr {
                let mut buf = Vec::new();
                let _ = stderr.read_to_end(&mut buf);
                if !buf.is_empty() {
                    let stderr_str = String::from_utf8_lossy(&buf);
                    if stderr_str.contains("ERROR") || stderr_str.contains("failed") {
                        tracing::error!("rpicam-vid stderr: {}", stderr_str);
                    }
                }
            }
        });

        tracing::info!(
            "rpicam-vid started, streaming H.264 with FEC {}+{} over raw L2...",
            DATA_SHARDS,
            PARITY_SHARDS
        );

        let mut buf = vec![0u8; 65536];
        let mut total_bytes = u64::default();
        let mut fail_count: u8 = 0;
        let mut fec_buffer: Vec<Vec<u8>> = Vec::with_capacity(DATA_SHARDS);
        let mut nal_buf: Vec<u8> = Vec::new();

        while running.load(Ordering::SeqCst) {
            match reader.read(&mut buf) {
                Ok(0) => {
                    tracing::info!("rpicam-vid stdout closed");
                    break;
                }
                Ok(n) => {
                    nal_buf.extend_from_slice(&buf[..n]);
                    total_bytes += n as u64;

                    while let Some(nal) = extract_next_nal(&mut nal_buf) {
                        let mut off = 0;
                        let mut frag_idx: u8 = 0;
                        while off < nal.len() {
                            let end = (off + MAX_SHARD_PAYLOAD).min(nal.len());
                            let mut frag = Vec::with_capacity(1 + end - off);
                            frag.push(frag_idx);
                            frag.extend_from_slice(&nal[off..end]);
                            fec_buffer.push(frag);
                            off = end;
                            frag_idx += 1;

                            if fec_buffer.len() == DATA_SHARDS {
                                send_fec_group(
                                    &socket,
                                    &rs,
                                    &fec_buffer,
                                    drone_id,
                                    &mut l2_seq,
                                    &mut fail_count,
                                );
                                fec_buffer.clear();
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Read error: {}", e);
                    break;
                }
            }
        }

        // Force-flush trailing NAL if buffer begins with a valid start code
        if nal_buf.len() > 4 && nal_buf[0] == 0 && nal_buf[1] == 0 {
            let start_code_len = if nal_buf[2] == 0 && nal_buf[3] == 1 {
                4
            } else if nal_buf[2] == 1 {
                3
            } else {
                0
            };
            if start_code_len > 0 {
                let nal = nal_buf[start_code_len..].to_vec();
                if !nal.is_empty() {
                    let mut off = 0;
                    let mut frag_idx: u8 = 0;
                    while off < nal.len() {
                        let end = (off + MAX_SHARD_PAYLOAD).min(nal.len());
                        let mut frag = Vec::with_capacity(1 + end - off);
                        frag.push(frag_idx);
                        frag.extend_from_slice(&nal[off..end]);
                        fec_buffer.push(frag);
                        if fec_buffer.len() == DATA_SHARDS {
                            send_fec_group(
                                &socket,
                                &rs,
                                &fec_buffer,
                                drone_id,
                                &mut l2_seq,
                                &mut fail_count,
                            );
                            fec_buffer.clear();
                        }
                        off = end;
                        frag_idx += 1;
                    }
                }
            }
            nal_buf.clear();
        }

        // Send any remaining partial group
        if !fec_buffer.is_empty() {
            send_fec_group(
                &socket,
                &rs,
                &fec_buffer,
                drone_id,
                &mut l2_seq,
                &mut fail_count,
            );
            fec_buffer.clear();
        }

        let _ = child.kill();
        let _ = child.wait();

        tracing::info!(
            "rpicam-vid stopped, sent {:.1} MB total",
            total_bytes as f64 / 1_048_576.0
        );

        if running.load(Ordering::SeqCst) {
            tracing::info!("Restarting in 2 seconds...");
            thread::sleep(Duration::from_secs(2));
        }
    }
}

fn extract_next_nal(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buf.len() > MAX_NAL_BUF {
        tracing::warn!("NAL buffer overflow ({}B), resetting", buf.len());
        buf.clear();
        return None;
    }
    let mut start = None;
    for i in 0..buf.len().saturating_sub(3) {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if buf[i + 2] == 0 && i + 3 < buf.len() && buf[i + 3] == 1 {
                start = Some(i);
                break;
            }
            if buf[i + 2] == 1 {
                start = Some(i);
                break;
            }
        }
    }
    let start = start?;

    let sc_len = if start + 3 < buf.len() && buf[start + 2] == 0 && buf[start + 3] == 1 {
        4
    } else {
        3
    };

    let search_from = start + sc_len;
    let mut end = None;
    for i in search_from..buf.len().saturating_sub(3) {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if buf[i + 2] == 0 && i + 3 < buf.len() && buf[i + 3] == 1 {
                end = Some(i);
                break;
            }
            if buf[i + 2] == 1 {
                end = Some(i);
                break;
            }
        }
    }

    if let Some(end) = end {
        let nal = buf[start + sc_len..end].to_vec();
        buf.drain(..end);
        Some(nal)
    } else {
        None
    }
}

fn send_fec_group(
    socket: &RawSocket,
    rs: &reed_solomon_erasure::galois_8::ReedSolomon,
    chunks: &[Vec<u8>],
    drone_id: u8,
    l2_seq: &mut u32,
    fail_count: &mut u8,
) {
    if chunks.is_empty() {
        return;
    }

    let shard_size = chunks.iter().map(|c| c.len()).max().unwrap_or(1);

    let mut shards: Vec<Vec<u8>> = Vec::with_capacity(TOTAL_SHARDS);
    for chunk in chunks {
        let mut shard = vec![0u8; shard_size];
        shard[..chunk.len()].copy_from_slice(chunk);
        shards.push(shard);
    }
    while shards.len() < DATA_SHARDS {
        shards.push(vec![0u8; shard_size]);
    }
    for _ in 0..PARITY_SHARDS {
        shards.push(vec![0u8; shard_size]);
    }

    if let Err(e) = rs.encode(&mut shards) {
        tracing::warn!("Reed-Solomon encode error: {:?}", e);
        return;
    }

    let mut group_ok = true;
    for (i, shard) in shards.iter().enumerate() {
        // Video packet header: [4B seq][1B shard_idx][1B total_shards][1B data_shards][1B pad][2B shard_len] = 10 bytes
        let mut payload = Vec::with_capacity(10 + shard.len());
        payload.extend_from_slice(&l2_seq.to_le_bytes());
        payload.push(i as u8);
        payload.push(TOTAL_SHARDS as u8);
        payload.push(chunks.len() as u8);
        payload.push(0u8);
        payload.extend_from_slice(&(shard.len() as u16).to_le_bytes());
        payload.extend_from_slice(shard);

        // Wrap in L2 header and send
        let header = link::L2Header {
            drone_id,
            payload_type: link::PAYLOAD_VIDEO,
            seq: *l2_seq,
        };
        let frame = header.encode(&payload);

        match socket.send(&frame) {
            Ok(_) => {}
            Err(e) => {
                group_ok = false;
                *fail_count = fail_count.saturating_add(1);
                if *fail_count <= 5 {
                    tracing::warn!("Video send error: {}", e);
                }
                if *fail_count > 30 {
                    tracing::warn!("Too many send failures, retrying...");
                    *fail_count = 0;
                    return;
                }
            }
        }
    }

    *l2_seq = l2_seq.wrapping_add(1);
    if group_ok {
        *fail_count = 0;
    }
}

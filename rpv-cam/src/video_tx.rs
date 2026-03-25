use std::io::{BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use reed_solomon_erasure::ReedSolomon;

use crate::link;
use crate::rawsock::RawSocket;

const DATA_SHARDS: usize = 2;
const PARITY_SHARDS: usize = 1;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;
const MAX_NAL_BUF: usize = 512 * 1024;

/// Maximum shard data bytes per fragment.
/// Wire frame = 802.11(24) + Radiotap(8) + L2(8) + video_hdr + frag_idx(2) + shard_data.
/// video_hdr = 12 bytes for 2+1 FEC: [4B seq][1B idx][1B total][1B data][1B pad][2B s0][2B s1].
/// Radiotap is prepended by rawsock::send, not counted here.
/// Constraint: L2(8) + video_hdr + frag_idx(2) + shard_data <= MAX_PAYLOAD(1400).
const VIDEO_HDR_LEN: usize = 12;
const FRAG_HDR_LEN: usize = 2; // u16 fragment index
const MAX_SHARD_DATA: usize = link::MAX_PAYLOAD - 8 - VIDEO_HDR_LEN - FRAG_HDR_LEN;

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

    let mut fec_block_seq: u32 = 0;
    let mut l2_pkt_seq: u32 = 0;

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
        // Cursor-based NAL buffer: head/tail avoid O(n) shifting on every extraction.
        // Only compact (copy_within) when the buffer fills up.
        let mut nal_buf: Vec<u8> = vec![0u8; MAX_NAL_BUF];
        let mut nal_head: usize = 0;
        let mut nal_tail: usize = 0;
        // Tracks consecutive read cycles with zero NAL extractions while buffer is full.
        // If excessive, the stream is garbage and we force-reset to avoid spinning.
        let mut nal_idle_cycles: u32 = 0;
        const NAL_IDLE_LIMIT: u32 = 200;
        // Reusable buffers for send path (avoids per-packet allocations)
        let mut l2_frame_buf: Vec<u8> = Vec::with_capacity(link::MAX_PAYLOAD);
        let mut send_buf: Vec<u8> = Vec::with_capacity(8 + 24 + link::MAX_PAYLOAD);
        let mut video_payload_buf: Vec<u8> = Vec::with_capacity(VIDEO_HDR_LEN + MAX_SHARD_DATA);

        while running.load(Ordering::SeqCst) {
            match reader.read(&mut buf) {
                Ok(0) => {
                    tracing::info!("rpicam-vid stdout closed");
                    break;
                }
                Ok(n) => {
                    total_bytes += n as u64;

                    // Append new bytes into the NAL cursor buffer
                    let copy_len = n.min(nal_buf.len().saturating_sub(nal_tail));
                    if copy_len == 0 {
                        // Buffer full — compact first
                        if nal_head > 0 {
                            let remaining = nal_tail - nal_head;
                            nal_buf.copy_within(nal_head..nal_tail, 0);
                            nal_head = 0;
                            nal_tail = remaining;
                            let retry = n.min(nal_buf.len().saturating_sub(nal_tail));
                            nal_buf[nal_tail..nal_tail + retry].copy_from_slice(&buf[..retry]);
                            nal_tail += retry;
                        }
                        // If still full after compaction, data is lost (overflow handled below)
                    } else {
                        nal_buf[nal_tail..nal_tail + copy_len].copy_from_slice(&buf[..copy_len]);
                        nal_tail += copy_len;
                    }

                    // Extract complete NAL units using cursor (no drain/truncate)
                    let mut extracted_any = false;
                    while let Some((nal, consumed)) =
                        extract_next_nal_cursor(&nal_buf, nal_head, nal_tail)
                    {
                        nal_head += consumed;
                        extracted_any = true;

                        let mut off = 0;
                        let mut frag_idx: u16 = 0;
                        while off < nal.len() {
                            let end = (off + MAX_SHARD_DATA).min(nal.len());
                            let mut frag = Vec::with_capacity(FRAG_HDR_LEN + end - off);
                            frag.extend_from_slice(&frag_idx.to_le_bytes());
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
                                    &mut fec_block_seq,
                                    &mut l2_pkt_seq,
                                    &mut fail_count,
                                    &mut l2_frame_buf,
                                    &mut send_buf,
                                    &mut video_payload_buf,
                                );
                                fec_buffer.clear();
                            }
                        }
                    }

                    // Compact when unconsumed prefix gets large
                    if nal_head > 0 && nal_head == nal_tail {
                        nal_head = 0;
                        nal_tail = 0;
                        nal_idle_cycles = 0;
                    } else if nal_head > 0 && nal_head > nal_buf.len() / 2 {
                        let remaining = nal_tail - nal_head;
                        nal_buf.copy_within(nal_head..nal_tail, 0);
                        nal_head = 0;
                        nal_tail = remaining;
                    }

                    // Idle detection: if buffer is full and we extracted nothing,
                    // the stream may be garbage. Force-reset after too many idle cycles.
                    if extracted_any {
                        nal_idle_cycles = 0;
                    } else if nal_tail - nal_head >= nal_buf.len() {
                        nal_idle_cycles += 1;
                        if nal_idle_cycles >= NAL_IDLE_LIMIT {
                            tracing::warn!(
                                "NAL buffer idle reset ({} cycles, {}B unparseable)",
                                nal_idle_cycles,
                                nal_tail - nal_head
                            );
                            nal_head = 0;
                            nal_tail = 0;
                            nal_idle_cycles = 0;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Read error: {}", e);
                    break;
                }
            }
        }

        // Force-flush trailing NAL if cursor buffer has a valid start code
        if nal_tail > nal_head + 4 {
            let data = &nal_buf[nal_head..nal_tail];
            if data[0] == 0 && data[1] == 0 {
                let start_code_len = if data[2] == 0 && data[3] == 1 {
                    4
                } else if data[2] == 1 {
                    3
                } else {
                    0
                };
                if start_code_len > 0 {
                    let nal = data[start_code_len..].to_vec();
                    if !nal.is_empty() {
                        let mut off = 0;
                        let mut frag_idx: u16 = 0;
                        while off < nal.len() {
                            let end = (off + MAX_SHARD_DATA).min(nal.len());
                            let mut frag = Vec::with_capacity(FRAG_HDR_LEN + end - off);
                            frag.extend_from_slice(&frag_idx.to_le_bytes());
                            frag.extend_from_slice(&nal[off..end]);
                            fec_buffer.push(frag);
                            if fec_buffer.len() == DATA_SHARDS {
                                send_fec_group(
                                    &socket,
                                    &rs,
                                    &fec_buffer,
                                    drone_id,
                                    &mut fec_block_seq,
                                    &mut l2_pkt_seq,
                                    &mut fail_count,
                                    &mut l2_frame_buf,
                                    &mut send_buf,
                                    &mut video_payload_buf,
                                );
                                fec_buffer.clear();
                            }
                            off = end;
                            frag_idx += 1;
                        }
                    }
                }
            }
        }

        // Send any remaining partial group
        if !fec_buffer.is_empty() {
            send_fec_group(
                &socket,
                &rs,
                &fec_buffer,
                drone_id,
                &mut fec_block_seq,
                &mut l2_pkt_seq,
                &mut fail_count,
                &mut l2_frame_buf,
                &mut send_buf,
                &mut video_payload_buf,
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

/// Cursor-based NAL extractor. O(1) per extraction — no drain/truncate/copy_within.
///
/// Scans `buf[head..tail]` for two Annex-B start codes. If found, returns a
/// slice pointing into `buf` (after the first start code, before the second)
/// and the number of bytes consumed (from `head` to the second start code).
///
/// Returns `None` if no complete NAL unit is found.
///
/// The caller must advance `head += consumed` after copying the NAL data.
fn extract_next_nal_cursor(buf: &[u8], head: usize, tail: usize) -> Option<(&[u8], usize)> {
    let data = &buf[head..tail];
    if data.len() < 4 {
        return None;
    }

    // Find first start code
    let mut sc1_offset = None;
    for i in 0..data.len().saturating_sub(3) {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 0 && i + 3 < data.len() && data[i + 3] == 1 {
                sc1_offset = Some(i);
                break;
            }
            if data[i + 2] == 1 {
                sc1_offset = Some(i);
                break;
            }
        }
    }
    let sc1 = sc1_offset?;

    let sc1_len = if sc1 + 3 < data.len() && data[sc1 + 2] == 0 && data[sc1 + 3] == 1 {
        4
    } else {
        3
    };

    let nal_start = sc1 + sc1_len;

    // Find second start code after this one
    for i in nal_start..data.len().saturating_sub(3) {
        if data[i] == 0 && data[i + 1] == 0 {
            let is_sc4 = data[i + 2] == 0 && i + 3 < data.len() && data[i + 3] == 1;
            let is_sc3 = data[i + 2] == 1;
            if is_sc4 || is_sc3 {
                let nal = &data[nal_start..i];
                let consumed = i; // bytes consumed from head: sc1 through sc2 start
                return Some((nal, consumed));
            }
        }
    }

    None
}

/// Minimum interval between shard sends to prevent hardware queue overflow.
/// At 50μs, max throughput is ~20,000 shards/sec = ~12MB/s. Conservative for 3Mbps link.
const MIN_SEND_INTERVAL: Duration = Duration::from_micros(50);

fn send_fec_group(
    socket: &RawSocket,
    rs: &reed_solomon_erasure::galois_8::ReedSolomon,
    chunks: &[Vec<u8>],
    drone_id: u8,
    fec_block_seq: &mut u32,
    l2_pkt_seq: &mut u32,
    fail_count: &mut u8,
    l2_frame_buf: &mut Vec<u8>,
    send_buf: &mut Vec<u8>,
    video_payload_buf: &mut Vec<u8>,
) {
    if chunks.is_empty() {
        return;
    }

    let shard_size = chunks.iter().map(|c| c.len()).max().unwrap_or(1);

    if shard_size > MAX_SHARD_DATA {
        tracing::warn!(
            "Shard size {} exceeds MAX_SHARD_DATA {}, truncating",
            shard_size,
            MAX_SHARD_DATA
        );
    }

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
    let mut last_send = Instant::now();

    // Original data shard lengths (before FEC padding). These are broadcast
    // in every shard's header so the receiver can truncate reconstructed shards.
    let shard0_len = chunks.get(0).map_or(0, |c| c.len()) as u16;
    let shard1_len = chunks.get(1).map_or(0, |c| c.len()) as u16;

    for (i, shard) in shards.iter().enumerate() {
        let elapsed = last_send.elapsed();
        if elapsed < MIN_SEND_INTERVAL {
            thread::sleep(MIN_SEND_INTERVAL - elapsed);
        }

        // Send data shards at their actual length (not padded to max).
        // The parity shard (last) must be full-size for RS math.
        // The receiver pads smaller shards back to max for reconstruction.
        let send_data = if i < DATA_SHARDS {
            // Data shard: send only the original data bytes, trimming FEC padding
            let orig_len = chunks.get(i).map_or(0, |c| c.len());
            if orig_len > 0 && orig_len <= shard.len() {
                &shard[..orig_len]
            } else {
                shard
            }
        } else {
            // Parity shard: send full padded size
            shard
        };

        // Build video payload into reusable video_payload_buf.
        // Header (12 bytes): [4B block_seq][1B shard_idx][1B total_shards]
        //   [1B data_shards][1B pad][2B shard0_len][2B shard1_len]
        video_payload_buf.clear();
        video_payload_buf.reserve(VIDEO_HDR_LEN + send_data.len());
        video_payload_buf.extend_from_slice(&fec_block_seq.to_le_bytes());
        video_payload_buf.push(i as u8);
        video_payload_buf.push(TOTAL_SHARDS as u8);
        video_payload_buf.push(chunks.len() as u8);
        video_payload_buf.push(0u8); // pad
        video_payload_buf.extend_from_slice(&shard0_len.to_le_bytes());
        video_payload_buf.extend_from_slice(&shard1_len.to_le_bytes());
        video_payload_buf.extend_from_slice(send_data);

        // Encode L2 header + video payload into l2_frame_buf (reuses buffer)
        let header = link::L2Header {
            drone_id,
            payload_type: link::PAYLOAD_VIDEO,
            seq: *l2_pkt_seq,
        };
        header.encode_into(video_payload_buf, l2_frame_buf);

        // Send using reusable send_buf
        match socket.send_with_buf(l2_frame_buf, send_buf) {
            Ok(_) => {
                last_send = Instant::now();
                *l2_pkt_seq = l2_pkt_seq.wrapping_add(1);
            }
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

    *fec_block_seq = fec_block_seq.wrapping_add(1);
    if group_ok {
        *fail_count = 0;
    }
}

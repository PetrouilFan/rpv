use bytes::{Buf, BytesMut};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// #30: Import video health flag from main
use crate::VIDEO_HEALTHY;

use reed_solomon_erasure::ReedSolomon;

use crate::link;
use crate::rawsock::RawSocket;

const DATA_SHARDS: usize = 1;
const PARITY_SHARDS: usize = 1;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;
const MAX_NAL_BUF: usize = 512 * 1024;

/// Video header: [4B block_seq][1B idx][1B total][1B data][1B pad][2B*DATA_SHARDS shard_lens]
const VIDEO_HDR_FIXED: usize = 8;
const VIDEO_HDR_LEN: usize = VIDEO_HDR_FIXED + DATA_SHARDS * 2;
const FRAG_HDR_LEN: usize = 2; // u16 fragment index
const MAX_SHARD_DATA: usize = link::MAX_PAYLOAD - 8 - VIDEO_HDR_LEN - FRAG_HDR_LEN;

/// Pre-allocated shard arena for zero-alloc FEC encoding.
/// Each slot is MAX_SHARD_DATA bytes, zero-filled remainder in-place.
struct ShardArena {
    slots: Vec<[u8; MAX_SHARD_DATA]>,
}

impl ShardArena {
    fn new() -> Self {
        Self {
            slots: vec![[0u8; MAX_SHARD_DATA]; DATA_SHARDS],
        }
    }

    /// Write NAL fragment into slot `idx` starting at `arena_offset`.
    /// Returns the number of bytes written into this slot.
    /// The caller tracks arena_offset across calls. When a slot is full,
    /// it advances to the next slot.
    fn write_frag(&mut self, slot: usize, offset: usize, data: &[u8]) -> usize {
        if slot >= DATA_SHARDS {
            return 0;
        }
        let space = MAX_SHARD_DATA - offset;
        let copy_len = data.len().min(space);
        self.slots[slot][offset..offset + copy_len].copy_from_slice(&data[..copy_len]);
        copy_len
    }

    /// Zero-pad slot `idx` from `filled` to MAX_SHARD_DATA.
    #[allow(dead_code)]
    fn pad_slot(&mut self, slot: usize, filled: usize) {
        if slot < DATA_SHARDS && filled < MAX_SHARD_DATA {
            self.slots[slot][filled..].fill(0);
        }
    }
}

/// Run the video capture and streaming loop.
pub fn run(
    running: Arc<AtomicBool>,
    socket: Arc<RawSocket>,
    drone_id: u8,
    bitrate: u32,
    intra: u32,
    hp_rx: Option<crossbeam_channel::Receiver<Vec<u8>>>,
    video_width: u32,
    video_height: u32,
    framerate: u32,
    video_device: String,
    camera_type: &str,
    rpicam_options: &str,
) {
    let is_csi = camera_type == "csi";
    tracing::info!(
        "Video sender ready (FEC {}+{}, L2 broadcast, device={}, type={})",
        DATA_SHARDS,
        PARITY_SHARDS,
        video_device,
        if is_csi {
            "csi (rpicam-vid)"
        } else {
            "usb (ffmpeg)"
        },
    );

    let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS)
        .expect("Failed to create Reed-Solomon encoder");

    let mut fec_block_seq: u32 = 0;
    let mut l2_pkt_seq: u32 = 0;
    let mut use_hw_encoder = true; // false after h264_v4l2m2m fails

    while running.load(Ordering::SeqCst) {
        let proc_name = if is_csi { "rpicam-vid" } else { "ffmpeg" };
        tracing::info!(
            "Starting {} (bitrate={}, intra={}, device={})...",
            proc_name,
            bitrate,
            intra,
            video_device,
        );

        let bitrate_s = format!("{}k", bitrate / 1000);
        // #12: Pre-format video_size once (avoids allocation per restart)
        let video_size_s = format!("{}x{}", video_width, video_height);
        let framerate_s = framerate.to_string();
        let gop_s = intra.to_string();
        let bufsize_s = format!("{}k", (bitrate / framerate).max(1) / 1000);

        let mut child = if is_csi {
            let mut cmd = Command::new("rpicam-vid");
            cmd.args(&[
                "-t",
                "0",
                "--inline",
                "-o",
                "pipe:1",
                "--libav-format",
                "h264",
                "--width",
                &video_width.to_string(),
                "--height",
                &video_height.to_string(),
                "--framerate",
                &framerate_s,
                "--bitrate",
                &bitrate_s,
            ]);
            if !rpicam_options.is_empty() {
                for opt in rpicam_options.split_whitespace() {
                    cmd.arg(opt);
                }
            }
            // --tune may not be available on all rpicam-vid versions
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            cmd
        } else {
            // Encoder: h264_v4l2m2m (Pi 5 VPU) or libx264 (software fallback)
            let encoder_codec = if use_hw_encoder {
                "h264_v4l2m2m"
            } else {
                "libx264"
            };
            let mut cmd = Command::new("ffmpeg");
            cmd.args(&[
                "-hide_banner",
                "-loglevel",
                "error",
                "-fflags",
                "nobuffer",
                "-avioflags",
                "direct",
                "-probesize",
                "32768",
                "-analyzeduration",
                "0",
                "-thread_queue_size",
                "1",
                "-rtbufsize",
                "1M",
                "-f",
                "v4l2",
                "-input_format",
                "mjpeg",
                "-video_size",
                &video_size_s,
                "-framerate",
                &framerate_s,
                "-i",
                &video_device,
                "-c:v",
                encoder_codec,
                "-b:v",
                &bitrate_s,
                "-bufsize",
                &bufsize_s,
                "-g",
                &gop_s,
            ]);
            if !use_hw_encoder {
                cmd.args(&[
                    "-preset",
                    "veryfast",
                    "-tune",
                    "zerolatency",
                    "-crf",
                    "28",
                    "-x264-params",
                    "rc-lookahead=0:sync-lookahead=0:sliced-threads=1",
                ]);
            }
            cmd.args(&["-f", "h264", "-an", "-y", "pipe:1"]);
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            cmd
        };

        // #32: If rpv-cam dies, kernel kills ffmpeg automatically (no orphaned zombie)
        unsafe {
            child.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }

        let mut child = match child.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to start ffmpeg: {}", e);
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        // Direct stdout read — no BufReader wrapper (eliminates 8KB buffer copy)
        let mut stdout = child.stdout.take().expect("No stdout");

        let stderr = child.stderr.take();
        // #10: Reuse String buffer for stderr lines (no per-line allocation)
        thread::spawn(move || {
            if let Some(mut stderr) = stderr {
                use std::io::BufRead;
                let mut reader = std::io::BufReader::new(&mut stderr);
                let mut line_buf = String::with_capacity(256);
                loop {
                    line_buf.clear();
                    match reader.read_line(&mut line_buf) {
                        Ok(0) => break, // EOF
                        Ok(_) => {
                            let line = line_buf.trim();
                            if !line.is_empty() {
                                if line.contains("ERROR")
                                    || line.contains("failed")
                                    || line.contains("error")
                                {
                                    tracing::error!("ffmpeg: {}", line);
                                } else {
                                    tracing::info!("ffmpeg: {}", line);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("ffmpeg stderr read error: {}", e);
                            break;
                        }
                    }
                }
            }
        });

        tracing::info!(
            "ffmpeg started (pid {}), streaming H.264 with FEC {}+{} over raw L2...",
            child.id(),
            DATA_SHARDS,
            PARITY_SHARDS
        );

        let mut read_buf = vec![0u8; 65536];
        let mut total_bytes = u64::default();
        let mut total_nals: u64 = 0;
        let mut total_groups: u64 = 0;
        let mut fail_count: u8 = 0;
        let mut last_stats = std::time::Instant::now();
        // #9: BytesMut ring buffer — O(1) advance instead of copy_within memory shifts
        let mut nal_buf = BytesMut::with_capacity(MAX_NAL_BUF);
        let mut nal_idle_cycles: u32 = 0;
        // #7: Lower threshold — 20 cycles instead of 200 (stuck encoder detection)
        const NAL_IDLE_LIMIT: u32 = 20;
        // #4: NAL watchdog — if no NALs within 2x frame interval, mark unhealthy
        let nal_watchdog_interval = Duration::from_millis(2 * (1000 / framerate.max(1)) as u64);
        let mut last_nal_time = std::time::Instant::now();
        // Reusable buffers for send path
        let mut l2_frame_buf: Vec<u8> = Vec::with_capacity(link::MAX_PAYLOAD);
        let mut send_buf: Vec<u8> = Vec::with_capacity(8 + 24 + link::MAX_PAYLOAD);
        let mut video_payload_buf: Vec<u8> = Vec::with_capacity(VIDEO_HDR_LEN + MAX_SHARD_DATA);
        // Pre-allocated shard arena (zero-alloc padding)
        let mut arena = ShardArena::new();
        // Pre-allocated FEC shard Vecs (reused across groups to avoid 6 heap allocs per group)
        let mut fec_shards: Vec<Vec<u8>> = (0..TOTAL_SHARDS)
            .map(|_| Vec::with_capacity(MAX_SHARD_DATA))
            .collect();

        while running.load(Ordering::SeqCst) {
            match stdout.read(&mut read_buf) {
                Ok(0) => {
                    tracing::info!("ffmpeg stdout closed");
                    break;
                }
                Ok(n) => {
                    total_bytes += n as u64;

                    // Per-read shard tracking (reinitialized each read)
                    let mut shards_in_group: usize = 0;
                    let mut slot_filled = [0usize; DATA_SHARDS];
                    let mut slot_frag_lens: [usize; DATA_SHARDS] = [0; DATA_SHARDS];

                    // #9: Append new data
                    if nal_buf.len() + n > MAX_NAL_BUF {
                        // #27: Scan to next start code instead of blindly discarding half
                        // This prevents splitting a NAL in the middle
                        if let Some(next_sc) = find_start_code(&nal_buf, nal_buf.len() / 4) {
                            nal_buf.advance(next_sc);
                        } else {
                            nal_buf.clear();
                        }
                    }
                    nal_buf.extend_from_slice(&read_buf[..n]);

                    // #4: Check NAL watchdog — encoder deadlock detection
                    if last_nal_time.elapsed() > nal_watchdog_interval {
                        VIDEO_HEALTHY.store(false, Ordering::Relaxed);
                    }

                    // Track NAL extraction
                    let mut extracted_any = false;
                    loop {
                        let (nal_data, consumed) = match extract_next_nal_cursor(&nal_buf) {
                            Some((nal, consumed)) => (nal.to_vec(), consumed),
                            None => break,
                        };
                        // #9: O(1) advance — just bumps the read pointer, no memcpy
                        nal_buf.advance(consumed);
                        extracted_any = true;
                        total_nals += 1;
                        // #30: Signal video health to telemetry
                        VIDEO_HEALTHY.store(true, Ordering::Relaxed);
                        let inter_nal_ms = last_nal_time.elapsed().as_millis();
                        last_nal_time = std::time::Instant::now();

                        if last_stats.elapsed().as_secs() >= 5 {
                            tracing::info!(
                                "Video stats: {:.1} MB, {} NALs, {} FEC groups in {}s, last NAL inter-arrival: {}ms",
                                total_bytes as f64 / 1_048_576.0,
                                total_nals,
                                total_groups,
                                last_stats.elapsed().as_secs(),
                                inter_nal_ms
                            );
                            last_stats = std::time::Instant::now();
                        }

                        let mut off = 0;
                        let mut frag_idx: u16 = 0;
                        while off < nal_data.len() {
                            let slot = shards_in_group % DATA_SHARDS;
                            let arena_offset = slot_filled[slot];

                            // If starting a new slot, write frag header first
                            if arena_offset == 0 {
                                let hdr_written =
                                    arena.write_frag(slot, 0, &frag_idx.to_le_bytes());
                                slot_filled[slot] = hdr_written;
                                slot_frag_lens[slot] = hdr_written;
                            }

                            // Write as much NAL data as fits in this slot
                            let nal_chunk =
                                &nal_data[off..nal_data.len().min(off + MAX_SHARD_DATA)];
                            let written = arena.write_frag(slot, slot_filled[slot], nal_chunk);
                            slot_filled[slot] += written;
                            off += written;
                            frag_idx += 1;

                            // If slot is full or NAL is done, advance
                            if slot_filled[slot] >= MAX_SHARD_DATA || off >= nal_data.len() {
                                shards_in_group += 1;

                                if shards_in_group == DATA_SHARDS {
                                    send_fec_group_arena(
                                        &socket,
                                        &rs,
                                        &arena,
                                        &slot_filled,
                                        &slot_frag_lens,
                                        drone_id,
                                        &mut fec_block_seq,
                                        &mut l2_pkt_seq,
                                        &mut fail_count,
                                        &mut l2_frame_buf,
                                        &mut send_buf,
                                        &mut video_payload_buf,
                                        &hp_rx,
                                        &mut fec_shards,
                                    );
                                    total_groups += 1;
                                    // Reset arena state
                                    shards_in_group = 0;
                                    slot_filled = [0; DATA_SHARDS];
                                    slot_frag_lens = [0; DATA_SHARDS];
                                }
                            }
                        }
                    }

                    // Flush remaining partial group
                    if shards_in_group > 0 {
                        send_fec_group_arena(
                            &socket,
                            &rs,
                            &arena,
                            &slot_filled,
                            &slot_frag_lens,
                            drone_id,
                            &mut fec_block_seq,
                            &mut l2_pkt_seq,
                            &mut fail_count,
                            &mut l2_frame_buf,
                            &mut send_buf,
                            &mut video_payload_buf,
                            &hp_rx,
                            &mut fec_shards,
                        );
                    }

                    // #9: No manual compaction needed — BytesMut::advance() handles it
                    if nal_buf.is_empty() {
                        nal_idle_cycles = 0;
                    } else if extracted_any {
                        nal_idle_cycles = 0;
                    } else if nal_buf.len() > MAX_NAL_BUF / 2 {
                        nal_idle_cycles += 1;
                        if nal_idle_cycles >= NAL_IDLE_LIMIT {
                            tracing::warn!(
                                "NAL buffer idle reset ({} cycles, {}B unparseable)",
                                nal_idle_cycles,
                                nal_buf.len()
                            );
                            nal_buf.clear();
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

        let _ = child.kill();
        let _ = child.wait();
        // #30: ffmpeg died — signal video health as unhealthy
        VIDEO_HEALTHY.store(false, Ordering::Relaxed);

        // Fallback: if h264_v4l2m2m produced 0 bytes, switch to libx264 (USB only)
        if !is_csi && total_bytes == 0 && use_hw_encoder {
            tracing::warn!("h264_v4l2m2m encoder failed, falling back to libx264 (software)");
            use_hw_encoder = false;
        }

        let proc_name = if is_csi { "rpicam-vid" } else { "ffmpeg" };
        tracing::info!(
            "{} stopped, sent {:.1} MB total",
            proc_name,
            total_bytes as f64 / 1_048_576.0
        );

        if running.load(Ordering::SeqCst) {
            tracing::info!("Restarting in 2 seconds...");
            thread::sleep(Duration::from_secs(2));
        }
    }
}

/// #20: Shared helper — find the next start code position in a byte slice.
/// Returns the byte offset of the start code (00 00 01 or 00 00 00 01).
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut pos = from;
    while pos < data.len().saturating_sub(2) {
        let zero = match memchr::memchr(0x00, &data[pos..]) {
            Some(p) => pos + p,
            None => return None,
        };
        if zero + 2 < data.len() && data[zero + 1] == 0 && data[zero + 2] == 1 {
            return Some(zero);
        }
        pos = zero + 1;
    }
    None
}

fn extract_next_nal_cursor(data: &[u8]) -> Option<(&[u8], usize)> {
    if data.len() < 4 {
        return None;
    }

    let mut search_from = 0;
    while search_from < data.len().saturating_sub(3) {
        let zero_pos = match memchr::memchr(0x00, &data[search_from..]) {
            Some(p) => search_from + p,
            None => return None,
        };

        // Detect 3-byte (00 00 01) or 4-byte (00 00 00 01) start code
        let sc_len = if zero_pos + 3 < data.len()
            && data[zero_pos + 1] == 0
            && data[zero_pos + 2] == 0
            && data[zero_pos + 3] == 1
        {
            4
        } else if zero_pos + 2 < data.len() && data[zero_pos + 1] == 0 && data[zero_pos + 2] == 1 {
            3
        } else {
            search_from = zero_pos + 1;
            continue;
        };

        // #20: Use shared helper for inner search (eliminates duplicate loop)
        let nal_start = zero_pos + sc_len;
        match find_start_code(data, nal_start) {
            Some(next_sc) => return Some((&data[zero_pos..next_sc], next_sc)),
            None => return None,
        }
    }
    None
}

/// Send an FEC group from pre-allocated arena slots (zero-alloc padding).
/// Drains high-priority channel (telemetry/RC/heartbeat) before each shard send.
fn send_fec_group_arena(
    socket: &RawSocket,
    rs: &reed_solomon_erasure::galois_8::ReedSolomon,
    arena: &ShardArena,
    slot_filled: &[usize; DATA_SHARDS],
    _slot_frag_lens: &[usize; DATA_SHARDS],
    drone_id: u8,
    fec_block_seq: &mut u32,
    l2_pkt_seq: &mut u32,
    fail_count: &mut u8,
    l2_frame_buf: &mut Vec<u8>,
    send_buf: &mut Vec<u8>,
    video_payload_buf: &mut Vec<u8>,
    hp_rx: &Option<crossbeam_channel::Receiver<Vec<u8>>>,
    fec_shards: &mut Vec<Vec<u8>>,
) {
    // Determine max shard size for RS encoding
    let mut shard_lens = [0usize; DATA_SHARDS];
    let mut max_shard_size = 0usize;
    for i in 0..DATA_SHARDS {
        shard_lens[i] = slot_filled[i];
        max_shard_size = max_shard_size.max(slot_filled[i]);
    }

    if max_shard_size == 0 {
        return;
    }

    // Reuse pre-allocated shard Vecs (resize + zero-fill instead of new allocs)
    for i in 0..DATA_SHARDS {
        fec_shards[i].resize(max_shard_size, 0);
        fec_shards[i].fill(0);
        let copy_len = slot_filled[i].min(max_shard_size);
        fec_shards[i][..copy_len].copy_from_slice(&arena.slots[i][..copy_len]);
    }
    for i in DATA_SHARDS..TOTAL_SHARDS {
        fec_shards[i].resize(max_shard_size, 0);
        fec_shards[i].fill(0);
    }

    if let Err(e) = rs.encode(&mut *fec_shards) {
        tracing::warn!("Reed-Solomon encode error: {:?}", e);
        return;
    }

    let mut group_ok = true;

    for (i, shard) in fec_shards.iter().enumerate() {
        // Skip parity shards — only send data shards (no FEC overhead)
        if i >= DATA_SHARDS {
            break;
        }
        // Drain high-priority packets (telemetry, RC, heartbeat) before this shard
        if let Some(ref hp) = hp_rx {
            while let Ok(hp_frame) = hp.try_recv() {
                let _ = socket.send(&hp_frame);
            }
        }

        // Data shards sent at original length, parity trimmed to max_shard_size
        let send_data = if i < DATA_SHARDS {
            let orig_len = slot_filled[i];
            if orig_len > 0 && orig_len <= shard.len() {
                &shard[..orig_len]
            } else {
                shard
            }
        } else {
            &shard[..max_shard_size.min(shard.len())]
        };

        // Debug: log first few shards
        if *fec_block_seq < 5 {
            tracing::info!(
                "SEND shard[{}]: {} bytes, first16={:02x?}",
                i,
                send_data.len(),
                &send_data[..16.min(send_data.len())]
            );
        }

        // Build video header dynamically
        video_payload_buf.clear();
        video_payload_buf.reserve(VIDEO_HDR_LEN + send_data.len());
        video_payload_buf.extend_from_slice(&fec_block_seq.to_le_bytes());
        video_payload_buf.push(i as u8);
        video_payload_buf.push(TOTAL_SHARDS as u8);
        video_payload_buf.push(DATA_SHARDS as u8);
        video_payload_buf.push(0u8); // pad
                                     // [u16; DATA_SHARDS] shard length array
        for &len in &shard_lens {
            video_payload_buf.extend_from_slice(&(len as u16).to_le_bytes());
        }
        video_payload_buf.extend_from_slice(send_data);

        let header = link::L2Header {
            drone_id,
            payload_type: link::PAYLOAD_VIDEO,
            seq: *l2_pkt_seq,
        };
        header.encode_into(video_payload_buf, l2_frame_buf);

        match socket.send_with_buf(l2_frame_buf, send_buf) {
            Ok(_) => {
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

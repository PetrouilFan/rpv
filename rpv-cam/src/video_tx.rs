use bytes::{Buf, BytesMut};
use std::env;
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
use crate::SocketTrait;

const DATA_SHARDS: usize = 4;
const PARITY_SHARDS: usize = 2;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;
const MAX_NAL_BUF: usize = 512 * 1024;
const NAL_START_CODE: [u8; 3] = [0x00, 0x00, 0x01];

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
    fn pad_slot(&mut self, slot: usize, filled: usize) {
        if slot < DATA_SHARDS && filled < MAX_SHARD_DATA {
            self.slots[slot][filled..].fill(0);
        }
    }
}

/// Run the video capture and streaming loop.
pub fn run(
    running: Arc<AtomicBool>,
    socket: Arc<dyn SocketTrait>,
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
    let is_csi = camera_type == "csi" || camera_type == "rpicam";
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

    // Test mode: if RPV_TEST_VIDEO is set, stream from file instead of camera
    if let Ok(test_path) = env::var("RPV_TEST_VIDEO") {
        tracing::info!(
            "TEST MODE: streaming H.264 from file '{}' ({}x{} {}fps)",
            test_path, video_width, video_height, framerate
        );
        run_test_video(
            running,
            socket,
            drone_id,
            &test_path,
            &mut fec_block_seq,
            &mut l2_pkt_seq,
            video_width,
            video_height,
        );
        return;
    }

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
            // rpicam-vid on this Pi 5 was built without libav support, so we pipe
            // raw YUV420 into ffmpeg for H.264 encoding via libx264 (software)
            // The Pi 5 has no h264_v4l2m2m hardware encoder like the Pi 4
            let shell_cmd = format!(
                "rpicam-vid -t 0 --codec yuv420 -o - --width {} --height {} --framerate {} | \
                 ffmpeg -hide_banner -loglevel error -f rawvideo -pix_fmt yuv420p -s {} -r {} -i pipe:0 \
                  -c:v libx264 -b:v {} -g {} -preset veryfast -tune zerolatency -crf 28 \
                  -x264-params rc-lookahead=0:sync-lookahead=0:sliced-threads=1:repeat-headers=1 \
                  -f h264 pipe:1",
                video_width, video_height, framerate_s,
                video_size_s, framerate_s, bitrate_s, gop_s,
            );
            let mut cmd = Command::new("sh");
            cmd.args(&["-c", &shell_cmd]);
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
        let mut fail_count: u32 = 0;
        let _last_stats = std::time::Instant::now();
        // #9: BytesMut ring buffer — O(1) advance instead of copy_within memory shifts
        let mut nal_buf = BytesMut::with_capacity(MAX_NAL_BUF);
        let _nal_idle_cycles: u32 = 0;
        // #4: NAL watchdog — if no NALs within 2x frame interval, mark unhealthy
        let frame_interval_ms = 1000 / framerate.max(1);
        let nal_watchdog_interval = Duration::from_millis(2 * frame_interval_ms as u64);
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

        let mut shards_in_group: usize = 0;
        let mut slot_filled = [0usize; DATA_SHARDS];
        let mut slot_frag_lens: [usize; DATA_SHARDS] = [0; DATA_SHARDS];

        while running.load(Ordering::SeqCst) {
            match stdout.read(&mut read_buf) {
                Ok(0) => {
                    tracing::info!("ffmpeg stdout closed");
                    break;
                }
                Ok(n) => {
                    total_bytes += n as u64;

                    if last_nal_time.elapsed() > nal_watchdog_interval {
                        VIDEO_HEALTHY.store(false, Ordering::Relaxed);
                    }

                    // Buffer bytes and extract complete NALs
                    if nal_buf.len() + n > MAX_NAL_BUF {
                        if let Some(next_sc) = find_start_code(&nal_buf, nal_buf.len() / 4) {
                            nal_buf.advance(next_sc);
                        } else {
                            nal_buf.clear();
                        }
                    }
                    nal_buf.extend_from_slice(&read_buf[..n]);

                    loop {
                        let (nal_data, consumed) = match extract_next_nal_cursor(&nal_buf) {
                            Some((nal, consumed)) => (nal.to_vec(), consumed),
                            None => break,
                        };
                        nal_buf.advance(consumed);
                        
                        // nal_data already has start code from extract_next_nal_cursor()
                        let nal_with_sc = nal_data.clone();

                        // Log NAL type for diagnostics
                        let nal_type = if nal_with_sc.len() >= 5 {
                            (nal_with_sc[4] & 0x1F)
                        } else if nal_with_sc.len() >= 4 {
                            (nal_with_sc[3] & 0x1F)
                        } else { 99 };
                        let total_nals = total_nals + 1;
                        if total_nals <= 10 {
                            tracing::info!(
                                "CAM NAL #{}: type={}, len={}, first4={:02x?}",
                                total_nals, nal_type, nal_with_sc.len(),
                                &nal_with_sc[..4.min(nal_with_sc.len())]
                            );
                        }

                        // Send NAL with fragment header: [type:1][data...]
                        // type 0 = fits in one shard, type 1 = first fragment,
                        // type 2 = continuation, type 3 = last fragment
                        let max_data = MAX_SHARD_DATA - 1; // reserve 1 byte for frag header
                        if nal_with_sc.len() <= max_data {
                            // Fits in one shard
                            let slot = shards_in_group % DATA_SHARDS;
                            let frag_start = slot_filled[slot];
                            arena.write_frag(slot, slot_filled[slot], &[0x00]);
                            slot_filled[slot] += 1;
                            arena.write_frag(slot, slot_filled[slot], &nal_with_sc);
                            slot_filled[slot] += nal_with_sc.len();
                            slot_frag_lens[slot] = slot_filled[slot] - frag_start;
                            shards_in_group += 1;
                            if shards_in_group == DATA_SHARDS {
                                match send_fec_group_arena(
                                    socket.as_ref(),
                                    &rs,
                                    &mut arena,
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
                                ) {
                                    Err(true) => {
                                        // ENXIO — socket dead, restart the loop
                                        break;
                                    }
                                    _ => {}
                                }
                                total_groups += 1;
                                shards_in_group = 0;
                                slot_filled = [0; DATA_SHARDS];
                                slot_frag_lens = [0; DATA_SHARDS];
                            }
                        } else {
                            // Multi-shard NAL
                            let mut off = 0;
                            let mut frag_num: u8 = 1; // 1=first, 2=cont, 3=last
                            while off < nal_with_sc.len() {
                                let slot = shards_in_group % DATA_SHARDS;
                                let frag_start = slot_filled[slot];
                                let chunk =
                                    &nal_with_sc[off..nal_with_sc.len().min(off + max_data)];
                                off += chunk.len();

                                let frag_type = if off >= nal_with_sc.len() {
                                    3u8
                                } else {
                                    frag_num
                                };
                                frag_num = 2;
                                arena.write_frag(slot, slot_filled[slot], &[frag_type]);
                                slot_filled[slot] += 1;
                                arena.write_frag(slot, slot_filled[slot], chunk);
                                slot_filled[slot] += chunk.len();
                                slot_frag_lens[slot] = slot_filled[slot] - frag_start;
                                shards_in_group += 1;
                                if shards_in_group == DATA_SHARDS {
                                    let _ = send_fec_group_arena(
                                        socket.as_ref(),
                                        &rs,
                                        &mut arena,
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
                                    shards_in_group = 0;
                                    slot_filled = [0; DATA_SHARDS];
                                    slot_frag_lens = [0; DATA_SHARDS];
                                }
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
    let search = &data[from..];
    // SIMD-accelerated search for 3-byte start code
    let Some(rel) = memchr::memmem::find(search, b"\x00\x00\x01") else {
        return None;
    };
    let pos = from + rel;
    // Check for 4-byte start code (00 00 00 01)
    if pos > 0 && data[pos - 1] == 0 {
        return Some(pos - 1);
    }
    Some(pos)
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
    socket: &dyn SocketTrait,
    rs: &reed_solomon_erasure::galois_8::ReedSolomon,
    arena: &mut ShardArena,
    slot_filled: &[usize; DATA_SHARDS],
    slot_frag_lens: &[usize; DATA_SHARDS],
    drone_id: u8,
    fec_block_seq: &mut u32,
    l2_pkt_seq: &mut u32,
    fail_count: &mut u32,
    l2_frame_buf: &mut Vec<u8>,
    send_buf: &mut Vec<u8>,
    _video_payload_buf: &mut Vec<u8>,
    hp_rx: &Option<crossbeam_channel::Receiver<Vec<u8>>>,
    fec_shards: &mut Vec<Vec<u8>>,
) -> Result<(), bool> {
    // Use actual fragment lengths from slot_frag_lens (not padded slot_filled)
    let mut shard_lens = [0usize; DATA_SHARDS];
    let mut max_shard_size = 0usize;
    for i in 0..DATA_SHARDS {
        shard_lens[i] = if slot_frag_lens[i] > 0 {
            slot_frag_lens[i]
        } else {
            slot_filled[i]
        };
        max_shard_size = max_shard_size.max(slot_filled[i]);
    }

    if max_shard_size == 0 {
        return Ok(());
    }

    // Reuse pre-allocated shard Vecs (resize + zero-fill instead of new allocs)
    for i in 0..DATA_SHARDS {
        // Zero-pad slot tail so stale bytes don't corrupt RS parity
        arena.pad_slot(i, slot_filled[i]);
        let copy_len = slot_filled[i].min(max_shard_size);
        fec_shards[i].clear();
        fec_shards[i].extend_from_slice(&arena.slots[i][..copy_len]);
        fec_shards[i].resize(max_shard_size, 0);
    }
    for i in DATA_SHARDS..TOTAL_SHARDS {
        fec_shards[i].resize(max_shard_size, 0);
    }

    if let Err(e) = rs.encode(&mut *fec_shards) {
        tracing::warn!("Reed-Solomon encode error: {:?}", e);
        return Ok(());
    }

    let mut group_ok = true;

    for (i, shard) in fec_shards.iter().enumerate() {
        // Rate limit: sleep between shard sends to avoid AR9271 TX ring overflow
        std::thread::sleep(std::time::Duration::from_micros(300));

        // Drain high-priority packets (telemetry, RC, heartbeat) before each shard
        // to reduce channel contention, but cap to avoid video timing perturbation.
        // Skip drain on first data shard (i=0) to prioritize video transmission.
        // HP traffic (telemetry, RC, heartbeat) gets drained on even DATA shards only.
        // Parity shards are NOT drained to preserve FEC recovery under marginal RF.
        //
        // NOTE: Even with bounded drain, HP traffic can still introduce jitter to video
        // under sustained load because HP frames are sent inline on the video thread.
        // Trade-off: HP latency vs video stability. Current bounds:
        // - Shard 0: No drain (video-first)
        // - Even data shards: max_drain_bytes: 256, max_packets: 2
        // - Parity shards: No drain (prioritize FEC recovery)
        if i % 2 == 0 && i != 0 && i < DATA_SHARDS {
            if let Some(ref hp) = hp_rx {
                let mut drained_bytes = 0;
                let max_drain_bytes = 256; // Reduced to minimize video jitter
                let max_packets = 2;
                let mut packet_count = 0;

                while packet_count < max_packets && drained_bytes < max_drain_bytes {
                    match hp.try_recv() {
                        Ok(hp_frame) => {
                            drained_bytes += hp_frame.len();
                            packet_count += 1;
                            // NOTE: send_with_buf reuses the send_buf scratch buffer.
                            // It clears the buffer internally after use, so subsequent
                            // video shard sends start with a clean buffer.
                            // This is safe because HP drain runs before video shard build.
                            let _ = socket.send_with_buf(&hp_frame, send_buf);
                        }
                        Err(_) => break,
                    }
                }
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

        // Build L2 frame directly into l2_frame_buf (eliminate intermediate copy)
        l2_frame_buf.clear();
        // L2 header: magic + drone_id + type + seq
        l2_frame_buf.extend_from_slice(&link::MAGIC);
        l2_frame_buf.push(drone_id);
        l2_frame_buf.push(link::PAYLOAD_VIDEO);
        l2_frame_buf.extend_from_slice(&l2_pkt_seq.to_le_bytes());
        // Video header
        l2_frame_buf.extend_from_slice(&fec_block_seq.to_le_bytes());
        l2_frame_buf.push(i as u8);
        l2_frame_buf.push(TOTAL_SHARDS as u8);
        l2_frame_buf.push(DATA_SHARDS as u8);
        l2_frame_buf.push(0u8);
        for &len in &shard_lens {
            l2_frame_buf.extend_from_slice(&(len as u16).to_le_bytes());
        }
        // Shard data
        l2_frame_buf.extend_from_slice(send_data);

        match socket.send_with_buf(l2_frame_buf, send_buf) {
            Ok(_) => {
                *l2_pkt_seq = l2_pkt_seq.wrapping_add(1);
            }
            Err(e) => {
                // NOTE: l2_pkt_seq only increments on successful sends (or successful retries).
                // This means TX gaps are invisible at L2 sequence level - the receiver cannot
                // detect which packets were attempted vs succeeded. This is a trade-off:
                // - Pro: L2 sequence reflects only successfully transmitted packets
                // - Con: Receiver cannot detect TX failures by sequence gaps alone
                group_ok = false;
                if e.raw_os_error() == Some(libc::ENXIO) || e.raw_os_error() == Some(libc::ENODEV) {
                    tracing::warn!("AR9271 firmware reset (ENXIO). Caller should reopen socket.");
                    return Err(true);
                }
                if e.raw_os_error() == Some(libc::EAGAIN)
                    || e.raw_os_error() == Some(libc::EWOULDBLOCK)
                {
                    // TX ring full — sleep briefly and retry once
                    std::thread::sleep(std::time::Duration::from_micros(500));
                    if socket.send_with_buf(l2_frame_buf, send_buf).is_ok() {
                        *l2_pkt_seq = l2_pkt_seq.wrapping_add(1);
                        // NOTE: Don't set group_ok=true here - there was still a failure
                        // that caused the retry. This ensures fail_count tracks
                        // retransmit events even when recovery succeeds.
                    } else {
                        // Adaptive pacing: after failed retry, add delay before next shard
                        // This helps under RF stress when TX ring stays full
                        std::thread::sleep(std::time::Duration::from_micros(200));
                    }
                }
                *fail_count = fail_count.saturating_add(1);
                if *fail_count <= 5 {
                    tracing::warn!("Video send error: {}", e);
                }
                // Adaptive pacing: increase inter-shard delay based on failure rate
                if *fail_count > 10 {
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
                if *fail_count > 30 {
                    tracing::warn!("Too many send failures, retrying...");
                    *fail_count = 0;
                    return Ok(());
                }
            }
        }
    }

    *fec_block_seq = fec_block_seq.wrapping_add(1);
    if group_ok {
        *fail_count = 0;
    }
    Ok(())
}

/// Test mode: stream an H.264 file in a loop using the same FEC pipeline.
fn run_test_video(
    running: Arc<AtomicBool>,
    socket: Arc<dyn SocketTrait>,
    drone_id: u8,
    test_path: &str,
    fec_block_seq: &mut u32,
    l2_pkt_seq: &mut u32,
    _video_width: u32,
    _video_height: u32,
) {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};

    let mut file = match File::open(test_path) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("Failed to open test video file '{}': {}", test_path, e);
            return;
        }
    };

    let rs = match ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS) {
        Ok(rs) => rs,
        Err(e) => {
            tracing::error!("Failed to create Reed-Solomon encoder: {:?}", e);
            return;
        }
    };

    let mut arena = ShardArena::new();
    let mut fec_shards: Vec<Vec<u8>> = (0..TOTAL_SHARDS)
        .map(|_| Vec::with_capacity(MAX_SHARD_DATA))
        .collect();
    let mut l2_frame_buf = Vec::new();
    let mut send_buf = Vec::new();
    let mut video_payload_buf = Vec::new();

    let mut nal_buf: Vec<u8> = Vec::with_capacity(MAX_NAL_BUF);
    let mut read_buf = [0u8; 65536];
    let mut shards_in_group = 0;
    let mut slot_filled = [0usize; DATA_SHARDS];
    let mut slot_frag_lens = [0usize; DATA_SHARDS];
    let mut total_nals: u64 = 0;
    let mut fail_count: u32 = 0;

    while running.load(Ordering::SeqCst) {
        // Read from file
        match file.read(&mut read_buf) {
            Ok(0) => {
                tracing::info!("EOF reached on test video, rewinding");
                // NOTE: Any remaining data in nal_buf without a trailing start code
                // is lost here. This is a known limitation - the last NAL in a stream
                // without a subsequent boundary marker won't be emitted.
                file.seek(SeekFrom::Start(0)).ok();
                nal_buf.clear();
                continue;
            }
            Ok(n) => {
                if nal_buf.len() + n > MAX_NAL_BUF {
                    if let Some(next_sc) = find_start_code(&nal_buf, nal_buf.len() / 4) {
                        nal_buf.drain(..next_sc);
                    } else {
                        nal_buf.clear();
                    }
                }
                nal_buf.extend_from_slice(&read_buf[..n]);
            }
            Err(e) => {
                tracing::error!("Read error from test video: {}", e);
                break;
            }
        }

        tracing::debug!("After read: nal_buf.len()={}", nal_buf.len());

        // Extract NAL units from buffer
        loop {
            let (nal_data, consumed) = match extract_next_nal_cursor(&nal_buf) {
                Some((nal, consumed)) => (nal.to_vec(), consumed),
                None => {
                    tracing::debug!("No NAL found in buffer (len={})", nal_buf.len());
                    break;
                }
            };
            nal_buf.drain(..consumed);

            tracing::debug!("Extracted NAL: len={}, first4={:02x?}", nal_data.len(), &nal_data[..4.min(nal_data.len())]);

            let nal_with_sc = nal_data.clone();

            let nal_type = if nal_with_sc.len() >= 5 {
                (nal_with_sc[4] & 0x1F)
            } else if nal_with_sc.len() >= 4 {
                (nal_with_sc[3] & 0x1F)
            } else {
                99
            };
            total_nals += 1;
            if total_nals <= 10 {
                tracing::info!(
                    "CAM NAL #{}: type={}, len={}, first4={:02x?}",
                    total_nals,
                    nal_type,
                    nal_with_sc.len(),
                    &nal_with_sc[..4.min(nal_with_sc.len())]
                );
            }

            // Fragment and send (same as live camera)
            let max_data = MAX_SHARD_DATA - 1;
            if nal_with_sc.len() <= max_data {
                // Single shard NAL
                let slot = shards_in_group % DATA_SHARDS;
                let frag_start = slot_filled[slot];
                arena.write_frag(slot, slot_filled[slot], &[0x00]);
                slot_filled[slot] += 1;
                arena.write_frag(slot, slot_filled[slot], &nal_with_sc);
                slot_filled[slot] += nal_with_sc.len();
                slot_frag_lens[slot] = slot_filled[slot] - frag_start;
                shards_in_group += 1;
                if shards_in_group == DATA_SHARDS {
                    let _ = send_fec_group_arena(
                        socket.as_ref(),
                        &rs,
                        &mut arena,
                        &slot_filled,
                        &slot_frag_lens,
                        drone_id,
                        fec_block_seq,
                        l2_pkt_seq,
                        &mut fail_count,
                        &mut l2_frame_buf,
                        &mut send_buf,
                        &mut video_payload_buf,
                        &None,
                        &mut fec_shards,
                    );
                    shards_in_group = 0;
                    slot_filled = [0; DATA_SHARDS];
                    slot_frag_lens = [0; DATA_SHARDS];
                }
            } else {
                // Multi-shard NAL
                let mut off = 0;
                let mut frag_num: u8 = 1;
                while off < nal_with_sc.len() {
                    let slot = shards_in_group % DATA_SHARDS;
                    let frag_start = slot_filled[slot];
                    let chunk = &nal_with_sc[off..nal_with_sc.len().min(off + max_data)];
                    off += chunk.len();
                    let frag_type = if off >= nal_with_sc.len() { 3 } else { frag_num };
                    frag_num = 2;
                    arena.write_frag(slot, slot_filled[slot], &[frag_type]);
                    slot_filled[slot] += 1;
                    arena.write_frag(slot, slot_filled[slot], chunk);
                    slot_filled[slot] += chunk.len();
                    slot_frag_lens[slot] = slot_filled[slot] - frag_start;
                    shards_in_group += 1;
                    if shards_in_group == DATA_SHARDS {
                        let _ = send_fec_group_arena(
                            socket.as_ref(),
                            &rs,
                            &mut arena,
                            &slot_filled,
                            &slot_frag_lens,
                            drone_id,
                            fec_block_seq,
                            l2_pkt_seq,
                            &mut fail_count,
                            &mut l2_frame_buf,
                            &mut send_buf,
                            &mut video_payload_buf,
                            &None,
                            &mut fec_shards,
                        );
                        shards_in_group = 0;
                        slot_filled = [0; DATA_SHARDS];
                        slot_frag_lens = [0; DATA_SHARDS];
                    }
                }
            }
        } // end NAL extraction loop

        // Rate limit slightly to avoid overwhelming the network
        std::thread::sleep(std::time::Duration::from_micros(100));
    } // end running loop
}


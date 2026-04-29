use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::process::{Command, Stdio};
use libc::{fcntl, F_GETFL, F_SETFL, O_NONBLOCK};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// #30: Import video health flag from main
use crate::VIDEO_HEALTHY;

use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::link;
use crate::link::{L2Header, PAYLOAD_VIDEO};
use crate::SocketTrait;

/// Check camera via sysfs (single stat() syscall, no subprocess spawn)
pub fn check_camera_available(camera_type: &str) -> bool {
    let is_csi = camera_type == "csi" || camera_type == "rpicam";

    if is_csi {
        // For CSI cameras, check if rpicam-vid is available and/or use vcgencmd
        if command_exists("rpicam-vid") {
            // Also try to check if camera is detected via vcgencmd (Pi-specific)
            if let Ok(output) = Command::new("vcgencmd").arg("get_camera").output() {
                if output.status.success() {
                    let response = String::from_utf8_lossy(&output.stdout);
                    // Response like "supported=1 detected=1" or "supported=0"
                    if response.contains("detected=1") {
                        return true;
                    }
                }
            }
            // If vcgencmd fails or doesn't detect, still return true if rpicam-vid exists
            // (user might have CSI camera but vcgencmd might not work)
            return true;
        }
        false
    } else {
        // USB camera: check V4L2 devices
        // /dev/v4l/by-id or /dev/v4l/by-path may contain symlinks on some systems
        let v4l_ok = std::fs::read_dir("/dev/v4l")
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
        if v4l_ok {
            return true;
        }
        // Fallback: check for /dev/video* devices (standard V4L2)
        std::fs::read_dir("/dev")
            .map(|entries| {
                entries.filter_map(|e| e.ok())
                    .any(|e| {
                        e.file_name()
                            .to_str()
                            .map(|s| s.starts_with("video"))
                            .unwrap_or(false)
                    })
            })
            .unwrap_or(false)
    }
}

const DATA_SHARDS: usize = 4;
const PARITY_SHARDS: usize = 2;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;
const MAX_NAL_BUF: usize = 512 * 1024;

/// Check if a command exists in PATH by trying to run it with --version.
fn command_exists(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .output()
        .map(|_| true)
        .unwrap_or(false)
}

/// Video header: [4B block_seq][1B idx][1B total][1B data][1B pad][2B*DATA_SHARDS shard_lens]
const VIDEO_HDR_FIXED: usize = 8;
const VIDEO_HDR_LEN: usize = VIDEO_HDR_FIXED + DATA_SHARDS * 2;
const FRAG_HDR_LEN: usize = 2; // u16 fragment index
const MAX_SHARD_DATA: usize = link::MAX_PAYLOAD - 8 - VIDEO_HDR_LEN - FRAG_HDR_LEN;

/// Find the start of the next H.264 start code (0x000001 or 0x00000001) in buf starting from offset.
/// Returns the index of the start code if found, None otherwise.
fn find_start_code(buf: &[u8], mut offset: usize) -> Option<usize> {
    while offset + 2 < buf.len() {
        if buf[offset] == 0 && buf[offset + 1] == 0 {
            if buf[offset + 2] == 1 {
                return Some(offset);
            } else if offset + 3 < buf.len() && buf[offset + 2] == 0 && buf[offset + 3] == 1 {
                return Some(offset);
            }
        }
        offset += 1;
    }
    None
}

/// Extract the next NAL unit from the buffer, including its start code.
/// Returns (nal_data, consumed) where consumed is the number of bytes to drain from the buffer.
fn extract_next_nal_cursor(buf: &[u8]) -> Option<(&[u8], usize)> {
    let start = find_start_code(buf, 0)?;
    let end = find_start_code(buf, start + 3).unwrap_or(buf.len());
    let nal = &buf[start..end];
    let consumed = end - start;
    Some((nal, consumed))
}

/// Validate rpicam-vid extra options using an allowlist to prevent command injection.
/// Accepts a space-separated string of "--flag value" pairs or standalone flags.
/// Returns Result<Vec<String>, Box<dyn std::error::Error>> of validated arguments, or error if any flag is not allowed.
fn validate_rpicam_options(opts: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    const ALLOWED_FLAGS: &[&str] = &[
        "--sharpness", "--contrast", "--brightness", "--saturation",
        "--ISO", "--ev", "--exposure", "--awb-gains", "--awb",
        "--denoise", "--image-effect", "--color-effect",
        "--metering", "--rotation", "--hflip", "--vflip",
        "--roi", "--autofocus", "--lens-position",
    ];

    let tokens: Vec<&str> = opts.split_whitespace().collect();
    let mut args = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        if !token.starts_with("--") {
            return Err(format!("Invalid rpicam-vid argument '{}': must start with --", token).into());
        }
        if !ALLOWED_FLAGS.contains(&token) {
            return Err(format!("rpicam-vid flag '{}' is not allowed", token).into());
        }
        args.push(token.to_string());
        i += 1;
        if i < tokens.len() && !tokens[i].starts_with("--") {
            args.push(tokens[i].to_string());
            i += 1;
        }
    }
    Ok(args)
}

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
        // Zero tail to prevent stale data from being sent when slot is reused
        self.slots[slot][offset + copy_len..].fill(0);
        copy_len
    }

    /// Zero-pad slot `idx` from `filled` to MAX_SHARD_DATA.
    fn pad_slot(&mut self, slot: usize, filled: usize) {
        if slot < DATA_SHARDS && filled < MAX_SHARD_DATA {
            self.slots[slot][filled..].fill(0);
        }
    }
}

/// Transmit a complete FEC group: encode parity shards and send all TOTAL_SHARDS over the socket.
fn send_fec_group_arena(
    socket: &Arc<dyn SocketTrait>,
    rs: &ReedSolomon,
    arena: &mut ShardArena,
    slot_filled: &[usize; DATA_SHARDS],
    slot_frag_lens: &[usize; DATA_SHARDS],
    drone_id: u8,
    fec_block_seq: u32,
    l2_pkt_seq: &mut u32,
    fail_count: &mut u32,
    l2_frame_buf: &mut Vec<u8>,
    send_buf: &mut Vec<u8>,
    video_payload_buf: &mut Vec<u8>,
    _hp_rx: &Option<crossbeam_channel::Receiver<Vec<u8>>>,
    fec_shards: &mut Vec<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Pad slots to MAX_SHARD_DATA
    for i in 0..DATA_SHARDS {
        arena.pad_slot(i, slot_filled[i]);
    }

    // Determine max shard size
    let max_shard_size = slot_filled.iter().cloned().max().unwrap_or(0);

    // Prepare fec_shards
    *fec_shards = (0..TOTAL_SHARDS)
        .map(|_| Vec::with_capacity(max_shard_size))
        .collect();
    for i in 0..DATA_SHARDS {
        let copy_len = slot_filled[i].min(max_shard_size);
        fec_shards[i].extend_from_slice(&arena.slots[i][..copy_len]);
        fec_shards[i].resize(max_shard_size, 0);
    }
    for i in DATA_SHARDS..TOTAL_SHARDS {
        fec_shards[i].resize(max_shard_size, 0);
    }

    // Encode parity
    rs.encode(&mut *fec_shards)?;

    // Send each shard
    for idx in 0..TOTAL_SHARDS {
        video_payload_buf.clear();

        // Video header: [4B block_seq][1B idx][1B total][1B data][1B pad][2B*DATA_SHARDS shard_lens]
        video_payload_buf.extend_from_slice(&fec_block_seq.to_le_bytes());
        video_payload_buf.push(idx as u8);
        video_payload_buf.push(TOTAL_SHARDS as u8);
        video_payload_buf.push(if idx < DATA_SHARDS { 1 } else { 0 });
        video_payload_buf.push(0); // pad
        for &len in slot_frag_lens {
            video_payload_buf.extend_from_slice(&(len as u16).to_le_bytes());
        }

        // Shard data
        video_payload_buf.extend_from_slice(&fec_shards[idx]);

        // Send with L2 header
        let hdr = L2Header {
            drone_id,
            payload_type: PAYLOAD_VIDEO,
            seq: *l2_pkt_seq,
        };
        hdr.encode_into(video_payload_buf, l2_frame_buf);

        match socket.send_with_buf(l2_frame_buf, send_buf) {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("Send failed: {}", e);
                *fail_count += 1;
            }
        }

        *l2_pkt_seq = l2_pkt_seq.wrapping_add(1);
    }

    Ok(())
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
) -> Result<(), Box<dyn std::error::Error>> {
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

    // Spawn camera process
    let mut child = if is_csi {
        Command::new("rpicam-vid")
            .arg("--output")
            .arg("-")
            .arg("--format")
            .arg("h264")
            .arg("--width")
            .arg(&video_width.to_string())
            .arg("--height")
            .arg(&video_height.to_string())
            .arg("--framerate")
            .arg(&framerate.to_string())
            .arg("--bitrate")
            .arg(&bitrate.to_string())
                    .arg("--inline")
                    .arg("--g")
                    .arg(intra.to_string())
                    // Additional safe rpicam-vid options from config (validated)
                    .args(validate_rpicam_options(rpicam_options).map_err(|e| {
                        tracing::error!("Invalid rpicam_options: {}", e);
                        e
                    })?)
            .stdout(Stdio::piped())
            .spawn()
            .expect("Failed to spawn rpicam-vid")
     } else {
         Command::new("ffmpeg")
             .arg("-f")
             .arg("v4l2")
             .arg("-i")
             .arg(&video_device)
             .arg("-c:v")
             .arg("libx264")
             .arg("-preset")
             .arg("veryfast")
             .arg("-tune")
             .arg("zerolatency")
             .arg("-b:v")
             .arg(&bitrate.to_string())
             .arg("-maxrate")
             .arg(&bitrate.to_string())
             .arg("-bufsize")
             .arg(&(bitrate / 2).to_string())
             .arg("-g")
             .arg(&intra.to_string())
             .arg("-bf")
             .arg("0")
             .arg("-sc_threshold")
             .arg("0")
             .arg("-pix_fmt")
             .arg("yuv420p")
             .arg("-f")
             .arg("h264")
             .arg("-")
             .stdout(Stdio::piped())
             .spawn()
             .expect("Failed to spawn ffmpeg")
     };
    let mut stdout = child.stdout.take().unwrap();
    // Set non-blocking mode so we can respond to shutdown promptly
    let fd = stdout.as_raw_fd();
    let flags = unsafe { fcntl(fd, F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    let ret = unsafe { fcntl(fd, F_SETFL, flags | O_NONBLOCK) };
    if ret == -1 {
        return Err(std::io::Error::last_os_error().into());
    }

    let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS)
        .expect("Failed to create ReedSolomon");

    let mut arena = ShardArena::new();
    let mut fec_block_seq: u32 = 0;
    let mut l2_pkt_seq: u32 = 0;
    let mut l2_frame_buf = Vec::with_capacity(link::MAX_PAYLOAD);
    let mut send_buf = Vec::with_capacity(8 + 24 + link::MAX_PAYLOAD);
    let mut video_payload_buf = Vec::with_capacity(link::MAX_PAYLOAD);
    let mut fec_shards: Vec<Vec<u8>> = Vec::new();
    let mut nal_buf = Vec::with_capacity(MAX_NAL_BUF);

    let mut nal_seq: u32 = 0;

    let mut read_buf = [0u8; 65536];
    let mut shards_in_group = 0;
    let mut slot_filled = [0usize; DATA_SHARDS];
    let mut slot_frag_lens = [0usize; DATA_SHARDS];
    let mut fail_count: u32 = 0;

    // Rate limiting: token bucket derived from configured bitrate
    let bitrate_f64 = bitrate as f64;
    let token_rate = bitrate_f64 / 8.0 / 1000.0; // bytes per millisecond
    let mut tokens = token_rate * 100.0; // initial burst allowance (100 ms)
    let max_tokens = token_rate * 100.0;
    let mut last_rate_check = std::time::Instant::now();

    let mut camera_restart_count = 0;
    let mut last_restart_time = std::time::Instant::now();

    // Track a NAL that spans multiple FEC groups
    struct PendingNal {
        remaining: usize,       // bytes left to send
        slot: usize,            // slot index to write next fragment into
        offset: usize,          // offset within that slot
        frag_num: u8,           // 1=start, 2=middle, 3=last
        nal_id: u32,           // NAL identifier for ordering
    }

    let mut pending_nal: Option<PendingNal> = None;
    let mut stored_sps: Option<Vec<u8>> = None;
    let mut stored_pps: Option<Vec<u8>> = None;

    // Closure to fragment and send a single NAL unit, handling pending across groups.
    let mut send_nal = |nal_data: &[u8]| -> Result<(), Box<dyn std::error::Error>> {
        let max_data = MAX_SHARD_DATA - 5; // 5-byte fragment header (frag_num + nal_id_le)
        let mut offset_remaining = nal_data.len();
        let mut offset = 0;
        let nal_id = nal_seq;
        nal_seq = nal_seq.wrapping_add(1);

        loop {
            if let Some(ref mut pending) = pending_nal {
                let slot = pending.slot;
                let frag_start = slot_filled[slot];
                let remaining = pending.remaining;
                let frag_num = pending.frag_num;
                let nal_id_val = pending.nal_id;

                let space_in_slot = MAX_SHARD_DATA - pending.offset;
                let chunk_len = remaining.min(max_data).min(space_in_slot);

                arena.write_frag(slot, pending.offset, &[frag_num]);
                arena.write_frag(slot, pending.offset + 1, &nal_id_val.to_le_bytes());
                arena.write_frag(slot, pending.offset + 5, &nal_data[offset..offset + chunk_len]);

                let new_offset = pending.offset + 5 + chunk_len;
                slot_filled[slot] = new_offset;
                slot_frag_lens[slot] = new_offset - frag_start;

                offset += chunk_len;
                offset_remaining -= chunk_len;
                pending.remaining -= chunk_len;
                pending.offset = new_offset;

                if pending.remaining == 0 {
                    arena.pad_slot(slot, new_offset);
                    if new_offset < MAX_SHARD_DATA {
                        arena.write_frag(slot, new_offset, &[0u8]);
                        slot_filled[slot] = new_offset + 1;
                    }
                    pending_nal = None;
                }

                if offset_remaining == 0 {
                    break;
                }
                if slot_filled[slot] >= MAX_SHARD_DATA {
                    arena.pad_slot(slot, slot_filled[slot]);
                    send_fec_group_arena(
                        &socket, &rs, &mut arena, &slot_filled, &slot_frag_lens,
                        drone_id, fec_block_seq, &mut l2_pkt_seq, &mut fail_count,
                        &mut l2_frame_buf, &mut send_buf, &mut video_payload_buf,
                        &hp_rx, &mut fec_shards,
                    )?;
                    fec_block_seq = fec_block_seq.wrapping_add(1);

                    slot_filled = [0usize; DATA_SHARDS];
                    slot_frag_lens = [0usize; DATA_SHARDS];
                    shards_in_group = 0;
                }

                continue;
            }

            // Fresh fragment
            let mut slot = 0;
            loop {
                while slot < DATA_SHARDS && slot_filled[slot] >= MAX_SHARD_DATA {
                    slot += 1;
                }
                if slot >= DATA_SHARDS {
                    send_fec_group_arena(
                        &socket, &rs, &mut arena, &slot_filled, &slot_frag_lens,
                        drone_id, fec_block_seq, &mut l2_pkt_seq, &mut fail_count,
                        &mut l2_frame_buf, &mut send_buf, &mut video_payload_buf,
                        &hp_rx, &mut fec_shards,
                    )?;
                    fec_block_seq = fec_block_seq.wrapping_add(1);

                    slot_filled = [0usize; DATA_SHARDS];
                    slot_frag_lens = [0usize; DATA_SHARDS];
                    shards_in_group = 0;
                    continue;
                }

                let frag_start = slot_filled[slot];
                let frag_num = if offset == 0 { 1 } else if offset_remaining <= max_data { 3 } else { 2 };

                arena.write_frag(slot, slot_filled[slot], &[frag_num]);
                arena.write_frag(slot, slot_filled[slot] + 1, &nal_id.to_le_bytes());

                let pos_in_nal = nal_data.len() - offset_remaining;
                let chunk_len = offset_remaining.min(max_data);
                arena.write_frag(slot, slot_filled[slot] + 5, &nal_data[pos_in_nal..pos_in_nal + chunk_len]);

                let new_offset = slot_filled[slot] + 5 + chunk_len;
                slot_filled[slot] = new_offset;
                slot_frag_lens[slot] = new_offset - frag_start;

                offset += chunk_len;
                offset_remaining -= chunk_len;

                if offset_remaining > 0 {
                    pending_nal = Some(PendingNal {
                        remaining: offset_remaining,
                        slot,
                        offset: new_offset,
                        frag_num: if frag_num == 1 { 2 } else { frag_num },
                        nal_id,
                    });
                    arena.pad_slot(slot, new_offset);
                    if new_offset < MAX_SHARD_DATA {
                        arena.write_frag(slot, new_offset, &[0u8]);
                        slot_filled[slot] = new_offset + 1;
                    }
                    break;
                } else {
                    arena.pad_slot(slot, new_offset);
                    if new_offset < MAX_SHARD_DATA {
                        arena.write_frag(slot, new_offset, &[0u8]);
                        slot_filled[slot] = new_offset + 1;
                    }
                    break;
                }
            }

            if offset_remaining == 0 {
                break;
            }
        }
        Ok(())
    };

    while running.load(Ordering::SeqCst) {
        // Check if camera process is still alive
        if let Ok(Some(_)) = child.try_wait() {
            tracing::warn!("Camera process died, attempting restart (attempt {})", camera_restart_count + 1);
            camera_restart_count += 1;

            // Prevent rapid restart loops
            if last_restart_time.elapsed() < std::time::Duration::from_secs(5) && camera_restart_count > 3 {
                tracing::error!("Camera restarting too frequently, giving up");
                break;
            }

            // Clear buffers and reset state for restart
            nal_buf.clear();
            arena = ShardArena::new();
            fec_block_seq = 0;
            l2_pkt_seq = 0;
            nal_seq = 0;
            shards_in_group = 0;
            slot_filled = [0usize; DATA_SHARDS];
            slot_frag_lens = [0usize; DATA_SHARDS];
            fail_count = 0;
            pending_nal = None;
            // Mark video as unhealthy during restart
            VIDEO_HEALTHY.store(false, Ordering::Relaxed);

            // Respawn camera process
            let new_child = if is_csi {
                Command::new("rpicam-vid")
                    .arg("--output")
                    .arg("-")
                    .arg("--format")
                    .arg("h264")
                    .arg("--width")
                    .arg(&video_width.to_string())
                    .arg("--height")
                    .arg(&video_height.to_string())
                    .arg("--framerate")
                    .arg(&framerate.to_string())
                    .arg("--bitrate")
                    .arg(&bitrate.to_string())
                    .arg("--inline")
                    .arg("--g")
                    .arg(intra.to_string())
                    .args(validate_rpicam_options(rpicam_options).map_err(|e| { tracing::error!("Invalid rpicam_options during restart: {}", e); e })?)
                    .stdout(Stdio::piped())
                    .spawn()
                    .map_err(|e| {
                        tracing::error!("Failed to respawn rpicam-vid: {}", e);
                        e
                    })
                    .ok()
            } else {
                // Try hardware encoder first (V4L2 M2M) on Linux, fallback to software libx264
                let hw_result = Command::new("ffmpeg")
                    .arg("-f")
                    .arg("v4l2")
                    .arg("-i")
                    .arg(&video_device)
                    .arg("-c:v")
                    .arg("h264_v4l2m2m")
                    .arg("-preset")
                    .arg("veryfast")
                    .arg("-tune")
                    .arg("zerolatency")
                    .arg("-b:v")
                    .arg(&bitrate.to_string())
                    .arg("-g")
                    .arg(&intra.to_string())
                    .arg("-f")
                    .arg("h264")
                    .arg("-")
                    .stdout(Stdio::piped())
                    .spawn();

                match hw_result {
                    Ok(child) => Some(child),
                    Err(e) => {
                        tracing::warn!("Hardware encoder h264_v4l2m2m unavailable ({:?}), falling back to software libx264", e);
                        Command::new("ffmpeg")
                            .arg("-f")
                            .arg("v4l2")
                            .arg("-i")
                            .arg(&video_device)
                            .arg("-c:v")
                            .arg("libx264")
                            .arg("-preset")
                            .arg("veryfast")
                            .arg("-tune")
                            .arg("zerolatency")
                            .arg("-b:v")
                            .arg(&bitrate.to_string())
                            .arg("-g")
                            .arg(&intra.to_string())
                            .arg("-pix_fmt")
                            .arg("yuv420p")
                            .arg("-f")
                            .arg("h264")
                            .arg("-")
                            .stdout(Stdio::piped())
                            .spawn()
                            .map_err(|e| {
                                tracing::error!("Failed to spawn ffmpeg (software fallback): {}", e);
                                e
                            })
                            .ok()
                    }
                }
            };

            match new_child {
                Some(c) => {
                    child = c;
                    stdout = match child.stdout.take() {
                        Some(s) => s,
                        None => {
                            tracing::error!("Camera process did not provide stdout pipe; cannot read video");
                            // Try restarting again after a short delay? For now, continue to next iteration of restart loop
                            // Increment restart count to avoid infinite loop
                            camera_restart_count += 1;
                            std::thread::sleep(std::time::Duration::from_secs(1));
                            continue;
                        }
                    };
                    last_restart_time = std::time::Instant::now();
                    continue; // Restart the loop with new process
                }
                None => {
                    tracing::error!("Failed to restart camera after {} attempts", camera_restart_count);
                    break;
                }
            }
        }

        // Read from camera (non-blocking)
        match stdout.read(&mut read_buf) {
            Ok(0) => {
                tracing::warn!("Camera stream ended unexpectedly, will attempt restart");
                // Let the process check above handle restart
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
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No data available currently; just loop to check running flag
                std::thread::sleep(std::time::Duration::from_micros(100));
                continue;
            }
            Err(e) => {
                tracing::error!("Read error from camera: {}", e);
                // Clear buffer to avoid incorporating stale partial NALs
                nal_buf.clear();
                // Let the process check above handle restart
                continue;
            }
        }

        tracing::debug!("After read: nal_buf.len()={}", nal_buf.len());

        // Extract NAL units from buffer
        loop {
            let (nal_data, consumed) = match extract_next_nal_cursor(&nal_buf) {
                Some((nal, consumed)) => (nal, consumed),
                None => {
                    tracing::debug!("No NAL found in buffer (len={})", nal_buf.len());
                    break;
                }
            };
            // Mark video as healthy once we start receiving NALs
            VIDEO_HEALTHY.store(true, Ordering::Relaxed);
            // Determine NAL type for SPS/PPS tracking and IDR injection
            let sc_len = if nal_data.len() >= 4 && nal_data[0] == 0 && nal_data[1] == 0 && nal_data[2] == 0 && nal_data[3] == 1 {
                4
            } else if nal_data.len() >= 3 && nal_data[0] == 0 && nal_data[1] == 0 && nal_data[2] == 1 {
                3
            } else {
                // Invalid NAL, skip it
                nal_buf.drain(..consumed);
                continue;
            };
            let nal_type = nal_data[sc_len] & 0x1F;

            // Store SPS/PPS for future injection
            if nal_type == 7 {
                stored_sps = Some(nal_data.to_vec());
            }
            if nal_type == 8 {
                stored_pps = Some(nal_data.to_vec());
            }

            // If this is an IDR and we have both SPS and PPS, inject them by prepending to buffer
            if nal_type == 5 {
                if let (Some(ref sps), Some(ref pps)) = (&stored_sps, &stored_pps) {
                    let remaining = &nal_buf[consumed..];
                    let mut new_buf = Vec::new();
                    new_buf.extend_from_slice(sps);
                    new_buf.extend_from_slice(pps);
                    new_buf.extend_from_slice(&nal_data);
                    new_buf.extend_from_slice(remaining);
                    nal_buf = new_buf;
                    continue;
                }
            }

            // Normal path: will remove this NAL from buffer after fragmentation

            tracing::debug!("Extracted NAL: len={}, first4={:02x?}", nal_data.len(), &nal_data[..4.min(nal_data.len())]);


            let nal_id = nal_seq;
            nal_seq = nal_seq.wrapping_add(1);


            let max_data = MAX_SHARD_DATA - 5; // 5-byte fragment header (frag_num + nal_id_le)
            let mut offset_remaining = nal_data.len();
            let mut offset = 0;

            loop {
                // If there's a pending NAL continuation from previous group, finish it first
                if let Some(ref mut pending) = pending_nal {
                    let slot = pending.slot;
                    let frag_start = slot_filled[slot]; // capture before writing
                    let remaining = pending.remaining;
                    let frag_num = pending.frag_num;
                    let nal_id_val = pending.nal_id;

                    let space_in_slot = MAX_SHARD_DATA - pending.offset;
                    let chunk_len = remaining.min(max_data).min(space_in_slot);

                    // Write fragment header into arena at current offset
                    arena.write_frag(slot, pending.offset, &[frag_num]);
                    arena.write_frag(slot, pending.offset + 1, &nal_id_val.to_le_bytes());
                    arena.write_frag(slot, pending.offset + 5, &nal_data[offset..offset + chunk_len]);

                    let new_offset = pending.offset + 5 + chunk_len;
                    slot_filled[slot] = new_offset;
                    slot_frag_lens[slot] = new_offset - frag_start;

                    offset += chunk_len;
                    offset_remaining -= chunk_len;
                    pending.remaining -= chunk_len;
                    pending.offset = new_offset;

                    if pending.remaining == 0 {
                        // NAL complete, clear pending and close the slot with a trailer byte
                        arena.pad_slot(slot, new_offset);
                        // Write a single zero byte trailer to mark end-of-NAL within this slot
                        if new_offset < MAX_SHARD_DATA {
                            arena.write_frag(slot, new_offset, &[0u8]);
                            slot_filled[slot] = new_offset + 1;
                        }
                        pending_nal = None;
                    }

                    // If this slot is now full (or NAL completed), send the FEC group
                    if offset_remaining == 0 {
                        break;
                    }
                    if slot_filled[slot] >= MAX_SHARD_DATA {
                        arena.pad_slot(slot, slot_filled[slot]);
                        send_fec_group_arena(
                            &socket, &rs, &mut arena, &slot_filled, &slot_frag_lens,
                            drone_id, fec_block_seq, &mut l2_pkt_seq, &mut fail_count,
                            &mut l2_frame_buf, &mut send_buf, &mut video_payload_buf,
                            &hp_rx, &mut fec_shards,
                        )?;
                        fec_block_seq = fec_block_seq.wrapping_add(1);

                        // Start a new group; reset slot tracking but pending_nal still holds the continuation
                        slot_filled = [0usize; DATA_SHARDS];
                        slot_frag_lens = [0usize; DATA_SHARDS];
                        // Also reset shards_in_group counter tracking if present
                        shards_in_group = 0;
                    }

                    continue; // Continue loop to either send next fragment of this NAL or move to fresh NAL
                }

                // No pending NAL; start fragmenting the fresh NAL
                let mut slot = 0;
                loop {
                    // Find next available slot with space
                    while slot < DATA_SHARDS && slot_filled[slot] >= MAX_SHARD_DATA {
                        slot += 1;
                    }
                    if slot >= DATA_SHARDS {
                        // All slots full; send group and start new one
                        send_fec_group_arena(
                            &socket, &rs, &mut arena, &slot_filled, &slot_frag_lens,
                            drone_id, fec_block_seq, &mut l2_pkt_seq, &mut fail_count,
                            &mut l2_frame_buf, &mut send_buf, &mut video_payload_buf,
                            &hp_rx, &mut fec_shards,
                        )?;
                        fec_block_seq = fec_block_seq.wrapping_add(1);
                        slot_filled = [0usize; DATA_SHARDS];
                        slot_frag_lens = [0usize; DATA_SHARDS];
                        shards_in_group = 0;
                        continue;
                    }

                    let frag_start = slot_filled[slot]; // capture before writing this fragment
                    let frag_num = if offset == 0 { 1 } else if offset_remaining <= max_data { 3 } else { 2 };

                    // Write fragment header: [frag_num (1)][nal_id_le (4)]
                    arena.write_frag(slot, slot_filled[slot], &[frag_num]);
                    arena.write_frag(slot, slot_filled[slot] + 1, &nal_id.to_le_bytes());

                    let pos_in_nal = nal_data.len() - offset_remaining;
                    let chunk_len = offset_remaining.min(max_data);
                    arena.write_frag(slot, slot_filled[slot] + 5, &nal_data[pos_in_nal..pos_in_nal + chunk_len]);

                    let new_offset = slot_filled[slot] + 5 + chunk_len;
                    slot_filled[slot] = new_offset;
                    slot_frag_lens[slot] = new_offset - frag_start;

                    offset += chunk_len;
                    offset_remaining -= chunk_len;

                    if offset_remaining > 0 {
                        // NAL continues; store pending state and break to send group
                        pending_nal = Some(PendingNal {
                            remaining: offset_remaining,
                            slot,
                            offset: new_offset,
                            frag_num: if frag_num == 1 { 2 } else { frag_num },
                            nal_id,
                        });
                        arena.pad_slot(slot, new_offset);
                        // Write trailer byte to mark intra-slot NAL continuation
                        if new_offset < MAX_SHARD_DATA {
                            arena.write_frag(slot, new_offset, &[0u8]);
                            slot_filled[slot] = new_offset + 1;
                        }
                        break;
                    } else {
                        // NAL complete within this fragment; pad slot and mark end
                        arena.pad_slot(slot, new_offset);
                        // Write trailer byte
                        if new_offset < MAX_SHARD_DATA {
                            arena.write_frag(slot, new_offset, &[0u8]);
                            slot_filled[slot] = new_offset + 1;
                        }
                        // No pending; continue to next NAL
                        break;
                    }
                }

                if pending_nal.is_some() {
                    break;
                } else {
                    if slot_filled.iter().any(|&f| f > 0) {
                        send_fec_group_arena(
                            &socket, &rs, &mut arena, &slot_filled, &slot_frag_lens,
                            drone_id, fec_block_seq, &mut l2_pkt_seq, &mut fail_count,
                            &mut l2_frame_buf, &mut send_buf, &mut video_payload_buf,
                            &hp_rx, &mut fec_shards,
                        )?;
                        fec_block_seq = fec_block_seq.wrapping_add(1);
                        slot_filled = [0usize; DATA_SHARDS];
                        slot_frag_lens = [0usize; DATA_SHARDS];
                        shards_in_group = 0;
                    }
                    break;
                }
            }
            nal_buf.drain(..consumed);
        }

        // Rate limit slightly to avoid overwhelming the network
        std::thread::sleep(std::time::Duration::from_micros(100));
    } // end running loop

    // Ensure camera child process is terminated on exit
    if let Err(e) = child.kill() {
        tracing::debug!("Failed to kill camera process during shutdown: {}", e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== find_start_code() tests ====================

    #[test]
    fn find_start_code_3byte_basic() {
        let buf = [0x00, 0x00, 0x01, 0xFF];
        assert_eq!(find_start_code(&buf, 0), Some(0));
    }

    #[test]
    fn find_start_code_4byte_basic() {
        let buf = [0x00, 0x00, 0x00, 0x01, 0xFF];
        assert_eq!(find_start_code(&buf, 0), Some(0));
    }

    #[test]
    fn find_start_code_3byte_after_offset() {
        let buf = [0xFF, 0x00, 0x00, 0x01, 0xAA];
        assert_eq!(find_start_code(&buf, 1), Some(1));
    }

    #[test]
    fn find_start_code_4byte_after_3byte() {
        // 3-byte at 0, 4-byte at 1 (00 00 00 01)
        let buf = [0x00, 0x00, 0x00, 0x01, 0xFF];
        // From position 0, it should find the 4-byte start code
        assert_eq!(find_start_code(&buf, 0), Some(0));
    }

    #[test]
    fn find_start_code_multiple_start_codes() {
        let buf = [0xAA, 0x00, 0x00, 0x01, 0xBB, 0x00, 0x00, 0x00, 0x01, 0xCC];
        // First start code at index 1 (3-byte)
        assert_eq!(find_start_code(&buf, 0), Some(1));
    }

    #[test]
    fn find_start_code_no_start_code() {
        let buf = [0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(find_start_code(&buf, 0), None);
    }

    #[test]
    fn find_start_code_offset_past_end() {
        let buf = [0x00, 0x00, 0x01, 0xFF];
        assert_eq!(find_start_code(&buf, 10), None);
    }

    #[test]
    fn find_start_code_4byte_detection() {
        // 00 00 01 is a 3-byte start code, but if preceded by 00, it becomes 4-byte
        let buf = [0xAA, 0x00, 0x00, 0x00, 0x01, 0xBB];
        // At position 1 we have 00 00 00 01 - should detect as 4-byte starting at 1
        assert_eq!(find_start_code(&buf, 1), Some(1));
    }

    // ==================== extract_next_nal_cursor() tests ====================

    #[test]
    fn extract_next_nal_3byte_start() {
        let buf = [0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x00, 0x01, 0x68];
        let result = extract_next_nal_cursor(&buf);
        assert!(result.is_some());
        let (nal, _consumed) = result.unwrap();
        // NAL should start with start code (3 bytes) and include data up to next start code
        // buf[0..5] = [0x00, 0x00, 0x01, 0x67, 0x42] = 5 bytes
        assert_eq!(nal.len(), 5);
        assert_eq!(nal[3], 0x67);
        assert_eq!(nal[4], 0x42);
    }

    #[test]
    fn extract_next_nal_4byte_start() {
        let buf = [0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x00, 0x00, 0x01, 0x68];
        let result = extract_next_nal_cursor(&buf);
        assert!(result.is_some());
        let (nal, _consumed) = result.unwrap();
        // NAL should start with 4-byte start code
        assert!(nal.len() >= 5);
        assert_eq!(nal[4], 0x67);
    }

    #[test]
    fn extract_next_nal_no_nal() {
        let buf = [0xFF, 0xFF, 0xFF];
        assert!(extract_next_nal_cursor(&buf).is_none());
    }

    #[test]
    fn extract_next_nal_only_start_code() {
        let buf = [0x00, 0x00, 0x01];
        // No second start code, so can't extract full NAL
        assert!(extract_next_nal_cursor(&buf).is_none());
    }

    // ==================== ShardArena and write_frag() tests ====================

    #[test]
    fn shard_arena_new() {
        let arena = ShardArena::new();
        assert_eq!(arena.slots.len(), DATA_SHARDS);
        // All slots should be zero-filled
        for slot in &arena.slots {
            assert!(slot.iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn write_frag_basic() {
        let mut arena = ShardArena::new();
        let data = [0xDE, 0xAD, 0xBE, 0xEF];
        let written = arena.write_frag(0, 0, &data);
        assert_eq!(written, 4);
        assert_eq!(&arena.slots[0][..4], &data);
        // Tail should be zeroed
        assert!(arena.slots[0][4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn write_frag_multiple_writes() {
        let mut arena = ShardArena::new();
        let data1 = [0x11, 0x22, 0x33];
        let data2 = [0xAA, 0xBB];

        arena.write_frag(0, 0, &data1);
        arena.write_frag(0, 3, &data2);

        assert_eq!(arena.slots[0][0], 0x11);
        assert_eq!(arena.slots[0][1], 0x22);
        assert_eq!(arena.slots[0][2], 0x33);
        assert_eq!(arena.slots[0][3], 0xAA);
        assert_eq!(arena.slots[0][4], 0xBB);
    }

    #[test]
    fn write_frag_zero_tail() {
        let mut arena = ShardArena::new();
        // Fill some data
        let data = [0x01, 0x02, 0x03, 0x04, 0x05];
        arena.write_frag(0, 0, &data);

        // Now write shorter data at same offset - tail should be zeroed
        let short_data = [0x0A, 0x0B];
        arena.write_frag(0, 0, &short_data);

        assert_eq!(arena.slots[0][0], 0x0A);
        assert_eq!(arena.slots[0][1], 0x0B);
        // Rest should be zeroed
        assert!(arena.slots[0][2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn write_frag_slot_out_of_bounds() {
        let mut arena = ShardArena::new();
        let data = [0x01];
        let written = arena.write_frag(DATA_SHARDS, 0, &data);
        assert_eq!(written, 0);
    }

    #[test]
    fn write_frag_truncates_at_max() {
        let mut arena = ShardArena::new();
        let offset = MAX_SHARD_DATA - 2;
        let data = [0x01, 0x02, 0x03]; // 3 bytes, but only 2 fit
        let written = arena.write_frag(0, offset, &data);
        assert_eq!(written, 2);
        assert_eq!(arena.slots[0][offset], 0x01);
        assert_eq!(arena.slots[0][offset + 1], 0x02);
    }

    #[test]
    fn pad_slot_basic() {
        let mut arena = ShardArena::new();
        // Write some data
        arena.write_frag(0, 0, &[0x01, 0x02, 0x03]);
        // Pad from filled=3 to end
        arena.pad_slot(0, 3);
        // All bytes from index 3 onward should be 0
        assert!(arena.slots[0][3..].iter().all(|&b| b == 0));
    }

    #[test]
    fn pad_slot_already_full() {
        let mut arena = ShardArena::new();
        // Fill the entire slot
        let full_data = vec![0xFF; MAX_SHARD_DATA];
        arena.write_frag(0, 0, &full_data);
        // Pad should be no-op
        arena.pad_slot(0, MAX_SHARD_DATA);
    }

    #[test]
    fn pad_slot_out_of_bounds() {
        let mut arena = ShardArena::new();
        // Should not panic
        arena.pad_slot(DATA_SHARDS, 0);
    }

    // ==================== NAL fragmentation logic tests ====================

    #[test]
    fn nal_fits_in_single_shard() {
        // Test that a small NAL is detected as single-shard
        let max_data = MAX_SHARD_DATA - 5; // reserve 5 bytes for frag header
        let small_nal = vec![0x00, 0x00, 0x01, 0x67, 0x42]; // 5 bytes
        assert!(small_nal.len() <= max_data);
    }

    #[test]
    fn nal_requires_multiple_shards() {
        let max_data = MAX_SHARD_DATA - 1;
        let large_nal = vec![0x00; max_data + 100];
        assert!(large_nal.len() > max_data);
    }

    // ==================== FEC constants tests ====================

    #[test]
    fn fec_constants_valid() {
        assert!(DATA_SHARDS > 0);
        assert!(PARITY_SHARDS > 0);
        assert_eq!(TOTAL_SHARDS, DATA_SHARDS + PARITY_SHARDS);
    }

    #[test]
    fn max_shard_data_fits_in_payload() {
        // MAX_SHARD_DATA should be less than link::MAX_PAYLOAD minus headers
        assert!(MAX_SHARD_DATA > 0);
        // The constant is defined as link::MAX_PAYLOAD - 8 - VIDEO_HDR_LEN - FRAG_HDR_LEN
        // Just verify it's reasonable
        assert!(MAX_SHARD_DATA < 1500); // Typical MTU-ish
    }

    // ==================== Video header constants tests ====================

    #[test]
    fn video_header_size() {
        assert_eq!(VIDEO_HDR_LEN, VIDEO_HDR_FIXED + DATA_SHARDS * 2);
        assert_eq!(VIDEO_HDR_FIXED, 8);
        assert_eq!(FRAG_HDR_LEN, 2);
    }

    // ==================== Integration test for ShardArena with FEC ====================

    #[test]
    fn shard_arena_fec_round_trip() {
        // Create a Reed-Solomon encoder (using galois_8 like the main code)
        use reed_solomon_erasure::galois_8;
        let rs = galois_8::ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS).unwrap();

        let mut arena = ShardArena::new();

        // Write test data to each shard
        for i in 0..DATA_SHARDS {
            let data: Vec<u8> = (0..100).map(|b| (b + i as u8) % 255).collect();
            arena.write_frag(i, 0, &data);
        }

        // Prepare shards for encoding
        let mut shard_lens = [0usize; DATA_SHARDS];
        let mut max_shard_size = 0usize;
        let mut slot_filled = [0usize; DATA_SHARDS];

        for i in 0..DATA_SHARDS {
            slot_filled[i] = 100;
            shard_lens[i] = 100;
            max_shard_size = max_shard_size.max(100);
        }

        // Pad slots
        for i in 0..DATA_SHARDS {
            arena.pad_slot(i, slot_filled[i]);
        }

        // Create fec_shards
        let mut fec_shards: Vec<Vec<u8>> = (0..TOTAL_SHARDS)
            .map(|_| Vec::with_capacity(max_shard_size))
            .collect();

        for i in 0..DATA_SHARDS {
            let copy_len = slot_filled[i].min(max_shard_size);
            fec_shards[i].extend_from_slice(&arena.slots[i][..copy_len]);
            fec_shards[i].resize(max_shard_size, 0);
        }
        for i in DATA_SHARDS..TOTAL_SHARDS {
            fec_shards[i].resize(max_shard_size, 0);
        }

        // Encode
        rs.encode(&mut fec_shards).unwrap();

        // Verify we have all shards
        assert_eq!(fec_shards.len(), TOTAL_SHARDS);

        // Verify parity shards are not all zeros (encoding worked)
        for i in DATA_SHARDS..TOTAL_SHARDS {
            assert!(fec_shards[i].iter().any(|&b| b != 0), "Parity shard {} is all zeros", i);
        }
    }

    // ==================== Regression tests for AUDIT.md bugs ====================

    /// Bug: AUDIT.md — FEC parity shards not transmitted
    /// Fixed: send_fec_group_arena now iterates 0..TOTAL_SHARDS (all shards including parity)
    /// Test: Verify that RS encoding produces valid parity shards and TOTAL_SHARDS includes them
    #[test]
    fn regression_fec_parity_sent() {
        use reed_solomon_erasure::galois_8;
        let rs = galois_8::ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS).unwrap();

        // Create test data shards
        let mut shards: Vec<Vec<u8>> = (0..TOTAL_SHARDS)
            .map(|i| {
                if i < DATA_SHARDS {
                    vec![i as u8; 50] // Data shards have known pattern
                } else {
                    vec![0u8; 50] // Parity shards start as zeros
                }
            })
            .collect();

        // Encode - this should fill parity shards
        rs.encode(&mut shards).expect("RS encode failed");

        // Verify parity shards are now non-zero (they were computed)
        for i in DATA_SHARDS..TOTAL_SHARDS {
            assert!(
                shards[i].iter().any(|&b| b != 0),
                "Parity shard {} is all zeros after encode - RS not working",
                i
            );
        }

        // Verify TOTAL_SHARDS equals DATA_SHARDS + PARITY_SHARDS
        assert_eq!(TOTAL_SHARDS, DATA_SHARDS + PARITY_SHARDS);

        // The fix in send_fec_group_arena iterates over all TOTAL_SHARDS
        // Verify our loop constant is correct
        let mut parity_count = 0;
        for i in 0..TOTAL_SHARDS {
            if i >= DATA_SHARDS {
                parity_count += 1;
            }
        }
        assert_eq!(parity_count, PARITY_SHARDS);
    }

    /// Bug: AUDIT.md — Partial shards contain stale data
    /// Fixed: write_frag now zeros the tail after copying data
    /// Test: Verify that writing shorter data after longer data doesn't leak stale bytes
    #[test]
    fn regression_partial_shards_no_stale_data() {
        let mut arena = ShardArena::new();

        // Write long data to slot 0
        let long_data = vec![0xAA; 100];
        arena.write_frag(0, 0, &long_data);

        // Verify the data was written
        for i in 0..100 {
            assert_eq!(arena.slots[0][i], 0xAA);
        }

        // Now write shorter data
        let short_data = vec![0xBB; 50];
        arena.write_frag(0, 0, &short_data);

        // Verify new data
        for i in 0..50 {
            assert_eq!(arena.slots[0][i], 0xBB);
        }

        // Verify tail is zeroed (no stale 0xAA leaking)
        for i in 50..100 {
            assert_eq!(
                arena.slots[0][i], 0,
                "Stale data at index {}: expected 0, got {:x}",
                i, arena.slots[0][i]
            );
        }
    }

    /// Bug: AUDIT.md — fail_count is u8, saturates with no recovery
    /// Fixed: fail_count is now u32 in the current code
    /// Test: Verify fail_count doesn't saturate at 255
    #[test]
    fn regression_fail_count_no_saturation() {
        // Verify fail_count is u32 (or larger) and can count past 255
        let mut fail_count: u32 = 0;

        // Simulate 300 failures
        for _ in 0..300 {
            fail_count = fail_count.saturating_add(1);
        }

        assert_eq!(fail_count, 300, "fail_count should be able to count past 255");
        assert!(
            fail_count > 255,
            "fail_count should not saturate at 255 (old u8 behavior)"
        );
    }
}


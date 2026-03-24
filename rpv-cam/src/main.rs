mod config;
mod discover;

use std::io::{BufReader, Read};
use std::net::{IpAddr, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reed_solomon_erasure::ReedSolomon;

const VIDEO_PORT: u16 = 5600;
const TELEMETRY_PORT: u16 = 5601;
const RC_PORT: u16 = 5602;
const HEARTBEAT_PORT: u16 = 5603;
const FEEDBACK_PORT: u16 = 5604;

const DATA_SHARDS: usize = 2;
const PARITY_SHARDS: usize = 1;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;
const STATUS_FILE: &str = "/tmp/rpv_link_status";

fn write_link_status(status: &str) {
    let _ = std::fs::write(STATUS_FILE, status);
}

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    tracing::info!("rpv-cam starting on Raspberry Pi Zero 2W");

    let (config, _was_default) = config::Config::load();
    tracing::info!("Config: {:?}", config);

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        tracing::info!("Shutting down...");
        r.store(false, Ordering::SeqCst);
    })
    .ok();

    tracing::info!("Discovering ground station...");
    write_link_status("searching");
    let fallback_ip: Option<IpAddr> = config.ground_ip.parse().ok();
    let ground_ip = discover::discover_ground(5, fallback_ip);
    tracing::info!("Ground station at {}", ground_ip);
    write_link_status("connected");

    let ground_addr: Arc<Mutex<Option<IpAddr>>> = Arc::new(Mutex::new(Some(ground_ip)));
    let last_heartbeat: Arc<Mutex<Instant>> = Arc::new(Mutex::new(Instant::now()));

    // Adaptive bitrate state: (loss_rate, timestamp)
    let bitrate_state: Arc<Mutex<(u8, Instant)>> = Arc::new(Mutex::new((0, Instant::now())));

    // Start heartbeat receiver
    let hb_running = running.clone();
    let hb_last = Arc::clone(&last_heartbeat);
    let hb_handle = thread::spawn(move || {
        heartbeat_receiver(hb_running, hb_last);
    });

    // Start heartbeat monitor (triggers re-discovery when heartbeat lost)
    let hm_running = running.clone();
    let hm_ground = Arc::clone(&ground_addr);
    let hm_last = Arc::clone(&last_heartbeat);
    let hm_fallback = fallback_ip;
    let hm_handle = thread::spawn(move || {
        heartbeat_monitor(hm_running, hm_ground, hm_last, hm_fallback);
    });

    // Start feedback receiver (adaptive bitrate)
    let fb_running = running.clone();
    let fb_bitrate = Arc::clone(&bitrate_state);
    let fb_handle = thread::spawn(move || {
        feedback_receiver(fb_running, fb_bitrate);
    });

    // Start video capture and streaming
    let video_running = running.clone();
    let video_ground = Arc::clone(&ground_addr);
    let video_bitrate = Arc::clone(&bitrate_state);
    let video_handle = thread::spawn(move || {
        video_loop(video_running, video_ground, video_bitrate);
    });

    // Start RC receiver
    let rc_running = running.clone();
    let rc_handle = thread::spawn(move || {
        rc_receiver(rc_running);
    });

    // Start telemetry sender
    let telem_running = running.clone();
    let telem_ground = Arc::clone(&ground_addr);
    let telem_handle = thread::spawn(move || {
        telemetry_sender(telem_running, telem_ground);
    });

    hb_handle.join().ok();
    hm_handle.join().ok();
    fb_handle.join().ok();
    video_handle.join().ok();
    rc_handle.join().ok();
    telem_handle.join().ok();

    tracing::info!("rpv-cam stopped");
}

fn heartbeat_receiver(running: Arc<AtomicBool>, last_heartbeat: Arc<Mutex<Instant>>) {
    let socket = match UdpSocket::bind(format!("0.0.0.0:{}", HEARTBEAT_PORT)) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                "Failed to bind heartbeat socket on port {}: {}",
                HEARTBEAT_PORT,
                e
            );
            return;
        }
    };

    let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
    tracing::info!("Heartbeat receiver listening on port {}", HEARTBEAT_PORT);

    let mut buf = [0u8; 64];

    while running.load(Ordering::SeqCst) {
        match socket.recv_from(&mut buf) {
            Ok((len, _addr)) => {
                if len >= 7 && &buf[..7] == b"rpv-bea" {
                    *last_heartbeat.lock().unwrap() = Instant::now();
                }
            }
            Err(_) => {
                // timeout or error, keep waiting
            }
        }
    }
}

fn heartbeat_monitor(
    running: Arc<AtomicBool>,
    ground_addr: Arc<Mutex<Option<IpAddr>>>,
    last_heartbeat: Arc<Mutex<Instant>>,
    fallback_ip: Option<IpAddr>,
) {
    use std::sync::atomic::AtomicBool as AtomicBoolFlag;
    tracing::info!("Heartbeat monitor started (timeout: 3s)");
    thread::sleep(Duration::from_secs(3)); // initial grace period

    let rediscovering = Arc::new(AtomicBoolFlag::new(false));

    while running.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_secs(1));

        let elapsed = last_heartbeat.lock().unwrap().elapsed();
        if elapsed > Duration::from_secs(3) && !rediscovering.load(Ordering::SeqCst) {
            tracing::warn!(
                "Heartbeat lost ({}s), triggering re-discovery...",
                elapsed.as_secs()
            );
            write_link_status("searching");

            rediscovering.store(true, Ordering::SeqCst);
            let rd = Arc::clone(&rediscovering);
            let rediscover_ground = Arc::clone(&ground_addr);
            let rediscover_last_hb = Arc::clone(&last_heartbeat);
            let rediscover_fallback = fallback_ip;
            thread::spawn(move || {
                match std::panic::catch_unwind(|| discover::discover_ground(5, rediscover_fallback))
                {
                    Ok(new_ip) => {
                        tracing::info!("Re-discovered ground station at {}", new_ip);
                        *rediscover_ground.lock().unwrap() = Some(new_ip);
                        // Reset heartbeat timer so we don't immediately re-trigger
                        *rediscover_last_hb.lock().unwrap() = Instant::now();
                        write_link_status("connected");
                    }
                    Err(_) => {
                        tracing::error!("Discovery panicked, will retry on next heartbeat check");
                    }
                }
                rd.store(false, Ordering::SeqCst);
            });
        }
    }
}

fn feedback_receiver(running: Arc<AtomicBool>, bitrate_state: Arc<Mutex<(u8, Instant)>>) {
    let socket = match UdpSocket::bind(format!("0.0.0.0:{}", FEEDBACK_PORT)) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                "Failed to bind feedback socket on port {}: {}",
                FEEDBACK_PORT,
                e
            );
            return;
        }
    };

    let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
    tracing::info!("Feedback receiver listening on port {}", FEEDBACK_PORT);

    let mut buf = [0u8; 64];

    while running.load(Ordering::SeqCst) {
        match socket.recv_from(&mut buf) {
            Ok((len, _addr)) => {
                if len >= 2 && buf[0] == b'L' {
                    let loss_rate = buf[1];
                    *bitrate_state.lock().unwrap() = (loss_rate, Instant::now());
                    tracing::info!("FEEDBACK: received loss_rate={}%", loss_rate);
                }
            }
            Err(_) => {
                // timeout or error, keep waiting
            }
        }
    }
}

fn video_loop(
    running: Arc<AtomicBool>,
    ground_addr: Arc<Mutex<Option<IpAddr>>>,
    bitrate_state: Arc<Mutex<(u8, Instant)>>,
) {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => {
            let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
            use std::os::unix::io::AsRawFd;
            let sndbuf: libc::c_int = 4 * 1024 * 1024;
            unsafe {
                libc::setsockopt(
                    s.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    &sndbuf as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
            s
        }
        Err(e) => {
            tracing::error!("Failed to bind video socket: {}", e);
            return;
        }
    };
    tracing::info!("Video sender ready (FEC {}+{})", DATA_SHARDS, PARITY_SHARDS);

    let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS)
        .expect("Failed to create Reed-Solomon encoder");

    while running.load(Ordering::SeqCst) {
        tracing::info!("Starting rpicam-vid...");

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
                "3500000",
                "--low-latency",
                "--flush",
                "--inline",
                "--intra",
                "60",
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
            "rpicam-vid started, streaming H.264 with FEC {}+{}...",
            DATA_SHARDS,
            PARITY_SHARDS
        );

        let mut buf = vec![0u8; 65536];
        let mut total_bytes = 0u64;
        let mut seq: u32 = 0;
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

                    // Extract complete NALUs and send
                    while let Some(nal) = extract_next_nal(&mut nal_buf) {
                        // Split into fragments at 1200 bytes with fragment index
                        let mut off = 0;
                        let mut frag_idx: u8 = 0;
                        while off < nal.len() {
                            let end = (off + 1200).min(nal.len());
                            let mut frag = Vec::with_capacity(1 + end - off);
                            frag.push(frag_idx);
                            frag.extend_from_slice(&nal[off..end]);
                            fec_buffer.push(frag);
                            off = end;
                            frag_idx += 1;

                            if fec_buffer.len() == DATA_SHARDS {
                                if let Some(ip) = *ground_addr.lock().unwrap() {
                                    let target = format!("{}:{}", ip, VIDEO_PORT);
                                    send_fec_group(
                                        &socket,
                                        &rs,
                                        &fec_buffer,
                                        seq,
                                        &target,
                                        &mut fail_count,
                                    );
                                    seq = seq.wrapping_add(1);
                                }
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

        // Send any remaining partial group
        if !fec_buffer.is_empty() {
            if let Some(ip) = *ground_addr.lock().unwrap() {
                let target = format!("{}:{}", ip, VIDEO_PORT);
                send_fec_group(&socket, &rs, &fec_buffer, seq, &target, &mut fail_count);
            }
            fec_buffer.clear();
        }

        let _ = child.kill();
        let _ = child.wait();

        tracing::info!(
            "rpicam-vid stopped, sent {:.1} MB total",
            total_bytes as f64 / 1_048_576.0
        );

        if running.load(Ordering::SeqCst) {
            tracing::info!("Restarting rpicam-vid in 2 seconds...");
            thread::sleep(Duration::from_secs(2));
        }
    }
}

const MAX_NAL_BUF: usize = 512 * 1024; // 512 KB hard cap

fn extract_next_nal(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    // Hard cap: if no start code found within 512KB, drain and reset
    if buf.len() > MAX_NAL_BUF {
        tracing::warn!("NAL buffer overflow ({}B), resetting", buf.len());
        buf.clear();
        return None;
    }
    // Scan for H.264 NAL start codes: 0x00000001 or 0x000001
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

    // Determine start code length (3 or 4 bytes)
    let sc_len = if start + 3 < buf.len() && buf[start + 2] == 0 && buf[start + 3] == 1 {
        4
    } else {
        3
    };

    // Find next start code after this one
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
        // No complete NALU yet, keep buffering
        None
    }
}

fn send_fec_group(
    socket: &UdpSocket,
    rs: &reed_solomon_erasure::galois_8::ReedSolomon,
    chunks: &[Vec<u8>],
    seq: u32,
    target: &str,
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
        // Header: [4B seq][1B shard_idx][1B total_shards][1B data_shards][1B pad][2B shard_len] = 10 bytes
        let mut packet = Vec::with_capacity(10 + shard.len());
        packet.extend_from_slice(&seq.to_le_bytes());
        packet.push(i as u8);
        packet.push(TOTAL_SHARDS as u8);
        packet.push(chunks.len() as u8); // actual data shards (may be < DATA_SHARDS for partial groups)
        packet.push(0u8); // padding
        packet.extend_from_slice(&(shard.len() as u16).to_le_bytes());
        packet.extend_from_slice(shard);

        match socket.send_to(&packet, target) {
            Ok(_) => {}
            Err(e) => {
                group_ok = false;
                *fail_count = fail_count.saturating_add(1);
                tracing::warn!("Video send error: {} (fail_count={})", e, fail_count);
                if *fail_count > 30 {
                    tracing::warn!(
                        "Too many send failures, will rely on heartbeat monitor for re-discovery"
                    );
                    *fail_count = 0;
                    return;
                }
            }
        }
    }
    if group_ok {
        *fail_count = 0;
    }
}

fn rc_receiver(running: Arc<AtomicBool>) {
    let bind_addr = format!("0.0.0.0:{}", RC_PORT);
    let socket = match UdpSocket::bind(&bind_addr) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Failed to bind RC socket on {}: {}", bind_addr, e);
            return;
        }
    };

    tracing::info!("RC receiver listening on {}", bind_addr);

    let mut buf = [0u8; 256];
    let rc_file_path = "/tmp/rpv_rc_channels";

    while running.load(Ordering::SeqCst) {
        match socket.recv_from(&mut buf) {
            Ok((len, _addr)) => {
                if len < 4 {
                    continue;
                }

                // Parse RC channels: [4B count][N*2B channels]
                let count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                let expected = 4 + count * 2;

                if len < expected || count > 126 {
                    continue;
                }

                let mut channels = Vec::with_capacity(count);
                for i in 0..count {
                    let offset = 4 + i * 2;
                    let ch = u16::from_le_bytes([buf[offset], buf[offset + 1]]);
                    channels.push(ch);
                }

                // Write channels to file for external integration (atomic write)
                let ch_str: Vec<String> = channels.iter().map(|c| c.to_string()).collect();
                let tmp_path = format!("{}.tmp", rc_file_path);
                if let Ok(mut f) = std::fs::File::create(&tmp_path) {
                    use std::io::Write;
                    let _ = f.write_all(ch_str.join(",").as_bytes());
                    let _ = f.flush();
                    let _ = std::fs::rename(&tmp_path, rc_file_path);
                }
            }
            Err(e) => {
                tracing::warn!("RC recv error: {}", e);
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn check_camera_available() -> bool {
    if let Ok(output) = std::process::Command::new("vcgencmd")
        .arg("get_camera")
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        // "supported=0 detected=0, libcamera interfaces=0"
        // or "supported=1 detected=1, libcamera interfaces=1"
        if let Some(val) = stdout.split("detected=").nth(1) {
            if let Some(count) = val.split(',').next() {
                return count.trim().parse::<i32>().unwrap_or(0) > 0;
            }
        }
    }
    false
}

fn telemetry_sender(running: Arc<AtomicBool>, ground_addr: Arc<Mutex<Option<IpAddr>>>) {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Failed to bind telemetry socket: {}", e);
            return;
        }
    };

    tracing::info!("Telemetry sender ready");

    let interval = Duration::from_millis(200); // 5Hz
    let camera_check_interval = Duration::from_secs(5);
    let mut last_camera_check = Instant::now();
    let mut camera_ok = check_camera_available();
    tracing::info!("Camera available: {}", camera_ok);

    while running.load(Ordering::SeqCst) {
        if last_camera_check.elapsed() > camera_check_interval {
            camera_ok = check_camera_available();
            last_camera_check = Instant::now();
        }

        if let Some(ip) = *ground_addr.lock().unwrap() {
            let target_addr = format!("{}:{}", ip, TELEMETRY_PORT);

            let telem = serde_json::json!({
                "lat": 0.0,
                "lon": 0.0,
                "alt": 0.0,
                "heading": 0.0,
                "speed": 0.0,
                "satellites": 0,
                "battery_v": 0.0,
                "battery_pct": 0,
                "mode": "UNKNOWN",
                "armed": false,
                "camera_ok": camera_ok,
            });

            if let Ok(data) = serde_json::to_string(&telem) {
                let _ = socket.send_to(data.as_bytes(), &target_addr);
            }
        }

        thread::sleep(interval);
    }
}

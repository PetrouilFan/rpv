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

    let config = config::Config::load();
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

    // Start video capture and streaming
    let video_running = running.clone();
    let video_ground = Arc::clone(&ground_addr);
    let video_handle = thread::spawn(move || {
        video_loop(video_running, video_ground);
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
    tracing::info!("Heartbeat monitor started (timeout: 3s)");
    thread::sleep(Duration::from_secs(3)); // initial grace period

    while running.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_secs(1));

        let elapsed = last_heartbeat.lock().unwrap().elapsed();
        if elapsed > Duration::from_secs(3) {
            tracing::warn!(
                "Heartbeat lost ({}s), triggering re-discovery...",
                elapsed.as_secs()
            );
            write_link_status("searching");

            // Non-blocking: spawn discovery in a background thread
            let rediscover_ground = Arc::clone(&ground_addr);
            let rediscover_fallback = fallback_ip;
            thread::spawn(move || {
                match std::panic::catch_unwind(|| discover::discover_ground(5, rediscover_fallback))
                {
                    Ok(new_ip) => {
                        tracing::info!("Re-discovered ground station at {}", new_ip);
                        *rediscover_ground.lock().unwrap() = Some(new_ip);
                        write_link_status("connected");
                        // Reset heartbeat timer so we don't immediately re-trigger
                    }
                    Err(_) => {
                        tracing::error!("Discovery panicked, will retry on next heartbeat check");
                    }
                }
            });

            // Wait before checking again
            thread::sleep(Duration::from_secs(6));
        }
    }
}

fn video_loop(running: Arc<AtomicBool>, ground_addr: Arc<Mutex<Option<IpAddr>>>) {
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
                "1280",
                "--height",
                "720",
                "--framerate",
                "20",
                "--codec",
                "h264",
                "--profile",
                "baseline",
                "--level",
                "4.1",
                "--bitrate",
                "2500000",
                "--low-latency",
                "--flush",
                "--inline",
                "--intra",
                "15",
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

        while running.load(Ordering::SeqCst) {
            match reader.read(&mut buf) {
                Ok(0) => {
                    tracing::info!("rpicam-vid stdout closed");
                    break;
                }
                Ok(n) => {
                    // Split into chunks, preferring NAL unit boundaries
                    let mut offset = 0;
                    while offset < n {
                        let remaining = n - offset;
                        let mut chunk_size = remaining.min(1300);

                        // Try to split at NAL start code boundary (0x000001 or 0x00000001)
                        if remaining > 1300 {
                            let scan_start = (offset + 900).min(n.saturating_sub(4));
                            let scan_end = (offset + 1300).min(n);
                            if let Some(nal_pos) = find_nal_start(&buf[scan_start..scan_end]) {
                                chunk_size = scan_start - offset + nal_pos;
                            }
                        }

                        if chunk_size == 0 {
                            chunk_size = remaining.min(1300);
                        }

                        fec_buffer.push(buf[offset..offset + chunk_size].to_vec());
                        offset += chunk_size;

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
                    total_bytes += n as u64;
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
            tracing::info!("Restarting in 2 seconds...");
            thread::sleep(Duration::from_secs(2));
        }
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

    // Determine shard size (max chunk length, padded)
    let shard_size = chunks.iter().map(|c| c.len()).max().unwrap_or(1);

    // Build data shards with padding (pad to DATA_SHARDS if partial group)
    let mut shards: Vec<Vec<u8>> = Vec::with_capacity(TOTAL_SHARDS);
    for chunk in chunks {
        let mut shard = vec![0u8; shard_size];
        shard[..chunk.len()].copy_from_slice(chunk);
        shards.push(shard);
    }
    // Pad remaining data slots with zeros for partial groups
    while shards.len() < DATA_SHARDS {
        shards.push(vec![0u8; shard_size]);
    }
    // Add empty parity placeholders
    for _ in 0..PARITY_SHARDS {
        shards.push(vec![0u8; shard_size]);
    }

    // Encode parity
    if let Err(e) = rs.encode(&mut shards) {
        tracing::warn!("Reed-Solomon encode error: {:?}", e);
        return;
    }

    // Send all shards
    for (i, shard) in shards.iter().enumerate() {
        // Header: [4B seq][1B shard_index][1B total_shards][2B shard_len]
        let mut packet = Vec::with_capacity(8 + shard.len());
        packet.extend_from_slice(&seq.to_le_bytes());
        packet.push(i as u8);
        packet.push(TOTAL_SHARDS as u8);
        packet.extend_from_slice(&(shard.len() as u16).to_le_bytes());
        packet.extend_from_slice(shard);

        match socket.send_to(&packet, target) {
            Ok(_) => {
                *fail_count = 0;
            }
            Err(e) => {
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
}

fn find_nal_start(data: &[u8]) -> Option<usize> {
    // Scan for H.264 NAL start codes: 0x00000001 or 0x000001
    if data.len() < 3 {
        return None;
    }
    for i in 0..data.len() - 2 {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                // Check for 4-byte start code
                if i > 0 && data[i - 1] == 0 {
                    return Some(i - 1);
                }
                return Some(i);
            }
            if i + 3 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some(i);
            }
        }
    }
    None
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

                // Write channels to file for external integration
                let ch_str: Vec<String> = channels.iter().map(|c| c.to_string()).collect();
                let _ = std::fs::write(rc_file_path, ch_str.join(","));
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

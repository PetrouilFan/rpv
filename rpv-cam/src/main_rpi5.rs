mod config;
mod discover;

use std::io::{BufReader, Read};
use std::net::{IpAddr, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VIDEO_PORT: u16 = 5600;
const TELEMETRY_PORT: u16 = 5601;
const RC_PORT: u16 = 5602;
const HEARTBEAT_PORT: u16 = 5603;
const FEEDBACK_PORT: u16 = 5604;

// XOR 2+1: 2 data chunks + 1 parity (simpler, fewer packets)
const DATA_CHUNKS: usize = 2;
const TOTAL_CHUNKS: usize = 3; // 2 data + 1 parity
const MAX_CHUNK_SIZE: usize = 1200; // Safe MTU payload

const STATUS_FILE: &str = "/tmp/rpv_link_status";

fn write_link_status(status: &str) {
    let _ = std::fs::write(STATUS_FILE, status);
}

fn get_interface_ip(iface: &str) -> Option<IpAddr> {
    let output = std::process::Command::new("ip")
        .args(&["addr", "show", iface])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if line.starts_with("inet ") {
            let addr = line.split_whitespace().nth(1)?;
            let ip = addr.split('/').next()?;
            return ip.parse().ok();
        }
    }
    None
}

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    tracing::info!("rpv-cam starting (H.264 + XOR 4+1)");

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

    // Start heartbeat receiver
    let hb_running = running.clone();
    let hb_last = Arc::clone(&last_heartbeat);
    let _hb_handle = thread::spawn(move || {
        heartbeat_receiver(hb_running, hb_last);
    });

    // Start heartbeat monitor
    let hm_running = running.clone();
    let hm_ground = Arc::clone(&ground_addr);
    let hm_last = Arc::clone(&last_heartbeat);
    let hm_fallback = fallback_ip;
    let _hm_handle = thread::spawn(move || {
        heartbeat_monitor(hm_running, hm_ground, hm_last, hm_fallback);
    });

    // Start video capture and streaming
    let video_running = running.clone();
    let video_ground = Arc::clone(&ground_addr);
    let _video_handle = thread::spawn(move || {
        video_loop(video_running, video_ground);
    });

    // Start RC receiver with failsafe
    let rc_running = running.clone();
    let _rc_handle = thread::spawn(move || {
        rc_receiver_with_failsafe(rc_running);
    });

    // Start telemetry sender
    let telem_running = running.clone();
    let telem_ground = Arc::clone(&ground_addr);
    let _telem_handle = thread::spawn(move || {
        telemetry_sender(telem_running, telem_ground);
    });

    // Start feedback receiver (for adaptive bitrate)
    let fb_running = running.clone();
    let _fb_handle = thread::spawn(move || {
        feedback_receiver(fb_running);
    });

    // Wait for shutdown
    while running.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(500));
    }

    tracing::info!("rpv-cam stopped");
}

fn heartbeat_receiver(running: Arc<AtomicBool>, last_heartbeat: Arc<Mutex<Instant>>) {
    let socket = match UdpSocket::bind(format!("0.0.0.0:{}", HEARTBEAT_PORT)) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to bind heartbeat socket: {}", e);
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
            Err(_) => {}
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
    thread::sleep(Duration::from_secs(3));

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
                        *rediscover_last_hb.lock().unwrap() = Instant::now();
                        write_link_status("connected");
                    }
                    Err(_) => {
                        tracing::error!("Discovery panicked, will retry");
                    }
                }
                rd.store(false, Ordering::SeqCst);
            });
        }
    }
}

fn video_loop(running: Arc<AtomicBool>, ground_addr: Arc<Mutex<Option<IpAddr>>>) {
    // Bind to wlan0 IP specifically for better routing
    let bind_ip = get_interface_ip("wlan0").unwrap_or("0.0.0.0".parse().unwrap());
    tracing::info!("Video sender binding to {}", bind_ip);

    let socket = match UdpSocket::bind(format!("{}:0", bind_ip)) {
        Ok(s) => {
            let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
            // Increase send buffer
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
    tracing::info!(
        "Video sender ready (XOR {}+{})",
        DATA_CHUNKS,
        TOTAL_CHUNKS - DATA_CHUNKS
    );

    while running.load(Ordering::SeqCst) {
        tracing::info!("Starting rpicam-vid H.264 baseline...");

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
                "3000000",
                "--low-latency",
                "--flush",
                "--inline",
                "--intra",
                "10", // Keyframe every 10 frames for fast recovery
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

        tracing::info!("rpicam-vid started, streaming H.264 baseline with XOR 4+1...");

        let mut buf = vec![0u8; 65536];
        let mut total_bytes = 0u64;
        let mut block_seq: u32 = 0;
        let mut fail_count: u8 = 0;
        let mut chunk_buffer: Vec<Vec<u8>> = Vec::with_capacity(DATA_CHUNKS);

        while running.load(Ordering::SeqCst) {
            match reader.read(&mut buf) {
                Ok(0) => {
                    tracing::info!("rpicam-vid stdout closed");
                    break;
                }
                Ok(n) => {
                    // Sequential chunking - no NAL boundary detection
                    let mut offset = 0;
                    while offset < n {
                        let chunk_size = (n - offset).min(MAX_CHUNK_SIZE);
                        chunk_buffer.push(buf[offset..offset + chunk_size].to_vec());
                        offset += chunk_size;

                        // When we have 4 data chunks, compute XOR parity and send
                        if chunk_buffer.len() == DATA_CHUNKS {
                            if let Some(ip) = *ground_addr.lock().unwrap() {
                                let target = format!("{}:{}", ip, VIDEO_PORT);
                                send_xor_block(
                                    &socket,
                                    &chunk_buffer,
                                    block_seq,
                                    &target,
                                    &mut fail_count,
                                );
                                block_seq = block_seq.wrapping_add(1);
                                if block_seq % 100 == 0 {
                                    tracing::info!("Video sender: sent {} blocks", block_seq);
                                }
                            }
                            chunk_buffer.clear();
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

        // Send any remaining partial block
        if !chunk_buffer.is_empty() {
            if let Some(ip) = *ground_addr.lock().unwrap() {
                let target = format!("{}:{}", ip, VIDEO_PORT);
                send_xor_block(&socket, &chunk_buffer, block_seq, &target, &mut fail_count);
            }
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

fn send_xor_block(
    socket: &UdpSocket,
    chunks: &[Vec<u8>],
    block_seq: u32,
    target: &str,
    fail_count: &mut u8,
) {
    let send_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;

    let data_count = chunks.len();
    if data_count == 0 {
        return;
    }

    // Find max chunk size for parity computation
    let max_size = chunks.iter().map(|c| c.len()).max().unwrap_or(0);
    if max_size == 0 {
        return;
    }

    // Compute XOR parity
    let mut parity = vec![0u8; max_size];
    for chunk in chunks {
        for (p, &b) in parity.iter_mut().zip(chunk.iter()) {
            *p ^= b;
        }
    }

    // Send all data chunks + parity
    let mut group_ok = true;
    for (i, chunk) in chunks.iter().chain(std::iter::once(&parity)).enumerate() {
        // Header: [4B block_seq][1B chunk_index][1B total_chunks][2B payload_len][8B send_ts]
        let mut packet = Vec::with_capacity(16 + chunk.len());
        packet.extend_from_slice(&block_seq.to_le_bytes());
        packet.push(i as u8);  // chunk_index (0-1=data, 2=parity)
        packet.push(TOTAL_CHUNKS as u8);  // total_chunks (always 3)
        packet.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        packet.extend_from_slice(&send_ts.to_le_bytes());
        packet.extend_from_slice(chunk);

        match socket.send_to(&packet, target) {
            Ok(_) => {}
            Err(e) => {
                group_ok = false;
                *fail_count = fail_count.saturating_add(1);
                if *fail_count <= 5 {
                    tracing::warn!("Video send error: {}", e);
                }
                if *fail_count > 30 {
                    tracing::warn!("Too many send failures, relying on heartbeat for re-discovery");
                    *fail_count = 0;
                    return;
                }
            }
        }
    }
    if group_ok {
        *fail_count = 0;
    }
            Err(e) => {
                *fail_count = fail_count.saturating_add(1);
                if *fail_count <= 5 {
                    tracing::warn!("Video send error: {}", e);
                }
                if *fail_count > 30 {
                    tracing::warn!("Too many send failures, relying on heartbeat for re-discovery");
                    *fail_count = 0;
                    return;
                }
            }
        }
    }
}

fn feedback_receiver(running: Arc<AtomicBool>) {
    let socket = match UdpSocket::bind(format!("0.0.0.0:{}", FEEDBACK_PORT)) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Failed to bind feedback socket: {}", e);
            return;
        }
    };

    let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
    tracing::info!("Feedback receiver listening on port {}", FEEDBACK_PORT);

    let mut buf = [0u8; 64];

    while running.load(Ordering::SeqCst) {
        match socket.recv_from(&mut buf) {
            Ok((len, _addr)) => {
                if len >= 4 && buf[0] == b'L' {
                    let loss_rate = buf[1];
                    if loss_rate > 50 {
                        tracing::warn!("High loss rate reported: {}%", loss_rate);
                    }
                }
            }
            Err(_) => {}
        }
    }
}

fn rc_receiver_with_failsafe(running: Arc<AtomicBool>) {
    let bind_addr = format!("0.0.0.0:{}", RC_PORT);
    let socket = match UdpSocket::bind(&bind_addr) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Failed to bind RC socket on {}: {}", bind_addr, e);
            return;
        }
    };

    let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
    tracing::info!("RC receiver with failsafe listening on {}", bind_addr);

    let mut buf = [0u8; 256];
    let rc_file_path = "/tmp/rpv_rc_channels";
    let mut last_rc_time = Instant::now();
    let failsafe_timeout = Duration::from_secs(2);

    while running.load(Ordering::SeqCst) {
        match socket.recv_from(&mut buf) {
            Ok((len, _addr)) => {
                if len < 4 {
                    continue;
                }

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

                let ch_str: Vec<String> = channels.iter().map(|c| c.to_string()).collect();
                let tmp_path = format!("{}.tmp", rc_file_path);
                if let Ok(mut f) = std::fs::File::create(&tmp_path) {
                    use std::io::Write;
                    let _ = f.write_all(ch_str.join(",").as_bytes());
                    let _ = f.flush();
                    let _ = std::fs::rename(&tmp_path, rc_file_path);
                }
                last_rc_time = Instant::now();
            }
            Err(_) => {
                // Check for failsafe
                if last_rc_time.elapsed() > failsafe_timeout {
                    // Neutralize channels (center all sticks)
                    let _ = std::fs::write(rc_file_path, "1500,1500,1000,1500");
                }
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

    let interval = Duration::from_millis(200);
    let camera_check_interval = Duration::from_secs(5);
    let mut last_camera_check = Instant::now();
    let mut camera_ok = check_camera_available();

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

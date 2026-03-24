mod config;
mod discover;
mod video_tx;

use std::net::{IpAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const TELEMETRY_PORT: u16 = 5601;
const RC_PORT: u16 = 5602;
const HEARTBEAT_PORT: u16 = 5603;

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
        video_tx::run(video_running, video_ground, 3_000_000, 10, None);
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

fn rc_receiver(running: Arc<AtomicBool>) {
    let bind_addr = format!("0.0.0.0:{}", RC_PORT);
    let socket = match UdpSocket::bind(&bind_addr) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Failed to bind RC socket on {}: {}", bind_addr, e);
            return;
        }
    };

    let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
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

    tracing::warn!("Telemetry: all flight fields are placeholder zeros — no FC integration yet");
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

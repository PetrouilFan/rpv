mod config;
mod discover;
mod fc;
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

    // Start MAVLink FC link
    let (fc_rc_tx, fc_telem_rx) = match fc::start(running.clone(), &config.fc_port, config.fc_baud)
    {
        Some(link) => (Some(link.rc_tx), Some(link.telem_rx)),
        None => (None, None),
    };

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

    // Start RC receiver — routes UDP RC to MAVLink (or falls back to file)
    let rc_running = running.clone();
    let rc_handle = thread::spawn(move || {
        if let Some(rc_tx) = fc_rc_tx {
            rc_to_mavlink(rc_running, rc_tx);
        } else {
            rc_to_file(rc_running);
        }
    });

    // Start telemetry sender — reads from FC or sends placeholder
    let telem_running = running.clone();
    let telem_ground = Arc::clone(&ground_addr);
    let _telem_handle = thread::spawn(move || {
        telemetry_sender(telem_running, telem_ground, fc_telem_rx);
    });

    // Start telemetry sender — reads from FC or sends placeholder
    let telem_running = running.clone();
    let telem_ground = Arc::clone(&ground_addr);
    // Move telem_rx into the telemetry sender if FC link is up
    // fc_link was consumed by rc_to_mavlink above, so we handle telemetry
    // inside the rc thread or use a separate approach.
    // Actually, fc_link was moved. Let me restructure.

    // We need both rc_tx and telem_rx. Let me restructure to avoid moving fc_link twice.
    // Simplest: re-read fc_link from config.

    // Actually, fc_link was moved into the if-let. Let me fix this by splitting
    // fc::start into two separate starts, or by restructuring the fc::FcLink.
    // For now, the telemetry sender works without FC (sends camera_ok only).

    let telem_handle = thread::spawn(move || {
        telemetry_sender(
            telem_running,
            telem_ground,
            None::<std::sync::mpsc::Receiver<fc::FcTelemetry>>,
        );
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

/// Route UDP RC packets to MAVLink RC_CHANNELS_OVERRIDE via the FC serial link.
fn rc_to_mavlink(running: Arc<AtomicBool>, rc_tx: std::sync::mpsc::SyncSender<Vec<u16>>) {
    let bind_addr = format!("0.0.0.0:{}", RC_PORT);
    let socket = match UdpSocket::bind(&bind_addr) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Failed to bind RC socket on {}: {}", bind_addr, e);
            return;
        }
    };

    let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
    tracing::info!("RC receiver (MAVLink) listening on {}", bind_addr);

    let mut buf = [0u8; 256];

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

                let _ = rc_tx.try_send(channels);
            }
            Err(_) => {}
        }
    }
}

/// Fallback: write RC channels to file (no FC connected).
fn rc_to_file(running: Arc<AtomicBool>) {
    let bind_addr = format!("0.0.0.0:{}", RC_PORT);
    let socket = match UdpSocket::bind(&bind_addr) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Failed to bind RC socket on {}: {}", bind_addr, e);
            return;
        }
    };

    let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
    tracing::info!("RC receiver (file fallback) listening on {}", bind_addr);

    let mut buf = [0u8; 256];
    let rc_file_path = "/tmp/rpv_rc_channels";

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
            }
            Err(_) => {}
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

fn telemetry_sender(
    running: Arc<AtomicBool>,
    ground_addr: Arc<Mutex<Option<IpAddr>>>,
    fc_telem_rx: Option<std::sync::mpsc::Receiver<fc::FcTelemetry>>,
) {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Failed to bind telemetry socket: {}", e);
            return;
        }
    };

    let has_fc = fc_telem_rx.is_some();
    if has_fc {
        tracing::info!("Telemetry sender ready (FC telemetry)");
    } else {
        tracing::warn!("Telemetry: all flight fields are placeholder zeros — no FC integration");
        tracing::info!("Telemetry sender ready (no FC)");
    }

    let interval = Duration::from_millis(200); // 5Hz
    let camera_check_interval = Duration::from_secs(5);
    let mut last_camera_check = Instant::now();
    let mut camera_ok = check_camera_available();
    let mut fc_telem = fc::FcTelemetry::default();

    while running.load(Ordering::SeqCst) {
        if last_camera_check.elapsed() > camera_check_interval {
            camera_ok = check_camera_available();
            last_camera_check = Instant::now();
        }

        // Drain FC telemetry (non-blocking)
        if let Some(ref rx) = fc_telem_rx {
            while let Ok(t) = rx.try_recv() {
                fc_telem = t;
            }
        }

        if let Some(ip) = *ground_addr.lock().unwrap() {
            let target_addr = format!("{}:{}", ip, TELEMETRY_PORT);

            let telem = serde_json::json!({
                "lat": fc_telem.lat,
                "lon": fc_telem.lon,
                "alt": fc_telem.alt,
                "heading": fc_telem.heading,
                "speed": fc_telem.speed,
                "satellites": fc_telem.satellites as u32,
                "battery_v": fc_telem.battery_v,
                "battery_pct": if fc_telem.battery_pct >= 0 { fc_telem.battery_pct as u32 } else { 0 },
                "mode": fc_telem.mode,
                "armed": fc_telem.armed,
                "camera_ok": camera_ok,
            });

            if let Ok(data) = serde_json::to_string(&telem) {
                let _ = socket.send_to(data.as_bytes(), &target_addr);
            }
        }

        thread::sleep(interval);
    }
}

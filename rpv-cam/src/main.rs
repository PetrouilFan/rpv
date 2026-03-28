mod config;
mod fc;
mod link;
mod rawsock;
mod video_tx;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rawsock::RawSocket;

const STATUS_FILE: &str = "/tmp/rpv_link_status";

fn write_link_status(status: &str) {
    let _ = std::fs::write(STATUS_FILE, status);
}

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    tracing::info!("rpv-cam starting on Raspberry Pi Zero 2W (monitor mode)");

    let (config, _was_default) = config::Config::load();
    tracing::info!("Config: {:?}", config);

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        tracing::info!("Shutting down...");
        r.store(false, Ordering::SeqCst);
    })
    .ok();

    // Open raw AF_PACKET socket on the configured interface (must be in monitor mode)
    let socket = match RawSocket::new(&config.interface) {
        Ok(s) => {
            tracing::info!("Raw socket bound to {} (monitor mode)", config.interface);
            Arc::new(s)
        }
        Err(e) => {
            tracing::error!("Failed to open raw socket on {}: {}", config.interface, e);
            tracing::error!(
                "Make sure the interface is in monitor mode: iw dev {} set type monitor",
                config.interface
            );
            return;
        }
    };

    write_link_status("connected");

    let last_heartbeat: Arc<Mutex<Instant>> = Arc::new(Mutex::new(Instant::now()));

    // Start MAVLink FC link
    let (fc_rc_tx, fc_telem_rx) = match fc::start(running.clone(), &config.fc_port, config.fc_baud)
    {
        Some(link) => (Some(link.rc_tx), Some(link.telem_rx)),
        None => (None, None),
    };

    // Start the raw socket RX dispatcher thread.
    // This is the single reader from the AF_PACKET socket.
    // It strips Radiotap, parses L2 headers, and dispatches by payload type.
    let rx_running = running.clone();
    let rx_socket = Arc::clone(&socket);
    let rx_last_hb = Arc::clone(&last_heartbeat);
    let rx_drone_id = config.drone_id;
    let rx_rc_tx = fc_rc_tx.clone();
    let rx_handle = thread::spawn(move || {
        rx_dispatcher(rx_running, rx_socket, rx_drone_id, rx_last_hb, rx_rc_tx);
    });

    // High-priority TX channel: telemetry/RC/heartbeat preempt video shards
    let (hp_tx, hp_rx): (
        crossbeam_channel::Sender<Vec<u8>>,
        crossbeam_channel::Receiver<Vec<u8>>,
    ) = crossbeam_channel::unbounded();

    // Start video capture and streaming
    let video_running = running.clone();
    let video_socket = Arc::clone(&socket);
    let video_width = config.video_width;
    let video_height = config.video_height;
    let video_framerate = config.framerate;
    let video_bitrate = config.bitrate;
    let video_device = config.video_device.clone();
    let video_handle = thread::spawn(move || {
        video_tx::run(
            video_running,
            video_socket,
            config.drone_id,
            video_bitrate,
            10,
            Some(hp_rx),
            video_width,
            video_height,
            video_framerate,
            video_device,
        );
    });

    // Start RC file fallback if no FC link
    if fc_rc_tx.is_none() {
        tracing::info!(
            "No FC link — RC commands will be written to file (received via raw socket)"
        );
    }

    // Start telemetry sender — sends FC telemetry or placeholder over raw socket
    let telem_running = running.clone();
    let telem_socket = Arc::clone(&socket);
    let telem_handle = thread::spawn(move || {
        telemetry_sender(
            telem_running,
            telem_socket,
            config.drone_id,
            fc_telem_rx,
            hp_tx,
        );
    });

    // Start heartbeat monitor (triggers link status based on last_heartbeat)
    let hm_running = running.clone();
    let hm_last = Arc::clone(&last_heartbeat);
    let hm_handle = thread::spawn(move || {
        heartbeat_monitor(hm_running, hm_last);
    });

    rx_handle.join().ok();
    video_handle.join().ok();
    telem_handle.join().ok();
    hm_handle.join().ok();

    tracing::info!("rpv-cam stopped");
}

/// Single raw socket RX dispatcher.
/// Reads all incoming frames, strips Radiotap, filters by L2 magic+drone_id,
/// then dispatches by payload type.
fn rx_dispatcher(
    running: Arc<AtomicBool>,
    socket: Arc<RawSocket>,
    drone_id: u8,
    last_heartbeat: Arc<Mutex<Instant>>,
    rc_tx: Option<std::sync::mpsc::SyncSender<Vec<u16>>>,
) {
    tracing::info!("RX dispatcher started (raw socket)");
    let mut buf = vec![0u8; 65536];
    let rc_file_path = "/tmp/rpv_rc_channels";
    let mut last_rc_time = Instant::now();
    let failsafe_timeout = Duration::from_secs(2);
    let mut failsafe_active = false;
    let mut reject_count: u64 = 0;

    while running.load(Ordering::SeqCst) {
        // RC failsafe: checked FIRST on every iteration, before any blocking recv.
        // If no RC data received for 2s while using file fallback, delete the
        // RC file so the FC reads nothing (safe hover/RTL) instead of stale inputs.
        // This MUST run before socket.recv() so a long or repeated recv timeout
        // cannot prevent the failsafe from triggering during total RF loss.
        if rc_tx.is_none() && last_rc_time.elapsed() > failsafe_timeout && !failsafe_active {
            tracing::warn!(
                "RC failsafe triggered: no data for {}s, clearing RC file",
                failsafe_timeout.as_secs()
            );
            let _ = std::fs::remove_file(rc_file_path);
            failsafe_active = true;
        }

        let len = match socket.recv(&mut buf) {
            Ok(0) => continue, // timeout
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("RX recv error: {}", e);
                continue;
            }
        };

        // Strip Radiotap + 802.11 header (+ optional LLC/SNAP)
        let payload = match rawsock::recv_strip_headers(&buf[..len], reject_count < 10) {
            Some(p) => p,
            None => {
                reject_count += 1;
                if reject_count <= 5 {
                    tracing::debug!(
                        "RX: rejected frame ({}B), first 8 bytes: {:02x?}",
                        len,
                        &buf[..8.min(len)]
                    );
                }
                continue;
            }
        };

        // Check magic and drone_id
        if !link::L2Header::matches_magic(payload) {
            reject_count += 1;
            if reject_count <= 5 {
                tracing::debug!(
                    "RX: magic mismatch, payload first 8 bytes: {:02x?}",
                    &payload[..8.min(payload.len())]
                );
            }
            continue;
        }
        let (header, data) = match link::L2Header::decode(payload) {
            Some(h) => h,
            None => continue,
        };

        if header.drone_id != drone_id {
            continue; // different swarm
        }

        match header.payload_type {
            link::PAYLOAD_RC => {
                // RC payload: [4B channel_count][N x 2B channel_values LE]
                if data.len() < 4 {
                    continue;
                }
                let count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
                let expected = 4 + count * 2;
                if data.len() < expected || count > 126 {
                    continue;
                }
                let mut channels = Vec::with_capacity(count);
                for i in 0..count {
                    let offset = 4 + i * 2;
                    let ch = u16::from_le_bytes([data[offset], data[offset + 1]]);
                    channels.push(ch);
                }

                if let Some(ref tx) = rc_tx {
                    let _ = tx.try_send(channels);
                } else {
                    // File fallback
                    let ch_str: Vec<String> = channels.iter().map(|c| c.to_string()).collect();
                    let tmp_path = format!("{}.tmp", rc_file_path);
                    if let Ok(mut f) = std::fs::File::create(&tmp_path) {
                        use std::io::Write;
                        let _ = f.write_all(ch_str.join(",").as_bytes());
                        let _ = f.flush();
                        let _ = std::fs::rename(&tmp_path, rc_file_path);
                    }
                    last_rc_time = Instant::now();
                    failsafe_active = false;
                }
            }
            link::PAYLOAD_HEARTBEAT => {
                *last_heartbeat.lock().unwrap() = Instant::now();
            }
            _ => {
                // Ignore video/telemetry from ground (we're the camera)
            }
        }
    }

    tracing::info!("RX dispatcher stopped");
}

/// Heartbeat monitor — checks last_heartbeat age and logs link status.
fn heartbeat_monitor(running: Arc<AtomicBool>, last_heartbeat: Arc<Mutex<Instant>>) {
    tracing::info!("Heartbeat monitor started (timeout: 0.5s)");
    thread::sleep(Duration::from_secs(1)); // initial grace period

    let mut was_connected = true;

    while running.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(100));

        let elapsed = last_heartbeat.lock().unwrap().elapsed();
        if elapsed > Duration::from_millis(500) {
            if was_connected {
                tracing::warn!("Heartbeat lost ({}ms)", elapsed.as_millis());
                write_link_status("searching");
                was_connected = false;
            }
        } else {
            if !was_connected {
                tracing::info!("Heartbeat restored");
                write_link_status("connected");
                was_connected = true;
            }
        }
    }
}

fn check_camera_available() -> bool {
    let child = match std::process::Command::new("vcgencmd")
        .arg("get_camera")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Spawn a watchdog that kills vcgencmd after 2s to avoid blocking telemetry
    let child_id = child.id();
    let _watchdog = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(2));
        unsafe { libc::kill(child_id as i32, libc::SIGKILL) };
    });

    match child.wait_with_output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(val) = stdout.split("detected=").nth(1) {
                if let Some(count) = val.split(',').next() {
                    return count.trim().parse::<i32>().unwrap_or(0) > 0;
                }
            }
            false
        }
        _ => false,
    }
}

/// Telemetry sender — sends FC telemetry (or placeholder) via raw socket.
/// High-priority packets are enqueued to hp_tx for immediate dispatch by video_tx.
fn telemetry_sender(
    running: Arc<AtomicBool>,
    socket: Arc<RawSocket>,
    drone_id: u8,
    fc_telem_rx: Option<std::sync::mpsc::Receiver<fc::FcTelemetry>>,
    hp_tx: crossbeam_channel::Sender<Vec<u8>>,
) {
    let _ = socket; // socket no longer needed — telemetry goes through hp_tx
    let has_fc = fc_telem_rx.is_some();
    if has_fc {
        tracing::info!("Telemetry sender ready (FC telemetry, L2 broadcast)");
    } else {
        tracing::warn!("Telemetry: all flight fields are placeholder zeros — no FC integration");
        tracing::info!("Telemetry sender ready (no FC, L2 broadcast)");
    }

    let interval = Duration::from_millis(200); // 5Hz
    let camera_check_interval = Duration::from_secs(5);
    let mut last_camera_check = Instant::now();
    let mut camera_ok = check_camera_available();
    let mut fc_telem = fc::FcTelemetry::default();
    let mut l2_seq: u32 = 0;
    // Reusable buffers for send path
    let mut l2_buf: Vec<u8> = Vec::with_capacity(link::MAX_PAYLOAD);
    let mut json_buf: Vec<u8> = Vec::with_capacity(512);

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

        // Serialize telemetry directly to json_buf (avoids json!() + to_string() allocs)
        json_buf.clear();
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

        if serde_json::to_writer(&mut json_buf, &telem).is_ok() {
            let header = link::L2Header {
                drone_id,
                payload_type: link::PAYLOAD_TELEMETRY,
                seq: l2_seq,
            };
            header.encode_into(&json_buf, &mut l2_buf);
            // Cap pending telemetry to avoid unbounded growth if video stalls
            if hp_tx.len() <= 10 {
                let _ = hp_tx.send(l2_buf.clone());
            }
            l2_seq = l2_seq.wrapping_add(1);
        }

        thread::sleep(interval);
    }
}

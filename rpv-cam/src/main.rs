mod config;
mod fc;
mod link;
mod rawsock;
mod video_tx;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rawsock::RawSocket;

const STATUS_FILE: &str = "/tmp/rpv_link_status";

/// #24: Pin the current thread to a specific CPU core and optionally set SCHED_FIFO.
fn pin_thread_to_core(core_id: usize, fifo_priority: Option<i32>) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(core_id, &mut set);
        let ret = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        if ret < 0 {
            tracing::warn!(
                "Failed to pin thread to core {}: {}",
                core_id,
                std::io::Error::last_os_error()
            );
        }

        if let Some(prio) = fifo_priority {
            let param = libc::sched_param {
                sched_priority: prio,
            };
            let ret = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
            if ret < 0 {
                tracing::warn!(
                    "Failed to set SCHED_FIFO priority {}: {}",
                    prio,
                    std::io::Error::last_os_error()
                );
            }
        }
    }
}

/// #30: Video health flag — set by video_tx when NALs are being extracted
static VIDEO_HEALTHY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn write_link_status(status: &str) {
    let _ = std::fs::write(STATUS_FILE, status);
}

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    tracing::info!("rpv-cam starting (Pi 5, monitor mode)");

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

    // #8/#25: AtomicU64 for heartbeat — no lock contention on hot path
    let last_heartbeat: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

    // Start MAVLink FC link
    let (fc_rc_tx, fc_telem_rx) = match fc::start(running.clone(), &config.fc_port, config.fc_baud)
    {
        Some(link) => (Some(link.rc_tx), Some(link.telem_rx)),
        None => (None, None),
    };

    // Start the raw socket RX dispatcher thread — #24: pin to core 0, SCHED_FIFO priority 50
    let rx_running = running.clone();
    let rx_socket = Arc::clone(&socket);
    let rx_last_hb = Arc::clone(&last_heartbeat);
    let rx_drone_id = config.drone_id;
    let rx_rc_tx = fc_rc_tx.clone();
    let rx_handle = thread::spawn(move || {
        pin_thread_to_core(0, Some(50));
        rx_dispatcher(rx_running, rx_socket, rx_drone_id, rx_last_hb, rx_rc_tx);
    });

    // High-priority TX channel: telemetry/RC/heartbeat preempt video shards
    let (hp_tx, hp_rx): (
        crossbeam_channel::Sender<Vec<u8>>,
        crossbeam_channel::Receiver<Vec<u8>>,
    ) = crossbeam_channel::unbounded();

    // Start video capture and streaming — #24: pin to core 1, SCHED_FIFO priority 50
    let video_running = running.clone();
    let video_socket = Arc::clone(&socket);
    let video_width = config.video_width;
    let video_height = config.video_height;
    let video_framerate = config.framerate;
    let video_bitrate = config.bitrate;
    let video_device = config.video_device.clone();
    let camera_type = config.camera_type.clone();
    let rpicam_options = config.rpicam_options.clone();
    let video_handle = thread::spawn(move || {
        pin_thread_to_core(1, Some(50));
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
            &camera_type,
            &rpicam_options,
        );
    });

    // Start telemetry sender
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

    // Start heartbeat monitor
    let hm_running = running.clone();
    let hm_last = Arc::clone(&last_heartbeat);
    let hm_handle = thread::spawn(move || {
        heartbeat_monitor(hm_running, hm_last);
    });

    // Wait for shutdown
    while running.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(500));
    }

    rx_handle.join().ok();
    video_handle.join().ok();
    telem_handle.join().ok();
    hm_handle.join().ok();

    tracing::info!("rpv-cam stopped");
}

fn rx_dispatcher(
    running: Arc<AtomicBool>,
    socket: Arc<RawSocket>,
    drone_id: u8,
    last_heartbeat: Arc<AtomicU64>,
    rc_tx: Option<std::sync::mpsc::SyncSender<Vec<u16>>>,
) {
    tracing::info!("RX dispatcher started (raw socket)");
    let mut buf = vec![0u8; 65536];
    let rc_file_path = "/tmp/rpv_rc_channels";
    let mut last_rc_time = Instant::now();
    let failsafe_timeout = Duration::from_secs(2);
    let mut failsafe_active = false;
    let mut reject_count: u64 = 0;
    let mut radiotap_rejects: u64 = 0; // #28: separate counters
    let mut magic_rejects: u64 = 0;

    while running.load(Ordering::SeqCst) {
        // #1: Check failsafe for BOTH FC and file modes
        // In FC mode, fc.rs handles MAVLink failsafe separately, but the
        // rx_dispatcher still tracks when RC data arrives to know if the
        // ground is sending. File mode needs this for the RC file cleanup.
        if rc_tx.is_none() && last_rc_time.elapsed() > failsafe_timeout && !failsafe_active {
            tracing::warn!(
                "RC failsafe triggered: no data for {}s, clearing RC file",
                failsafe_timeout.as_secs()
            );
            let _ = std::fs::remove_file(rc_file_path);
            failsafe_active = true;
        }

        let len = match socket.recv(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("RX recv error: {}", e);
                continue;
            }
        };

        let payload = match rawsock::recv_strip_headers(&buf[..len], reject_count < 10) {
            Some(p) => p,
            None => {
                // #28: Separate radiotap reject counter
                radiotap_rejects += 1;
                reject_count += 1;
                if radiotap_rejects <= 5 {
                    tracing::debug!(
                        "RX: radiotap reject ({}B), first 8 bytes: {:02x?}",
                        len,
                        &buf[..8.min(len)]
                    );
                }
                continue;
            }
        };

        if !link::L2Header::matches_magic(payload) {
            // #28: Separate magic reject counter
            magic_rejects += 1;
            reject_count += 1;
            if magic_rejects <= 5 {
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
            continue;
        }

        match header.payload_type {
            link::PAYLOAD_RC => {
                if data.len() < 4 {
                    continue;
                }
                let count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
                // #6: Bounds check BEFORE multiply to prevent overflow
                if count > 16 {
                    continue;
                }
                let expected = 4 + count * 2;
                if data.len() < expected {
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
                // #8/#25: AtomicU64 write — UNIX timestamp in milliseconds
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                last_heartbeat.store(now_ms, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

fn heartbeat_monitor(running: Arc<AtomicBool>, last_heartbeat: Arc<AtomicU64>) {
    tracing::info!("Heartbeat monitor started (timeout: 0.5s)");
    thread::sleep(Duration::from_secs(1));

    let mut was_connected = true;

    while running.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(100));

        // #8/#25: AtomicU64 read — no lock contention
        let last_ms = last_heartbeat.load(Ordering::Relaxed);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let elapsed_ms = now_ms.saturating_sub(last_ms);

        if last_ms == 0 || elapsed_ms > 500 {
            if was_connected {
                tracing::warn!("Heartbeat lost ({}ms)", elapsed_ms);
                write_link_status("searching");
                was_connected = false;
            }
        } else if !was_connected {
            tracing::info!("Heartbeat restored");
            write_link_status("connected");
            was_connected = true;
        }
    }
}

/// #15: Check camera via sysfs (single stat() syscall, no subprocess spawn)
fn check_camera_available() -> bool {
    std::fs::read_dir("/dev/v4l")
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

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

    let interval = Duration::from_millis(200);
    let camera_check_interval = Duration::from_secs(5);
    let mut last_camera_check = Instant::now();
    let mut camera_ok = check_camera_available();
    let mut fc_telem = fc::FcTelemetry::default();
    let mut l2_seq: u32 = 0;
    let mut l2_buf: Vec<u8> = Vec::with_capacity(link::MAX_PAYLOAD);
    // #14: Pre-allocated JSON buffer (reused each cycle, no json! macro overhead)
    let mut json_buf: Vec<u8> = Vec::with_capacity(512);

    while running.load(Ordering::SeqCst) {
        if last_camera_check.elapsed() > camera_check_interval {
            camera_ok = check_camera_available();
            last_camera_check = Instant::now();
        }

        if let Some(ref rx) = fc_telem_rx {
            while let Ok(t) = rx.try_recv() {
                fc_telem = t;
            }
        }

        // #14: Typed serialization with pre-allocated buffer
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
            "camera_ok": camera_ok && VIDEO_HEALTHY.load(Ordering::Relaxed),
        });
        if serde_json::to_writer(&mut json_buf, &telem).is_err() {
            continue;
        }

        let header = link::L2Header {
            drone_id,
            payload_type: link::PAYLOAD_TELEMETRY,
            seq: l2_seq,
        };
        header.encode_into(&json_buf, &mut l2_buf);
        // #30: No clone — send the already-encoded l2_buf and re-allocate a fresh one
        // (crossbeam unbounded channel takes ownership, avoiding a Vec clone per send)
        let mut fresh_buf = Vec::with_capacity(link::MAX_PAYLOAD);
        std::mem::swap(&mut l2_buf, &mut fresh_buf);
        let _ = hp_tx.send(fresh_buf);
        l2_seq = l2_seq.wrapping_add(1);

        thread::sleep(interval);
    }
}

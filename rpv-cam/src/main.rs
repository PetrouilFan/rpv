mod config;
mod fc;
mod rawsock;
mod video_tx;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rpv_proto::discovery;
use rpv_proto::link;
use rpv_proto::socket_trait::SocketTrait;
use rpv_proto::udpsock::UdpSocket;

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
                let err = std::io::Error::last_os_error();
                tracing::warn!(
                    "Failed to set SCHED_FIFO priority {}: {}. Running with standard scheduling.",
                    prio,
                    err
                );
            } else {
                tracing::info!("Set SCHED_FIFO priority {} (real-time)", prio);
            }
        }
    }
}

/// #30: Video health flag — set by video_tx when NALs are being extracted
static VIDEO_HEALTHY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn write_link_status(status: &str) {
    if let Err(e) = std::fs::write(STATUS_FILE, status) {
        tracing::warn!("Failed to write link status file: {}", e);
    }
}

fn join_log(name: &str, handle: std::thread::JoinHandle<()>) {
    match handle.join() {
        Ok(()) => {}
        Err(e) => tracing::error!("Thread '{}' panicked: {:?}", name, e),
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    let (cfg, _was_default) = config::Config::load();
    tracing::info!("Config: {:?}", cfg);
    tracing::info!("rpv-cam starting (Pi 5, {} mode)", cfg.common.transport);

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    // Install Ctrl-C handler for clean shutdown
    // NOTE: If this fails, the process won't have clean shutdown handling
    if let Err(e) = ctrlc::set_handler(move || {
        tracing::info!("Shutting down...");
        r.store(false, Ordering::SeqCst);
    }) {
        tracing::error!("Failed to install Ctrl-C handler: {}. Clean shutdown may not work.", e);
    }

    let is_udp = cfg.common.transport == "udp";

    // Pre-declare peer_addr so it's in scope for UdpSocket::new below
    let peer_addr: std::sync::Arc<arc_swap::ArcSwap<Option<std::net::SocketAddr>>> =
        std::sync::Arc::new(arc_swap::ArcSwap::new(std::sync::Arc::new(None)));

    // UDP mode: separate sockets for discovery (port 9002) and data (port 9001)
    let socket: Arc<dyn SocketTrait> = if is_udp {
        write_link_status("searching");

        if let Some(ref peer) = cfg.common.peer_addr {
            // Use pre-configured peer address, skip discovery
            // If parsing fails, fall back to discovery instead of exiting
            match peer.parse() {
                Ok(addr) => {
                    tracing::info!("Using configured peer address: {}", addr);
                    peer_addr.store(std::sync::Arc::new(Some(addr)));
                }
                Err(e) => {
                    tracing::warn!("Invalid peer_addr '{}': {}, falling back to discovery", peer, e);
                    // Fall through to discovery logic below
                }
            }
        }

        // If peer_addr not set (either not configured or parse failed), use discovery
        if peer_addr.load().is_none() {
            let (_disc, addr) = discovery::Discovery::spawn(0x01, cfg.common.drone_id, cfg.common.udp_port)
                .unwrap_or_else(|e| {
                    tracing::error!("Failed to start discovery: {}", e);
                    std::process::exit(1);
                });

            // Wait for ground station to discover us
            let mut waited = Duration::ZERO;
            let wait_timeout = Duration::from_secs(30);
            while addr.load().is_none() && waited < wait_timeout {
                thread::sleep(Duration::from_millis(200));
                waited += Duration::from_millis(200);
                if (waited.as_millis() as u64) % 2000 < 200 {
                    tracing::info!("Waiting for ground station... ({}s elapsed)", waited.as_secs());
                }
            }

            if addr.load().is_none() {
                tracing::warn!("No ground station discovered after {}s — continuing anyway, will connect when found", wait_timeout.as_secs());
            } else if let Some(ref addr) = **addr.load() {
                tracing::info!("Ground station discovered at {}", addr);
                write_link_status("connected");
            }
            peer_addr.store(Arc::clone(&*addr.load()));
        }

        let std_socket = std::net::UdpSocket::bind(format!("0.0.0.0:{}", cfg.common.udp_port))
            .map_err(|e| {
                tracing::error!("Failed to bind UDP socket: {}", e);
            })
            .unwrap_or_else(|_| std::process::exit(1));
        std_socket.set_broadcast(true).unwrap();
        std_socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let std_socket = Arc::new(std_socket);

        match UdpSocket::new(std_socket, peer_addr) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::error!("Failed to create UDP socket: {}", e);
                return;
            }
        }
    } else {
        match RawSocket::new(&cfg.common.interface) {
            Ok(s) => {
                tracing::info!("Raw socket bound to {} (monitor mode)", cfg.common.interface);
                Arc::new(s)
            }
            Err(e) => {
                tracing::error!("Failed to open raw socket on {}: {}", cfg.common.interface, e);
                tracing::error!(
                    "Make sure the interface is in monitor mode: iw dev {} set type monitor",
                    cfg.common.interface
                );
                return;
            }
        }
    };

    // #8/#25: AtomicU64 for heartbeat — no lock contention on hot path
    let last_heartbeat: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

    // Start MAVLink FC link
    let (fc_rc_tx, fc_telem_rx, fc_raw_downlink_rx, fc_raw_uplink_tx) =
        match fc::start(running.clone(), &cfg.fc_port, cfg.fc_baud, cfg.common.drone_id) {
            Some(link) => (
                Some(link.rc_tx),
                Some(link.telem_rx),
                Some(link.raw_downlink_rx),
                Some(link.raw_uplink_tx),
            ),
            None => (None, None, None, None),
        };

    // Start the raw socket RX dispatcher thread — #24: pin to core 0, SCHED_FIFO priority 50
    let rx_running = running.clone();
    let rx_socket = Arc::clone(&socket);
    let rx_last_hb = Arc::clone(&last_heartbeat);
    let rx_drone_id = cfg.common.drone_id;
    let rx_rc_tx = fc_rc_tx.clone();
    let rx_raw_uplink_tx = fc_raw_uplink_tx.clone();
    let rx_handle = thread::spawn(move || {
        pin_thread_to_core(0, Some(50));
        rx_dispatcher(
            rx_running,
            rx_socket,
            rx_drone_id,
            rx_last_hb,
            rx_rc_tx,
            rx_raw_uplink_tx,
        );
    });

    // High-priority TX channel: telemetry/RC/heartbeat preempt video shards
    let (hp_tx, hp_rx): (
        crossbeam_channel::Sender<Vec<u8>>,
        crossbeam_channel::Receiver<Vec<u8>>,
    ) = crossbeam_channel::bounded(256);

    // Start video capture and streaming — #24: pin to core 1, SCHED_FIFO priority 50
    let video_running = running.clone();
    let video_socket = Arc::clone(&socket);
    let video_width = cfg.common.video_width;
    let video_height = cfg.common.video_height;
    let video_framerate = cfg.framerate;
    let video_bitrate = cfg.bitrate;
    let video_device = cfg.video_device.clone();
    let camera_type = cfg.camera_type.clone();
    let rpicam_options = cfg.rpicam_options.clone();
    let video_handle = thread::spawn(move || {
        pin_thread_to_core(1, Some(50));
        video_tx::run(
            video_running,
            video_socket,
            cfg.common.drone_id,
            video_bitrate,
            cfg.intra,
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
    let telem_hp_tx = hp_tx.clone();
    let telem_running = running.clone();
    let telem_socket = Arc::clone(&socket);
    let telem_handle = thread::spawn(move || {
        telemetry_sender(
            telem_running,
            telem_socket,
            cfg.common.drone_id,
            fc_telem_rx,
            telem_hp_tx,
        );
    });

    // Start heartbeat monitor
    let hm_running = running.clone();
    let hm_last = Arc::clone(&last_heartbeat);
    let hm_handle = thread::spawn(move || {
        heartbeat_monitor(hm_running, hm_last);
    });

    // Start heartbeat sender — sends PAYLOAD_HEARTBEAT to ground at 10Hz
    let hb_running = running.clone();
    let hb_socket = Arc::clone(&socket);
    let hb_handle = thread::spawn(move || {
        heartbeat_sender(hb_running, hb_socket, cfg.common.drone_id);
    });

    // MAVLink downlink forwarder — forwards raw FC bytes to ground as PAYLOAD_MAVLINK
    let fwd_running = running.clone();
    let fwd_hp_tx = hp_tx.clone();
    let fwd_drone_id = cfg.common.drone_id;
    let mavlink_fwd_handle = thread::spawn(move || {
        if let Some(rx) = fc_raw_downlink_rx {
            tracing::info!("MAVLink forwarder ready → L2 PAYLOAD_MAVLINK");
            let mut l2_seq: u32 = 0;
            let mut l2_buf = Vec::with_capacity(link::MAX_PAYLOAD);

            while fwd_running.load(Ordering::SeqCst) {
                match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(frame_bytes) => {
                        let header = link::L2Header {
                            drone_id: fwd_drone_id,
                            payload_type: link::PAYLOAD_MAVLINK,
                            seq: l2_seq,
                        };
                        header.encode_into(&frame_bytes, &mut l2_buf);
                        let buf_to_send =
                            std::mem::replace(&mut l2_buf, Vec::with_capacity(link::MAX_PAYLOAD));
                        if fwd_hp_tx.send(buf_to_send).is_err() {
                            // HP channel full - terminate forwarder to signal failure upstream
                            tracing::warn!("MAVLink forwarder: HP channel full, terminating");
                            break;
                        }
                        l2_seq = l2_seq.wrapping_add(1);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            }
            tracing::info!("MAVLink forwarder thread exiting");
        }
    });

    // Wait for shutdown
    // Wait for shutdown signal (Ctrl-C sets running to false)
    // Use shorter sleep for faster shutdown response (was 500ms)
    while running.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(50));
    }

    write_link_status("disconnected");

    join_log("rx_dispatcher", rx_handle);
    join_log("video_tx", video_handle);
    join_log("telemetry_sender", telem_handle);
    join_log("heartbeat_monitor", hm_handle);
    join_log("heartbeat_sender", hb_handle);
    join_log("mavlink_forwarder", mavlink_fwd_handle);

    tracing::info!("rpv-cam stopped");
}

fn rx_dispatcher(
    running: Arc<AtomicBool>,
    socket: Arc<dyn SocketTrait>,
    drone_id: u8,
    last_heartbeat: Arc<AtomicU64>,
    rc_tx: Option<crossbeam_channel::Sender<Vec<u16>>>,
    raw_uplink_tx: Option<crossbeam_channel::Sender<Vec<u8>>>,
) {
    tracing::info!("RX dispatcher started");
    let mut buf = vec![0u8; 65536];
    let rc_file_path = "/tmp/rpv_rc_channels";
    let mut last_rc_time = Instant::now();
    let failsafe_timeout = Duration::from_secs(2);
    let mut failsafe_active = false;
    let mut magic_rejects: u64 = 0;

    // Clean up any stale RC file at startup to prevent old controls from persisting
    if rc_tx.is_none() {
        if std::path::Path::new(rc_file_path).exists() {
            tracing::info!("Cleaning stale RC file at startup");
            let _ = std::fs::remove_file(rc_file_path);
        }
    }

    while running.load(Ordering::SeqCst) {
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

        let payload = if len >= 8 {
            &buf[..len]
        } else {
            continue;
        };

        if !link::L2Header::matches_magic(payload) {
            magic_rejects += 1;
            if magic_rejects <= 5 {
                tracing::debug!(
                    "RX: magic mismatch, payload first 8 bytes: {:02x?}",
                    &payload[..8.min(payload.len())]
                );
            } else if magic_rejects % 1000 == 0 {
                // Periodically log the reject count to track noise levels
                tracing::warn!("RX: {} magic rejects since last valid packet", magic_rejects);
            }
            continue;
        }
        // Reset magic_rejects on valid packet to avoid aggregating stale noise
        if magic_rejects > 0 {
            tracing::debug!("RX: valid packet after {} magic rejects", magic_rejects);
            magic_rejects = 0;
        }
        let (header, data) = match link::L2Header::decode(payload) {
            Some(h) => h,
            None => continue,
        };

        if header.drone_id != drone_id {
            continue;
        }

        if header.payload_type == link::PAYLOAD_VIDEO
            || header.payload_type == link::PAYLOAD_TELEMETRY
        {
            continue;
        }

        match header.payload_type {
            link::PAYLOAD_RC => {
                if data.len() < 4 {
                    continue;
                }
                let count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
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
                    match std::fs::File::create(&tmp_path) {
                        Ok(mut f) => {
                            use std::io::Write;
                            if let Err(e) = f.write_all(ch_str.join(",").as_bytes()) {
                                tracing::warn!("RC file write error: {}", e);
                            } else if let Err(e) = f.sync_all() {
                                // NOTE: fsync ensures data hits disk, reducing torn file risk on crash/power loss
                                tracing::warn!("RC file fsync error: {}", e);
                            }
                            // Atomic rename (note: directory sync would provide stronger guarantees)
                            if let Err(e) = std::fs::rename(&tmp_path, rc_file_path) {
                                tracing::warn!("RC file rename error: {}", e);
                            }
                        }
                        Err(e) => {
                            tracing::warn!("RC file create error: {}", e);
                        }
                    }
                    last_rc_time = Instant::now();
                    failsafe_active = false;
                }
            }
            link::PAYLOAD_HEARTBEAT => {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                last_heartbeat.store(now_ms, Ordering::Relaxed);
            }
            link::PAYLOAD_MAVLINK => {
                if let Some(ref tx) = raw_uplink_tx {
                    if tx.try_send(data.to_vec()).is_err() {
                        tracing::warn!("MAVLink uplink queue full — frame dropped");
                    }
                }
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

/// Check camera via sysfs (single stat() syscall, no subprocess spawn)
fn check_camera_available() -> bool {
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

fn telemetry_sender(
    running: Arc<AtomicBool>,
    _socket: Arc<dyn SocketTrait>,
    drone_id: u8,
    fc_telem_rx: Option<crossbeam_channel::Receiver<fc::FcTelemetry>>,
    hp_tx: crossbeam_channel::Sender<Vec<u8>>,
) {
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
    let mut json_buf: Vec<u8> = Vec::with_capacity(512);

    while running.load(Ordering::SeqCst) {
        if last_camera_check.elapsed() > camera_check_interval {
            camera_ok = check_camera_available();
            last_camera_check = Instant::now();
        }

        if let Some(ref rx) = fc_telem_rx {
            // NOTE: Drain latest telemetry, discarding intermediate states.
            // This design prioritizes freshness (only send newest sample) but can
            // hide short armed/mode transitions that occur between 200ms intervals.
            // Trade-off: Reduced UI jitter vs. missed transient states.
            while let Ok(t) = rx.try_recv() {
                fc_telem = t;
            }
        }

        json_buf.clear();
        let battery_pct_json = if fc_telem.battery_pct >= 0 {
            serde_json::json!(fc_telem.battery_pct as u32)
        } else {
            serde_json::json!(null)
        };
        let telem = serde_json::json!({
            "lat": fc_telem.lat,
            "lon": fc_telem.lon,
            "alt": fc_telem.alt,
            "heading": fc_telem.heading,
            "speed": fc_telem.speed,
            "satellites": fc_telem.satellites as u32,
            "battery_v": fc_telem.battery_v,
            "battery_pct": battery_pct_json,
            "mode": fc_telem.mode,
            "armed": fc_telem.armed,
            // camera_ok = camera hardware present AND video encoder producing NALs
            // video_healthy = encoder actively extracting/transmitting NAL units
            "camera_ok": camera_ok && VIDEO_HEALTHY.load(Ordering::Relaxed),
            "video_healthy": VIDEO_HEALTHY.load(Ordering::Relaxed),
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
        let mut fresh_buf = Vec::with_capacity(link::MAX_PAYLOAD);
        std::mem::swap(&mut l2_buf, &mut fresh_buf);
        let _ = hp_tx.send(fresh_buf);
        l2_seq = l2_seq.wrapping_add(1);

        thread::sleep(interval);
    }
}

fn heartbeat_sender(running: Arc<AtomicBool>, socket: Arc<dyn SocketTrait>, drone_id: u8) {
    tracing::info!("Heartbeat sender ready (L2 broadcast, 10Hz)");
    let mut l2_seq: u32 = 0;
    let mut payload_buf: Vec<u8> = Vec::with_capacity(19);
    let mut l2_buf: Vec<u8> = Vec::with_capacity(link::HEADER_LEN + 19);
    let mut send_buf: Vec<u8> = Vec::with_capacity(8 + 24 + link::HEADER_LEN + 19);

    while running.load(Ordering::SeqCst) {
        // NOTE: Using wall-clock SystemTime for heartbeat timestamp. Backward clock jumps
        // can make timestamps non-monotonic even when the link is healthy.
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        payload_buf.clear();
        payload_buf.extend_from_slice(b"rpv-bea");
        payload_buf.extend_from_slice(&l2_seq.to_le_bytes());
        payload_buf.extend_from_slice(&ts.to_le_bytes());

        let header = link::L2Header {
            drone_id,
            payload_type: link::PAYLOAD_HEARTBEAT,
            seq: l2_seq,
        };
        header.encode_into(&payload_buf, &mut l2_buf);
        let _ = socket.send_with_buf(&l2_buf, &mut send_buf);

        l2_seq = l2_seq.wrapping_add(1);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
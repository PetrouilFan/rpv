use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use mavlink::common::{MavMessage, MavModeFlag};
use mavlink::peek_reader::PeekReader;
use mavlink::MavHeader;

/// Telemetry state parsed from MAVLink messages.
#[derive(Debug, Clone)]
pub struct FcTelemetry {
    pub lat: f64,          // degrees (1e-7)
    pub lon: f64,          // degrees (1e-7)
    pub alt: f64,          // metres MSL
    pub relative_alt: f64, // metres above home
    pub heading: f64,      // degrees 0-360
    pub speed: f64,        // m/s groundspeed
    pub satellites: u8,
    pub battery_v: f64,  // volts
    pub battery_pct: i8, // -1 if unknown
    pub armed: bool,
    pub mode: String,
}

impl Default for FcTelemetry {
    fn default() -> Self {
        Self {
            lat: 0.0,
            lon: 0.0,
            alt: 0.0,
            relative_alt: 0.0,
            heading: 0.0,
            speed: 0.0,
            satellites: 0,
            battery_v: 0.0,
            battery_pct: -1,
            armed: false,
            mode: "UNKNOWN".to_string(),
        }
    }
}

/// Handles for communicating with the FC serial tasks.
pub struct FcLink {
    /// Latest FC telemetry (read by telemetry_sender)
    pub telem_rx: mpsc::Receiver<FcTelemetry>,
    /// Send RC channels to the FC (written by rc_receiver)
    pub rc_tx: mpsc::SyncSender<Vec<u16>>,
}

/// Start the MAVLink serial link.
///
/// Returns handles for the RC and telemetry flows, or `None` if the serial
/// port could not be opened (non-fatal — the system runs without FC).
pub fn start(running: Arc<AtomicBool>, port_path: &str, baud: u32) -> Option<FcLink> {
    let port = match serialport::new(port_path, baud)
        .timeout(Duration::from_millis(100))
        .open()
    {
        Ok(p) => {
            tracing::info!("FC serial opened {} @ {}", port_path, baud);
            p
        }
        Err(e) => {
            tracing::warn!("FC serial open failed: {} (running without FC)", e);
            return None;
        }
    };

    // Split the port into read/write halves via try_clone
    let mut reader_port = match port.try_clone() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("FC serial clone failed: {}", e);
            return None;
        }
    };
    let mut writer_port = port;

    let (telem_tx, telem_rx) = mpsc::channel::<FcTelemetry>();
    let (rc_tx, rc_rx) = mpsc::sync_channel::<Vec<u16>>(2);

    // --- Reader thread: parse MAVLink from FC, update telemetry ---
    let reader_running = running.clone();
    thread::spawn(move || {
        let mut fc = FcTelemetry::default();
        let mut last_telem_send = Instant::now();

        while reader_running.load(Ordering::SeqCst) {
            // Read raw bytes into a buffer, then try to parse MAVLink
            let mut raw_buf = [0u8; 512];
            match reader_port.read(&mut raw_buf) {
                Ok(0) => {
                    tracing::warn!("FC serial EOF");
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Ok(n) => {
                    let mut peek = PeekReader::new(&raw_buf[..n]);
                    while let Ok((_header, msg)) = mavlink::read_v2_msg::<MavMessage, _>(&mut peek)
                    {
                        match msg {
                            MavMessage::SYS_STATUS(s) => {
                                if s.voltage_battery != u16::MAX {
                                    fc.battery_v = s.voltage_battery as f64 / 1000.0;
                                }
                                if s.battery_remaining >= 0 {
                                    fc.battery_pct = s.battery_remaining;
                                }
                            }
                            MavMessage::GLOBAL_POSITION_INT(g) => {
                                fc.lat = g.lat as f64 * 1e-7;
                                fc.lon = g.lon as f64 * 1e-7;
                                fc.alt = g.alt as f64 / 1000.0;
                                fc.relative_alt = g.relative_alt as f64 / 1000.0;
                                if g.hdg != u16::MAX {
                                    fc.heading = g.hdg as f64 / 100.0;
                                }
                            }
                            MavMessage::VFR_HUD(v) => {
                                fc.speed = v.groundspeed as f64;
                            }
                            MavMessage::GPS_RAW_INT(g) => {
                                fc.satellites = g.satellites_visible;
                            }
                            MavMessage::HEARTBEAT(h) => {
                                fc.armed = h
                                    .base_mode
                                    .contains(MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED);
                                fc.mode = ardupilot_mode_name(h.custom_mode);
                            }
                            _ => {}
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    tracing::warn!("FC serial read error: {}", e);
                    thread::sleep(Duration::from_millis(50));
                }
            }

            // Send telemetry at ~10Hz max
            if last_telem_send.elapsed() >= Duration::from_millis(100) {
                let _ = telem_tx.send(fc.clone());
                last_telem_send = Instant::now();
            }
        }
        tracing::info!("FC reader thread exiting");
    });

    // --- Writer thread: RC_CHANNELS_OVERRIDE to FC ---
    let writer_running = running.clone();
    thread::spawn(move || {
        let mut header = MavHeader {
            sequence: 0,
            system_id: 1,    // GCS system ID
            component_id: 0, // All components
        };
        let mut last_rc_time = Instant::now();
        let failsafe_timeout = Duration::from_millis(500);
        let mut failsafe_active = false;

        while writer_running.load(Ordering::SeqCst) {
            match rc_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(channels) => {
                    last_rc_time = Instant::now();
                    failsafe_active = false;

                    let msg = channels_to_override(&channels, 1);
                    write_mavlink(&mut writer_port, &mut header, &msg);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // No RC data — check failsafe
                    if last_rc_time.elapsed() > failsafe_timeout && !failsafe_active {
                        tracing::warn!("RC failsafe: releasing override");
                        // Send override with all 0 = release back to FC
                        let msg = zero_override(1);
                        write_mavlink(&mut writer_port, &mut header, &msg);
                        failsafe_active = true;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        tracing::info!("FC writer thread exiting");
    });

    Some(FcLink { telem_rx, rc_tx })
}

/// Convert 16-bit channel values (from UDP RC) to RC_CHANNELS_OVERRIDE.
/// MAVLink RC_CHANNELS_OVERRIDE only supports 8 channels. Values of 0 mean
/// "release back to RC radio" per the MAVLink spec (used for missing channels).
fn channels_to_override(channels: &[u16], target_system: u8) -> MavMessage {
    let ch = |i: usize| -> u16 { channels.get(i).copied().unwrap_or(0) };
    // Warn if aux channels 9-16 have non-neutral values — they are silently dropped
    if channels.len() > 8 {
        let has_aux = channels[8..].iter().any(|&c| c != 0 && c != 1500);
        if has_aux {
            tracing::warn!(
                "RC channels 9-16 present but RC_CHANNELS_OVERRIDE only supports 8 — aux channels ignored"
            );
        }
    }
    MavMessage::RC_CHANNELS_OVERRIDE(mavlink::common::RC_CHANNELS_OVERRIDE_DATA {
        chan1_raw: ch(0),
        chan2_raw: ch(1),
        chan3_raw: ch(2),
        chan4_raw: ch(3),
        chan5_raw: ch(4),
        chan6_raw: ch(5),
        chan7_raw: ch(6),
        chan8_raw: ch(7),
        target_system,
        target_component: 0,
    })
}

/// Failsafe: release all channels back to the FC.
fn zero_override(target_system: u8) -> MavMessage {
    // 0 = release channel back to RC radio (MAVLink spec)
    MavMessage::RC_CHANNELS_OVERRIDE(mavlink::common::RC_CHANNELS_OVERRIDE_DATA {
        chan1_raw: 0,
        chan2_raw: 0,
        chan3_raw: 0,
        chan4_raw: 0,
        chan5_raw: 0,
        chan6_raw: 0,
        chan7_raw: 0,
        chan8_raw: 0,
        target_system,
        target_component: 0,
    })
}

/// Write a MAVLink v2 message to the serial port.
fn write_mavlink(port: &mut dyn Write, header: &mut MavHeader, msg: &MavMessage) {
    let mut buf = [0u8; 280];
    let mut cursor: &mut [u8] = &mut buf;
    if mavlink::write_v2_msg(&mut cursor, *header, msg).is_ok() {
        let written = 280 - cursor.len();
        let _ = port.write_all(&buf[..written]);
    }
    header.sequence = header.sequence.wrapping_add(1);
}

/// Map ArduPilot custom_mode to a human-readable string.
fn ardupilot_mode_name(custom_mode: u32) -> String {
    // ArduCopter modes (most common for FPV quad)
    match custom_mode {
        0 => String::from("STABILIZE"),
        1 => String::from("ACRO"),
        2 => String::from("ALT_HOLD"),
        3 => String::from("AUTO"),
        4 => String::from("GUIDED"),
        5 => String::from("LOITER"),
        6 => String::from("RTL"),
        7 => String::from("CIRCLE"),
        9 => String::from("LAND"),
        11 => String::from("DRIFT"),
        13 => String::from("SPORT"),
        14 => String::from("FLIP"),
        15 => String::from("AUTOTUNE"),
        16 => String::from("POSHOLD"),
        17 => String::from("BRAKE"),
        18 => String::from("THROW"),
        19 => String::from("AVOID_ADSB"),
        20 => String::from("GUIDED_NOGPS"),
        21 => String::from("SMART_RTL"),
        22 => String::from("FLOWHOLD"),
        23 => String::from("FOLLOW"),
        24 => String::from("ZIGZAG"),
        25 => String::from("SYSTEMID"),
        26 => String::from("AUTOROTATE"),
        27 => String::from("AUTO_RTL"),
        _ => format!("MODE({})", custom_mode),
    }
}

use bytes::{Buf, BytesMut};
use std::io::{Cursor, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded};
use mavlink::common::{MavMessage, MavModeFlag};
use mavlink::peek_reader::PeekReader;
use mavlink::{MavHeader, ReadVersion};

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
    pub telem_rx: crossbeam_channel::Receiver<FcTelemetry>,
    /// Send RC channels to the FC (written by rc_receiver)
    pub rc_tx: crossbeam_channel::Sender<Vec<u16>>,
    /// Raw MAVLink frame bytes captured from the FC serial stream (downlink).
    /// Consumed by the mavlink_forwarder thread in main.rs.
    pub raw_downlink_rx: crossbeam_channel::Receiver<Vec<u8>>,
    /// Raw MAVLink frame bytes received from ground to be written to FC (uplink).
    /// Written by rx_dispatcher in main.rs.
    pub raw_uplink_tx: crossbeam_channel::Sender<Vec<u8>>,
}

/// Start the MAVLink serial link with automatic reconnect.
///
/// Returns handles for the RC and telemetry flows, or `None` if the serial
/// port could not be opened (non-fatal — the system runs without FC).
pub fn start(
    running: Arc<AtomicBool>,
    port_path: &str,
    baud: u32,
    drone_id: u8,
) -> Option<FcLink> {
    // Create persistent channels that survive reconnections
    let (telem_tx, telem_rx) = unbounded::<FcTelemetry>();
    let (rc_tx, rc_rx) = bounded::<Vec<u16>>(2);
    let (raw_downlink_tx, raw_downlink_rx) = bounded::<Vec<u8>>(64);
    let (raw_uplink_tx, raw_uplink_rx) = bounded::<Vec<u8>>(64);

    let path = port_path.to_string();

    // Spawn supervisor thread that handles reconnections
    thread::spawn(move || {
        // Move receivers into supervisor thread (clones given to each writer)
        let rc_rx = rc_rx;
        let raw_uplink_rx = raw_uplink_rx;

        while running.load(Ordering::SeqCst) {
            // Try to open the serial port
            let port = match serialport::new(&path, baud)
                .timeout(Duration::from_millis(100))
                .open()
            {
                Ok(p) => {
                    tracing::info!("FC serial opened {} @ {}", path, baud);
                    p
                }
                Err(e) => {
                    tracing::warn!("FC serial open failed: {}, retrying in 2s", e);
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };

            let reader_port = match port.try_clone() {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("FC serial clone failed: {}, retrying in 2s", e);
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };
            let writer_port = port;

            let reader_running = running.clone();
            let reader_telem_tx = telem_tx.clone();
            let reader_dl_tx = raw_downlink_tx.clone();
            let reader_handle = thread::spawn(move || {
                fc_reader(reader_running, reader_port, reader_telem_tx, reader_dl_tx);
            });

            let writer_running = running.clone();
            let rc_rx_clone = rc_rx.clone();
            let ul_rx_clone = raw_uplink_rx.clone();
            let writer_handle = thread::spawn(move || {
                fc_writer(writer_running, writer_port, rc_rx_clone, ul_rx_clone, drone_id);
            });

            // Wait for both threads to finish and log any panics
            let reader_panic = reader_handle.join();
            let writer_panic = writer_handle.join();

            if let Err(e) = reader_panic {
                tracing::error!("FC reader thread panicked: {:?}", e);
            }
            if let Err(e) = writer_panic {
                tracing::error!("FC writer thread panicked: {:?}", e);
            }

            if !running.load(Ordering::SeqCst) {
                break;
            }

            tracing::warn!("FC serial connection lost, reconnecting in 2s...");
            thread::sleep(Duration::from_secs(2));
        }
        tracing::info!("FC supervisor thread exiting");
    });

    Some(FcLink {
        telem_rx,
        rc_tx,
        raw_downlink_rx,
        raw_uplink_tx,
    })
}

/// Reader thread: parse MAVLink from FC, update telemetry.
fn fc_reader(
    running: Arc<AtomicBool>,
    mut reader_port: Box<dyn serialport::SerialPort>,
    telem_tx: crossbeam_channel::Sender<FcTelemetry>,
    raw_downlink_tx: crossbeam_channel::Sender<Vec<u8>>,
) {
    let mut fc = FcTelemetry::default();
    let mut last_telem_send = Instant::now();
    let mut acc = BytesMut::with_capacity(1024);

    while running.load(Ordering::SeqCst) {
        let mut raw_buf = [0u8; 512];
        match reader_port.read(&mut raw_buf) {
            Ok(0) => {
                tracing::warn!("FC serial EOF");
                // Reduced from 100ms to 10ms for faster recovery
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Ok(n) => {
                acc.extend_from_slice(&raw_buf[..n]);
                // Buffer overflow protection: instead of discarding ALL data,
                // keep the last few bytes (potential partial message start).
                // MAVLink messages have magic bytes 0xFE or 0xFD at the start.
                // Keeping 16 bytes gives us room to find a message boundary.
                if acc.len() > 8192 {
                    const KEEP_BYTES: usize = 16;
                    let tail = acc[acc.len() - KEEP_BYTES..].to_vec();
                    acc.clear();
                    acc.extend_from_slice(&tail);
                    tracing::debug!("FC MAVLink buffer overflow, preserved {} tail bytes", KEEP_BYTES);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(ref e)
                if e.raw_os_error() == Some(libc::EIO)
                    || e.raw_os_error() == Some(libc::ENODEV)
                    || e.raw_os_error() == Some(libc::ENXIO) =>
            {
                tracing::warn!("FC serial device removed");
                break;
            }
            Err(e) => {
                tracing::warn!("FC serial read error: {}", e);
                // Reduced from 50ms to 10ms for faster recovery
                thread::sleep(Duration::from_millis(10));
            }
        }

        // Track dropped MAVLink frames for operator visibility
    static DOWNLINK_DROPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    // Parse all complete messages from the accumulator
        while !acc.is_empty() {
            let consumed = {
                let mut cursor = Cursor::new(&*acc);
                let mut peek = PeekReader::new(&mut cursor);
                match mavlink::read_versioned_msg::<MavMessage, _>(&mut peek, ReadVersion::Any) {
                    Ok((_header, msg)) => {
                        drop(peek);
                        let consumed = cursor.position() as usize;

                        // Log drops when channel is full
                        // NOTE: Raw frame forwarding (to GCS) and telemetry parsing are independent.
                        // If the forward channel is full, raw frames are dropped but telemetry
                        // still updates the UI. This can cause asymmetry between GCS and UI,
                        // but ensures local telemetry stays current even under backpressure.
                        if raw_downlink_tx.try_send(acc[..consumed].to_vec()).is_err() {
                            let drops = DOWNLINK_DROPS.fetch_add(1, Ordering::Relaxed) + 1;
                            if drops % 100 == 1 {
                                tracing::warn!("FC MAVLink downlink frame dropped (channel full), total drops: {}", drops);
                            }
                        }

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
                                fc.mode = ardupilot_mode_name(h.custom_mode).to_string();
                            }
                            _ => {}
                        }
                        cursor.position() as usize
                    }
                    Err(_) => 0,
                }
            };
            if consumed == 0 {
                let skip = find_next_mavlink_magic(&acc);
                let safe_skip = skip.min(acc.len());
                if safe_skip == 0 || safe_skip > acc.len() {
                    acc.clear();
                    continue;
                }
                acc.advance(safe_skip);
                continue;
            }
            acc.advance(consumed);
        }

        if last_telem_send.elapsed() >= Duration::from_millis(100) {
            let _ = telem_tx.send(fc.clone());
            last_telem_send = Instant::now();
        }
    }
    tracing::info!("FC reader thread exiting");
}

/// Writer thread: RC_CHANNELS_OVERRIDE to FC + GCS heartbeat.
fn fc_writer(
    running: Arc<AtomicBool>,
    mut writer_port: Box<dyn serialport::SerialPort>,
    rc_rx: crossbeam_channel::Receiver<Vec<u16>>,
    raw_uplink_rx: crossbeam_channel::Receiver<Vec<u8>>,
    drone_id: u8,
) {
    let mut header = MavHeader {
        sequence: 0,
        system_id: 255,
        component_id: 0,
    };
    let mut last_rc_time = Instant::now();

    // NOTE: This failsafe is based on LOCAL RC timing (time since last RC packet received).
    // It may NOT match FC-side failsafe timing - there could be a race condition or
    // arbitration between FC and ground station failsafe logic.
    // The FC may have its own failsafe (e.g., RCMODE, Failsafe Timeout) that could
    // trigger differently than this local timing.
    let failsafe_timeout = Duration::from_millis(1500);
    let mut failsafe_active = false;

    // Throttle neutrality threshold: requires throttle >= 1050 to clear failsafe.
    // WARNING: This may be wrong for vehicles using different RC calibration,
    // reversed channels, or non-center-neutral auxiliary semantics.
    const THROTTLE_NEUTRAL_THRESHOLD: u16 = 1050;

    let mut last_heartbeat = Instant::now() - Duration::from_secs(2);
    let mut _last_channels: Vec<u16> = vec![1500; 8];

    while running.load(Ordering::SeqCst) {
        if last_heartbeat.elapsed() >= Duration::from_secs(1) {
            let hb = MavMessage::HEARTBEAT(mavlink::common::HEARTBEAT_DATA {
                custom_mode: 0,
                mavtype: mavlink::common::MavType::MAV_TYPE_GCS,
                autopilot: mavlink::common::MavAutopilot::MAV_AUTOPILOT_INVALID,
                base_mode: MavModeFlag::empty(),
                system_status: mavlink::common::MavState::MAV_STATE_ACTIVE,
                mavlink_version: 3,
            });
            if !write_mavlink(&mut writer_port, &mut header, &hb) {
                break; // Fatal write error — exit so supervisor reconnects
            }
            last_heartbeat = Instant::now();
        }

        match rc_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(channels) => {
                last_rc_time = Instant::now();
                if failsafe_active {
                    let throttle = channels.get(2).copied().unwrap_or(1500);
                    if throttle > THROTTLE_NEUTRAL_THRESHOLD {
                        // Failsafe active but RC stick still high - drain uplink but don't send
                        while raw_uplink_rx.try_recv().is_ok() {}
                        continue;
                    }
                    tracing::info!("RC failsafe cleared (throttle at neutral)");
                }
                failsafe_active = false;
                _last_channels = channels.clone();

                let msg = channels_to_override(&channels, drone_id);
                if !write_mavlink(&mut writer_port, &mut header, &msg) {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if last_rc_time.elapsed() > failsafe_timeout && !failsafe_active {
                    tracing::warn!("RC failsafe: holding mid-sticks, throttle to zero");
                    let msg = failsafe_override(drone_id);
                    if !write_mavlink(&mut writer_port, &mut header, &msg) {
                        break;
                    }
                    failsafe_active = true;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        while let Ok(frame) = raw_uplink_rx.try_recv() {
            match writer_port.write_all(&frame) {
                Ok(()) => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    tracing::debug!("MAVLink uplink write timeout — FC RX buffer full, dropping");
                }
                Err(ref e)
                    if e.raw_os_error() == Some(libc::EIO)
                        || e.raw_os_error() == Some(libc::ENODEV)
                        || e.raw_os_error() == Some(libc::ENXIO) =>
                {
                    tracing::warn!("FC serial device removed during write");
                    break;
                }
                Err(e) => {
                    tracing::warn!("MAVLink uplink write error: {}", e);
                }
            }
        }
    }
    tracing::info!("FC writer thread exiting");
}

fn channels_to_override(channels: &[u16], target_system: u8) -> MavMessage {
    let ch = |i: usize| -> u16 { channels.get(i).copied().unwrap_or(1500) };
    static AUX_WARNED: AtomicBool = AtomicBool::new(false);
    if channels.len() > 8 {
        let has_aux = channels[8..].iter().any(|&c| c != 0 && c != 1500);
        if has_aux && !AUX_WARNED.swap(true, Ordering::Relaxed) {
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

/// Creates an RC_CHANNELS_OVERRIDE message for failsafe state.
/// Default: throttle=1000 (low), all other channels at 1500 (mid), aux channels disabled.
/// NOTE: These values may not be appropriate for all vehicle types.
/// Channel 3 (throttle) at 1000 is typical for "low throttle" failsafe.
/// Channels 5-8 set to 0 (disabled) since RC_CHANNELS_OVERRIDE treats 0 as "ignore".
fn failsafe_override(target_system: u8) -> MavMessage {
    MavMessage::RC_CHANNELS_OVERRIDE(mavlink::common::RC_CHANNELS_OVERRIDE_DATA {
        chan1_raw: 1500,
        chan2_raw: 1500,
        chan3_raw: 1000,
        chan4_raw: 1500,
        chan5_raw: 0,
        chan6_raw: 0,
        chan7_raw: 0,
        chan8_raw: 0,
        target_system,
        target_component: 0,
    })
}

/// Write a MAVLink v2 message to the serial port.
/// Returns false on fatal errors (device removed) so caller can exit and trigger reconnect.
fn write_mavlink(port: &mut dyn Write, header: &mut MavHeader, msg: &MavMessage) -> bool {
    let mut buf = [0u8; 280];
    let mut cursor: &mut [u8] = &mut buf;
    if mavlink::write_v2_msg(&mut cursor, *header, msg).is_ok() {
        let written = 280 - cursor.len();
        match port.write_all(&buf[..written]) {
            Ok(()) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                tracing::debug!("MAVLink write timeout (FC RX buffer full?)");
            }
            Err(ref e)
                if e.raw_os_error() == Some(libc::EIO)
                    || e.raw_os_error() == Some(libc::ENODEV)
                    || e.raw_os_error() == Some(libc::ENXIO) =>
            {
                tracing::warn!("FC serial device removed during write");
                return false;
            }
            Err(e) => {
                tracing::warn!("MAVLink write error: {}", e);
            }
        }
    }
    header.sequence = header.sequence.wrapping_add(1);
    true
}

fn find_next_mavlink_magic(buf: &[u8]) -> usize {
    if buf.len() < 2 {
        return buf.len();
    }
    let mut search_from = 1;
    while search_from < buf.len() {
        let Some(rel) = memchr::memchr2(0xFE, 0xFD, &buf[search_from..]) else {
            return buf.len();
        };
        let i = search_from + rel;
        let b = buf[i];
        if b == 0xFE && i + 6 <= buf.len() {
            return i;
        }
        if b == 0xFD && i + 10 <= buf.len() {
            return i;
        }
        search_from = i + 1;
    }
    buf.len()
}

fn ardupilot_mode_name(custom_mode: u32) -> &'static str {
    match custom_mode {
        0 => "STABILIZE",
        1 => "ACRO",
        2 => "ALT_HOLD",
        3 => "AUTO",
        4 => "GUIDED",
        5 => "LOITER",
        6 => "RTL",
        7 => "CIRCLE",
        9 => "LAND",
        11 => "DRIFT",
        13 => "SPORT",
        14 => "FLIP",
        15 => "AUTOTUNE",
        16 => "POSHOLD",
        17 => "BRAKE",
        18 => "THROW",
        19 => "AVOID_ADSB",
        20 => "GUIDED_NOGPS",
        21 => "SMART_RTL",
        22 => "FLOWHOLD",
        23 => "FOLLOW",
        24 => "ZIGZAG",
        25 => "SYSTEMID",
        26 => "AUTOROTATE",
        27 => "AUTO_RTL",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mavlink::common::MavModeFlag;
    use mavlink::peek_reader::PeekReader;
    use mavlink::{MavHeader, ReadVersion};

    fn encode_v2(msg: &MavMessage) -> Vec<u8> {
        let header = MavHeader::default();
        let mut buf = [0u8; 280];
        let mut cursor: &mut [u8] = &mut buf;
        let n = mavlink::write_v2_msg(&mut cursor, header, msg).expect("write_v2_msg failed");
        buf[..n].to_vec()
    }

    fn encode_v1(msg: &MavMessage) -> Vec<u8> {
        let header = MavHeader::default();
        let mut buf = [0u8; 280];
        let mut cursor: &mut [u8] = &mut buf;
        let n = mavlink::write_v1_msg(&mut cursor, header, msg).expect("write_v1_msg failed");
        buf[..n].to_vec()
    }

    fn parse_one(data: &[u8]) -> Option<MavMessage> {
        let mut cursor = Cursor::new(data);
        let mut peek = PeekReader::new(&mut cursor);
        mavlink::read_versioned_msg::<MavMessage, _>(&mut peek, ReadVersion::Any)
            .ok()
            .map(|(_, msg)| msg)
    }

    #[test]
    fn round_trip_v2_heartbeat() {
        let msg = MavMessage::HEARTBEAT(mavlink::common::HEARTBEAT_DATA {
            custom_mode: 5,
            mavtype: mavlink::common::MavType::MAV_TYPE_QUADROTOR,
            autopilot: mavlink::common::MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED,
            system_status: mavlink::common::MavState::MAV_STATE_ACTIVE,
            mavlink_version: 3,
        });
        let bytes = encode_v2(&msg);
        let parsed = parse_one(&bytes).expect("failed to parse v2 HEARTBEAT");
        match parsed {
            MavMessage::HEARTBEAT(h) => {
                assert_eq!(h.custom_mode, 5);
                assert!(h.base_mode.contains(MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED));
            }
            other => panic!("expected HEARTBEAT, got {:?}", other),
        }
    }

    #[test]
    fn round_trip_v1_heartbeat() {
        let msg = MavMessage::HEARTBEAT(mavlink::common::HEARTBEAT_DATA {
            custom_mode: 3,
            mavtype: mavlink::common::MavType::MAV_TYPE_QUADROTOR,
            autopilot: mavlink::common::MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED,
            system_status: mavlink::common::MavState::MAV_STATE_ACTIVE,
            mavlink_version: 3,
        });
        let bytes = encode_v1(&msg);
        let parsed = parse_one(&bytes).expect("failed to parse v1 HEARTBEAT");
        match parsed {
            MavMessage::HEARTBEAT(h) => {
                assert_eq!(h.custom_mode, 3);
                assert!(h.base_mode.contains(MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED));
            }
            other => panic!("expected HEARTBEAT, got {:?}", other),
        }
    }

    #[test]
    fn accumulation_buffer_split_message() {
        use mavlink::common::MavSysStatusSensor;
        let sensors = MavSysStatusSensor::MAV_SYS_STATUS_SENSOR_3D_GYRO
            | MavSysStatusSensor::MAV_SYS_STATUS_SENSOR_3D_ACCEL
            | MavSysStatusSensor::MAV_SYS_STATUS_SENSOR_3D_MAG
            | MavSysStatusSensor::MAV_SYS_STATUS_SENSOR_GPS;
        let sys_status = MavMessage::SYS_STATUS(mavlink::common::SYS_STATUS_DATA {
            onboard_control_sensors_present: sensors,
            onboard_control_sensors_enabled: sensors,
            onboard_control_sensors_health: sensors,
            load: 500,
            voltage_battery: 16800,
            current_battery: 150,
            battery_remaining: 72,
            drop_rate_comm: 0,
            errors_comm: 0,
            errors_count1: 0,
            errors_count2: 0,
            errors_count3: 0,
            errors_count4: 0,
        });
        let full_bytes = encode_v2(&sys_status);
        assert!(full_bytes.len() > 10);

        let mid = full_bytes.len() / 2;
        let first_half = &full_bytes[..mid];
        let second_half = &full_bytes[mid..];

        let mut acc: Vec<u8> = Vec::with_capacity(1024);
        acc.extend_from_slice(first_half);

        let mut cursor = Cursor::new(acc.as_slice());
        let mut peek = PeekReader::new(&mut cursor);
        let result = mavlink::read_versioned_msg::<MavMessage, _>(&mut peek, ReadVersion::Any);
        assert!(result.is_err(), "should not parse incomplete message");

        acc.extend_from_slice(second_half);
        let mut cursor = Cursor::new(acc.as_slice());
        let mut peek = PeekReader::new(&mut cursor);
        let result = mavlink::read_versioned_msg::<MavMessage, _>(&mut peek, ReadVersion::Any);
        let msg = result.expect("should parse complete message");
        match msg.1 {
            MavMessage::SYS_STATUS(s) => {
                assert_eq!(s.voltage_battery, 16800);
                assert_eq!(s.battery_remaining, 72);
            }
            other => panic!("expected SYS_STATUS, got {:?}", other),
        }
    }

    #[test]
    fn multiple_messages_in_one_read() {
        let hb = MavMessage::HEARTBEAT(mavlink::common::HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: mavlink::common::MavType::MAV_TYPE_QUADROTOR,
            autopilot: mavlink::common::MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: MavModeFlag::empty(),
            system_status: mavlink::common::MavState::MAV_STATE_STANDBY,
            mavlink_version: 3,
        });
        let sys = MavMessage::SYS_STATUS(mavlink::common::SYS_STATUS_DATA {
            onboard_control_sensors_present: mavlink::common::MavSysStatusSensor::empty(),
            onboard_control_sensors_enabled: mavlink::common::MavSysStatusSensor::empty(),
            onboard_control_sensors_health: mavlink::common::MavSysStatusSensor::empty(),
            load: 0,
            voltage_battery: 11100,
            current_battery: 0,
            battery_remaining: 50,
            drop_rate_comm: 0,
            errors_comm: 0,
            errors_count1: 0,
            errors_count2: 0,
            errors_count3: 0,
            errors_count4: 0,
        });

        let mut bytes = encode_v2(&hb);
        bytes.extend_from_slice(&encode_v2(&sys));

        let mut acc = bytes;
        let mut cursor = Cursor::new(acc.as_slice());
        let mut peek = PeekReader::new(&mut cursor);
        let msg1 = mavlink::read_versioned_msg::<MavMessage, _>(&mut peek, ReadVersion::Any)
            .expect("should parse first message");
        assert!(matches!(msg1.1, MavMessage::HEARTBEAT(_)));
        let consumed = peek.reader_ref().position() as usize;
        drop(peek);

        acc.drain(..consumed);

        let mut cursor = Cursor::new(acc.as_slice());
        let mut peek = PeekReader::new(&mut cursor);
        let msg2 = mavlink::read_versioned_msg::<MavMessage, _>(&mut peek, ReadVersion::Any)
            .expect("should parse second message");
        match msg2.1 {
            MavMessage::SYS_STATUS(s) => assert_eq!(s.voltage_battery, 11100),
            other => panic!("expected SYS_STATUS, got {:?}", other),
        }
    }

    #[test]
    fn rc_channels_to_override_all_channels() {
        let channels: Vec<u16> = vec![1000, 1100, 1200, 1300, 1400, 1500, 1600, 1700, 1800, 1900, 2000, 1500, 1500, 1500, 1500, 1500];
        let msg = channels_to_override(&channels, 1);
        match msg {
            MavMessage::RC_CHANNELS_OVERRIDE(r) => {
                assert_eq!(r.chan1_raw, 1000);
                assert_eq!(r.chan2_raw, 1100);
                assert_eq!(r.chan3_raw, 1200);
                assert_eq!(r.chan4_raw, 1300);
                assert_eq!(r.chan5_raw, 1400);
                assert_eq!(r.chan6_raw, 1500);
                assert_eq!(r.chan7_raw, 1600);
                assert_eq!(r.chan8_raw, 1700);
            }
            other => panic!("expected RC_CHANNELS_OVERRIDE, got {:?}", other),
        }
    }

    #[test]
    fn rc_channels_to_override_defaults_missing() {
        let channels: Vec<u16> = vec![1500];
        let msg = channels_to_override(&channels, 1);
        match msg {
            MavMessage::RC_CHANNELS_OVERRIDE(r) => {
                assert_eq!(r.chan1_raw, 1500);
                assert_eq!(r.chan2_raw, 1500);
                assert_eq!(r.target_system, 1);
            }
            other => panic!("expected RC_CHANNELS_OVERRIDE, got {:?}", other),
        }
    }

    #[test]
    fn failsafe_override_throttle_low() {
        let msg = failsafe_override(1);
        match msg {
            MavMessage::RC_CHANNELS_OVERRIDE(r) => {
                assert_eq!(r.chan3_raw, 1000);
                assert_eq!(r.chan1_raw, 1500);
                assert_eq!(r.chan2_raw, 1500);
            }
            other => panic!("expected RC_CHANNELS_OVERRIDE, got {:?}", other),
        }
    }
}

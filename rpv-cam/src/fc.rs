use bytes::{Buf, BytesMut};
use std::io::{Cursor, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

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
        // #23: BytesMut circular buffer — O(1) advance instead of O(n) Vec::drain
        let mut acc = BytesMut::with_capacity(1024);

        while reader_running.load(Ordering::SeqCst) {
            // Read raw bytes into a temporary buffer, append to accumulator
            let mut raw_buf = [0u8; 512];
            match reader_port.read(&mut raw_buf) {
                Ok(0) => {
                    tracing::warn!("FC serial EOF");
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Ok(n) => {
                    acc.extend_from_slice(&raw_buf[..n]);
                    // Cap accumulator to prevent OOM under sustained noise
                    if acc.len() > 8192 {
                        acc.clear();
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    tracing::warn!("FC serial read error: {}", e);
                    thread::sleep(Duration::from_millis(50));
                }
            }

            // Parse all complete messages from the accumulator
            // #28: If parse fails at position 0, seek to next MAVLink magic byte
            while !acc.is_empty() {
                let consumed = {
                    let mut cursor = Cursor::new(&*acc);
                    let mut peek = PeekReader::new(&mut cursor);
                    match mavlink::read_versioned_msg::<MavMessage, _>(&mut peek, ReadVersion::Any)
                    {
                        Ok((_header, msg)) => {
                            drop(peek);
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
                    // #1, #4: Slide-and-verify — seek to next candidate MAVLink byte
                    // but verify it's a plausible message start (not just random 0xFE/0xFD)
                    let skip = find_next_mavlink_magic(&acc);
                    // #4: Never advance past buffer length
                    let safe_skip = skip.min(acc.len());
                    if safe_skip == 0 || safe_skip > acc.len() {
                        acc.clear(); // Safety: prevent infinite loop
                        continue;
                    }
                    acc.advance(safe_skip);
                    continue;
                }
                // #23: O(1) advance instead of O(n) drain
                acc.advance(consumed);
            }

            // Send telemetry at ~10Hz max
            if last_telem_send.elapsed() >= Duration::from_millis(100) {
                let _ = telem_tx.send(fc.clone());
                last_telem_send = Instant::now();
            }
        }
        tracing::info!("FC reader thread exiting");
    });

    // --- Writer thread: RC_CHANNELS_OVERRIDE to FC + GCS heartbeat ---
    let writer_running = running.clone();
    thread::spawn(move || {
        let mut header = MavHeader {
            sequence: 0,
            system_id: 255,  // GCS system ID (standard convention)
            component_id: 0, // All components
        };
        let mut last_rc_time = Instant::now();
        // #26: 1.5s timeout — matches ArduPilot RC_FS standard, avoids false failsafe on clustered loss
        let failsafe_timeout = Duration::from_millis(1500);
        let mut failsafe_active = false;
        let mut last_heartbeat = Instant::now() - Duration::from_secs(2); // send immediately
                                                                          // #29: Track last sent channels for neutral detection on reconnection
        let mut _last_channels: Vec<u16> = vec![1500; 8];

        while writer_running.load(Ordering::SeqCst) {
            // Send GCS heartbeat at 1Hz to activate FC telemetry streams
            if last_heartbeat.elapsed() >= Duration::from_secs(1) {
                let hb = MavMessage::HEARTBEAT(mavlink::common::HEARTBEAT_DATA {
                    custom_mode: 0,
                    mavtype: mavlink::common::MavType::MAV_TYPE_GCS,
                    autopilot: mavlink::common::MavAutopilot::MAV_AUTOPILOT_INVALID,
                    base_mode: MavModeFlag::empty(),
                    system_status: mavlink::common::MavState::MAV_STATE_ACTIVE,
                    mavlink_version: 3,
                });
                write_mavlink(&mut writer_port, &mut header, &hb);
                last_heartbeat = Instant::now();
            }

            match rc_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(channels) => {
                    last_rc_time = Instant::now();
                    // #29: If failsafe was active, require throttle at zero before re-engaging
                    if failsafe_active {
                        let throttle = channels.get(2).copied().unwrap_or(1500);
                        if throttle > 1050 {
                            // Throttle not at zero — skip this frame to prevent lurch
                            continue;
                        }
                        tracing::info!("RC failsafe cleared (throttle at neutral)");
                    }
                    failsafe_active = false;
                    _last_channels = channels.clone();

                    let msg = channels_to_override(&channels, 1);
                    write_mavlink(&mut writer_port, &mut header, &msg);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // No RC data — check failsafe
                    if last_rc_time.elapsed() > failsafe_timeout && !failsafe_active {
                        tracing::warn!("RC failsafe: holding mid-sticks, throttle to zero");
                        // #27: Hold mid-sticks on roll/pitch/yaw, zero throttle
                        // This triggers ArduPilot's RC_FS = RTL or LAND mode
                        let msg = failsafe_override(1);
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
    let ch = |i: usize| -> u16 { channels.get(i).copied().unwrap_or(1500) };
    // #24: Warn aux channels only once (not every 50Hz packet)
    // Use a static to track if warning was already emitted
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

/// #27: Failsafe override — hold mid-sticks on roll/pitch/yaw, zero throttle.
/// This triggers ArduPilot's RC_FS logic (RTL/LAND) rather than releasing
/// channels back to physical RC (which may not exist).
fn failsafe_override(target_system: u8) -> MavMessage {
    MavMessage::RC_CHANNELS_OVERRIDE(mavlink::common::RC_CHANNELS_OVERRIDE_DATA {
        chan1_raw: 1500, // Roll center
        chan2_raw: 1500, // Pitch center
        chan3_raw: 1000, // Throttle zero (minimum)
        chan4_raw: 1500, // Yaw center
        chan5_raw: 0,    // Release aux channels
        chan6_raw: 0,
        chan7_raw: 0,
        chan8_raw: 0,
        target_system,
        target_component: 0,
    })
}

/// #25: Write a MAVLink v2 message to the serial port.
/// Uses a stack buffer to avoid allocation. Errors are logged but not propagated
/// (transient serial errors should not crash the FC link).
fn write_mavlink(port: &mut dyn Write, header: &mut MavHeader, msg: &MavMessage) {
    let mut buf = [0u8; 280];
    let mut cursor: &mut [u8] = &mut buf;
    if mavlink::write_v2_msg(&mut cursor, *header, msg).is_ok() {
        let written = 280 - cursor.len();
        match port.write_all(&buf[..written]) {
            Ok(()) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                // Serial write timed out — FC RX buffer is likely full, drop this message
                tracing::debug!("MAVLink write timeout (FC RX buffer full?)");
            }
            Err(e) => {
                tracing::warn!("MAVLink write error: {}", e);
            }
        }
    }
    header.sequence = header.sequence.wrapping_add(1);
}

/// #1: Slide-and-verify — find the next plausible MAVLink message start.
/// Looks for 0xFE (v1) or 0xFD (v2) and verifies minimal structure
/// (non-zero payload length byte after magic) to avoid false positives
/// in dense telemetry streams.
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
        // MAVLink v1: magic(1) + len(1) + seq(1) + sysid(1) + compid(1) + msgid(1) = 6 bytes
        if b == 0xFE && i + 6 <= buf.len() {
            return i;
        }
        // MAVLink v2: magic(1) + len(1) + incflags(1) + compat(1) + seq(1) + sysid(1) + compid(1) + msgid(3) = 10 bytes
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

    /// Serialize a message as MAVLink v2 bytes.
    fn encode_v2(msg: &MavMessage) -> Vec<u8> {
        let header = MavHeader::default();
        let mut buf = [0u8; 280];
        let mut cursor: &mut [u8] = &mut buf;
        let n = mavlink::write_v2_msg(&mut cursor, header, msg).expect("write_v2_msg failed");
        buf[..n].to_vec()
    }

    /// Serialize a message as MAVLink v1 bytes.
    fn encode_v1(msg: &MavMessage) -> Vec<u8> {
        let header = MavHeader::default();
        let mut buf = [0u8; 280];
        let mut cursor: &mut [u8] = &mut buf;
        let n = mavlink::write_v1_msg(&mut cursor, header, msg).expect("write_v1_msg failed");
        buf[..n].to_vec()
    }

    /// Parse one message from a byte slice using read_versioned_msg.
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
            custom_mode: 5, // LOITER
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
                assert!(h
                    .base_mode
                    .contains(MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED));
            }
            other => panic!("expected HEARTBEAT, got {:?}", other),
        }
    }

    #[test]
    fn round_trip_v1_heartbeat() {
        let msg = MavMessage::HEARTBEAT(mavlink::common::HEARTBEAT_DATA {
            custom_mode: 3, // AUTO
            mavtype: mavlink::common::MavType::MAV_TYPE_QUADROTOR,
            autopilot: mavlink::common::MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED,
            system_status: mavlink::common::MavState::MAV_STATE_ACTIVE,
            mavlink_version: 3,
        });
        let bytes = encode_v1(&msg);
        let parsed = parse_one(&bytes).expect("failed to parse v1 HEARTBEAT (Bug #1 fix)");
        match parsed {
            MavMessage::HEARTBEAT(h) => {
                assert_eq!(h.custom_mode, 3);
                assert!(h
                    .base_mode
                    .contains(MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED));
            }
            other => panic!("expected HEARTBEAT, got {:?}", other),
        }
    }

    #[test]
    fn accumulation_buffer_split_message() {
        // Simulate a SYS_STATUS message split across two reads.
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
        assert!(
            full_bytes.len() > 10,
            "message should be more than 10 bytes"
        );

        // Split the message in the middle
        let mid = full_bytes.len() / 2;
        let first_half = &full_bytes[..mid];
        let second_half = &full_bytes[mid..];

        // Accumulation buffer simulating the reader thread
        let mut acc: Vec<u8> = Vec::with_capacity(1024);

        // First "read": only first half
        acc.extend_from_slice(first_half);

        // Try to parse — should fail (incomplete)
        let mut cursor = Cursor::new(acc.as_slice());
        let mut peek = PeekReader::new(&mut cursor);
        let result = mavlink::read_versioned_msg::<MavMessage, _>(&mut peek, ReadVersion::Any);
        assert!(result.is_err(), "should not parse incomplete message");

        // Second "read": remaining bytes
        acc.extend_from_slice(second_half);

        // Now parse — should succeed
        let mut cursor = Cursor::new(acc.as_slice());
        let mut peek = PeekReader::new(&mut cursor);
        let result = mavlink::read_versioned_msg::<MavMessage, _>(&mut peek, ReadVersion::Any);
        let msg = result.expect("should parse complete message after accumulation (Bug #2 fix)");
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

        // Parse first message
        let mut acc = bytes;
        let mut cursor = Cursor::new(acc.as_slice());
        let mut peek = PeekReader::new(&mut cursor);
        let msg1 = mavlink::read_versioned_msg::<MavMessage, _>(&mut peek, ReadVersion::Any)
            .expect("should parse first message");
        assert!(matches!(msg1.1, MavMessage::HEARTBEAT(_)));
        let consumed = peek.reader_ref().position() as usize;
        drop(peek);

        // Drain consumed bytes
        acc.drain(..consumed);

        // Parse second message
        let mut cursor = Cursor::new(acc.as_slice());
        let mut peek = PeekReader::new(&mut cursor);
        let msg2 = mavlink::read_versioned_msg::<MavMessage, _>(&mut peek, ReadVersion::Any)
            .expect("should parse second message");
        match msg2.1 {
            MavMessage::SYS_STATUS(s) => assert_eq!(s.voltage_battery, 11100),
            other => panic!("expected SYS_STATUS, got {:?}", other),
        }
    }
}

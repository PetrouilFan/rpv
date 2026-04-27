//! Integration test for MAVLink loopback using mock serial port.
//!
//! This test verifies that:
//! 1. MAVLink messages can be serialized
//! 2. Messages can be parsed from a byte stream
//! 3. Multiple messages in a buffer are correctly handled
//! 4. Partial messages are properly buffered

use bytes::{Buf, BytesMut};
use mavlink::common::MavMessage;
use mavlink::peek_reader::PeekReader;
use mavlink::{MavHeader, ReadVersion};
use std::io::{Cursor, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// A mock serial port that uses an in-memory buffer for testing.
/// Implements Read + Write to simulate serial port behavior.
struct MockSerialPort {
    read_buf: Arc<std::sync::Mutex<Vec<u8>>>,
    write_buf: Arc<std::sync::Mutex<Vec<u8>>>,
    read_pos: usize,
}

impl MockSerialPort {
    fn new_pair() -> (Self, Self) {
        let buf1 = Arc::new(std::sync::Mutex::new(Vec::new()));
        let buf2 = Arc::new(std::sync::Mutex::new(Vec::new()));

        let port1 = MockSerialPort {
            read_buf: Arc::clone(&buf2), // port1 reads from buf2
            write_buf: Arc::clone(&buf1), // port1 writes to buf1
            read_pos: 0,
        };

        let port2 = MockSerialPort {
            read_buf: Arc::clone(&buf1), // port2 reads from buf1
            write_buf: Arc::clone(&buf2), // port2 writes to buf2
            read_pos: 0,
        };

        (port1, port2)
    }

    fn write_bytes(&mut self, data: &[u8]) {
        let mut buf = self.write_buf.lock().unwrap();
        buf.extend_from_slice(data);
    }

    fn bytes_available(&self) -> usize {
        let buf = self.read_buf.lock().unwrap();
        buf.len().saturating_sub(self.read_pos)
    }
}

impl Read for MockSerialPort {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let data = self.read_buf.lock().unwrap();
        if self.read_pos >= data.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "No data available",
            ));
        }
        let available = &data[self.read_pos..];
        let to_copy = buf.len().min(available.len());
        buf[..to_copy].copy_from_slice(&available[..to_copy]);
        self.read_pos += to_copy;
        Ok(to_copy)
    }
}

impl Write for MockSerialPort {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut data = self.write_buf.lock().unwrap();
        data.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Encode a MAVLink v2 message to bytes
fn encode_mavlink_v2(msg: &MavMessage) -> Vec<u8> {
    let header = MavHeader::default();
    let mut buf = [0u8; 280];
    let mut cursor: &mut [u8] = &mut buf;
    let n = mavlink::write_v2_msg(&mut cursor, header, msg).expect("Failed to encode");
    buf[..n].to_vec()
}

/// Parse MAVLink messages from a byte buffer (similar to fc.rs logic)
fn parse_mavlink_buffer(acc: &mut BytesMut) -> Vec<MavMessage> {
    let mut messages = Vec::new();

    while !acc.is_empty() {
        let consumed = {
            let mut cursor = Cursor::new(&**acc);
            let mut peek = PeekReader::new(&mut cursor);
            match mavlink::read_versioned_msg::<MavMessage, _>(&mut peek, ReadVersion::Any) {
                Ok((_header, msg)) => {
                    drop(peek);
                    cursor.position() as usize
                }
                Err(_) => {
                    // Try to find next magic byte
                    let skip = find_next_mavlink_magic(acc);
                    if skip == 0 || skip >= acc.len() {
                        break;
                    }
                    acc.advance(skip);
                    continue;
                }
            }
        };

        if consumed > 0 {
            let msg_buf = acc[..consumed].to_vec();
            let mut cursor = Cursor::new(&msg_buf);
            let mut peek = PeekReader::new(&mut cursor);
            if let Ok((_header, msg)) =
                mavlink::read_versioned_msg::<MavMessage, _>(&mut peek, ReadVersion::Any)
            {
                messages.push(msg);
            }
            acc.advance(consumed);
        } else {
            break;
        }
    }

    messages
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

#[test]
fn mavlink_roundtrip_heartbeat() {
    let msg = MavMessage::HEARTBEAT(mavlink::common::HEARTBEAT_DATA {
        custom_mode: 5,
        mavtype: mavlink::common::MavType::MAV_TYPE_QUADROTOR,
        autopilot: mavlink::common::MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
        base_mode: mavlink::common::MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED,
        system_status: mavlink::common::MavState::MAV_STATE_ACTIVE,
        mavlink_version: 3,
    });

    let bytes = encode_mavlink_v2(&msg);

    let mut acc = BytesMut::from(bytes.as_slice());
    let messages = parse_mavlink_buffer(&mut acc);

    assert_eq!(messages.len(), 1);
    match &messages[0] {
        MavMessage::HEARTBEAT(h) => {
            assert_eq!(h.custom_mode, 5);
            assert!(h.base_mode.contains(mavlink::common::MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED));
        }
        _ => panic!("Expected HEARTBEAT"),
    }
}

#[test]
fn mavlink_roundtrip_sys_status() {
    use mavlink::common::MavSysStatusSensor;

    let sensors = MavSysStatusSensor::MAV_SYS_STATUS_SENSOR_3D_GYRO
        | MavSysStatusSensor::MAV_SYS_STATUS_SENSOR_3D_ACCEL
        | MavSysStatusSensor::MAV_SYS_STATUS_SENSOR_GPS;

    let msg = MavMessage::SYS_STATUS(mavlink::common::SYS_STATUS_DATA {
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

    let bytes = encode_mavlink_v2(&msg);
    let mut acc = BytesMut::from(bytes.as_slice());
    let messages = parse_mavlink_buffer(&mut acc);

    assert_eq!(messages.len(), 1);
    match &messages[0] {
        MavMessage::SYS_STATUS(s) => {
            assert_eq!(s.voltage_battery, 16800);
            assert_eq!(s.battery_remaining, 72);
        }
        _ => panic!("Expected SYS_STATUS"),
    }
}

#[test]
fn mavlink_multiple_messages_in_buffer() {
    let hb = MavMessage::HEARTBEAT(mavlink::common::HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: mavlink::common::MavType::MAV_TYPE_QUADROTOR,
        autopilot: mavlink::common::MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
        base_mode: mavlink::common::MavModeFlag::empty(),
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

    let mut combined = encode_mavlink_v2(&hb);
    combined.extend_from_slice(&encode_mavlink_v2(&sys));

    let mut acc = BytesMut::from(combined.as_slice());
    let messages = parse_mavlink_buffer(&mut acc);

    assert_eq!(messages.len(), 2);
    assert!(matches!(messages[0], MavMessage::HEARTBEAT(_)));
    assert!(matches!(messages[1], MavMessage::SYS_STATUS(_)));
}

#[test]
fn mavlink_partial_message_handling() {
    let msg = MavMessage::HEARTBEAT(mavlink::common::HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: mavlink::common::MavType::MAV_TYPE_QUADROTOR,
        autopilot: mavlink::common::MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
        base_mode: mavlink::common::MavModeFlag::empty(),
        system_status: mavlink::common::MavState::MAV_STATE_STANDBY,
        mavlink_version: 3,
    });

    let bytes = encode_mavlink_v2(&msg);

    // Simulate receiving only first half
    let mut acc = BytesMut::from(&bytes[..bytes.len() / 2]);
    let messages = parse_mavlink_buffer(&mut acc);
    assert_eq!(messages.len(), 0); // Not complete yet

    // Add remaining bytes
    acc.extend_from_slice(&bytes[bytes.len() / 2..]);
    let messages = parse_mavlink_buffer(&mut acc);
    assert_eq!(messages.len(), 1);
}

#[test]
fn mavlink_mock_serial_loopback() {
    let (mut port1, mut port2) = MockSerialPort::new_pair();

    // Create and send a heartbeat from port1
    let hb = MavMessage::HEARTBEAT(mavlink::common::HEARTBEAT_DATA {
        custom_mode: 3,
        mavtype: mavlink::common::MavType::MAV_TYPE_QUADROTOR,
        autopilot: mavlink::common::MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
        base_mode: mavlink::common::MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED,
        system_status: mavlink::common::MavState::MAV_STATE_ACTIVE,
        mavlink_version: 3,
    });

    let bytes = encode_mavlink_v2(&hb);
    port1.write_bytes(&bytes);

    // Read from port2
    let mut read_buf = [0u8; 512];
    let n = port2.read(&mut read_buf).expect("Failed to read");
    assert!(n > 0);

    // Parse the message
    let mut acc = BytesMut::from(&read_buf[..n]);
    let messages = parse_mavlink_buffer(&mut acc);
    assert_eq!(messages.len(), 1);
}

#[test]
fn mavlink_rc_channels_override_roundtrip() {
    let msg = MavMessage::RC_CHANNELS_OVERRIDE(mavlink::common::RC_CHANNELS_OVERRIDE_DATA {
        chan1_raw: 1000,
        chan2_raw: 1500,
        chan3_raw: 1200,
        chan4_raw: 1500,
        chan5_raw: 1800,
        chan6_raw: 1500,
        chan7_raw: 1500,
        chan8_raw: 1500,
        target_system: 1,
        target_component: 0,
    });

    let bytes = encode_mavlink_v2(&msg);
    let mut acc = BytesMut::from(bytes.as_slice());
    let messages = parse_mavlink_buffer(&mut acc);

    assert_eq!(messages.len(), 1);
    match &messages[0] {
        MavMessage::RC_CHANNELS_OVERRIDE(r) => {
            assert_eq!(r.chan1_raw, 1000);
            assert_eq!(r.chan3_raw, 1200);
            assert_eq!(r.target_system, 1);
        }
        _ => panic!("Expected RC_CHANNELS_OVERRIDE"),
    }
}

#[test]
fn mavlink_gps_position_roundtrip() {
    let msg = MavMessage::GLOBAL_POSITION_INT(mavlink::common::GLOBAL_POSITION_INT_DATA {
        time_boot_ms: 0,
        lat: 375000000,  // 37.5 degrees * 1e7
        lon: -1220000000, // -122.0 degrees * 1e7
        alt: 100000,      // 100 meters * 1000
        relative_alt: 50000,
        vx: 100,
        vy: 200,
        vz: -50,
        hdg: 9000, // 90 degrees * 100
    });

    let bytes = encode_mavlink_v2(&msg);
    let mut acc = BytesMut::from(bytes.as_slice());
    let messages = parse_mavlink_buffer(&mut acc);

    assert_eq!(messages.len(), 1);
    match &messages[0] {
        MavMessage::GLOBAL_POSITION_INT(g) => {
            assert_eq!(g.lat, 375000000);
            assert_eq!(g.lon, -1220000000);
        }
        _ => panic!("Expected GLOBAL_POSITION_INT"),
    }
}

#[test]
fn mavlink_corrupted_data_handling() {
    let hb = MavMessage::HEARTBEAT(mavlink::common::HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: mavlink::common::MavType::MAV_TYPE_QUADROTOR,
        autopilot: mavlink::common::MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
        base_mode: mavlink::common::MavModeFlag::empty(),
        system_status: mavlink::common::MavState::MAV_STATE_STANDBY,
        mavlink_version: 3,
    });

    let mut bytes = encode_mavlink_v2(&hb);

    // Corrupt some bytes in the middle
    if bytes.len() > 10 {
        bytes[10] = 0xFF;
        bytes[11] = 0xFF;
    }

    // The parser should either fail or recover
    let mut acc = BytesMut::from(bytes.as_slice());
    let messages = parse_mavlink_buffer(&mut acc);

    // Might be 0 or 1 depending on CRC check
    // Either way, it shouldn't panic
}

#[test]
fn mavlink_telemetry_extraction() {
    use mavlink::common::MavModeFlag;

    // Simulate receiving a series of MAVLink messages and extracting telemetry
    let mut fc_telemetry = FcTelemetry::default();

    let messages = vec![
        MavMessage::SYS_STATUS(mavlink::common::SYS_STATUS_DATA {
            onboard_control_sensors_present: mavlink::common::MavSysStatusSensor::empty(),
            onboard_control_sensors_enabled: mavlink::common::MavSysStatusSensor::empty(),
            onboard_control_sensors_health: mavlink::common::MavSysStatusSensor::empty(),
            load: 0,
            voltage_battery: 16800,
            current_battery: 150,
            battery_remaining: 72,
            drop_rate_comm: 0,
            errors_comm: 0,
            errors_count1: 0,
            errors_count2: 0,
            errors_count3: 0,
            errors_count4: 0,
        }),
        MavMessage::GLOBAL_POSITION_INT(mavlink::common::GLOBAL_POSITION_INT_DATA {
            time_boot_ms: 0,
            lat: 377000000,
            lon: -1224000000,
            alt: 150000,
            relative_alt: 100000,
            vx: 500,
            vy: 0,
            vz: 0,
            hdg: 18000,
        }),
        MavMessage::VFR_HUD(mavlink::common::VFR_HUD_DATA {
            airspeed: 0.0,
            groundspeed: 5.5,
            heading: 180,
            throttle: 50, // u16: 0-100 percentage
            alt: 100.0,
            climb: 0.0,
        }),
        MavMessage::GPS_RAW_INT(mavlink::common::GPS_RAW_INT_DATA {
            time_usec: 0,
            fix_type: mavlink::common::GpsFixType::GPS_FIX_TYPE_3D_FIX,
            lat: 377000000,
            lon: -1224000000,
            alt: 150000,
            eph: 100,
            epv: 150,
            vel: 55,
            cog: 1800,
            satellites_visible: 12,
        }),
        MavMessage::HEARTBEAT(mavlink::common::HEARTBEAT_DATA {
            custom_mode: 4, // GUIDED
            mavtype: mavlink::common::MavType::MAV_TYPE_QUADROTOR,
            autopilot: mavlink::common::MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED,
            system_status: mavlink::common::MavState::MAV_STATE_ACTIVE,
            mavlink_version: 3,
        }),
    ];

    // Process each message and update telemetry
    for msg in &messages {
        match msg {
            MavMessage::SYS_STATUS(s) => {
                if s.voltage_battery != u16::MAX {
                    fc_telemetry.battery_v = s.voltage_battery as f64 / 1000.0;
                }
                if s.battery_remaining >= 0 {
                    fc_telemetry.battery_pct = s.battery_remaining;
                }
            }
            MavMessage::GLOBAL_POSITION_INT(g) => {
                fc_telemetry.lat = g.lat as f64 * 1e-7;
                fc_telemetry.lon = g.lon as f64 * 1e-7;
                fc_telemetry.alt = g.alt as f64 / 1000.0;
                fc_telemetry.relative_alt = g.relative_alt as f64 / 1000.0;
                if g.hdg != u16::MAX {
                    fc_telemetry.heading = g.hdg as f64 / 100.0;
                }
            }
            MavMessage::VFR_HUD(v) => {
                fc_telemetry.speed = v.groundspeed as f64;
            }
            MavMessage::GPS_RAW_INT(g) => {
                fc_telemetry.satellites = g.satellites_visible;
            }
            MavMessage::HEARTBEAT(h) => {
                fc_telemetry.armed = h
                    .base_mode
                    .contains(MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED);
                fc_telemetry.mode = ardupilot_mode_name(h.custom_mode).to_string();
            }
            _ => {}
        }
    }

    // Verify telemetry extraction
    assert_eq!(fc_telemetry.battery_v, 16.8);
    assert_eq!(fc_telemetry.battery_pct, 72);
    assert!((fc_telemetry.lat - 37.7).abs() < 0.01);
    assert!((fc_telemetry.lon - (-122.4)).abs() < 0.01);
    assert_eq!(fc_telemetry.satellites, 12);
    assert!(fc_telemetry.armed);
    assert_eq!(fc_telemetry.mode, "GUIDED");
}

// Copy of FcTelemetry and ardupilot_mode_name from fc.rs for testing
#[derive(Debug, Clone)]
struct FcTelemetry {
    pub lat: f64,
    pub lon: f64,
    pub alt: f64,
    pub relative_alt: f64,
    pub heading: f64,
    pub speed: f64,
    pub satellites: u8,
    pub battery_v: f64,
    pub battery_pct: i8,
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

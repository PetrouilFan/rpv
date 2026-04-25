use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use rpv_proto::link;
use rpv_proto::socket_trait::SocketTrait;
use tracing::{info, warn};

const RC_CHANNELS: usize = 16;
const RC_CENTER: u16 = 1500;
const RC_MIN: u16 = 1000;
const RC_MAX: u16 = 2000;
const RC_FREQUENCY_HZ: u64 = 50;
const RC_INTERVAL_US: u64 = 1_000_000 / RC_FREQUENCY_HZ;

#[cfg(feature = "gamepad")]
const DEVICE_NAMES: [&str; 8] = [
    "/dev/input/event0",
    "/dev/input/event1",
    "/dev/input/event2",
    "/dev/input/event3",
    "/dev/input/js0",
    "/dev/input/js1",
    "/dev/evdev",
    "/dev/input/by-id/usb-*_event-joystick",
];

#[cfg(feature = "gamepad")]
use evdev::{Device, EventType, KeyCode};

pub struct RCTx {
    socket: Arc<dyn SocketTrait>,
    drone_id: u8,
    running: Arc<AtomicBool>,
    channels: Arc<ArcSwap<[u16; RC_CHANNELS]>>,
    #[cfg(feature = "gamepad")]
    device: Option<Device>,
    #[cfg(not(feature = "gamepad"))]
    device: Option<()>,
    last_send: Instant,
    seq: u32,
}

impl RCTx {
    pub fn new(
        socket: Arc<dyn SocketTrait>,
        drone_id: u8,
        running: Arc<AtomicBool>,
    ) -> Self {
        let channels = Arc::new(ArcSwap::new(Arc::new([RC_CENTER; RC_CHANNELS])));
        
        #[cfg(feature = "gamepad")]
        let device = Self::find_gamepad();
        #[cfg(not(feature = "gamepad"))]
        let device = None;

        #[cfg(feature = "gamepad")]
        if device.is_some() {
            info!("Gamepad detected, using for RC input");
        } else {
            warn!("No gamepad detected, using safe defaults");
        }

        Self {
            socket,
            drone_id,
            running,
            channels,
            device,
            last_send: Instant::now(),
            seq: 0,
        }
    }

    #[cfg(feature = "gamepad")]
    fn find_gamepad() -> Option<Device> {
        for name in DEVICE_NAMES.iter() {
            if std::path::Path::new(name).exists() {
                if let Ok(dev) = Device::open(name) {
                    info!("Opened gamepad device: {}", name);
                    return Some(dev);
                }
            }
        }
        // Try to find any event device
        if let Ok(entries) = std::fs::read_dir("/dev/input") {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("event") {
                        if let Ok(dev) = Device::open(&path) {
                            info!("Opened event device: {:?}", path);
                            return Some(dev);
                        }
                    }
                }
            }
        }
        None
    }

    #[cfg(not(feature = "gamepad"))]
    #[allow(dead_code)]
    fn find_gamepad() -> Option<()> {
        warn!("Gamepad support not compiled in (enable 'gamepad' feature)");
        None
    }

    pub fn channels(&self) -> Arc<ArcSwap<[u16; RC_CHANNELS]>> {
        self.channels.clone()
    }

    pub fn run(mut self) {
        info!("RC transmitter ready (L2 broadcast, {}Hz, deadline-based)", RC_FREQUENCY_HZ);

        while self.running.load(Ordering::SeqCst) {
            let now = Instant::now();
            let elapsed = now.duration_since(self.last_send);

            if elapsed >= Duration::from_micros(RC_INTERVAL_US) {
                self.send_rc_packet();
                self.last_send = now;
            } else {
                std::thread::sleep(Duration::from_micros(100));
            }

            #[cfg(feature = "gamepad")]
            self.poll_gamepad();
        }
    }

    #[cfg(feature = "gamepad")]
    fn poll_gamepad(&mut self) {
        if let Some(ref mut dev) = self.device {
            let events: Vec<_> = match dev.fetch_events() {
                Ok(events) => events.collect(),
                Err(e) => {
                    debug!("Gamepad read error: {}", e);
                    return;
                }
            };
            
            for event in events {
                if event.event_type() == EventType::KEY {
                    let code = event.code();
                    let value = event.value();
                    self.handle_key_event(code, value);
                }
            }
        }
    }

    #[cfg(feature = "gamepad")]
    fn handle_key_event(&mut self, code: u16, value: i32) {
        // Map common gamepad buttons to RC channels
        let arr = self.channels.load();
        let mut arr_copy = (**arr).clone();
        let mut updated = false;

        match code {
            // D-pad up/down -> throttle
            544 if value == 1 => { arr_copy[2] = 2000; updated = true; }
            544 if value == 0 => { arr_copy[2] = 1000; updated = true; }
            545 if value == 1 => { arr_copy[2] = 1000; updated = true; }
            545 if value == 0 => { arr_copy[2] = 1500; updated = true; }
            // A button -> arm/disarm
            304 if value == 1 => { arr_copy[4] = 2000; updated = true; }
            304 if value == 0 => { arr_copy[4] = 1000; updated = true; }
            // B button -> mode switch
            305 if value == 1 => { arr_copy[5] = 2000; updated = true; }
            305 if value == 0 => { arr_copy[5] = 1000; updated = true; }
            _ => {}
        }

        if updated {
            self.channels.store(Arc::new(arr_copy));
        }
    }

    fn send_rc_packet(&mut self) {
        let channels = self.channels.load();
        
        let mut payload = Vec::with_capacity(4 + RC_CHANNELS * 2);
        payload.extend_from_slice(&(RC_CHANNELS as u32).to_le_bytes());
        for &ch in channels.iter() {
            payload.extend_from_slice(&ch.to_le_bytes());
        }

        let mut l2_buf = Vec::with_capacity(link::HEADER_LEN + payload.len());
        let header = link::L2Header {
            drone_id: self.drone_id,
            payload_type: link::PAYLOAD_RC,
            seq: self.seq,
        };
        header.encode_into(&payload, &mut l2_buf);

        let mut send_buf = Vec::with_capacity(8 + 24 + link::HEADER_LEN + payload.len());
        if let Err(e) = self.socket.send_with_buf(&l2_buf, &mut send_buf) {
            warn!("RC send error: {}", e);
        }

        self.seq = self.seq.wrapping_add(1);
    }
}

#[cfg(feature = "gamepad")]
struct GamepadInput {
    device: Device,
}

#[cfg(feature = "gamepad")]
impl GamepadInput {
    fn auto_detect() -> Option<Self> {
        let gamepad_path = match Self::find_gamepad_path() {
            Some(p) => p,
            None => {
                tracing::error!("No gamepad found in /dev/input");
                return None;
            }
        };

        info!("Gamepad found at {}", gamepad_path.display());

        let mut device = match Device::open(&gamepad_path) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("FATAL: Failed to open gamepad {}: {}. Run as root or add user to 'input' group.", gamepad_path.display(), e);
                return None;
            }
        };

        match device.grab() {
            Ok(()) => {}
            Err(e) => {
                tracing::error!(
                    "FATAL: Failed to grab gamepad: {}. Another process may be using it.",
                    e
                );
                return None;
            }
        };

        info!("Gamepad initialized successfully");
        Some(Self { device })
    }

    fn find_gamepad_path() -> Option<std::path::PathBuf> {
        let dev_path = std::path::PathBuf::from("/dev/input");
        if !dev_path.exists() {
            tracing::error!("/dev/input doesn't exist");
            return None;
        }

        let entries = match std::fs::read_dir(dev_path) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("Failed to read /dev/input: {}", e);
                return None;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name() {
                let name_str = name.to_string_lossy();
                if name_str.starts_with("event") || name_str.starts_with("js") {
                    if let Ok(device) = Device::open(&path) {
                        let has_abs = device.supported_absolute_axes().is_some();
                        let has_keys = device.supported_keys().is_some();
                        if has_abs && has_keys {
                            info!(
                                "Found gamepad: {} ({})",
                                path.display(),
                                device.name().unwrap_or_default()
                            );
                            return Some(path);
                        }
                    }
                }
            }
        }
        tracing::error!("No gamepad device found in /dev/input");
        None
    }

    fn get_axis_value(device: &Device, code: u16) -> Option<i32> {
        let state = device.cached_state();
        let abs_vals = state.abs_vals()?;
        if code as usize >= abs_vals.len() {
            return None;
        }
        Some(abs_vals[code as usize].value)
    }

    fn read_input(&mut self, channels: &mut [u16; 16]) {
        match self.device.fetch_events() {
            Ok(_events) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                tracing::error!("Gamepad read error (disconnected?): {}", e);
                channels.fill(RC_MID);
                channels[2] = RC_MIN;
                return;
            }
        }

        let axis_yaw = Self::get_axis_value(&self.device, 0x00);
        let axis_thr = Self::get_axis_value(&self.device, 0x01);
        let axis_rol = Self::get_axis_value(&self.device, 0x03);
        let axis_pit = Self::get_axis_value(&self.device, 0x04);

        channels[0] = Self::axis_to_rc(axis_rol, false, false);
        channels[1] = Self::axis_to_rc(axis_pit, true, false);
        channels[2] = Self::axis_to_rc(axis_thr, true, true);
        channels[3] = Self::axis_to_rc(axis_yaw, false, false);

        let keys = match self.device.cached_state().key_vals() {
            Some(k) => k,
            None => return,
        };

        channels[4] = Self::button_to_rc(keys.contains(KeyCode(0x120)));
        channels[5] = Self::button_to_rc(keys.contains(KeyCode(0x121)));
        channels[6] = Self::button_to_rc(keys.contains(KeyCode(0x122)));
        channels[7] = Self::button_to_rc(keys.contains(KeyCode(0x123)));
        channels[8] = Self::button_to_rc(keys.contains(KeyCode(0x124)));
        channels[9] = Self::button_to_rc(keys.contains(KeyCode(0x125)));
        channels[10] = Self::button_to_rc(keys.contains(KeyCode(0x126)));
        channels[11] = Self::button_to_rc(keys.contains(KeyCode(0x127)));
        channels[12] = Self::button_to_rc(keys.contains(KeyCode(0x128)));
        channels[13] = Self::button_to_rc(keys.contains(KeyCode(0x129)));
        channels[14] = Self::button_to_rc(keys.contains(KeyCode(0x12a)));
        channels[15] = Self::button_to_rc(keys.contains(KeyCode(0x12b)));
    }

    fn axis_to_rc(axis: Option<i32>, invert: bool, throttle_mode: bool) -> u16 {
        let value = match axis {
            Some(v) => v,
            None => return RC_MID,
        };

        let value = if invert { -value } else { value };

        if throttle_mode {
            let normalized = ((value + 32767) as f64 / 65534.0).clamp(0.0, 1.0);
            (RC_MIN as f64 + normalized * (RC_MAX as f64 - RC_MIN as f64)) as u16
        } else {
            let abs_val = value.abs();
            let effective_range = 32767 - DEADZONE;
            let with_deadzone = if abs_val <= DEADZONE {
                0.0
            } else {
                ((abs_val - DEADZONE) as f64 / effective_range as f64).clamp(0.0, 1.0)
                    * value.signum() as f64
            };
            (RC_MID as f64 + with_deadzone * (RC_MID as f64 - RC_MIN as f64)) as u16
        }
    }

    fn button_to_rc(pressed: bool) -> u16 {
        if pressed {
            RC_MAX
        } else {
            RC_MIN
        }
    }
}

const DEADZONE: i32 = 1000;
const RC_MID: u16 = 1500;
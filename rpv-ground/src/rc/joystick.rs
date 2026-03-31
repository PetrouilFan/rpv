use evdev::{Device, KeyCode};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

use crate::link;
use crate::rawsock::RawSocket;

const RC_MIN: u16 = 1000;
const RC_MID: u16 = 1500;
const RC_MAX: u16 = 2000;
const DEADZONE: i32 = 4096;
const RC_INTERVAL: Duration = Duration::from_millis(50);

struct GamepadInput {
    device: Device,
}

impl GamepadInput {
    fn auto_detect() -> Option<Self> {
        let gamepad_path = match Self::find_gamepad_path() {
            Some(p) => p,
            None => {
                error!("No gamepad found in /dev/input");
                return None;
            }
        };

        info!("Gamepad found at {}", gamepad_path.display());

        let mut device = match Device::open(&gamepad_path) {
            Ok(d) => d,
            Err(e) => {
                // #1: Hard error — pilot has no control visibility
                error!("FATAL: Failed to open gamepad {}: {}. Run as root or add user to 'input' group.", gamepad_path.display(), e);
                return None;
            }
        };

        match device.grab() {
            Ok(()) => {}
            Err(e) => {
                // #1: Hard error — falling back to "safe defaults" hides the problem
                error!(
                    "FATAL: Failed to grab gamepad: {}. Another process may be using it.",
                    e
                );
                return None;
            }
        };

        info!("Gamepad initialized successfully");
        Some(Self { device })
    }

    // #5: Check both event* and js* devices
    fn find_gamepad_path() -> Option<PathBuf> {
        let dev_path = PathBuf::from("/dev/input");
        if !dev_path.exists() {
            error!("/dev/input doesn't exist");
            return None;
        }

        let entries = match std::fs::read_dir(dev_path) {
            Ok(e) => e,
            Err(e) => {
                error!("Failed to read /dev/input: {}", e);
                return None;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name() {
                let name_str = name.to_string_lossy();
                // #5: Check both evdev (event*) and joydev (js*) devices
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
        error!("No gamepad device found in /dev/input");
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
        // #7: Check for dropped events (EV_SYN/SYN_DROPPED)
        match self.device.fetch_events() {
            Ok(_events) => {
                // Events consumed — cached_state is now up to date
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No new events — cached_state is still valid
            }
            Err(e) => {
                // #21: Device disconnected or error — return safe defaults
                error!("Gamepad read error (disconnected?): {}", e);
                channels.fill(RC_MID);
                channels[2] = RC_MIN; // Throttle zero
                return;
            }
        }

        let axis_x = Self::get_axis_value(&self.device, 0x00);
        let axis_y = Self::get_axis_value(&self.device, 0x01);
        let throttle = Self::get_axis_value(&self.device, 0x02);
        let axis_rz = Self::get_axis_value(&self.device, 0x03);

        channels[0] = Self::axis_to_rc(axis_x, false, false);
        channels[1] = Self::axis_to_rc(axis_y, true, false);
        channels[2] = Self::axis_to_rc(throttle, false, true);
        channels[3] = Self::axis_to_rc(axis_rz, false, false);

        let keys = match self.device.cached_state().key_vals() {
            Some(k) => k,
            None => return,
        };

        // Button codes: BTN_A(0x130) through BTN_THUMBR(0x139) + extras
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

    // #6: Continuous deadzone mapping — no jump at threshold
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
            // #6: Linear remap with continuous deadzone
            // Map [-32767, 32767] → [-1.0, 1.0] with deadzone suppression
            let abs_val = value.abs();
            let effective_range = 32767 - DEADZONE;
            let with_deadzone = if abs_val <= DEADZONE {
                0.0
            } else {
                // Continuous: no jump, smooth transition from deadzone edge
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

pub struct RCTx {
    socket: Arc<RawSocket>,
    drone_id: u8,
    // #16: Use [u16; 16] instead of Vec<u16> — fixed size, no heap allocation
    channels: Arc<std::sync::Mutex<[u16; 16]>>,
    gamepad: Option<GamepadInput>,
    l2_seq: u32,
    running: Arc<AtomicBool>,
}

impl RCTx {
    pub fn new(socket: Arc<RawSocket>, drone_id: u8, running: Arc<AtomicBool>) -> Self {
        let gamepad = GamepadInput::auto_detect();

        if gamepad.is_some() {
            info!("Gamepad input enabled");
        } else {
            // #1: Print visible warning so pilot knows there's no controller
            warn!("NO GAMEPAD DETECTED — RC channels stuck at safe defaults (throttle zero). Drone will not arm.");
            info!("No gamepad detected, using safe defaults");
        }

        Self {
            socket,
            drone_id,
            // #2: Throttle default to RC_MIN (1000) to keep throttle at zero
            // This is correct for "no gamepad" — throttle must be zero.
            // The failsafe issue (#2) is in fc.rs, not here — the FC writer
            // should check if any real RC data has EVER been received.
            channels: Arc::new(std::sync::Mutex::new({
                let mut ch = [RC_MID; 16];
                ch[2] = RC_MIN; // Throttle at zero when no gamepad
                ch
            })),
            gamepad,
            l2_seq: 0,
            running,
        }
    }

    pub fn channels(&self) -> Arc<std::sync::Mutex<[u16; 16]>> {
        Arc::clone(&self.channels)
    }

    pub fn run(&mut self) {
        info!("RC transmitter ready (L2 broadcast, 20Hz, deadline-based)");

        let mut l2_buf: Vec<u8> = Vec::with_capacity(link::MAX_PAYLOAD);
        let mut send_buf: Vec<u8> = Vec::with_capacity(8 + 24 + link::MAX_PAYLOAD);
        // #16: Pre-allocated payload buffer (reused each cycle)
        let mut payload_buf: Vec<u8> = Vec::with_capacity(4 + 16 * 2);
        let mut next_send = Instant::now();
        let mut max_jitter_us: u64 = 0;
        let mut jitter_samples: u64 = 0;

        while self.running.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now < next_send {
                // #19: Loop sleep to handle EINTR
                let mut remaining = next_send - now;
                while remaining > Duration::ZERO {
                    let before = Instant::now();
                    std::thread::sleep(remaining);
                    let elapsed = before.elapsed();
                    if elapsed >= remaining {
                        break;
                    }
                    remaining -= elapsed;
                }
            }

            let actual = Instant::now();
            let slip = actual.duration_since(next_send);
            if slip.as_micros() > 0 {
                let slip_us = slip.as_micros() as u64;
                if slip_us > max_jitter_us {
                    max_jitter_us = slip_us;
                }
                jitter_samples += 1;
                if jitter_samples % 3000 == 0 {
                    tracing::debug!(
                        "RC: max scheduling jitter {} us over {} samples",
                        max_jitter_us,
                        jitter_samples
                    );
                    max_jitter_us = 0;
                }
            }

            next_send = actual + RC_INTERVAL;

            if let Some(ref mut gp) = self.gamepad {
                let mut channel_buf = [0u16; 16];
                gp.read_input(&mut channel_buf);
                // #9: Single lock acquisition — merge gamepad update + read
                let mut channels = self.channels.lock().unwrap();
                *channels = channel_buf;
            }

            // #9, #16: Single lock, copy array by value (no heap alloc)
            let channels = *self.channels.lock().unwrap();

            // #16: Reuse payload buffer — no per-cycle allocation
            payload_buf.clear();
            let count = channels.len() as u32;
            payload_buf.extend_from_slice(&count.to_le_bytes());
            for &ch in &channels {
                payload_buf.extend_from_slice(&ch.to_le_bytes());
            }

            let header = link::L2Header {
                drone_id: self.drone_id,
                payload_type: link::PAYLOAD_RC,
                seq: self.l2_seq,
            };
            header.encode_into(&payload_buf, &mut l2_buf);
            let _ = self.socket.send_with_buf(&l2_buf, &mut send_buf);
            self.l2_seq = self.l2_seq.wrapping_add(1);
        }
    }
}

use evdev::{Device, EventSummary, EventType, AbsCode, KeyCode};
use std::collections::HashMap;
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
const RC_INTERVAL: Duration = Duration::from_millis(20);

struct GamepadInput {
    device: Device,
    axes: Arc<std::sync::Mutex<HashMap<AbsCode, i32>>>,
    buttons: Arc<std::sync::Mutex<HashMap<KeyCode, bool>>>,
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
        
        let device = match Device::open(&gamepad_path) {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to open gamepad: {}", e);
                return None;
            }
        };

        let device = match device.grab() {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to grab gamepad: {}", e);
                return None;
            }
        };

        info!("Gamepad initialized successfully");
        Some(Self {
            device,
            axes: Arc::new(std::sync::Mutex::new(HashMap::new())),
            buttons: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    fn find_gamepad_path() -> Option<PathBuf> {
        let dev_path = PathBuf::from("/dev/input");
        if !dev_path.exists() {
            return None;
        }

        let entries = match std::fs::read_dir(dev_path) {
            Ok(e) => e,
            Err(_) => return None,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name() {
                let name_str = name.to_string_lossy();
                if name_str.starts_with("event") {
                    if let Ok(device) = Device::open(&path) {
                        if device.supported_events().contains(EventType::ABS) 
                            && device.supported_events().contains(EventType::KEY) {
                            return Some(path);
                        }
                    }
                }
            }
        }
        None
    }

    fn read_input(&self, channels: &mut [u16; 16]) {
        let mut axis_map = self.axes.lock().unwrap();
        let mut button_map = self.buttons.lock().unwrap();
        
        let events_result = self.device.fetch_events();
        let Ok(events) = events_result else {
            return;
        };

        for event in events.flatten() {
            match event.destructure() {
                EventSummary::Abs(_, code, value) => {
                    axis_map.insert(code, value);
                }
                EventSummary::Key(_, code, value) => {
                    button_map.insert(code, value != 0);
                }
                _ => {}
            }
        }

        drop(axis_map);
        drop(button_map);

        let axis_map = self.axes.lock().unwrap();
        let button_map = self.buttons.lock().unwrap();

        channels[0] = Self::axis_to_rc(axis_map.get(&AbsCode(0x00)), false, false);      
        channels[1] = Self::axis_to_rc(axis_map.get(&AbsCode(0x01)), true, false);      
        channels[2] = Self::axis_to_rc(axis_map.get(&AbsCode(0x02)), false, true);      
        channels[3] = Self::axis_to_rc(axis_map.get(&AbsCode(0x03)), false, false);     
        
        channels[4] = Self::button_to_rc(button_map.get(&KeyCode(0x120)));
        channels[5] = Self::button_to_rc(button_map.get(&KeyCode(0x121)));             
        channels[6] = Self::button_to_rc(button_map.get(&KeyCode(0x122)));              
        channels[7] = Self::button_to_rc(button_map.get(&KeyCode(0x123)));              
        channels[8] = Self::button_to_rc(button_map.get(&KeyCode(0x124)));             
        channels[9] = Self::button_to_rc(button_map.get(&KeyCode(0x125)));             
        channels[10] = Self::button_to_rc(button_map.get(&KeyCode(0x126)));            
        channels[11] = Self::button_to_rc(button_map.get(&KeyCode(0x127)));            
        channels[12] = Self::button_to_rc(button_map.get(&KeyCode(0x128)));            
        channels[13] = Self::button_to_rc(button_map.get(&KeyCode(0x129)));            
        channels[14] = Self::button_to_rc(button_map.get(&KeyCode(0x12a)));            
        channels[15] = Self::button_to_rc(button_map.get(&KeyCode(0x12b)));            
    }

    fn axis_to_rc(axis: Option<&i32>, invert: bool, throttle_mode: bool) -> u16 {
        let &value = match axis {
            Some(v) => v,
            None => return RC_MID,
        };

        let value = if invert { -value } else { value };

        if throttle_mode {
            let normalized = ((value + 32767) as f64 / 65534.0).clamp(0.0, 1.0);
            (RC_MIN as f64 + normalized * (RC_MAX as f64 - RC_MIN as f64)) as u16
        } else {
            let with_deadzone = if value.abs() < DEADZONE {
                0
            } else {
                value - value.signum() * DEADZONE
            };
            let normalized = (with_deadzone as f64 / (32767 - DEADZONE) as f64).clamp(-1.0, 1.0);
            (RC_MID as f64 + normalized * (RC_MID as f64 - RC_MIN as f64)) as u16
        }
    }

    fn button_to_rc(button: Option<&bool>) -> u16 {
        match button {
            Some(true) => RC_MAX,
            _ => RC_MIN,
        }
    }
}

pub struct RCTx {
    socket: Arc<RawSocket>,
    drone_id: u8,
    channels: std::sync::Mutex<Vec<u16>>,
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
            info!("No gamepad detected, using safe defaults");
        }

        Self {
            socket,
            drone_id,
            channels: std::sync::Mutex::new({
                let mut ch = vec![1500u16; 16];
                ch[2] = 1000;
                ch
            }),
            gamepad,
            l2_seq: 0,
            running,
        }
    }

    pub fn run(&mut self) {
        info!("RC transmitter ready (L2 broadcast, 50Hz, deadline-based)");

        let mut l2_buf: Vec<u8> = Vec::with_capacity(link::MAX_PAYLOAD);
        let mut send_buf: Vec<u8> = Vec::with_capacity(8 + 24 + link::MAX_PAYLOAD);
        let mut next_send = Instant::now();
        let mut max_jitter_us: u64 = 0;
        let mut jitter_samples: u64 = 0;

        while self.running.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now < next_send {
                std::thread::sleep(next_send - now);
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

            if let Some(ref gp) = self.gamepad {
                let mut channel_buf = [0u16; 16];
                gp.read_input(&mut channel_buf);
                
                let mut channels = self.channels.lock().unwrap();
                for (i, ch) in channel_buf.iter().enumerate() {
                    if i < channels.len() {
                        channels[i] = *ch;
                    }
                }
            }

            let channels = {
                let locked = self.channels.lock().unwrap();
                locked.clone()
            };

            let count = channels.len() as u32;
            let mut payload = Vec::with_capacity(4 + channels.len() * 2);
            payload.extend_from_slice(&count.to_le_bytes());
            for &ch in channels.iter() {
                payload.extend_from_slice(&ch.to_le_bytes());
            }

            let header = link::L2Header {
                drone_id: self.drone_id,
                payload_type: link::PAYLOAD_RC,
                seq: self.l2_seq,
            };
            header.encode_into(&payload, &mut l2_buf);
            let _ = self.socket.send_with_buf(&l2_buf, &mut send_buf);
            self.l2_seq = self.l2_seq.wrapping_add(1);
        }
    }
}
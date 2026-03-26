use evdev::{Device, EventSummary, EventType, AbsCode, KeyCode, AbsInfo};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};

const RC_MIN: u16 = 1000;
const RC_MID: u16 = 1500;
const RC_MAX: u16 = 2000;
const DEADZONE: i32 = 4096;

pub struct GamepadInput {
    device: Device,
    axes: Arc<std::sync::Mutex<HashMap<AbsCode, i32>>>,
    buttons: Arc<std::sync::Mutex<HashMap<KeyCode, bool>>>,
}

impl GamepadInput {
    pub fn auto_detect() -> Option<Self> {
        let gamepad_path = match Self::find_gamepad_path() {
            Some(p) => p,
            None => {
                error!("No gamepad found in /dev/input");
                return None;
            }
        };
        
        info!("Gamepad detected at {}", gamepad_path.display());
        
        let device = match Device::open(&gamepad_path) {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to open gamepad at {}: {}", gamepad_path.display(), e);
                return None;
            }
        };

        if let Ok(name) = device.name() {
            info!("Gamepad name: {}", name);
        }

        let device = match device.grab() {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to grab gamepad: {}", e);
                return None;
            }
        };

        let axes = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let buttons = Arc::new(std::sync::Mutex::new(HashMap::new()));

        info!("Gamepad initialized successfully");
        Some(Self {
            device,
            axes,
            buttons,
        })
    }

    fn find_gamepad_path() -> Option<PathBuf> {
        let dev_path = std::path::PathBuf::from("/dev/input");
        
        if !dev_path.exists() {
            warn!("/dev/input doesn't exist");
            return None;
        }

        let entries = match std::fs::read_dir(dev_path) {
            Ok(e) => e,
            Err(e) => {
                warn!("Failed to read /dev/input: {}", e);
                return None;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name() {
                let name_str = name.to_string_lossy();
                if name_str.starts_with("event") {
                    match Device::open(&path) {
                        Ok(device) => {
                            let has_abs = device.supported_events().contains(EventType::ABS);
                            let has_key = device.supported_events().contains(EventType::KEY);
                            if has_abs && has_key {
                                info!("Found gamepad device: {}", path.display());
                                return Some(path);
                            }
                        }
                        Err(_) => continue,
                    }
                }
            }
        }

        warn!("No gamepad device found");
        None
    }

    pub fn read_input(&self, channels: &mut [u16; 16]) {
        let mut axis_map = self.axes.lock().unwrap();
        let mut button_map = self.buttons.lock().unwrap();
        
        let events_result = self.device.fetch_events();
        let Ok(events) = events_result else {
            warn!("Error fetching gamepad events");
            return;
        };

        for event in events.flatten() {
            match event.destructure() {
                EventSummary::Abs(ev, code, value) => {
                    axis_map.insert(code, value);
                }
                EventSummary::Key(ev, code, value) => {
                    button_map.insert(code, value != 0);
                }
                _ => {}
            }
        }

        drop(axis_map);
        drop(button_map);

        let axis_map = self.axes.lock().unwrap();
        let button_map = self.buttons.lock().unwrap();

        Self::map_to_rc_channels(&axis_map, &button_map, channels);
    }

    fn map_to_rc_channels(
        axes: &HashMap<AbsCode, i32>,
        buttons: &HashMap<KeyCode, bool>,
        channels: &mut [u16; 16],
    ) {
        channels[0] = Self::axis_to_rc(axes.get(&AbsCode(0x00)), false, false);      
        channels[1] = Self::axis_to_rc(axes.get(&AbsCode(0x01)), true, false);      
        channels[2] = Self::axis_to_rc(axes.get(&AbsCode(0x02)), false, true);      
        channels[3] = Self::axis_to_rc(axes.get(&AbsCode(0x03)), false, false);     
        
        channels[4] = Self::button_to_rc(buttons.get(&KeyCode(0x120)));
        channels[5] = Self::button_to_rc(buttons.get(&KeyCode(0x121)));             
        channels[6] = Self::button_to_rc(buttons.get(&KeyCode(0x122)));             
        channels[7] = Self::button_to_rc(buttons.get(&KeyCode(0x123)));             
        
        channels[8] = Self::button_to_rc(buttons.get(&KeyCode(0x124)));             
        channels[9] = Self::button_to_rc(buttons.get(&KeyCode(0x125)));             
        channels[10] = Self::button_to_rc(buttons.get(&KeyCode(0x126)));            
        channels[11] = Self::button_to_rc(buttons.get(&KeyCode(0x127)));            
        
        channels[12] = Self::button_to_rc(buttons.get(&KeyCode(0x128)));            
        channels[13] = Self::button_to_rc(buttons.get(&KeyCode(0x129)));            
        channels[14] = Self::button_to_rc(buttons.get(&KeyCode(0x12a)));            
        channels[15] = Self::button_to_rc(buttons.get(&KeyCode(0x12b)));            
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
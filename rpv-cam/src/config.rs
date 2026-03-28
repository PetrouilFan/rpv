use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_interface")]
    pub interface: String,
    #[serde(default = "default_drone_id")]
    pub drone_id: u8,
    #[serde(default = "default_video_device")]
    pub video_device: String,
    #[serde(default = "default_fc_port")]
    pub fc_port: String,
    #[serde(default = "default_fc_baud")]
    pub fc_baud: u32,
    #[serde(default = "default_video_width")]
    pub video_width: u32,
    #[serde(default = "default_video_height")]
    pub video_height: u32,
    #[serde(default = "default_framerate")]
    pub framerate: u32,
    #[serde(default = "default_bitrate")]
    pub bitrate: u32,
}

fn default_interface() -> String {
    "wlan1".to_string()
}

fn default_drone_id() -> u8 {
    0
}

fn default_video_device() -> String {
    "/dev/video0".to_string()
}

fn default_fc_port() -> String {
    "/dev/ttyAMA0".to_string()
}

fn default_fc_baud() -> u32 {
    115200
}

fn default_video_width() -> u32 {
    960
}

fn default_video_height() -> u32 {
    540
}

fn default_framerate() -> u32 {
    30
}

fn default_bitrate() -> u32 {
    3_000_000
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interface: default_interface(),
            drone_id: default_drone_id(),
            video_device: default_video_device(),
            fc_port: default_fc_port(),
            fc_baud: default_fc_baud(),
            video_width: default_video_width(),
            video_height: default_video_height(),
            framerate: default_framerate(),
            bitrate: default_bitrate(),
        }
    }
}

impl Config {
    pub fn load() -> (Self, bool) {
        let config_path = config_path();
        if let Ok(data) = std::fs::read_to_string(&config_path) {
            (toml::from_str(&data).unwrap_or_default(), false)
        } else {
            let cfg = Config::default();
            cfg.save();
            (cfg, true)
        }
    }

    pub fn save(&self) {
        let config_path = config_path();
        if let Some(parent) = config_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(data) = toml::to_string_pretty(self) {
            let _ = std::fs::write(config_path, data);
        }
    }
}

fn config_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(format!("{}/.config/rpv/cam.toml", home))
}

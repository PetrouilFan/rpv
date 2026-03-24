use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub camera_ip: String,
    pub video_port: u16,
    pub telemetry_port: u16,
    pub rc_port: u16,
    pub video_width: u32,
    pub video_height: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            camera_ip: "192.168.100.1".to_string(),
            video_port: 5600,
            telemetry_port: 5601,
            rc_port: 5602,
            video_width: 960,
            video_height: 540,
        }
    }
}

impl Config {
    pub fn load() -> (Self, bool) {
        let config_path = config_path();
        if let Ok(data) = std::fs::read_to_string(&config_path) {
            (toml::from_str(&data).unwrap_or_default(), false)
        } else {
            (Config::default(), true)
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
    std::path::PathBuf::from(format!("{}/.config/rpv/config.toml", home))
}

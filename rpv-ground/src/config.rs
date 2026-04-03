use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_interface")]
    pub interface: String,
    #[serde(default = "default_drone_id")]
    pub drone_id: u8,
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default = "default_udp_port")]
    pub udp_port: u16,
    #[serde(default = "default_ap_ssid")]
    pub ap_ssid: String,
    #[serde(default = "default_ap_channel")]
    pub ap_channel: u32,
    #[serde(default = "default_video_width")]
    pub video_width: u32,
    #[serde(default = "default_video_height")]
    pub video_height: u32,
    #[serde(default = "default_gcs_uplink_port")]
    pub gcs_uplink_port: u16,
    #[serde(default = "default_gcs_downlink_port")]
    pub gcs_downlink_port: u16,
}

fn default_interface() -> String {
    "wlan1".to_string()
}

fn default_drone_id() -> u8 {
    0
}

fn default_transport() -> String {
    "udp".to_string()
}

fn default_udp_port() -> u16 {
    9001
}

fn default_ap_ssid() -> String {
    "rpv-link".to_string()
}

fn default_ap_channel() -> u32 {
    6
}

fn default_video_width() -> u32 {
    960
}

fn default_video_height() -> u32 {
    540
}

fn default_gcs_uplink_port() -> u16 {
    14551
}

fn default_gcs_downlink_port() -> u16 {
    14550
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interface: default_interface(),
            drone_id: default_drone_id(),
            transport: default_transport(),
            udp_port: default_udp_port(),
            ap_ssid: default_ap_ssid(),
            ap_channel: default_ap_channel(),
            video_width: default_video_width(),
            video_height: default_video_height(),
            gcs_uplink_port: default_gcs_uplink_port(),
            gcs_downlink_port: default_gcs_downlink_port(),
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
    std::path::PathBuf::from(format!("{}/.config/rpv/ground.toml", home))
}

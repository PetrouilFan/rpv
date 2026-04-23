use serde::{Deserialize, Serialize};

/// Configuration fields shared by both camera and ground station.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommonConfig {
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
    /// Pre-configured peer address (IP:port). If set, skip discovery and use this directly.
    #[serde(default)]
    pub peer_addr: Option<String>,
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

impl Default for CommonConfig {
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
            peer_addr: None,
        }
    }
}

impl CommonConfig {
    pub fn config_dir() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        std::path::PathBuf::from(format!("{}/.config/rpv", home))
    }

    /// Parse config from TOML string, logging a warning on parse errors
    /// instead of silently replacing with defaults.
    pub fn parse_toml(toml_str: &str) -> (Self, bool) {
        match toml::from_str(toml_str) {
            Ok(cfg) => (cfg, false),
            Err(e) => {
                tracing::warn!("Config parse error: {}, using defaults", e);
                (Self::default(), true)
            }
        }
    }

    /// Load config from a file, creating defaults if the file doesn't exist.
    /// Returns (config, was_default) where was_default=true means defaults were used.
    pub fn load_from_file(path: &std::path::Path) -> (Self, bool) {
        if let Ok(data) = std::fs::read_to_string(path) {
            Self::parse_toml(&data)
        } else {
            (Self::default(), true)
        }
    }

    pub fn save_to_file(&self, path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(data) = toml::to_string_pretty(self) {
            let _ = std::fs::write(path, data);
        }
    }
}
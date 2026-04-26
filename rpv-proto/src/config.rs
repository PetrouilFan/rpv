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
    #[serde(default = "default_tcp_port")]
    pub tcp_port: Option<u16>,
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
    1
}
fn default_transport() -> String {
    "udp".to_string()
}
fn default_udp_port() -> u16 {
    9001
}
fn default_tcp_port() -> Option<u16> {
    Some(9003)
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
            tcp_port: default_tcp_port(),
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

/// Validate config, logging any errors.
/// Returns true if config is valid.
    pub fn validate_and_log(&self) -> bool {
        let errors = self.validate();
        if errors.is_empty() {
            true
        } else {
            for err in &errors {
                tracing::error!("Config error: {}", err);
            }
            false
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

    /// Validate configuration values.
    /// Returns a vec of validation error messages (empty if valid).
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        if self.udp_port < 1024 {
            errors.push(format!("udp_port {} is in well-known port range (0-1023)", self.udp_port));
        }
        if self.udp_port > 65535 {
            errors.push(format!("udp_port {} exceeds max (65535)", self.udp_port));
        }
        if self.ap_channel == 0 || self.ap_channel > 14 {
            errors.push(format!("ap_channel {} invalid (should be 1-14)", self.ap_channel));
        }
        if self.video_width == 0 || self.video_width > 4096 || self.video_width % 8 != 0 {
            errors.push(format!("video_width {} invalid (should be 320-4096, divisible by 8)", self.video_width));
        }
        if self.video_height == 0 || self.video_height > 2160 || self.video_height % 8 != 0 {
            errors.push(format!("video_height {} invalid (should be 240-2160, divisible by 8)", self.video_height));
        }
        if self.drone_id == 0 {
            errors.push("drone_id should be 1-255".to_string());
        }
        if self.transport != "udp" && self.transport != "raw" && self.transport != "tcp" {
            errors.push(format!("transport '{}' invalid (should be 'udp', 'tcp', or 'raw')", self.transport));
        }

        errors
    }
}
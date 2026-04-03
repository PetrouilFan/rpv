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
    #[serde(default = "default_video_device")]
    pub video_device: String,
    #[serde(default = "default_camera_type")]
    pub camera_type: String,
    #[serde(default = "default_rpicam_options")]
    pub rpicam_options: String,
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
    #[serde(default = "default_intra")]
    pub intra: u32,
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
fn default_video_device() -> String {
    "/dev/video0".to_string()
}
fn default_camera_type() -> String {
    "usb".to_string()
}
fn default_rpicam_options() -> String {
    String::new()
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
    1_000_000
}
fn default_intra() -> u32 {
    1
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
            video_device: default_video_device(),
            camera_type: default_camera_type(),
            rpicam_options: default_rpicam_options(),
            fc_port: default_fc_port(),
            fc_baud: default_fc_baud(),
            video_width: default_video_width(),
            video_height: default_video_height(),
            framerate: default_framerate(),
            bitrate: default_bitrate(),
            intra: default_intra(),
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

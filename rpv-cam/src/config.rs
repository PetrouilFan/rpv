use serde::{Deserialize, Serialize};

use rpv_proto::config::CommonConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(flatten)]
    pub common: CommonConfig,
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
    #[serde(default = "default_framerate")]
    pub framerate: u32,
    #[serde(default = "default_bitrate")]
    pub bitrate: u32,
    #[serde(default = "default_intra")]
    pub intra: u32,
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

fn default_framerate() -> u32 {
    30
}

fn default_bitrate() -> u32 {
    3_000_000
}

fn default_intra() -> u32 {
    30
}

impl Default for Config {
    fn default() -> Self {
        Self {
            common: CommonConfig::default(),
            video_device: default_video_device(),
            camera_type: default_camera_type(),
            rpicam_options: default_rpicam_options(),
            fc_port: default_fc_port(),
            fc_baud: default_fc_baud(),
            framerate: default_framerate(),
            bitrate: default_bitrate(),
            intra: default_intra(),
        }
    }
}

impl Config {
    /// Parse TOML where fields can appear either at top-level or inside [common].
    /// The toml crate merges [common] contents into the top-level table, so we
    /// extract known common fields from [common] and treat everything else as camera fields.
    fn parse_toml(data: &str) -> Result<Self, toml::de::Error> {
        let value: toml::Value = data.parse()?;

        // Extract [common] section as toml::Value
        let common_value = value.get("common")
            .cloned()
            .unwrap_or(toml::Value::Table(toml::map::Map::new()));

        // Known CommonConfig field names
        const COMMON_FIELDS: &[&str] = &[
            "interface", "drone_id", "transport", "udp_port",
            "ap_ssid", "ap_channel", "video_width", "video_height", "peer_addr",
        ];

        // Split [common] into common fields vs camera fields
        let mut common_values = toml::map::Map::new();
        let mut cam_values = toml::map::Map::new();
        for (k, v) in common_value.as_table().expect("common must be a table") {
            if COMMON_FIELDS.contains(&k.as_str()) {
                common_values.insert(k.clone(), v.clone());
            } else {
                cam_values.insert(k.clone(), v.clone());
            }
        }

        // Also grab top-level fields that aren't "common"
        if let Some(top) = value.as_table() {
            for (k, v) in top {
                if k != "common" {
                    cam_values.insert(k.clone(), v.clone());
                }
            }
        }

        let common: CommonConfig = toml::Value::Table(common_values).try_into()?;
        let cam_value = toml::Value::Table(cam_values);
        let mut cfg: Config = cam_value.try_into()?;
        cfg.common = common;
        Ok(cfg)
    }

    pub fn load() -> (Self, bool) {
        let config_path = CommonConfig::config_dir().join("cam.toml");
        if let Ok(data) = std::fs::read_to_string(&config_path) {
            match Self::parse_toml(&data) {
                Ok(cfg) => (cfg, false),
                Err(e) => {
                    tracing::warn!("Config parse error in {}: {}, using defaults", config_path.display(), e);
                    (Self::default(), true)
                }
            }
        } else {
            let cfg = Self::default();
            cfg.save();
            (cfg, true)
        }
    }

    pub fn save(&self) {
        let config_path = CommonConfig::config_dir().join("cam.toml");
        if let Ok(data) = toml::to_string_pretty(self) {
            let _ = std::fs::write(config_path, data);
        }
    }
}

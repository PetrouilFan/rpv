use serde::{Deserialize, Serialize};

use std::os::unix::fs::PermissionsExt;

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
    "csi".to_string()
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
    pub fn load() -> (Self, bool) {
        let config_path = CommonConfig::config_dir().join("cam.toml");
        let (cfg, was_default) = if let Ok(data) = std::fs::read_to_string(&config_path) {
            match toml::from_str(&data) {
                Ok(cfg) => (cfg, false),
                Err(e) => {
                    tracing::warn!(
                        "Config parse error in {}: {}, using defaults",
                        config_path.display(),
                        e
                    );
                    (Self::default(), true)
                }
            }
        } else {
            let cfg = Self::default();
            cfg.save();
            (cfg, true)
        };

        // Validate configuration; if invalid, fall back to defaults
        let errors = cfg.validate();
        if !errors.is_empty() {
            for err in &errors {
                tracing::error!("Config validation error: {}", err);
            }
            tracing::error!("Using default configuration due to validation errors");
            return (Self::default(), true);
        }

        (cfg, was_default)
    }

    /// Validate camera-specific configuration.
    /// Returns a list of error messages (empty if valid).
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        // Validate common config
        errors.extend(self.common.validate());

        // Bitrate: 100 kbps – 20 Mbps
        if self.bitrate < 100_000 || self.bitrate > 20_000_000 {
            errors.push(format!("bitrate {} invalid (should be 100000..20000000 bits/sec)", self.bitrate));
        }

        // Framerate: 1 – 120 fps
        if self.framerate == 0 || self.framerate > 120 {
            errors.push(format!("framerate {} invalid (should be 1..=120)", self.framerate));
        }

        // intra (keyframe interval): 1 – 300
        if self.intra == 0 || self.intra > 300 {
            errors.push(format!("intra {} invalid (should be 1..=300 frames)", self.intra));
        }

        // camera_type allowlist
        match self.camera_type.as_str() {
            "csi" | "rpicam" | "usb" => {}
            _ => errors.push(format!("camera_type '{}' invalid (must be 'csi', 'rpicam', or 'usb')", self.camera_type)),
        }

        // Validate video_device exists if camera_type is usb
        if self.camera_type == "usb" {
            let path = std::path::Path::new(&self.video_device);
            if !path.exists() {
                errors.push(format!("video_device '{}' does not exist", self.video_device));
            } else if let Ok(meta) = std::fs::metadata(path) {
                if !meta.file_type().is_char_device() {
                    errors.push(format!("video_device '{}' is not a character device", self.video_device));
                }
            }
        }

        // Validate fc_port exists and is a character device
        let fc_path = std::path::Path::new(&self.fc_port);
        if !fc_path.exists() {
            errors.push(format!("fc_port '{}' does not exist", self.fc_port));
        } else if let Ok(meta) = std::fs::metadata(fc_path) {
            if !meta.file_type().is_char_device() {
                errors.push(format!("fc_port '{}' is not a character device", self.fc_port));
            }
        }

        // Validate fc_baud reasonable
        if self.fc_baud < 9600 || self.fc_baud > 3_000_000 {
            errors.push(format!("fc_baud {} invalid (typical 9600..3000000)", self.fc_baud));
        }

        errors
    }

    pub fn save(&self) {
        let config_path = CommonConfig::config_dir().join("cam.toml");
        if let Ok(data) = toml::to_string_pretty(self) {
            let _ = std::fs::write(&config_path, data);
            // Set restrictive permissions (owner read/write only)
            let _ = std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

use serde::{Deserialize, Serialize};

use std::os::unix::fs::PermissionsExt;

use rpv_proto::config::CommonConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(flatten)]
    pub common: CommonConfig,
    #[serde(default = "default_gcs_uplink_port")]
    pub gcs_uplink_port: u16,
    #[serde(default = "default_gcs_downlink_port")]
    pub gcs_downlink_port: u16,
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
            common: CommonConfig::default(),
            gcs_uplink_port: default_gcs_uplink_port(),
            gcs_downlink_port: default_gcs_downlink_port(),
        }
    }
}

impl Config {
    pub fn load() -> (Self, bool) {
        let config_path = CommonConfig::config_dir().join("ground.toml");
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
            let cfg = Config::default();
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

    /// Validate ground-specific configuration.
    /// Returns a list of error messages (empty if valid).
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        // Validate common config
        errors.extend(self.common.validate());

        // Validate GCS ports: must be in range 1024-65535 and not equal
        if self.gcs_uplink_port < 1024 || self.gcs_uplink_port > 65535 {
            errors.push(format!(
                "gcs_uplink_port {} invalid (should be 1024..65535)",
                self.gcs_uplink_port
            ));
        }
        if self.gcs_downlink_port < 1024 || self.gcs_downlink_port > 65535 {
            errors.push(format!(
                "gcs_downlink_port {} invalid (should be 1024..65535)",
                self.gcs_downlink_port
            ));
        }
        if self.gcs_uplink_port == self.gcs_downlink_port {
            errors.push("gcs_uplink_port and gcs_downlink_port must be different".to_string());
        }

        errors
    }

    pub fn save(&self) {
        let config_path = CommonConfig::config_dir().join("ground.toml");
        if let Ok(data) = toml::to_string_pretty(self) {
            let _ = std::fs::write(&config_path, data);
            // Set restrictive permissions (0600)
            let _ = std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

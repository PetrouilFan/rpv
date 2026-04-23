use serde::{Deserialize, Serialize};

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
        if let Ok(data) = std::fs::read_to_string(&config_path) {
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
        }
    }

    pub fn save(&self) {
        let config_path = CommonConfig::config_dir().join("ground.toml");
        if let Ok(data) = toml::to_string_pretty(self) {
            let _ = std::fs::write(config_path, data);
        }
    }
}

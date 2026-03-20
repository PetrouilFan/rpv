use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Telemetry {
    pub lat: f64,
    pub lon: f64,
    pub alt: f64,
    pub heading: f64,
    pub speed: f64,
    pub satellites: u32,
    pub battery_v: f64,
    pub battery_pct: u32,
    pub mode: String,
    pub armed: bool,
    #[serde(default = "default_true")]
    pub camera_ok: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Telemetry {
    fn default() -> Self {
        Self {
            lat: 0.0,
            lon: 0.0,
            alt: 0.0,
            heading: 0.0,
            speed: 0.0,
            satellites: 0,
            battery_v: 0.0,
            battery_pct: 0,
            mode: "UNKNOWN".to_string(),
            armed: false,
            camera_ok: true,
        }
    }
}

pub struct TelemetryReceiver {
    state: Arc<Mutex<Telemetry>>,
}

impl TelemetryReceiver {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(Telemetry::default())),
        }
    }

    pub fn get_state(&self) -> Arc<Mutex<Telemetry>> {
        Arc::clone(&self.state)
    }

    pub async fn run(&self, port: u16) {
        let bind_addr = format!("0.0.0.0:{}", port);
        let socket = match UdpSocket::bind(&bind_addr).await {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to bind telemetry socket on {}: {}", bind_addr, e);
                return;
            }
        };

        info!("Telemetry receiver listening on {}", bind_addr);
        let mut buf = vec![0u8; 4096];

        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, _)) => {
                    if let Ok(json_str) = std::str::from_utf8(&buf[..len]) {
                        if let Ok(telem) = serde_json::from_str::<Telemetry>(json_str) {
                            let mut state = self.state.lock().unwrap();
                            *state = telem;
                        }
                    }
                }
                Err(e) => {
                    warn!("Telemetry recv error: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }
        }
    }
}

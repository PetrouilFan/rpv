use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;
use tracing::{info, warn};

use crate::LinkStatus;

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
    link_status: Arc<Mutex<LinkStatus>>,
}

impl TelemetryReceiver {
    pub fn new(link_status: Arc<Mutex<LinkStatus>>) -> Self {
        Self {
            state: Arc::new(Mutex::new(Telemetry::default())),
            link_status,
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
            let timeout = tokio::time::Duration::from_secs(3);
            match tokio::time::timeout(timeout, socket.recv_from(&mut buf)).await {
                Ok(Ok((len, _))) => {
                    if let Ok(json_str) = std::str::from_utf8(&buf[..len]) {
                        if let Ok(telem) = serde_json::from_str::<Telemetry>(json_str) {
                            let mut state = self.state.lock().unwrap();
                            *state = telem;

                            // Set link to Connected when telemetry arrives
                            if let Ok(mut status) = self.link_status.lock() {
                                if *status == LinkStatus::Searching || *status == LinkStatus::SignalLost {
                                    *status = LinkStatus::Connected;
                                    info!("Telemetry: camera connected");
                                }
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!("Telemetry recv error: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
                Err(_) => {
                    // Timeout - no telemetry for 3 seconds
                    if let Ok(mut status) = self.link_status.lock() {
                        if *status == LinkStatus::Connected {
                            *status = LinkStatus::SignalLost;
                            warn!("Telemetry: no data for 3s, signal lost");
                        }
                    }
                }
            }
        }
    }
}

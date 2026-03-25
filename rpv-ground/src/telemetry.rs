use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::info;

use crate::link_state::LinkStateHandle;

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
    link_state: LinkStateHandle,
    rx: crossbeam_channel::Receiver<Vec<u8>>,
}

impl TelemetryReceiver {
    pub fn new(link_state: LinkStateHandle, rx: crossbeam_channel::Receiver<Vec<u8>>) -> Self {
        Self {
            state: Arc::new(Mutex::new(Telemetry::default())),
            link_state,
            rx,
        }
    }

    pub fn get_state(&self) -> Arc<Mutex<Telemetry>> {
        Arc::clone(&self.state)
    }

    pub fn run(&self) {
        info!("Telemetry receiver ready (L2 payload channel)");

        let mut last_telem_time = Instant::now();
        let timeout = std::time::Duration::from_secs(3);

        loop {
            match self.rx.recv_timeout(timeout) {
                Ok(payload) => {
                    if let Ok(json_str) = std::str::from_utf8(&payload) {
                        if let Ok(telem) = serde_json::from_str::<Telemetry>(json_str) {
                            let mut state = self.state.lock().unwrap();
                            *state = telem;
                            last_telem_time = Instant::now();

                            // Telemetry activity can transition Searching -> Connected,
                            // but cannot override SignalLost (heartbeat has priority).
                            self.link_state.telemetry_activity();
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    // Telemetry timeout is informational only — heartbeat handles
                    // the actual SignalLost transition to avoid multi-writer races.
                    if last_telem_time.elapsed() > timeout {
                        // No action needed; heartbeat_monitor is the authority.
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    info!("Telemetry payload channel closed");
                    return;
                }
            }
        }
    }
}

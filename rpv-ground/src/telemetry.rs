use arc_swap::ArcSwap;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
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
    pub battery_pct: Option<u32>,
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
            battery_pct: None,
            mode: "UNKNOWN".to_string(),
            armed: false,
            camera_ok: true,
        }
    }
}

pub struct TelemetryReceiver {
    state: Arc<ArcSwap<Telemetry>>,
    link_state: LinkStateHandle,
    rx: crossbeam_channel::Receiver<Bytes>,
}

impl TelemetryReceiver {
    pub fn new(link_state: LinkStateHandle, rx: crossbeam_channel::Receiver<Bytes>) -> Self {
        Self {
            state: Arc::new(ArcSwap::from_pointee(Telemetry::default())),
            link_state,
            rx,
        }
    }

    pub fn get_state(&self) -> Arc<ArcSwap<Telemetry>> {
        Arc::clone(&self.state)
    }

    pub fn run(&self) {
        info!("Telemetry receiver ready (L2 payload channel)");

        // #22: 100ms timeout — fast Ctrl+C shutdown (was 3s)
        let timeout = std::time::Duration::from_millis(100);

        loop {
            match self.rx.recv_timeout(timeout) {
                Ok(payload) => {
                    // #3: Use from_slice directly — faster, no UTF-8 intermediate check
                    if let Ok(telem) = serde_json::from_slice::<Telemetry>(&payload) {
                        if !telem.camera_ok {
                            tracing::debug!(
                                "Telemetry camera_ok=false: {:?}",
                                String::from_utf8_lossy(&payload).chars().take(100)
                            );
                        }
                        self.state.store(Arc::new(telem));
                        self.link_state.telemetry_activity();
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    info!("Telemetry payload channel closed");
                    return;
                }
            }
        }
    }
}

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

/// Link states encoded as u8 for atomic access.
const SEARCHING: u8 = 0;
const CONNECTED: u8 = 1;
const SIGNAL_LOST: u8 = 2;
const NO_CAMERA: u8 = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkStatus {
    Searching,
    Connected,
    SignalLost,
    NoCamera,
}

impl LinkStatus {
    fn from_u8(v: u8) -> Self {
        match v {
            SEARCHING => LinkStatus::Searching,
            CONNECTED => LinkStatus::Connected,
            SIGNAL_LOST => LinkStatus::SignalLost,
            NO_CAMERA => LinkStatus::NoCamera,
            _ => LinkStatus::Searching,
        }
    }
}

/// Unified link-state machine shared across heartbeat, telemetry, video, and UI.
///
/// #5: camera_ok tracked as a separate AtomicBool so heartbeat_restored
/// can check it on every transition, preventing NoCamera from being lost
/// after SignalLost -> Connected.
pub struct LinkStateMachine {
    state: AtomicU8,
    camera_ok: AtomicBool,
}

impl LinkStateMachine {
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(SEARCHING),
            camera_ok: AtomicBool::new(true),
        }
    }

    /// Wrap into a shareable handle.
    pub fn into_shared(self) -> LinkStateHandle {
        LinkStateHandle {
            state: Arc::new(self),
        }
    }

    pub fn get(&self) -> LinkStatus {
        LinkStatus::from_u8(self.state.load(Ordering::SeqCst))
    }

    /// Heartbeat timeout detected -> SignalLost.
    /// This is the ONLY way to transition into SignalLost (highest priority).
    pub fn heartbeat_lost(&self) {
        let cur = self.state.load(Ordering::SeqCst);
        if cur != SIGNAL_LOST {
            self.state.store(SIGNAL_LOST, Ordering::SeqCst);
            tracing::warn!("Link: heartbeat lost -> SignalLost");
        }
    }

    /// Heartbeat restored -> Connected (or NoCamera if camera unavailable).
    /// #5: Checks camera_ok flag on every transition so NoCamera isn't lost.
    pub fn heartbeat_restored(&self) {
        let cur = self.state.load(Ordering::SeqCst);
        if cur == SEARCHING || cur == SIGNAL_LOST {
            if self.camera_ok.load(Ordering::SeqCst) {
                self.state.store(CONNECTED, Ordering::SeqCst);
                tracing::info!("Link: heartbeat restored -> Connected");
            } else {
                self.state.store(NO_CAMERA, Ordering::SeqCst);
                tracing::info!("Link: heartbeat restored but camera unavailable -> NoCamera");
            }
        }
    }

    /// Telemetry received while in Searching -> Connected.
    /// Does NOT override SignalLost (must wait for heartbeat recovery).
    pub fn telemetry_activity(&self) {
        let cur = self.state.load(Ordering::SeqCst);
        if cur == SEARCHING {
            self.state.store(CONNECTED, Ordering::SeqCst);
            tracing::info!("Link: telemetry received -> Connected");
        }
    }

    /// Video decoded a frame -> Connected.
    /// Only transitions from Searching, not from SignalLost or NoCamera.
    pub fn video_frame_decoded(&self) {
        let cur = self.state.load(Ordering::SeqCst);
        if cur == SEARCHING {
            self.state.store(CONNECTED, Ordering::SeqCst);
            tracing::info!("Link: video frame decoded -> Connected");
        }
    }

    /// Camera not available -> NoCamera.
    /// #5: Sets camera_ok flag so heartbeat_restored can check it.
    pub fn camera_unavailable(&self) {
        self.camera_ok.store(false, Ordering::SeqCst);
        let cur = self.state.load(Ordering::SeqCst);
        if cur != NO_CAMERA {
            self.state.store(NO_CAMERA, Ordering::SeqCst);
            tracing::warn!("Link: camera unavailable -> NoCamera");
        }
    }

    /// Camera available again -> Searching (let heartbeat/telemetry confirm Connected).
    /// #5: Clears camera_ok flag.
    pub fn camera_available(&self) {
        self.camera_ok.store(true, Ordering::SeqCst);
        let cur = self.state.load(Ordering::SeqCst);
        if cur == NO_CAMERA {
            self.state.store(SEARCHING, Ordering::SeqCst);
            tracing::info!("Link: camera available -> Searching");
        }
    }
}

/// Shareable handle wrapping the state machine in an Arc.
#[derive(Clone)]
pub struct LinkStateHandle {
    state: Arc<LinkStateMachine>,
}

impl LinkStateHandle {
    pub fn new() -> Self {
        LinkStateMachine::new().into_shared()
    }

    pub fn get(&self) -> LinkStatus {
        self.state.get()
    }

    pub fn heartbeat_lost(&self) {
        self.state.heartbeat_lost();
    }

    pub fn heartbeat_restored(&self) {
        self.state.heartbeat_restored();
    }

    pub fn telemetry_activity(&self) {
        self.state.telemetry_activity();
    }

    pub fn video_frame_decoded(&self) {
        self.state.video_frame_decoded();
    }

    pub fn camera_unavailable(&self) {
        self.state.camera_unavailable();
    }

    pub fn camera_available(&self) {
        self.state.camera_available();
    }
}

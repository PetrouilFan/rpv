use std::sync::atomic::{AtomicU8, Ordering};
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
    #[allow(dead_code)]
    fn to_u8(self) -> u8 {
        match self {
            LinkStatus::Searching => SEARCHING,
            LinkStatus::Connected => CONNECTED,
            LinkStatus::SignalLost => SIGNAL_LOST,
            LinkStatus::NoCamera => NO_CAMERA,
        }
    }

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
/// Precedence model:
/// - Heartbeat is the primary liveness source.
///   When heartbeat times out -> SignalLost (highest priority transition).
///   When heartbeat restores -> Connected.
/// - Telemetry activity enriches state but cannot override SignalLost back to Connected.
///   Only heartbeat recovery can do that.
/// - Video decoded frames confirm Connected but cannot override SignalLost.
/// - NoCamera is set from telemetry camera_ok=false, cleared when camera_ok=true.
///
/// This prevents the race where telemetry or video sets Connected right after
/// heartbeat declares SignalLost.
pub struct LinkStateMachine {
    state: AtomicU8,
}

impl LinkStateMachine {
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(SEARCHING),
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

    /// Heartbeat restored -> Connected.
    /// Overrides any state (Searching, SignalLost, NoCamera).
    pub fn heartbeat_restored(&self) {
        let cur = self.state.load(Ordering::SeqCst);
        if cur != CONNECTED {
            self.state.store(CONNECTED, Ordering::SeqCst);
            tracing::info!("Link: heartbeat restored -> Connected");
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
    pub fn camera_unavailable(&self) {
        let cur = self.state.load(Ordering::SeqCst);
        if cur != NO_CAMERA {
            self.state.store(NO_CAMERA, Ordering::SeqCst);
            tracing::warn!("Link: camera unavailable -> NoCamera");
        }
    }

    /// Camera available again -> Searching (let heartbeat/telemetry confirm Connected).
    pub fn camera_available(&self) {
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

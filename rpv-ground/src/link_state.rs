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
    /// Checks camera_ok flag on every transition so NoCamera isn't lost.
    /// Also handles transitioning from NoCamera to Connected if camera has become available.
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
        } else if cur == NO_CAMERA && self.camera_ok.load(Ordering::SeqCst) {
            // Camera became available while we were in NoCamera — heartbeat is active, so go straight to Connected
            self.state.store(CONNECTED, Ordering::SeqCst);
            tracing::info!("Link: camera available + heartbeat -> Connected");
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
    /// Sets camera_ok flag so heartbeat_restored can check it.
    pub fn camera_unavailable(&self) {
        self.camera_ok.store(false, Ordering::SeqCst);
        let cur = self.state.load(Ordering::SeqCst);
        if cur != NO_CAMERA {
            self.state.store(NO_CAMERA, Ordering::SeqCst);
            tracing::warn!("Link: camera unavailable -> NoCamera");
        }
    }

    /// Camera available again.
    /// If currently in NoCamera, we know heartbeat is still active (since we only stay in NoCamera
    /// while heartbeats continue), so transition directly to Connected.
    /// Otherwise (e.g., Searching) — wait for heartbeat to confirm.
    pub fn camera_available(&self) {
        self.camera_ok.store(true, Ordering::SeqCst);
        let cur = self.state.load(Ordering::SeqCst);
        if cur == NO_CAMERA {
            self.state.store(CONNECTED, Ordering::SeqCst);
            tracing::info!("Link: camera available -> Connected");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_state_searching_to_connected_via_heartbeat() {
        let state = LinkStateMachine::new();
        assert_eq!(state.get(), LinkStatus::Searching);

        state.heartbeat_restored();
        assert_eq!(state.get(), LinkStatus::Connected);

        state.heartbeat_lost();
        assert_eq!(state.get(), LinkStatus::SignalLost);

        state.heartbeat_restored();
        assert_eq!(state.get(), LinkStatus::Connected);
    }

    #[test]
    fn link_state_heartbeat_restored_checks_camera_ok() {
        let state = LinkStateMachine::new();

        state.camera_unavailable();
        state.heartbeat_restored();
        assert_eq!(state.get(), LinkStatus::NoCamera);

        state.camera_available();
        // After camera_available while in NoCamera, we transition directly to Connected
        assert_eq!(state.get(), LinkStatus::Connected);
    }

    #[test]
    fn link_state_telemetry_does_not_override_signal_lost() {
        let state = LinkStateMachine::new();

        state.heartbeat_restored();
        assert_eq!(state.get(), LinkStatus::Connected);

        state.heartbeat_lost();
        assert_eq!(state.get(), LinkStatus::SignalLost);

        state.telemetry_activity();
        assert_eq!(state.get(), LinkStatus::SignalLost);
    }

    #[test]
    fn link_state_video_frame_decoded_only_from_searching() {
        let state = LinkStateMachine::new();

        state.video_frame_decoded();
        assert_eq!(state.get(), LinkStatus::Connected);

        state.heartbeat_lost();
        assert_eq!(state.get(), LinkStatus::SignalLost);

        state.video_frame_decoded();
        assert_eq!(state.get(), LinkStatus::SignalLost);
    }
}

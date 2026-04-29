use std::os::fd::{FromRawFd, IntoRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use rpv_proto::link;
use rpv_proto::socket_trait::SocketTrait;
use tracing::{info, warn};

const RC_CHANNELS: usize = 16;
const RC_CENTER: u16 = 1500;
const RC_MIN: u16 = 1000;
const RC_MAX: u16 = 2000;
const RC_FREQUENCY_HZ: u64 = 50;
const RC_INTERVAL_US: u64 = 1_000_000 / RC_FREQUENCY_HZ;

const JS_DEV: &str = "/dev/input/js0";
const JOY_MIN: f64 = -1024.0;
const JOY_MAX: f64 = 1023.0;
const JOY_RANGE: f64 = JOY_MAX - JOY_MIN;
const DEADBAND: f64 = 8.0;

const JOY_EVENT_BUTTON: u8 = 0x01;
const JOY_EVENT_AXIS: u8 = 0x02;
const JOY_EVENT_INIT: u8 = 0x80;

#[repr(C)]
struct JoyEvent {
    time: u32,
    value: i16,
    etype: u8,
    number: u8,
}

pub struct RCTx {
    socket: Arc<dyn SocketTrait>,
    drone_id: u8,
    running: Arc<AtomicBool>,
    channels: Arc<ArcSwap<[u16; RC_CHANNELS]>>,
    js_fd: Option<RawFd>,
    last_send: Instant,
    seq: u32,
}

impl RCTx {
    pub fn new(socket: Arc<dyn SocketTrait>, drone_id: u8, running: Arc<AtomicBool>) -> Self {
        let channels = Arc::new(ArcSwap::new(Arc::new([RC_CENTER; RC_CHANNELS])));
        let js_fd = Self::open_joystick();

        Self {
            socket,
            drone_id,
            running,
            channels,
            js_fd,
            last_send: Instant::now(),
            seq: 0,
        }
    }

    fn open_joystick() -> Option<RawFd> {
        if !std::path::Path::new(JS_DEV).exists() {
            warn!("No joystick device at {}", JS_DEV);
            return None;
        }

        let fd = unsafe {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(false)
                .open(JS_DEV)
                .ok()?
                .into_raw_fd();

            if f < 0 {
                return None;
            }

            let flags = libc::fcntl(f, libc::F_GETFL, 0);
            if flags >= 0 {
                libc::fcntl(f, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            f
        };

        info!("Opened joystick device: {} (fd={})", JS_DEV, fd);
        Some(fd)
    }

    pub fn channels(&self) -> Arc<ArcSwap<[u16; RC_CHANNELS]>> {
        self.channels.clone()
    }

    pub fn run(mut self) {
        info!(
            "RC transmitter ready (L2 broadcast, {}Hz, deadline-based)",
            RC_FREQUENCY_HZ
        );
        if self.js_fd.is_some() {
            info!(
                "RC: initial channels ch0={} ch1={} ch2={} ch3={} ch4={}",
                self.channels.load()[0],
                self.channels.load()[1],
                self.channels.load()[2],
                self.channels.load()[3],
                self.channels.load()[4]
            );
        } else {
            info!("RC: no joystick device, sending safe defaults");
        }

        while self.running.load(Ordering::SeqCst) {
            let now = Instant::now();
            let elapsed = now.duration_since(self.last_send);

            if elapsed >= Duration::from_micros(RC_INTERVAL_US) {
                self.send_rc_packet();
                self.last_send = now;
            } else {
                std::thread::sleep(Duration::from_micros(100));
            }

            if let Some(fd) = self.js_fd {
                self.poll_joystick(fd);
            }
        }

        if let Some(fd) = self.js_fd {
            unsafe {
                let _ = std::fs::File::from_raw_fd(fd);
            }
        }
    }

    fn poll_joystick(&mut self, fd: RawFd) {
        let mut arr_copy = (**self.channels.load()).clone();
        let mut updated = false;

        loop {
            let mut event = JoyEvent {
                time: 0,
                value: 0,
                etype: 0,
                number: 0,
            };

            let n = unsafe {
                libc::read(
                    fd,
                    &mut event as *mut _ as *mut libc::c_void,
                    std::mem::size_of::<JoyEvent>(),
                )
            };

            if n < 0 {
                break;
            }
            if n == 0 {
                break;
            }

            if (event.etype & JOY_EVENT_INIT) != 0 {
                continue;
            }

            let value = event.value as i32;
            let num = event.number as usize;

            match event.etype & !JOY_EVENT_INIT {
                x if x == JOY_EVENT_AXIS => {
                    if num < RC_CHANNELS {
                        let rc = Self::joystick_to_rc(num, value);
                        arr_copy[num] = rc;
                        updated = true;
                    }
                }
                x if x == JOY_EVENT_BUTTON => {
                    Self::apply_button(num as u8, value, &mut arr_copy, &mut updated);
                }
                _ => {}
            }
        }

        if updated {
            self.channels.store(Arc::new(arr_copy));
        }
    }

    fn joystick_to_rc(axis: usize, raw_value: i32) -> u16 {
        let rc_min = RC_MIN as f64;
        let rc_max = RC_MAX as f64;
        let rc_range = rc_max - rc_min;
        let center = (rc_min + rc_max) / 2.0;

        let value = raw_value as f64;

        let deadboxed = if value.abs() < DEADBAND {
            0.0
        } else if value > 0.0 {
            value - DEADBAND
        } else {
            value + DEADBAND
        };
        let effective_range = JOY_RANGE - 2.0 * DEADBAND - 1.0;
        let normalized = deadboxed / effective_range;

        let result = match axis {
            0 => center + normalized * rc_range, // ch0 = Roll (Aileron)
            1 => {
                let t = (deadboxed - (JOY_MIN + DEADBAND)) / effective_range;
                rc_min + t.clamp(0.0, 1.0) * rc_range
            }
            2 => center + normalized * rc_range, // ch2 = Yaw
            3 => center - normalized * rc_range, // ch3 = Pitch (inverted)
            _ => center + normalized * rc_range,
        };

        (result.round() as u16).clamp(RC_MIN, RC_MAX)
    }

    fn apply_button(btn: u8, value: i32, arr: &mut [u16; 16], updated: &mut bool) {
        match btn {
            0 if value == 1 => {
                arr[4] = 2000;
                *updated = true;
            }
            0 if value == 0 => {
                arr[4] = 1000;
                *updated = true;
            }
            1 if value == 1 => {
                arr[5] = 2000;
                *updated = true;
            }
            1 if value == 0 => {
                arr[5] = 1000;
                *updated = true;
            }
            2 if value == 1 => {
                arr[6] = 2000;
                *updated = true;
            }
            2 if value == 0 => {
                arr[6] = 1000;
                *updated = true;
            }
            3 if value == 1 => {
                arr[7] = 2000;
                *updated = true;
            }
            3 if value == 0 => {
                arr[7] = 1000;
                *updated = true;
            }
            _ => {}
        }
    }

    fn send_rc_packet(&mut self) {
        let channels = self.channels.load();

        static SEND_COUNT: AtomicU64 = AtomicU64::new(0);
        if SEND_COUNT.fetch_add(1, Ordering::Relaxed) % 500 == 0 {
            info!(
                "RC: ch0={} ch1={} ch2={} ch3={} ch4={}",
                channels[0], channels[1], channels[2], channels[3], channels[4]
            );
        }

        let mut payload = Vec::with_capacity(4 + RC_CHANNELS * 2);
        payload.extend_from_slice(&(RC_CHANNELS as u32).to_le_bytes());
        for &ch in channels.iter() {
            payload.extend_from_slice(&ch.to_le_bytes());
        }

        let mut l2_buf = Vec::with_capacity(link::HEADER_LEN + payload.len());
        let header = link::L2Header {
            drone_id: self.drone_id,
            payload_type: link::PAYLOAD_RC,
            seq: self.seq,
        };
        header.encode_into(&payload, &mut l2_buf);

        let mut send_buf = Vec::with_capacity(8 + 24 + link::HEADER_LEN + payload.len());
        if let Err(e) = self.socket.send_with_buf(&l2_buf, &mut send_buf) {
            warn!("RC send error: {}", e);
        }

        self.seq = self.seq.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roll_center_1500() {
        assert_eq!(RCTx::joystick_to_rc(0, 0), 1500);
        assert_eq!(RCTx::joystick_to_rc(2, 0), 1500);
        assert_eq!(RCTx::joystick_to_rc(3, 0), 1500);
    }

    #[test]
    fn roll_range() {
        assert_eq!(RCTx::joystick_to_rc(0, -1024), 1000);
        assert_eq!(RCTx::joystick_to_rc(0, 1023), 2000);
    }

    #[test]
    fn throttle_range() {
        assert_eq!(RCTx::joystick_to_rc(1, -1024), 1000);
        assert_eq!(RCTx::joystick_to_rc(1, 1023), 2000);
    }

    #[test]
    fn pitch_range() {
        assert_eq!(RCTx::joystick_to_rc(3, 1023), 1000);
        assert_eq!(RCTx::joystick_to_rc(3, -1024), 2000);
    }
}

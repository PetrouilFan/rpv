use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

use crate::link;
use crate::rawsock::RawSocket;

/// Target RC transmission interval (50Hz = 20ms).
const RC_INTERVAL: Duration = Duration::from_millis(20);

pub struct RCTx {
    socket: Arc<RawSocket>,
    drone_id: u8,
    channels: std::sync::Mutex<Vec<u16>>,
    l2_seq: u32,
    running: Arc<AtomicBool>,
}

impl RCTx {
    pub fn new(socket: Arc<RawSocket>, drone_id: u8, running: Arc<AtomicBool>) -> Self {
        Self {
            socket,
            drone_id,
            channels: std::sync::Mutex::new({
                let mut ch = vec![1500u16; 16];
                ch[2] = 1000; // throttle low on init (safety critical)
                ch
            }),
            l2_seq: 0,
            running,
        }
    }

    pub fn run(&mut self) {
        info!("RC transmitter ready (L2 broadcast, 50Hz, deadline-based)");

        let mut l2_buf: Vec<u8> = Vec::with_capacity(link::MAX_PAYLOAD);
        let mut send_buf: Vec<u8> = Vec::with_capacity(8 + 24 + link::MAX_PAYLOAD);
        let mut next_send = Instant::now();
        let mut max_jitter_us: u64 = 0;
        let mut jitter_samples: u64 = 0;

        while self.running.load(Ordering::SeqCst) {
            // Deadline-based scheduling: sleep until the next target time
            let now = Instant::now();
            if now < next_send {
                std::thread::sleep(next_send - now);
            }

            // Track scheduling jitter
            let actual = Instant::now();
            let slip = actual.duration_since(next_send);
            if slip.as_micros() > 0 {
                let slip_us = slip.as_micros() as u64;
                if slip_us > max_jitter_us {
                    max_jitter_us = slip_us;
                }
                jitter_samples += 1;
                if jitter_samples % 3000 == 0 {
                    tracing::debug!(
                        "RC: max scheduling jitter {} us over {} samples",
                        max_jitter_us,
                        jitter_samples
                    );
                    max_jitter_us = 0;
                }
            }

            // Schedule next send based on absolute deadline, not relative sleep
            next_send = actual + RC_INTERVAL;

            let channels = {
                let locked = self.channels.lock().unwrap();
                locked.clone()
            };

            let count = channels.len() as u32;
            let mut payload = Vec::with_capacity(4 + channels.len() * 2);
            payload.extend_from_slice(&count.to_le_bytes());
            for &ch in channels.iter() {
                payload.extend_from_slice(&ch.to_le_bytes());
            }

            let header = link::L2Header {
                drone_id: self.drone_id,
                payload_type: link::PAYLOAD_RC,
                seq: self.l2_seq,
            };
            header.encode_into(&payload, &mut l2_buf);
            let _ = self.socket.send_with_buf(&l2_buf, &mut send_buf);
            self.l2_seq = self.l2_seq.wrapping_add(1);
        }
    }
}

use std::sync::Arc;
use tracing::info;

use crate::link;
use crate::rawsock::RawSocket;

pub struct RCTx {
    socket: Arc<RawSocket>,
    drone_id: u8,
    channels: std::sync::Mutex<Vec<u16>>,
    l2_seq: u32,
}

impl RCTx {
    pub fn new(socket: Arc<RawSocket>, drone_id: u8) -> Self {
        Self {
            socket,
            drone_id,
            channels: std::sync::Mutex::new({
                let mut ch = vec![1500u16; 16];
                ch[2] = 1000; // throttle low on init (safety critical)
                ch
            }),
            l2_seq: 0,
        }
    }

    pub fn run(&mut self) {
        info!("RC transmitter ready (L2 broadcast, 50Hz)");

        loop {
            std::thread::sleep(std::time::Duration::from_millis(20));

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
            let frame = header.encode(&payload);
            let _ = self.socket.send(&frame);
            self.l2_seq = self.l2_seq.wrapping_add(1);
        }
    }
}

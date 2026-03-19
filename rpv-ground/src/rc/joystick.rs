use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;
use tracing::{info, warn};

pub struct RCTx {
    socket: Option<UdpSocket>,
    camera_addr: String,
    _port: u16,
    channels: Arc<Mutex<Vec<u16>>>,
}

impl RCTx {
    pub fn new(camera_ip: &str, port: u16) -> Self {
        Self {
            socket: None,
            camera_addr: format!("{}:{}", camera_ip, port),
            _port: port,
            channels: Arc::new(Mutex::new(vec![1500; 16])),
        }
    }

    pub async fn run(&mut self) {
        match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => self.socket = Some(s),
            Err(e) => {
                warn!("Failed to create RC socket: {}", e);
                return;
            }
        }

        info!("RC transmitter ready, target: {}", self.camera_addr);

        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(20));

        loop {
            interval.tick().await;

            // Clone channels data to avoid holding MutexGuard across await
            let channels = {
                let locked = self.channels.lock().unwrap();
                locked.clone()
            };

            let count = channels.len() as u32;
            let mut packet = Vec::with_capacity(4 + channels.len() * 2);
            packet.extend_from_slice(&count.to_le_bytes());
            for &ch in channels.iter() {
                packet.extend_from_slice(&ch.to_le_bytes());
            }

            if let Some(ref socket) = self.socket {
                let _ = socket.send_to(&packet, &self.camera_addr).await;
            }
        }
    }
}

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;
use tracing::{info, warn};

pub struct RCTx {
    socket: Option<UdpSocket>,
    cam_ip: Arc<Mutex<Option<IpAddr>>>,
    port: u16,
    channels: Arc<Mutex<Vec<u16>>>,
}

impl RCTx {
    pub fn new(cam_ip: Arc<Mutex<Option<IpAddr>>>, port: u16) -> Self {
        Self {
            socket: None,
            cam_ip,
            port,
            channels: Arc::new(Mutex::new({
                let mut ch = vec![1500u16; 16];
                ch[2] = 1000; // throttle low on init (safety critical)
                ch
            })),
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

        info!("RC transmitter ready on port {}", self.port);

        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(20));

        loop {
            interval.tick().await;

            let cam_addr = {
                let locked = self.cam_ip.lock().unwrap();
                locked.map(|ip| format!("{}:{}", ip, self.port))
            };

            let cam_addr = match cam_addr {
                Some(addr) => addr,
                None => continue,
            };

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
                let _ = socket.send_to(&packet, &cam_addr).await;
            }
        }
    }
}

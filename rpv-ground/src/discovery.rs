use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::LinkStatus;

pub fn run(responder_running: Arc<AtomicBool>, link_status: Arc<Mutex<LinkStatus>>) {
    let socket = match UdpSocket::bind("0.0.0.0:5599") {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to bind discovery socket on port 5599: {}", e);
            return;
        }
    };

    tracing::info!("Discovery responder listening on 0.0.0.0:5599");

    let mut buf = [0u8; 256];

    while responder_running.load(Ordering::SeqCst) {
        match socket.recv_from(&mut buf) {
            Ok((len, sender)) => {
                if &buf[..len] == b"rpv-cam" {
                    let _ = socket.send_to(b"rpv-ground", sender);
                    let mut needs_log = false;
                    if let Ok(mut status) = link_status.lock() {
                        if *status == LinkStatus::Searching {
                            *status = LinkStatus::Connected;
                            needs_log = true;
                        }
                    }
                    if needs_log {
                        tracing::info!("Discovery: camera {} connected", sender.ip());
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Discovery recv error: {}", e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    tracing::info!("Discovery responder stopped");
}

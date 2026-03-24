use std::net::{IpAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::LinkStatus;

pub fn run(
    responder_running: Arc<AtomicBool>,
    link_status: Arc<Mutex<LinkStatus>>,
    cam_ip: Arc<Mutex<Option<IpAddr>>>,
) {
    let socket = match UdpSocket::bind("0.0.0.0:5599") {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to bind discovery socket on port 5599: {}", e);
            return;
        }
    };

    let _ = socket.set_broadcast(true);

    tracing::info!("Discovery responder listening on 0.0.0.0:5599");

    let mut buf = [0u8; 256];
    let mut last_broadcast = Instant::now();
    let broadcast_interval = std::time::Duration::from_secs(10);

    while responder_running.load(Ordering::SeqCst) {
        // Periodic broadcast so cameras can discover us after ground restarts
        if last_broadcast.elapsed() >= broadcast_interval {
            let _ = socket.send_to(b"rpv-ground", "255.255.255.255:5599");
            last_broadcast = Instant::now();
        }

        // Non-blocking recv using a short timeout to allow periodic broadcast
        let _ = socket.set_read_timeout(Some(std::time::Duration::from_millis(500)));

        match socket.recv_from(&mut buf) {
            Ok((len, sender)) => {
                if &buf[..len] == b"rpv-cam" {
                    let _ = socket.send_to(b"rpv-ground", sender);
                    let camera_ip = sender.ip();

                    // Update cam_ip for RC transmitter and heartbeat sender
                    *cam_ip.lock().unwrap() = Some(camera_ip);

                    let mut needs_log = false;
                    if let Ok(mut status) = link_status.lock() {
                        if *status == LinkStatus::Searching {
                            *status = LinkStatus::Connected;
                            needs_log = true;
                        }
                    }
                    if needs_log {
                        tracing::info!("Discovery: camera {} connected", camera_ip);
                    }
                }
            }
            Err(_) => {
                // timeout or error, continue to allow periodic broadcast
            }
        }
    }

    tracing::info!("Discovery responder stopped");
}

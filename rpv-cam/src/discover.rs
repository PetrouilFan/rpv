use std::net::{IpAddr, UdpSocket};
use std::time::Duration;

pub fn discover_ground(timeout_secs: u64, fallback_ip: Option<IpAddr>) -> IpAddr {
    let socket = UdpSocket::bind("0.0.0.0:0").unwrap_or_else(|e| {
        panic!("Failed to bind ephemeral socket: {}", e);
    });
    socket
        .set_broadcast(true)
        .expect("Failed to enable broadcast on discovery socket");
    socket
        .set_read_timeout(Some(Duration::from_secs(timeout_secs)))
        .expect("Failed to set read timeout on discovery socket");

    let mut attempt = 0u64;
    let max_broadcast_attempts = 3;

    loop {
        attempt += 1;
        tracing::info!(
            "Discovery attempt #{}: broadcasting on 255.255.255.255:5599",
            attempt
        );

        let _ = socket.send_to(b"rpv-cam", "255.255.255.255:5599");

        // Also send unicast to fallback IP if available (radio link may not support broadcast)
        if let Some(ip) = fallback_ip {
            let _ = socket.send_to(b"rpv-cam", format!("{}:5599", ip));
        }

        // Read responses, skipping our own broadcast echo
        loop {
            let mut buf = [0u8; 64];
            match socket.recv_from(&mut buf) {
                Ok((len, sender)) => {
                    if &buf[..len] == b"rpv-ground" {
                        let ground_ip = sender.ip();
                        tracing::info!("Ground station found at {}", ground_ip);
                        return ground_ip;
                    }
                    // Skip "rpv-cam" echo and other noise, keep waiting
                }
                Err(_) => {
                    // Timeout or error — break inner loop, retry broadcast
                    break;
                }
            }
        }

        // After several failed broadcast attempts, use fallback IP if available
        if attempt >= max_broadcast_attempts {
            if let Some(ip) = fallback_ip {
                tracing::info!(
                    "Broadcast discovery failed after {} attempts, using static ground IP: {}",
                    attempt,
                    ip
                );
                return ip;
            }
        }
    }
}

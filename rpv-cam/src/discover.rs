use std::net::{IpAddr, UdpSocket};
use std::time::Duration;

pub fn discover_ground(timeout_secs: u64) -> IpAddr {
    let socket =
        UdpSocket::bind("0.0.0.0:5599").expect("Failed to bind discovery socket on port 5599");
    socket
        .set_broadcast(true)
        .expect("Failed to enable broadcast on discovery socket");
    socket
        .set_read_timeout(Some(Duration::from_secs(timeout_secs)))
        .expect("Failed to set read timeout on discovery socket");

    let mut attempt = 0u64;

    loop {
        attempt += 1;
        tracing::info!(
            "Discovery attempt #{}: broadcasting on 255.255.255.255:5599",
            attempt
        );

        let _ = socket.send_to(b"rpv-cam", "255.255.255.255:5599");

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
    }
}

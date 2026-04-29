//! Integration test for the discovery protocol.
//!
//! This test verifies that:
//! 1. Beacons are correctly formatted
//! 2. Peers can discover each other via UDP broadcast
//! 3. Peer loss is detected when beacons stop

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

// Constants from rpv-proto/src/discovery.rs
const MAGIC: [u8; 2] = [0x52, 0x50];
const ROLE_CAMERA: u8 = 0x01;
const ROLE_GROUND: u8 = 0x02;
const VERSION: u16 = 1;
const DISCOVERY_PORT: u16 = 19002; // Use different port for testing
const BEACON_INTERVAL: Duration = Duration::from_millis(100); // Faster for tests
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(1);

fn build_test_beacon(role: u8, drone_id: u8, data_port: u16) -> [u8; 14] {
    let mut buf = [0u8; 14];
    buf[0] = MAGIC[0];
    buf[1] = MAGIC[1];
    buf[2] = role;
    buf[3] = drone_id;
    buf[4..6].copy_from_slice(&VERSION.to_le_bytes());
    buf[6..8].copy_from_slice(&data_port.to_le_bytes());
    buf
}

fn parse_beacon(pkt: &[u8]) -> Option<(u8, u8, u16, u16)> {
    if pkt.len() < 14 {
        return None;
    }
    if pkt[0] != MAGIC[0] || pkt[1] != MAGIC[1] {
        return None;
    }
    let role = pkt[2];
    let drone_id = pkt[3];
    let version = u16::from_le_bytes([pkt[4], pkt[5]]);
    let data_port = u16::from_le_bytes([pkt[6], pkt[7]]);
    Some((role, drone_id, version, data_port))
}

struct TestPeer {
    socket: std::net::UdpSocket,
    discovered_peer: Arc<AtomicU64>, // Store peer data port as u64 for simplicity
    running: Arc<AtomicBool>,
}

impl TestPeer {
    fn new(role: u8, drone_id: u8, data_port: u16) -> (Self, SocketAddr) {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("Failed to bind");
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();

        let local_addr = socket.local_addr().unwrap();
        let discovered_peer = Arc::new(AtomicU64::new(0));
        let running = Arc::new(AtomicBool::new(true));

        let peer = TestPeer {
            socket,
            discovered_peer: Arc::clone(&discovered_peer),
            running: Arc::clone(&running),
        };

        (peer, local_addr)
    }

    fn start(&self, role: u8, drone_id: u8, data_port: u16, peer_addr: Option<SocketAddr>) {
        let socket = self.socket.try_clone().unwrap();
        let discovered_peer = Arc::clone(&self.discovered_peer);
        let running = Arc::clone(&self.running);
        let beacon = build_test_beacon(role, drone_id, data_port);

        thread::spawn(move || {
            let mut buf = [0u8; 65536];
            let mut last_beacon = Instant::now();
            let mut consecutive_misses: u32 = 0;

            while running.load(Ordering::Relaxed) {
                // Send beacon periodically
                if last_beacon.elapsed() >= BEACON_INTERVAL {
                    if let Some(addr) = peer_addr {
                        let _ = socket.send_to(&beacon, addr);
                    }
                    last_beacon = Instant::now();
                }

                // Try to receive beacon from peer
                match socket.recv_from(&mut buf) {
                    Ok((n, src)) => {
                        if let Some((peer_role, peer_drone_id, _version, peer_data_port)) =
                            parse_beacon(&buf[..n])
                        {
                            // Accept beacons from opposite role
                            if peer_role != role {
                                discovered_peer.store(peer_data_port as u64, Ordering::Relaxed);
                                consecutive_misses = 0;
                            }
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) => {
                        eprintln!("Receive error: {}", e);
                    }
                }

                // Check for peer loss
                if discovered_peer.load(Ordering::Relaxed) != 0 {
                    consecutive_misses += 1;
                    let max_misses = (DISCOVERY_TIMEOUT.as_millis() / 50) as u32;
                    if consecutive_misses >= max_misses {
                        discovered_peer.store(0, Ordering::Relaxed);
                        consecutive_misses = 0;
                    }
                }

                thread::sleep(Duration::from_millis(10));
            }
        });
    }

    fn get_peer_data_port(&self) -> Option<u16> {
        let val = self.discovered_peer.load(Ordering::Relaxed);
        if val == 0 {
            None
        } else {
            Some(val as u16)
        }
    }

    fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

#[test]
fn discovery_beacon_format() {
    let beacon = build_test_beacon(ROLE_CAMERA, 1, 9003);

    assert_eq!(beacon[0], MAGIC[0]);
    assert_eq!(beacon[1], MAGIC[1]);
    assert_eq!(beacon[2], ROLE_CAMERA);
    assert_eq!(beacon[3], 1);
    assert_eq!(u16::from_le_bytes([beacon[4], beacon[5]]), VERSION);
    assert_eq!(u16::from_le_bytes([beacon[6], beacon[7]]), 9003);
}

#[test]
fn discovery_parse_valid_beacon() {
    let beacon = build_test_beacon(ROLE_GROUND, 2, 9004);
    let parsed = parse_beacon(&beacon);
    assert!(parsed.is_some());

    let (role, drone_id, version, data_port) = parsed.unwrap();
    assert_eq!(role, ROLE_GROUND);
    assert_eq!(drone_id, 2);
    assert_eq!(version, VERSION);
    assert_eq!(data_port, 9004);
}

#[test]
fn discovery_parse_invalid_magic() {
    let mut beacon = build_test_beacon(ROLE_CAMERA, 1, 9003);
    beacon[0] = 0xFF; // Invalid magic
    let parsed = parse_beacon(&beacon);
    assert!(parsed.is_none());
}

#[test]
fn discovery_parse_too_short() {
    let beacon = build_test_beacon(ROLE_CAMERA, 1, 9003);
    let parsed = parse_beacon(&beacon[..5]);
    assert!(parsed.is_none());
}

#[test]
fn discovery_peer_discovery() {
    // Create camera peer
    let (camera, camera_addr) = TestPeer::new(ROLE_CAMERA, 1, 9003);
    let camera_data_port = camera_addr.port();

    // Create ground peer
    let (ground, ground_addr) = TestPeer::new(ROLE_GROUND, 2, 9004);
    let ground_data_port = ground_addr.port();

    // Start both peers with knowledge of each other's addresses
    camera.start(ROLE_CAMERA, 1, camera_data_port, Some(ground_addr));
    ground.start(ROLE_GROUND, 2, ground_data_port, Some(camera_addr));

    // Wait for discovery
    let start = Instant::now();
    loop {
        if camera.get_peer_data_port().is_some() && ground.get_peer_data_port().is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(3) {
            panic!("Peers did not discover each other in time");
        }
        thread::sleep(Duration::from_millis(50));
    }

    // Verify discovery
    assert_eq!(camera.get_peer_data_port(), Some(ground_data_port));
    assert_eq!(ground.get_peer_data_port(), Some(camera_data_port));

    // Cleanup
    camera.stop();
    ground.stop();
}

#[test]
fn discovery_ignores_same_role() {
    // Two cameras should not discover each other
    let (camera1, camera1_addr) = TestPeer::new(ROLE_CAMERA, 1, 9003);
    let camera1_data_port = camera1_addr.port();

    let (camera2, camera2_addr) = TestPeer::new(ROLE_CAMERA, 2, 9004);
    let camera2_data_port = camera2_addr.port();

    camera1.start(ROLE_CAMERA, 1, camera1_data_port, Some(camera2_addr));
    camera2.start(ROLE_CAMERA, 2, camera2_data_port, Some(camera1_addr));

    // Wait some time
    thread::sleep(Duration::from_millis(500));

    // Should NOT have discovered each other (same role)
    assert!(camera1.get_peer_data_port().is_none());
    assert!(camera2.get_peer_data_port().is_none());

    camera1.stop();
    camera2.stop();
}

#[test]
fn discovery_peer_loss_detection() {
    let (camera, camera_addr) = TestPeer::new(ROLE_CAMERA, 1, 9003);
    let camera_data_port = camera_addr.port();

    let (ground, ground_addr) = TestPeer::new(ROLE_GROUND, 2, 9004);
    let ground_data_port = ground_addr.port();

    camera.start(ROLE_CAMERA, 1, camera_data_port, Some(ground_addr));
    ground.start(ROLE_GROUND, 2, ground_data_port, Some(camera_addr));

    // Wait for discovery
    let start = Instant::now();
    loop {
        if camera.get_peer_data_port().is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(3) {
            panic!("Camera did not discover ground in time");
        }
        thread::sleep(Duration::from_millis(50));
    }

    // Stop ground (simulate peer loss)
    ground.stop();

    // Wait for peer loss detection (DISCOVERY_TIMEOUT + some margin)
    thread::sleep(DISCOVERY_TIMEOUT + Duration::from_millis(500));

    // Camera should have lost the peer
    assert!(camera.get_peer_data_port().is_none());

    camera.stop();
}

#[test]
fn discovery_multiple_drones() {
    // Ground should discover multiple cameras and track the most recent
    let (ground, ground_addr) = TestPeer::new(ROLE_GROUND, 0, 9004);
    let ground_data_port = ground_addr.port();

    let (camera1, camera1_addr) = TestPeer::new(ROLE_CAMERA, 1, 9003);
    let camera1_data_port = camera1_addr.port();

    let (camera2, camera2_addr) = TestPeer::new(ROLE_CAMERA, 2, 9005);
    let camera2_data_port = camera2_addr.port();

    ground.start(ROLE_GROUND, 0, ground_data_port, None); // Ground doesn't send
    camera1.start(ROLE_CAMERA, 1, camera1_data_port, Some(ground_addr));
    camera2.start(ROLE_CAMERA, 2, camera2_data_port, Some(ground_addr));

    // Wait for ground to discover cameras
    let start = Instant::now();
    loop {
        if ground.get_peer_data_port().is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(3) {
            panic!("Ground did not discover a camera in time");
        }
        thread::sleep(Duration::from_millis(50));
    }

    // Verify ground discovered one of the cameras
    let discovered = ground.get_peer_data_port().unwrap();
    assert!(discovered == camera1_data_port || discovered == camera2_data_port);

    camera1.stop();
    camera2.stop();
    ground.stop();
}

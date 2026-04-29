use std::io;
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

const MAGIC: [u8; 2] = [0x52, 0x50];
const VERSION: u16 = 1;

const BEACON_INTERVAL: Duration = Duration::from_millis(500);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

const BROADCAST_ADDR: &str = "255.255.255.255";
const DISCOVERY_PORT: u16 = 9002;

/// Beacon format (14 bytes):
/// [0..2]  Magic: b"RP"
/// [2]     Role: 0x01 = camera, 0x02 = ground
/// [3]     Drone ID
/// [4..6]  Version (u16 LE)
/// [6..8]  Data port (u16 LE)
/// [8..14] Reserved
pub struct Discovery {
    /// Running flag for graceful shutdown - discovery thread checks this
    running: Arc<AtomicBool>,
}

impl Discovery {
    pub fn spawn(
        role: u8,
        drone_id: u8,
        data_port: u16,
    ) -> io::Result<(Self, Arc<ArcSwap<Option<SocketAddr>>>)> {
        let sock = StdUdpSocket::bind(format!("0.0.0.0:{}", DISCOVERY_PORT))?;
        sock.set_broadcast(true)?;
        sock.set_read_timeout(Some(Duration::from_millis(100)))?;

        let peer_addr = Arc::new(ArcSwap::new(Arc::new(None)));
        let last_seen = Arc::new(AtomicU64::new(0));
        let running = Arc::new(AtomicBool::new(true));

        let beacon = build_beacon(role, drone_id, data_port);
        let broadcast_target: SocketAddr = format!("{}:{}", BROADCAST_ADDR, DISCOVERY_PORT)
            .parse()
            .unwrap();

        let peer_addr_clone = Arc::clone(&peer_addr);
        let last_seen_clone = Arc::clone(&last_seen);
        let running_clone = Arc::clone(&running);

        thread::spawn(move || {
            let result = std::panic::catch_unwind(|| {
                discovery_loop(
                    sock,
                    beacon,
                    broadcast_target,
                    data_port,
                    peer_addr_clone,
                    last_seen_clone,
                    running_clone,
                    role,
                );
            });
            if let Err(err) = result {
                tracing::error!("Discovery thread panicked: {:?}", err);
            }
        });

        let disc = Self {
            running: Arc::clone(&running),
        };
        Ok((disc, peer_addr))
    }

    /// Gracefully stop the discovery thread
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

fn build_beacon(role: u8, drone_id: u8, data_port: u16) -> [u8; 14] {
    let mut buf = [0u8; 14];
    buf[0] = MAGIC[0];
    buf[1] = MAGIC[1];
    buf[2] = role;
    buf[3] = drone_id;
    buf[4..6].copy_from_slice(&VERSION.to_le_bytes());
    buf[6..8].copy_from_slice(&data_port.to_le_bytes());
    buf
}

#[allow(clippy::too_many_arguments)]
fn discovery_loop(
    socket: StdUdpSocket,
    beacon: [u8; 14],
    broadcast_target: SocketAddr,
    _my_data_port: u16,
    peer_addr: Arc<ArcSwap<Option<SocketAddr>>>,
    last_seen: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    local_role: u8,
) {
    let mut buf = [0u8; 65536];
    let mut last_beacon = Instant::now();
    let mut last_log = Instant::now();
    let mut consecutive_misses: u32 = 0;

    // Check running flag to allow graceful shutdown
    while running.load(Ordering::Relaxed) {
        if last_beacon.elapsed() >= BEACON_INTERVAL {
            let _ = socket.send_to(&beacon, broadcast_target);
            last_beacon = Instant::now();
        }

        match socket.recv_from(&mut buf) {
            Ok((n, src)) if n >= 14 => {
                let pkt = &buf[..n];
                if pkt[0] == MAGIC[0] && pkt[1] == MAGIC[1] {
                    let peer_role = pkt[2];
                    let peer_data_port = u16::from_le_bytes([pkt[6], pkt[7]]);
                    // Accept beacons only from the opposite role (camera vs ground)
                    if peer_role != local_role {
                        // Use atomic counter for peer presence tracking
                        // This is immune to clock jumps (NTP, suspend/resume)
                        static PEER_SEEN_COUNT: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(0);
                        PEER_SEEN_COUNT.fetch_add(1, Ordering::Relaxed);
                        last_seen.store(PEER_SEEN_COUNT.load(Ordering::Relaxed), Ordering::Relaxed);

                        let peer_data_addr = match format!("{}:{}", src.ip(), peer_data_port)
                            .parse::<SocketAddr>()
                        {
                            Ok(addr) => addr,
                            Err(e) => {
                                tracing::warn!("Failed to parse peer address: {}", e);
                                continue;
                            }
                        };
                        let current = peer_addr.load();
                        let changed = match current.as_ref() {
                            Some(existing) => *existing != peer_data_addr,
                            None => true,
                        };
                        if changed {
                            tracing::info!("Discovered peer at {}", peer_data_addr);
                        }
                        peer_addr.store(Arc::new(Some(peer_data_addr)));
                        consecutive_misses = 0;
                    }
                }
            }
            Ok(_) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                tracing::warn!("Discovery recv error: {}", e);
            }
        }

        // NOTE: For peer-loss detection, we now use a counter-based approach
        // instead of wall-clock. The consecutive_misses counter tracks how many
        // polling cycles have passed without receiving a valid peer beacon.
        // This is more robust against clock jumps than wall-clock timestamps.
        let current = peer_addr.load();
        if current.is_some() {
            // Increment misses on each poll cycle; reset on successful receive
            // If we miss DISCOVERY_TIMEOUT / 50ms cycles (default: 3000/50 = 60), peer is lost
            let max_misses = (DISCOVERY_TIMEOUT.as_millis() / 50) as u32;
            consecutive_misses += 1;
            if consecutive_misses >= max_misses {
                tracing::warn!("Peer lost, re-entering discovery mode");
                peer_addr.store(Arc::new(None));
                consecutive_misses = 0;
            }
        } else {
            // Reset counter when no peer - allows clean rediscovery
            consecutive_misses = 0;
        }

        if last_log.elapsed() >= Duration::from_secs(5) {
            let current = peer_addr.load();
            if let Some(ref addr) = current.as_ref() {
                tracing::debug!("Discovery: connected to {}", addr);
            } else {
                tracing::debug!("Discovery: searching for peer...");
            }
            last_log = Instant::now();
        }

        thread::sleep(Duration::from_millis(50));
    }
}

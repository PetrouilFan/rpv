use std::io;
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::Arc;

use arc_swap::ArcSwap;

pub struct UdpSocket {
    socket: Arc<StdUdpSocket>,
    peer: Arc<ArcSwap<Option<SocketAddr>>>,
    broadcast_addr: SocketAddr,
}

impl UdpSocket {
    pub fn new(
        socket: Arc<StdUdpSocket>,
        peer: Arc<ArcSwap<Option<SocketAddr>>>,
    ) -> io::Result<Self> {
        let broadcast_addr: SocketAddr = "10.42.0.255:9001".parse().unwrap();

        tracing::info!("UDP socket ready (shared with discovery)");
        Ok(Self {
            socket,
            peer,
            broadcast_addr,
        })
    }

    pub fn send_with_buf(&self, payload: &[u8], _buf: &mut Vec<u8>) -> io::Result<usize> {
        let current = self.peer.load();
        if let Some(addr) = current.as_ref() {
            self.socket.send_to(payload, *addr)
        } else {
            self.socket.send_to(payload, self.broadcast_addr)
        }
    }

    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self.socket.recv_from(buf) {
            Ok((n, _addr)) => Ok(n),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                Ok(0)
            }
            Err(e) => Err(e),
        }
    }
}

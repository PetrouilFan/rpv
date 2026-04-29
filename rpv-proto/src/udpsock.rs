use std::io;
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::socket_trait::SocketTrait;

pub struct UdpSocket {
    socket: Arc<StdUdpSocket>,
    peer: Arc<ArcSwap<Option<SocketAddr>>>,
    broadcast_addr: SocketAddr,
}

impl UdpSocket {
    pub fn new(
        socket: Arc<StdUdpSocket>,
        peer: Arc<ArcSwap<Option<SocketAddr>>>,
        udp_port: u16,
    ) -> io::Result<Self> {
        let broadcast_addr: SocketAddr = format!("255.255.255.255:{}", udp_port)
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        tracing::info!(
            "UDP socket ready (shared with discovery, port {})",
            udp_port
        );
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
            Ok((n, addr)) => {
                tracing::trace!("UDP recv: {} bytes from {}", n, addr);
                Ok(n)
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                Ok(0)
            }
            Err(e) => Err(e),
        }
    }
}

impl SocketTrait for UdpSocket {
    fn send_with_buf(&self, payload: &[u8], buf: &mut Vec<u8>) -> io::Result<usize> {
        UdpSocket::send_with_buf(self, payload, buf)
    }
    fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        UdpSocket::recv(self, buf)
    }
    fn recreate(&self) -> std::io::Result<Box<dyn SocketTrait + Send + Sync>> {
        UdpSocket::new(
            self.socket.clone(),
            self.peer.clone(),
            self.broadcast_addr.port(),
        )
        .map(|s| Box::new(s) as Box<dyn SocketTrait + Send + Sync>)
    }
    fn reconnect(&self) -> std::io::Result<()> {
        // UDP is connectionless, nothing to reconnect
        Ok(())
    }
}

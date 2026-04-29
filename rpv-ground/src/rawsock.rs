/// Raw AF_PACKET socket for send/receive on a WiFi interface in monitor mode.
use std::io;
use rpv_proto::rawsocket_base::RawSocketBase;
use rpv_proto::socket_trait::SocketTrait;

pub struct RawSocket(RawSocketBase);

impl RawSocket {
    pub fn new(iface: &str) -> io::Result<Self> {
        RawSocketBase::new(iface, libc::ARPHRD_IEEE80211_RADIOTAP as i32).map(Self)
    }

    pub fn send_with_buf(&self, payload: &[u8], buf: &mut Vec<u8>) -> io::Result<usize> {
        self.0.send_with_buf(payload, buf)
    }

    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.recv(buf)
    }
}

impl SocketTrait for RawSocket {
    fn send_with_buf(&self, payload: &[u8], buf: &mut Vec<u8>) -> io::Result<usize> {
        RawSocket::send_with_buf(self, payload, buf)
    }
    fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        RawSocket::recv(self, buf)
    }
    fn recreate(&self) -> io::Result<Box<dyn SocketTrait + Send + Sync>> {
        RawSocket::new(self.0.iface()).map(|s| Box::new(s) as Box<dyn SocketTrait + Send + Sync>)
    }
    fn reconnect(&self) -> io::Result<()> {
        Ok(())
    }
}

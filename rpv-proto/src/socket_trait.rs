/// Common socket trait for abstracting over raw 802.11 and UDP transports.
pub trait SocketTrait: Send + Sync {
    fn send_with_buf(&self, payload: &[u8], buf: &mut Vec<u8>) -> std::io::Result<usize>;
    fn recv(&self, buf: &mut [u8]) -> std::io::Result<usize>;
}
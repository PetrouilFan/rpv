use std::io;
use std::io::{Read, Write};
use std::net::{TcpStream, TcpListener};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use arc_swap::ArcSwap;

use crate::socket_trait::SocketTrait;

/// TCP socket with length-prefixed framing.
/// 
/// Since TCP is a stream protocol, we use 4-byte length prefix (u32, little-endian)
/// before each payload to delimit messages. This ensures message boundaries are preserved.
pub struct TcpSocket {
    /// The underlying TCP stream wrapped in ArcSwap for potential reconnection support.
    /// None means not connected. Mutex provides interior mutability for read/write.
    stream: Arc<ArcSwap<Option<Mutex<TcpStream>>>>,
    /// Read buffer for incomplete frames (interior mutability via Mutex)
    read_buf: Mutex<Vec<u8>>,
}

impl TcpSocket {
    /// Create a TCP client socket and connect to the specified address.
    ///
    /// # Arguments
    /// * `remote_addr` - The remote address to connect to (e.g., "192.168.1.100:9003")
    /// * `timeout_ms` - Connection and read/write timeout in milliseconds
    pub fn new_client(remote_addr: &str, timeout_ms: u64) -> io::Result<Self> {
        let stream = TcpStream::connect(remote_addr)
            .map_err(|e| {
                tracing::error!("TCP connect to {} failed: {}", remote_addr, e);
                e
            })?;
        
        let timeout = Duration::from_millis(timeout_ms);
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        
        tracing::info!("TCP client connected to {}", remote_addr);
        
        Ok(Self {
            stream: Arc::new(ArcSwap::new(Arc::new(Some(Mutex::new(stream))))),
            read_buf: Mutex::new(Vec::new()),
        })
    }
    
    /// Create a TCP server socket that listens for one connection.
    ///
    /// # Arguments
    /// * `listen_addr` - The local address to listen on (e.g., "0.0.0.0:9003")
    /// * `timeout_ms` - Read/write timeout for accepted connection in milliseconds
    pub fn new_server(listen_addr: &str, timeout_ms: u64) -> io::Result<Self> {
        let listener = TcpListener::bind(listen_addr)
            .map_err(|e| {
                tracing::error!("TCP bind to {} failed: {}", listen_addr, e);
                e
            })?;
        
        tracing::info!("TCP server listening on {}", listen_addr);
        
        // Accept one connection (blocking with timeout handled after accept)
        let (stream, peer_addr) = listener.accept()
            .map_err(|e| {
                tracing::error!("TCP accept failed: {}", e);
                e
            })?;
        
        let timeout = Duration::from_millis(timeout_ms);
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        
        tracing::info!("TCP server accepted connection from {}", peer_addr);
        
        Ok(Self {
            stream: Arc::new(ArcSwap::new(Arc::new(Some(Mutex::new(stream))))),
            read_buf: Mutex::new(Vec::new()),
        })
    }
    
    /// Check if the socket is connected.
    pub fn is_connected(&self) -> bool {
        self.stream.load().is_some()
    }
}

impl SocketTrait for TcpSocket {
    /// Send a payload with length-prefixed framing.
    ///
    /// The wire format is: [4-byte length (LE)][payload bytes]
    fn send_with_buf(&self, payload: &[u8], _buf: &mut Vec<u8>) -> io::Result<usize> {
        let stream_opt = self.stream.load();
        if let Some(ref stream_mutex) = **stream_opt {
            let mut stream = stream_mutex.lock().unwrap();
            // Write length prefix + payload
            let len_bytes = (payload.len() as u32).to_le_bytes();
            stream.write_all(&len_bytes)
                .map_err(|e| {
                    tracing::warn!("TCP send length prefix failed: {}", e);
                    e
                })?;
            stream.write_all(payload)
                .map_err(|e| {
                    tracing::warn!("TCP send payload failed: {}", e);
                    e
                })?;
            Ok(payload.len())
        } else {
            Err(io::Error::new(io::ErrorKind::NotConnected, "TCP socket not connected"))
        }
    }
    
    /// Receive a framed message from the TCP stream.
    ///
    /// This method handles the length-prefixed framing:
    /// 1. Buffer incoming data until we have at least 4 bytes (length prefix)
    /// 2. Parse the length and buffer until we have the complete payload
    /// 3. Copy the payload into `buf` and return the length
    ///
    /// Returns:
    /// - Ok(0) if no complete frame is available (timeout or would block)
    /// - Ok(n) with the number of bytes copied to buf
    /// - Err(e) on error
    fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let stream_opt = self.stream.load();
        if let Some(ref stream_mutex) = **stream_opt {
            let mut stream = stream_mutex.lock().unwrap();
            let mut read_buf = self.read_buf.lock().unwrap();
            
            loop {
                // Try to parse a complete frame from read_buf
                if read_buf.len() >= 4 {
                    let len = u32::from_le_bytes([read_buf[0], read_buf[1], read_buf[2], read_buf[3]]) as usize;
                    
                    // Sanity check on frame length
                    if len > 65536 {
                        tracing::warn!("TCP frame too large: {} bytes, clearing buffer", len);
                        read_buf.clear();
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "Frame too large"));
                    }
                    
                    if read_buf.len() >= 4 + len {
                        // We have a complete frame
                        if len > buf.len() {
                            tracing::warn!("TCP recv buffer too small: need {} bytes, have {}", len, buf.len());
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "Buffer too small for frame"));
                        }
                        let frame_data = read_buf[4..4+len].to_vec();
                        buf[..len].copy_from_slice(&frame_data);
                        read_buf.drain(0..4+len);
                        return Ok(len);
                    }
                }
                
                // Read more data from stream
                let mut tmp = [0u8; 4096];
                match stream.read(&mut tmp) {
                    Ok(0) => {
                        // Connection closed
                        tracing::warn!("TCP connection closed by peer");
                        return Ok(0);
                    }
                    Ok(n) => {
                        read_buf.extend_from_slice(&tmp[..n]);
                        // Continue loop to try parsing frame again
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock 
                              || e.kind() == io::ErrorKind::TimedOut => {
                        // No more data available now, return 0 to indicate no complete frame
                        return Ok(0);
                    }
                    Err(e) => {
                        tracing::warn!("TCP recv error: {}", e);
                        return Err(e);
                    }
                }
            }
        } else {
            Err(io::Error::new(io::ErrorKind::NotConnected, "TCP socket not connected"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    
    #[test]
    fn test_tcp_socket_framing() {
        // Start a server
        let server_addr = "127.0.0.1:19003";
        let server_addr_clone = server_addr.to_string();
        let handle = thread::spawn(move || {
            let socket = TcpSocket::new_server(&server_addr_clone, 1000).unwrap();
            socket
        });
        
        // Give server time to start
        thread::sleep(Duration::from_millis(100));
        
        // Connect client
        let client = TcpSocket::new_client(server_addr, 1000).unwrap();
        let server = handle.join().unwrap();
        
        // Test send/receive
        let payload = b"Hello, TCP!";
        let mut send_buf = Vec::new();
        let n = client.send_with_buf(payload, &mut send_buf).unwrap();
        assert_eq!(n, payload.len());
        
        // Server receives
        let mut recv_buf = vec![0u8; 1024];
        let n = server.recv(&mut recv_buf).unwrap();
        assert_eq!(&recv_buf[..n], payload);
    }
}

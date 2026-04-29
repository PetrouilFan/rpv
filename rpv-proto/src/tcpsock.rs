use std::io;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::FromRawFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use arc_swap::ArcSwap;

use crate::link;
use crate::socket_trait::SocketTrait; // for MAX_PAYLOAD and HEADER_LEN

/// TCP socket with length-prefixed framing.
///
/// Since TCP is a stream protocol, we use 4-byte length prefix (u32, little-endian)
/// before each payload to delimit messages. This ensures message boundaries are preserved.
///
/// The socket uses separate read and write handles to avoid head-of-line blocking
/// and lock contention between concurrent readers and writers.
pub struct TcpSocket {
    /// TCP connection stream (interior mutability for reconnect)
    stream: Arc<Mutex<Option<TcpStream>>>,
    /// Read buffer for incomplete frames (interior mutability via Mutex)
    read_buf: Mutex<Vec<u8>>,
    /// Listener for server mode (kept to re-accept connections after disconnect)
    listener: Option<TcpListener>,
    /// Target address for client mode (kept to reconnect after disconnect)
    target_addr: Option<SocketAddr>,
    /// Timeout in milliseconds for read/write operations
    timeout_ms: u64,
}

impl TcpSocket {
    /// Create a TCP client socket and connect to the specified address.
    ///
    /// # Arguments
    /// * `remote_addr` - The remote address to connect to (e.g., "192.168.1.100:9003")
    /// * `timeout_ms` - Connection and read/write timeout in milliseconds
    pub fn new_client(remote_addr: &str, timeout_ms: u64) -> io::Result<Self> {
        let stream = TcpStream::connect(remote_addr).map_err(|e| {
            tracing::error!("TCP connect to {} failed: {}", remote_addr, e);
            e
        })?;

        let timeout = Duration::from_millis(timeout_ms);
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;

        // Split into read and write halves to avoid lock contention
        let read_stream = stream.try_clone().map_err(|e| {
            tracing::error!("Failed to clone TCP stream for read half: {}", e);
            e
        })?;

        tracing::info!("TCP client connected to {}", remote_addr);

        let addr: SocketAddr = remote_addr
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        Ok(Self {
            stream: Arc::new(Mutex::new(Some(stream))),
            read_buf: Mutex::new(Vec::new()),
            listener: None,
            target_addr: Some(addr),
            timeout_ms,
        })
    }

    /// Create a TCP server socket that listens for one connection.
    ///
    /// # Arguments
    /// * `listen_addr` - The local address to listen on (e.g., "0.0.0.0:9003")
    /// * `timeout_ms` - Read/write timeout for accepted connection in milliseconds
    pub fn new_server(listen_addr: &str, timeout_ms: u64) -> io::Result<Self> {
        let addr: SocketAddr = listen_addr.parse().map_err(|e| {
            tracing::error!("Invalid listen address {}: {}", listen_addr, e);
            io::Error::new(io::ErrorKind::InvalidInput, e)
        })?;

        // SAFETY: Domain is validated (AF_INET or AF_INET6), SOCK_STREAM, protocol 0
        let fd = unsafe {
            let domain = if addr.is_ipv4() {
                libc::AF_INET
            } else {
                libc::AF_INET6
            };
            libc::socket(domain, libc::SOCK_STREAM, 0)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Set SO_REUSEADDR to allow rebinding after crash (TIME_WAIT)
        let optval: libc::c_int = 1;
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            // SAFETY: fd is valid and owned by us, checked >= 0 before close
            unsafe {
                libc::close(fd);
            }
            return Err(io::Error::last_os_error());
        }

        // Bind to address
        let ret = unsafe {
            match addr {
                SocketAddr::V4(v4) => {
                    let mut sockaddr: libc::sockaddr_in = std::mem::zeroed();
                    sockaddr.sin_family = libc::AF_INET as u16;
                    sockaddr.sin_port = v4.port().to_be();
                    // IPv4 address: convert to network byte order
                    sockaddr.sin_addr.s_addr = v4.ip().to_bits().to_be();
                    libc::bind(
                        fd,
                        &sockaddr as *const _ as *const libc::sockaddr,
                        std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    )
                }
                SocketAddr::V6(v6) => {
                    let mut sockaddr: libc::sockaddr_in6 = std::mem::zeroed();
                    sockaddr.sin6_family = libc::AF_INET6 as u16;
                    sockaddr.sin6_port = v6.port().to_be();
                    sockaddr.sin6_addr.s6_addr = v6.ip().octets();
                    libc::bind(
                        fd,
                        &sockaddr as *const _ as *const libc::sockaddr,
                        std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                    )
                }
            }
        };
        if ret < 0 {
            // SAFETY: fd is valid and owned by us, checked >= 0 before close
            unsafe {
                libc::close(fd);
            }
            return Err(io::Error::last_os_error());
        }

        // Listen
        let ret = unsafe { libc::listen(fd, 128) };
        if ret < 0 {
            // SAFETY: fd is valid and owned by us, checked >= 0 before close
            unsafe {
                libc::close(fd);
            }
            return Err(io::Error::last_os_error());
        }

        // SAFETY: fd is valid and newly created, from_raw_fd takes ownership safely
        let listener = unsafe { TcpListener::from_raw_fd(fd) };

        tracing::info!("TCP server listening on {}", listen_addr);

        // Accept one connection (blocking with timeout handled after accept)
        let (stream, peer_addr) = listener.accept().map_err(|e| {
            tracing::error!("TCP accept failed: {}", e);
            e
        })?;

        let timeout = Duration::from_millis(timeout_ms);
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;

        // Split into read and write halves to avoid lock contention
        let read_stream = stream.try_clone().map_err(|e| {
            tracing::error!("Failed to clone accepted TCP stream for read half: {}", e);
            e
        })?;

        tracing::info!("TCP server accepted connection from {}", peer_addr);

        Ok(Self {
            stream: Arc::new(Mutex::new(Some(stream))),
            read_buf: Mutex::new(Vec::new()),
            listener: Some(listener),
            target_addr: None,
            timeout_ms,
        })
    }

    /// Check if the socket is connected.
    pub fn is_connected(&self) -> bool {
        self.stream.lock().map(|opt| opt.is_some()).unwrap_or(false)
    }
}

impl SocketTrait for TcpSocket {
    /// Send a payload with length-prefixed framing.
    ///
    /// The wire format is: [4-byte length (LE)][payload bytes]
    fn send_with_buf(&self, payload: &[u8], _buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut stream_opt = self
            .stream
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "TCP stream mutex poisoned"))?;
        if let Some(ref mut stream) = *stream_opt {
            // Write length prefix + payload
            let len_bytes = (payload.len() as u32).to_le_bytes();
            stream.write_all(&len_bytes).map_err(|e| {
                tracing::warn!("TCP send length prefix failed: {}", e);
                e
            })?;
            stream.write_all(payload).map_err(|e| {
                tracing::warn!("TCP send payload failed: {}", e);
                e
            })?;
            Ok(payload.len())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "TCP socket not connected",
            ))
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
        let mut stream_opt = self
            .stream
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "TCP stream mutex poisoned"))?;
        if let Some(ref mut stream) = *stream_opt {
            let mut read_buf = match self.read_buf.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    tracing::warn!("TCP read_buf mutex poisoned, recovering");
                    poisoned.into_inner()
                }
            };

            loop {
                // Try to parse a complete frame from read_buf
                if read_buf.len() >= 4 {
                    let len =
                        u32::from_le_bytes([read_buf[0], read_buf[1], read_buf[2], read_buf[3]])
                            as usize;

                    // Sanity check: frame size must not exceed maximum protocol size.
                    // Maximum payload is link::MAX_PAYLOAD (1400) plus L2 header (HEADER_LEN=8).
                    // Any larger frame is malicious or corrupted and will be rejected.
                    let max_frame = link::MAX_PAYLOAD + link::HEADER_LEN;
                    if len > max_frame {
                        tracing::warn!(
                            "TCP frame too large: {} bytes (max {}), clearing buffer",
                            len,
                            max_frame
                        );
                        read_buf.clear();
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Frame too large",
                        ));
                    }

                    if read_buf.len() >= 4 + len {
                        // We have a complete frame
                        if len > buf.len() {
                            tracing::warn!(
                                "TCP recv buffer too small: need {} bytes, have {}",
                                len,
                                buf.len()
                            );
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "Buffer too small for frame",
                            ));
                        }
                        let frame_data = read_buf[4..4 + len].to_vec();
                        buf[..len].copy_from_slice(&frame_data);
                        read_buf.drain(0..4 + len);
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
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::TimedOut =>
                    {
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
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "TCP socket not connected",
            ))
        }
    }

    fn recreate(&self) -> std::io::Result<Box<dyn SocketTrait + Send + Sync>> {
        if let Some(ref listener) = self.listener {
            // Server mode: re-bind and accept
            let listen_addr = listener
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| "0.0.0.0:0".to_string());
            TcpSocket::new_server(&listen_addr, self.timeout_ms)
                .map(|s| Box::new(s) as Box<dyn SocketTrait + Send + Sync>)
        } else if let Some(addr) = self.target_addr {
            // Client mode: re-connect
            TcpSocket::new_client(&addr.to_string(), self.timeout_ms)
                .map(|s| Box::new(s) as Box<dyn SocketTrait + Send + Sync>)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "No listener or target address for TCP recreate",
            ))
        }
    }

    fn reconnect(&self) -> std::io::Result<()> {
        // Server mode: accept a new connection from the existing listener
        if let Some(listener) = &self.listener {
            let (new_stream, peer_addr) = listener.accept().map_err(|e| {
                tracing::warn!("TCP server accept failed during reconnect: {}", e);
                e
            })?;
            let timeout = Duration::from_millis(self.timeout_ms);
            new_stream.set_read_timeout(Some(timeout))?;
            new_stream.set_write_timeout(Some(timeout))?;

            *self.stream.lock().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "TCP stream mutex poisoned during reconnect",
                )
            })? = Some(new_stream);

            tracing::info!("TCP server reconnected: new connection from {}", peer_addr);
            return Ok(());
        }

        // Client mode: reconnect to the stored target address
        if let Some(addr) = self.target_addr {
            let new_stream = TcpStream::connect(addr).map_err(|e| {
                tracing::warn!("TCP client reconnect to {} failed: {}", addr, e);
                e
            })?;
            let timeout = Duration::from_millis(self.timeout_ms);
            new_stream.set_read_timeout(Some(timeout))?;
            new_stream.set_write_timeout(Some(timeout))?;

            *self.stream.lock().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "TCP stream mutex poisoned during reconnect",
                )
            })? = Some(new_stream);

            tracing::info!("TCP client reconnected to {}", addr);
            return Ok(());
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "No listener or target address for TCP reconnect",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_tcp_socket_framing() {
        // Skip if loopback interface is not available (e.g., in networkless containers)
        if std::net::TcpListener::bind("127.0.0.1:0").is_err() {
            eprintln!("Skipping TCP framing test: loopback unavailable");
            return;
        }

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

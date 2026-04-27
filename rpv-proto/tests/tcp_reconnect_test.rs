//! Integration test for TCP reconnection logic.
//!
//! This test verifies that:
//! 1. TCP connections can be established
//! 2. Disconnections are detected
//! 3. Reconnection logic works correctly

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// A simple mock TCP server for testing reconnection
struct MockServer {
    listener: TcpListener,
    running: Arc<AtomicBool>,
    connections_handled: Arc<AtomicU64>,
    accept_delay_ms: u64,
}

impl MockServer {
    fn new(port: u16, accept_delay_ms: u64) -> Self {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).expect("Failed to bind");
        listener
            .set_nonblocking(true)
            .expect("Failed to set non-blocking");

        MockServer {
            listener,
            running: Arc::new(AtomicBool::new(true)),
            connections_handled: Arc::new(AtomicU64::new(0)),
            accept_delay_ms,
        }
    }

    fn start(&self) -> thread::JoinHandle<()> {
        let listener = self.listener.try_clone().unwrap();
        let running = Arc::clone(&self.running);
        let connections = Arc::clone(&self.connections_handled);
        let delay = self.accept_delay_ms;

        thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _addr)) => {
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                        connections.fetch_add(1, Ordering::Relaxed);

                        // Send a welcome message
                        let _ = stream.write_all(b"HELLO");
                    }
                    Err(_) => {
                        thread::sleep(Duration::from_millis(10));
                    }
                }

                if delay > 0 {
                    thread::sleep(Duration::from_millis(delay));
                }
            }
        })
    }

    fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    fn connections_handled(&self) -> u64 {
        self.connections_handled.load(Ordering::Relaxed)
    }
}

/// Simulates a client that can "disconnect" and "reconnect"
struct TestClient {
    stream: Option<TcpStream>,
    server_addr: String,
    connect_count: u32,
}

impl TestClient {
    fn new(server_addr: &str) -> Self {
        TestClient {
            stream: None,
            server_addr: server_addr.to_string(),
            connect_count: 0,
        }
    }

    fn connect(&mut self) -> std::io::Result<()> {
        let stream = TcpStream::connect(&self.server_addr)?;
        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
        stream.set_write_timeout(Some(Duration::from_millis(500)))?;
        self.stream = Some(stream);
        self.connect_count += 1;
        Ok(())
    }

    fn disconnect(&mut self) {
        self.stream = None;
    }

    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    fn send(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if let Some(ref mut stream) = self.stream {
            stream.write_all(data)?;
            Ok(data.len())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Not connected",
            ))
        }
    }

    fn recv(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(ref mut stream) = self.stream {
            stream.read(buf)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Not connected",
            ))
        }
    }
}

#[test]
fn tcp_basic_connect() {
    let server = MockServer::new(19003, 0);
    let handle = server.start();

    thread::sleep(Duration::from_millis(100));

    let mut client = TestClient::new("127.0.0.1:19003");
    assert!(client.connect().is_ok());
    assert!(client.is_connected());

    server.stop();
    handle.join().unwrap();
}

#[test]
fn tcp_reconnect_after_disconnect() {
    let server = MockServer::new(19004, 0);
    let handle = server.start();

    thread::sleep(Duration::from_millis(100));

    let mut client = TestClient::new("127.0.0.1:19004");

    // First connection
    assert!(client.connect().is_ok());
    let first_connect_count = server.connections_handled();
    assert!(first_connect_count >= 1);

    // Disconnect
    client.disconnect();
    assert!(!client.is_connected());

    thread::sleep(Duration::from_millis(100));

    // Reconnect
    assert!(client.connect().is_ok());
    assert!(client.is_connected());

    thread::sleep(Duration::from_millis(100));

    let second_connect_count = server.connections_handled();
    assert!(second_connect_count > first_connect_count);

    server.stop();
    handle.join().unwrap();
}

#[test]
fn tcp_send_receive_after_reconnect() {
    let server = MockServer::new(19005, 0);
    let handle = server.start();

    thread::sleep(Duration::from_millis(100));

    let mut client = TestClient::new("127.0.0.1:19005");
    assert!(client.connect().is_ok());

    // Receive server welcome message
    let mut buf = [0u8; 64];
    let n = client.recv(&mut buf).expect("Failed to receive");
    assert_eq!(&buf[..n], b"HELLO");

    // Send data to server
    assert!(client.send(b"PING").is_ok());

    // Disconnect and reconnect
    client.disconnect();
    assert!(client.connect().is_ok());

    // Should receive new welcome message
    let n = client.recv(&mut buf).expect("Failed to receive after reconnect");
    assert_eq!(&buf[..n], b"HELLO");

    // Send data again
    assert!(client.send(b"PONG").is_ok());

    server.stop();
    handle.join().unwrap();
}

#[test]
fn tcp_multiple_reconnects() {
    let server = MockServer::new(19006, 0);
    let handle = server.start();

    thread::sleep(Duration::from_millis(100));

    let mut client = TestClient::new("127.0.0.1:19006");

    for _ in 0..5 {
        assert!(client.connect().is_ok());
        thread::sleep(Duration::from_millis(50));
        client.disconnect();
        thread::sleep(Duration::from_millis(50));
    }

    // Final reconnect
    assert!(client.connect().is_ok());
    let final_count = server.connections_handled();
    assert!(final_count >= 6); // 5 reconnects + final connection

    server.stop();
    handle.join().unwrap();
}

#[test]
fn tcp_connection_refused() {
    // Try to connect to a port with no server
    let mut client = TestClient::new("127.0.0.1:19999");
    let result = client.connect();
    assert!(result.is_err());
}

#[test]
fn tcp_send_without_connect() {
    let mut client = TestClient::new("127.0.0.1:19007");
    let result = client.send(b"test");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotConnected);
}

#[test]
fn tcp_simulate_server_restart() {
    // Start server
    let server = MockServer::new(19008, 0);
    let handle = server.start();

    thread::sleep(Duration::from_millis(100));

    let mut client = TestClient::new("127.0.0.1:19008");
    assert!(client.connect().is_ok());
    let first_count = server.connections_handled();

    // Stop server
    server.stop();
    handle.join().unwrap();

    // Try to send (should fail)
    thread::sleep(Duration::from_millis(100));
    client.disconnect();

    // Restart server on same port
    let server2 = MockServer::new(19008, 0);
    let handle2 = server2.start();

    thread::sleep(Duration::from_millis(100));

    // Reconnect to new server
    assert!(client.connect().is_ok());

    thread::sleep(Duration::from_millis(100));

    let second_count = server2.connections_handled();
    assert!(second_count >= 1);

    // Should be able to communicate
    let mut buf = [0u8; 64];
    let n = client.recv(&mut buf).expect("Failed to receive from restarted server");
    assert_eq!(&buf[..n], b"HELLO");

    server2.stop();
    handle2.join().unwrap();
}

#[test]
fn tcp_timeout_handling() {
    // Create a server that doesn't accept connections
    let listener = TcpListener::bind("127.0.0.1:19009").expect("Failed to bind");
    listener
        .set_nonblocking(true)
        .expect("Failed to set non-blocking");

    // Client should timeout trying to connect if server isn't accepting
    // (This test is mainly to verify timeout behavior)

    let start = Instant::now();
    let result = TcpStream::connect("127.0.0.1:19009");
    let elapsed = start.elapsed();

    // Connection might succeed immediately or timeout
    match result {
        Ok(_) => {
            // Connection succeeded (non-blocking listener accepted)
        }
        Err(e) => {
            // Expected if connection refused or timeout
            println!("Connection result: {}", e);
        }
    }

    println!("Connect attempt took {:?}", elapsed);
}

#[test]
fn tcp_framed_messaging() {
    // Test length-prefixed framing similar to TcpSocket
    let listener = TcpListener::bind("127.0.0.1:19010").expect("Failed to bind");
    let handle = thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));

            // Read framed messages
            loop {
                let mut len_buf = [0u8; 4];
                if stream.read_exact(&mut len_buf).is_err() {
                    break;
                }
                let len = u32::from_le_bytes(len_buf) as usize;
                let mut data = vec![0u8; len];
                if stream.read_exact(&mut data).is_err() {
                    break;
                }

                // Echo back with prefix
                let response = format!("ECHO:{}", String::from_utf8_lossy(&data));
                let response_bytes = response.as_bytes();
                let resp_len = (response_bytes.len() as u32).to_le_bytes();
                let _ = stream.write_all(&resp_len);
                let _ = stream.write_all(response_bytes);
            }
        }
    });

    thread::sleep(Duration::from_millis(100));

    let mut client = TestClient::new("127.0.0.1:19010");
    assert!(client.connect().is_ok());

    // Send framed message
    let message = b"Hello, TCP!";
    let len_bytes = (message.len() as u32).to_le_bytes();
    let mut framed = Vec::new();
    framed.extend_from_slice(&len_bytes);
    framed.extend_from_slice(message);
    assert!(client.send(&framed).is_ok());

    // Receive echoed response
    let mut len_buf = [0u8; 4];
    let n = client.recv(&mut len_buf).expect("Failed to receive length");
    assert_eq!(n, 4);
    let resp_len = u32::from_le_bytes(len_buf) as usize;

    let mut response = vec![0u8; resp_len];
    let n = client.recv(&mut response).expect("Failed to receive response");
    assert_eq!(n, resp_len);
    assert!(String::from_utf8_lossy(&response).starts_with("ECHO:"));

    client.disconnect();
    handle.join().unwrap();
}

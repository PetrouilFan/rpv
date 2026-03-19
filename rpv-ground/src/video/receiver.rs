use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{info, warn};

#[derive(Clone)]
pub struct VideoFrame {
    pub data: Vec<u8>,
}

pub struct VideoReceiver {
    socket: UdpSocket,
    tx: mpsc::UnboundedSender<VideoFrame>,
}

impl VideoReceiver {
    pub async fn new(port: u16, tx: mpsc::UnboundedSender<VideoFrame>) -> std::io::Result<Self> {
        let bind_addr = format!("0.0.0.0:{}", port);
        let socket = UdpSocket::bind(&bind_addr).await?;
        info!("Video receiver listening on {}", bind_addr);

        Ok(Self { socket, tx })
    }

    pub async fn run(&self) {
        let mut buf = vec![0u8; 65536];
        let mut packet_count = 0u64;

        loop {
            match self.socket.recv_from(&mut buf).await {
                Ok((len, _addr)) => {
                    if len < 4 {
                        continue;
                    }

                    // Try to parse as our custom protocol first
                    // Header: [4B frame_index][2B slice_index][2B slice_count][2B data_len][N data]
                    let is_custom = len >= 10 && {
                        // Check if the packet looks like our protocol
                        // The data_len field should be reasonable
                        let data_len = u16::from_le_bytes([buf[8], buf[9]]) as usize;
                        data_len > 0 && (10 + data_len) == len
                    };

                    let frame_data = if is_custom {
                        buf[10..len].to_vec()
                    } else {
                        // Raw H.264 data (from ffmpeg, etc.)
                        buf[..len].to_vec()
                    };

                    packet_count += 1;

                    let frame = VideoFrame {
                        data: frame_data,
                    };

                    let _ = self.tx.send(frame);
                }
                Err(e) => {
                    warn!("Video receive error: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                }
            }
        }
    }
}

use std::io::Write;
use std::process::{Command, Stdio};
use std::io::Read;
use tracing::{info, warn, error};

#[derive(Clone)]
pub struct DecodedYUV {
    pub y_data: Vec<u8>,
    pub u_data: Vec<u8>,
    pub v_data: Vec<u8>,
}

pub struct VideoDecoder {
    decoded_yuv: std::sync::Arc<std::sync::Mutex<Option<DecodedYUV>>>,
    width: u32,
    height: u32,
}

impl VideoDecoder {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            decoded_yuv: std::sync::Arc::new(std::sync::Mutex::new(None)),
            width,
            height,
        }
    }

    pub fn get_frame(&self) -> std::sync::Arc<std::sync::Mutex<Option<DecodedYUV>>> {
        std::sync::Arc::clone(&self.decoded_yuv)
    }

    pub fn spawn(
        &self,
        rx: tokio::sync::mpsc::UnboundedReceiver<crate::video::receiver::VideoFrame>,
    ) {
        let decoded = std::sync::Arc::clone(&self.decoded_yuv);
        let width = self.width;
        let height = self.height;

        std::thread::spawn(move || {
            decode_loop(decoded, rx, width, height);
        });
    }
}

fn decode_loop(
    decoded: std::sync::Arc<std::sync::Mutex<Option<DecodedYUV>>>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::video::receiver::VideoFrame>,
    width: u32,
    height: u32,
) {
    let y_size = (width * height) as usize;
    let uv_size = (width / 2 * height / 2) as usize;
    let total_size = y_size + uv_size * 2;

    loop {
        let child = Command::new("ffmpeg")
            .args(&[
                "-loglevel", "error",
                "-f", "h264",
                "-i",                 "pipe:0",
                "-f", "rawvideo",
                "-pix_fmt", "yuv420p",
                "-video_size", &format!("{}x{}", width, height),
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to spawn ffmpeg: {}", e);
                break;
            }
        };

        let mut stdin = child.stdin.take().expect("No stdin");
        let stdout = child.stdout.take().expect("No stdout");
        let mut stderr = child.stderr.take().expect("No stderr");

        info!("ffmpeg decoder started: {}x{} YUV420p", width, height);

        let decoded_clone = std::sync::Arc::clone(&decoded);
        let read_handle = std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut frame_buf = vec![0u8; total_size];
            let mut frame_count = 0u64;

            loop {
                match reader.read_exact(&mut frame_buf) {
                    Ok(()) => {
                        let y_data = frame_buf[0..y_size].to_vec();
                        let u_data = frame_buf[y_size..y_size + uv_size].to_vec();
                        let v_data = frame_buf[y_size + uv_size..].to_vec();

                        let mut f = decoded_clone.lock().unwrap();
                        *f = Some(DecodedYUV { y_data, u_data, v_data });
                        frame_count += 1;
                        if frame_count % 30 == 0 {
                            info!("Decoded {} frames (YUV420p)", frame_count);
                        }
                    }
                    Err(e) => {
                        error!("ffmpeg stdout read error after {} frames: {}", frame_count, e);
                        break;
                    }
                }
            }
            info!("Read thread exiting after {} frames", frame_count);
        });

        let mut closed = false;
        while let Some(frame) = rx.blocking_recv() {
            if stdin.write_all(&frame.data).is_err() {
                warn!("ffmpeg stdin write error");
                closed = true;
                break;
            }
        }

        drop(stdin);
        let _ = read_handle.join();

        // Read stderr
        let mut err_buf = Vec::new();
        let _ = stderr.read_to_end(&mut err_buf);
        if !err_buf.is_empty() {
            warn!("ffmpeg: {}", String::from_utf8_lossy(&err_buf));
        }

        let _ = child.kill();
        let _ = child.wait();

        if closed {
            break;
        }

        info!("Restarting ffmpeg decoder...");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    info!("Decoder thread exiting");
}

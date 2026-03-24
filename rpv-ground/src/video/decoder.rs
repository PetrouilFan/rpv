use std::io::{BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::time::Instant;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct DecodedFrame {
    pub nv12_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub send_ts_us: Option<u64>,
    pub recv_time: Option<Instant>,
}

pub struct VideoDecoder {
    frame_tx: crossbeam_channel::Sender<DecodedFrame>,
    frame_rx: crossbeam_channel::Receiver<DecodedFrame>,
    width: u32,
    height: u32,
}

impl VideoDecoder {
    pub fn new(width: u32, height: u32) -> Self {
        // Small queue for low latency
        let (frame_tx, frame_rx) = crossbeam_channel::bounded::<DecodedFrame>(4);
        Self {
            frame_tx,
            frame_rx,
            width,
            height,
        }
    }

    pub fn get_rx(&self) -> crossbeam_channel::Receiver<DecodedFrame> {
        self.frame_rx.clone()
    }

    pub fn spawn(&self, rx: tokio::sync::mpsc::Receiver<crate::video::receiver::VideoFrame>) {
        let frame_tx = self.frame_tx.clone();
        let width = self.width;
        let height = self.height;
        std::thread::spawn(move || {
            decode_loop(frame_tx, rx, width, height);
        });
    }
}

pub fn nv12_to_rgba(
    y_plane: &[u8],
    uv_plane: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    rgba: &mut [u8],
) {
    let mut i = 0;
    for row in 0..height {
        let uv_row = row / 2;
        for col in 0..width {
            let y_idx = row * stride + col;
            if y_idx >= y_plane.len() {
                break;
            }
            let y_val = y_plane[y_idx] as i32;

            // NV12: UV is interleaved, stride applies to UV plane too
            let uv_idx = uv_row * stride + (col & !1);
            if uv_idx + 1 >= uv_plane.len() {
                i += 4;
                continue;
            }
            let u_val = uv_plane[uv_idx] as i32 - 128;
            let v_val = uv_plane[uv_idx + 1] as i32 - 128;

            // BT.601 YUV to RGB conversion
            let c = y_val - 16;
            let r = ((298 * c + 409 * v_val + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * c - 100 * u_val - 208 * v_val + 128) >> 8).clamp(0, 255) as u8;
            let b = ((298 * c + 517 * u_val + 128) >> 8).clamp(0, 255) as u8;

            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
            i += 4;
        }
    }
}

fn decode_loop(
    frame_tx: crossbeam_channel::Sender<DecodedFrame>,
    mut rx: tokio::sync::mpsc::Receiver<crate::video::receiver::VideoFrame>,
    width: u32,
    height: u32,
) {
    // NV12 format: Y plane (width * height) + UV plane (width * height / 2)
    // Stride is typically aligned to 32 or 64 bytes on Pi hardware
    let stride = ((width + 31) / 32) * 32; // Align to 32 bytes
    let y_size = (stride * height) as usize;
    let uv_size = (stride * height / 2) as usize;
    let total_size = y_size + uv_size;

    info!(
        "H.264 decoder initialized: {}x{} stride={} NV12",
        width, height, stride
    );

    loop {
        // Try hardware decode first with NV12 output
        let hw_args = vec![
            "-loglevel",
            "error",
            "-hwaccel",
            "v4l2m2m",
            "-hwaccel_output_format",
            "nv12",
            "-fflags",
            "nobuffer",
            "-flags",
            "low_delay",
            "-thread_queue_size",
            "4096",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-threads",
            "2",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "nv12",
            "pipe:1",
        ];

        // Software fallback with NV12
        let sw_args = vec![
            "-loglevel",
            "error",
            "-fflags",
            "nobuffer",
            "-flags",
            "low_delay",
            "-thread_queue_size",
            "4096",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-threads",
            "2",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "nv12",
            "pipe:1",
        ];

        // Try hardware decode first
        let child = Command::new("ffmpeg")
            .args(&hw_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .or_else(|_| {
                warn!("Hardware decode failed, falling back to software");
                Command::new("ffmpeg")
                    .args(&sw_args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            });

        let mut child = match child {
            Ok(c) => {
                info!(
                    "FFmpeg decoder started: {}x{} NV12 (stride={})",
                    width, height, stride
                );
                c
            }
            Err(e) => {
                error!("Failed to spawn ffmpeg: {}", e);
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };

        let mut stdin = child.stdin.take().expect("No stdin");
        let stdout = child.stdout.take().expect("No stdout");
        let stderr = child.stderr.take().expect("No stderr");

        // Drain stderr in background
        let stderr_handle = std::thread::spawn(move || {
            let mut err_buf = Vec::new();
            let mut stderr_reader = BufReader::new(stderr);
            let _ = stderr_reader.read_to_end(&mut err_buf);
            if !err_buf.is_empty() {
                for line in String::from_utf8_lossy(&err_buf).lines() {
                    if !line.is_empty() {
                        warn!("ffmpeg: {}", line);
                    }
                }
            }
        });

        // Read decoded frames in background
        let frame_tx_clone = frame_tx.clone();
        let read_handle = std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut frame_buf = vec![0u8; total_size];
            let mut frame_count = 0u64;

            loop {
                match reader.read_exact(&mut frame_buf) {
                    Ok(()) => {
                        // Extract NV12 planes
                        let y_data = frame_buf[0..y_size].to_vec();
                        let uv_data = frame_buf[y_size..y_size + uv_size].to_vec();

                        let frame = DecodedFrame {
                            nv12_data: frame_buf.clone(),
                            width,
                            height,
                            stride,
                            send_ts_us: None,
                            recv_time: None,
                        };

                        // Non-blocking send - drop frame if queue full
                        if let Err(_) = frame_tx_clone.try_send(frame) {
                            // Queue full, drop frame for low latency
                        }

                        frame_count += 1;
                        if frame_count % 30 == 0 {
                            info!("Decoded {} frames (NV12)", frame_count);
                        }
                    }
                    Err(e) => {
                        error!(
                            "ffmpeg stdout read error after {} frames: {}",
                            frame_count, e
                        );
                        break;
                    }
                }
            }
            info!("Read thread exiting after {} frames", frame_count);
        });

        // Feed H.264 data to ffmpeg stdin
        loop {
            let frame = match rx.blocking_recv() {
                Some(f) => f,
                None => {
                    info!("Decoder input channel closed");
                    break;
                }
            };

            // Write immediately - no buffering delay
            if stdin.write_all(&frame.data).is_err() {
                warn!("ffmpeg stdin write error");
                break;
            }
        }

        // Cleanup
        drop(stdin);
        let _ = read_handle.join();
        let _ = stderr_handle.join();
        let _ = child.kill();
        let _ = child.wait();

        info!("Restarting ffmpeg decoder...");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    info!("Decoder thread exiting");
}

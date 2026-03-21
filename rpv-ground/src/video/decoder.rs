use std::io::{BufReader, BufWriter, Read, Write};
use std::process::{Command, Stdio};
use std::time::Instant;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct DecodedYUV {
    pub y_data: Vec<u8>,
    pub u_data: Vec<u8>,
    pub v_data: Vec<u8>,
    pub send_ts_us: Option<u64>,
    pub recv_time: Option<Instant>,
}

pub struct VideoDecoder {
    frame_tx: crossbeam_channel::Sender<DecodedYUV>,
    frame_rx: crossbeam_channel::Receiver<DecodedYUV>,
    width: u32,
    height: u32,
}

impl VideoDecoder {
    pub fn new(width: u32, height: u32) -> Self {
        let (frame_tx, frame_rx) = crossbeam_channel::bounded::<DecodedYUV>(1);
        Self {
            frame_tx,
            frame_rx,
            width,
            height,
        }
    }

    pub fn get_rx(&self) -> crossbeam_channel::Receiver<DecodedYUV> {
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

fn decode_loop(
    frame_tx: crossbeam_channel::Sender<DecodedYUV>,
    mut rx: tokio::sync::mpsc::Receiver<crate::video::receiver::VideoFrame>,
    width: u32,
    height: u32,
) {
    let y_size = (width * height) as usize;
    let uv_size = (width / 2 * height / 2) as usize;
    let total_size = y_size + uv_size * 2;

    let pending_ts: std::sync::Arc<std::sync::Mutex<Option<u64>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let pending_ts_clone = std::sync::Arc::clone(&pending_ts);
    let pending_recv: std::sync::Arc<std::sync::Mutex<Option<Instant>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let pending_recv_clone = std::sync::Arc::clone(&pending_recv);

    loop {
        // Create a FIFO for passing H.264 data to ffmpeg via shell pipe.
        // Direct Rust pipe doesn't work with ffmpeg's codec detection.
        let fifo_dir = std::env::temp_dir().join("rpv");
        let _ = std::fs::create_dir_all(&fifo_dir);
        let fifo_path = fifo_dir.join(format!("video_{}.fifo", std::process::id()));
        let _ = std::fs::remove_file(&fifo_path);
        let _ = std::process::Command::new("mkfifo")
            .arg(&fifo_path)
            .status();

        let fifo_path_clone = fifo_path.clone();
        let child = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "cat '{}' | /usr/bin/ffmpeg -loglevel error -flags low_delay \
                 -thread_queue_size 4096 -f h264 -i pipe:0 \
                 -f rawvideo -pix_fmt yuv420p -",
                fifo_path.display()
            ))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to spawn ffmpeg: {}", e);
                let _ = std::fs::remove_file(&fifo_path);
                break;
            }
        };

        let stdin = match std::fs::OpenOptions::new()
            .write(true)
            .open(&fifo_path_clone)
        {
            Ok(f) => BufWriter::new(f),
            Err(e) => {
                error!("Failed to open FIFO: {}", e);
                let _ = child.kill();
                let _ = std::fs::remove_file(&fifo_path_clone);
                break;
            }
        };
        let mut stdin = stdin;
        let stdout = child.stdout.take().expect("No stdout");
        let stderr = child.stderr.take().expect("No stderr");

        info!("ffmpeg decoder started: {}x{} YUV420p", width, height);

        // Drain stderr in background to prevent blocking
        let stderr_handle = std::thread::spawn(move || {
            let mut err_buf = Vec::new();
            let mut stderr_reader = BufReader::new(stderr);
            let _ = stderr_reader.read_to_end(&mut err_buf);
            if !err_buf.is_empty() {
                for line in String::from_utf8_lossy(&err_buf).lines() {
                    warn!("ffmpeg: {}", line);
                }
            }
        });

        let frame_tx_clone = frame_tx.clone();
        let ts_reader = std::sync::Arc::clone(&pending_ts_clone);
        let recv_reader = std::sync::Arc::clone(&pending_recv_clone);
        let read_handle = std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut frame_buf = vec![0u8; total_size];
            let mut frame_count = 0u64;
            let mut decode_latencies: Vec<f64> = Vec::new();

            loop {
                match reader.read_exact(&mut frame_buf) {
                    Ok(()) => {
                        let y_data = frame_buf[0..y_size].to_vec();
                        let u_data = frame_buf[y_size..y_size + uv_size].to_vec();
                        let v_data = frame_buf[y_size + uv_size..].to_vec();

                        let send_ts = *ts_reader.lock().unwrap();
                        let recv_t = *recv_reader.lock().unwrap();

                        let decode_ms = recv_t.map(|rt| rt.elapsed().as_micros() as f64 / 1000.0);

                        let yuv = DecodedYUV {
                            y_data,
                            u_data,
                            v_data,
                            send_ts_us: send_ts,
                            recv_time: recv_t,
                        };

                        let _ = frame_tx_clone.try_send(yuv);

                        if let Some(dl) = decode_ms {
                            decode_latencies.push(dl);
                        }

                        frame_count += 1;
                        if frame_count % 30 == 0 {
                            if !decode_latencies.is_empty() {
                                let avg = decode_latencies.iter().sum::<f64>()
                                    / decode_latencies.len() as f64;
                                decode_latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
                                let p50 = decode_latencies[decode_latencies.len() / 2];
                                let p95 = decode_latencies
                                    [(decode_latencies.len() as f64 * 0.95) as usize];
                                info!(
                                    "DECODE PIPELINE (n={}): avg={:.1}ms p50={:.1}ms p95={:.1}ms",
                                    decode_latencies.len(),
                                    avg,
                                    p50,
                                    p95
                                );
                                decode_latencies.clear();
                            }
                            info!("Decoded {} frames (YUV420p)", frame_count);
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

        let mut closed = false;
        let ts_writer = std::sync::Arc::clone(&pending_ts_clone);
        let recv_writer = std::sync::Arc::clone(&pending_recv_clone);
        let mut buf = Vec::with_capacity(1024 * 1024);
        let mut initial_flushed = false;

        loop {
            let frame = match rx.blocking_recv() {
                Some(f) => f,
                None => {
                    info!("Decoder input channel closed");
                    break;
                }
            };

            buf.extend_from_slice(&frame.data);

            // Drain queued frames to keep up
            for extra in std::iter::from_fn(|| rx.try_recv().ok()) {
                buf.extend_from_slice(&extra.data);
            }

            *ts_writer.lock().unwrap() = frame.send_ts_us;
            *recv_writer.lock().unwrap() = Some(frame.recv_time);

            // Initial buffer: accumulate enough data for ffmpeg codec detection
            let should_flush = if !initial_flushed {
                buf.len() >= 65536
            } else {
                true
            };

            if should_flush && !buf.is_empty() {
                if stdin.write_all(&buf).is_err() {
                    warn!("ffmpeg stdin write error");
                    closed = true;
                    break;
                }
                buf.clear();
                initial_flushed = true;
            }
        }

        drop(stdin);
        let _ = read_handle.join();
        let _ = stderr_handle.join();

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&fifo_path_clone);

        if closed {
            break;
        }

        info!("Restarting ffmpeg decoder...");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    info!("Decoder thread exiting");
}

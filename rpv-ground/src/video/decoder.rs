use std::io::{BufReader, BufWriter, Read, Write};
use std::process::{Command, Stdio};
use std::time::Instant;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct DecodedFrame {
    pub y_data: Vec<u8>,
    pub u_data: Vec<u8>,
    pub v_data: Vec<u8>,
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
        let (frame_tx, frame_rx) = crossbeam_channel::bounded::<DecodedFrame>(32);
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

fn decode_loop(
    frame_tx: crossbeam_channel::Sender<DecodedFrame>,
    mut rx: tokio::sync::mpsc::Receiver<crate::video::receiver::VideoFrame>,
    width: u32,
    height: u32,
) {
    let y_size = (width * height) as usize;
    let uv_size = (width / 2 * height / 2) as usize;
    let total_size = y_size + uv_size * 2;

    let (ts_tx, ts_rx) = crossbeam_channel::bounded::<(Option<u64>, Option<Instant>)>(8);

    loop {
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
                "cat '{}' | /usr/bin/ffmpeg -loglevel error \
                 -c:v h264_v4l2m2m -num_output_buffers 16 -num_capture_buffers 16 \
                 -f h264 -i pipe:0 \
                 -fflags nobuffer -f rawvideo -pix_fmt yuv420p -",
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

        let stdin =
            match open_fifo_with_timeout(&fifo_path_clone, std::time::Duration::from_secs(5)) {
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
        let ts_rx_clone = ts_rx.clone();
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

                        let (send_ts, recv_t) = ts_rx_clone.try_recv().unwrap_or((None, None));

                        let decode_ms = recv_t.map(|rt| rt.elapsed().as_micros() as f64 / 1000.0);

                        let yuv = DecodedFrame {
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

            for extra in std::iter::from_fn(|| rx.try_recv().ok()) {
                buf.extend_from_slice(&extra.data);
            }

            const MAX_BUF: usize = 4 * 1024 * 1024;
            if buf.len() > MAX_BUF {
                warn!(
                    "H.264 buffer exceeded {}MB, dropping old data",
                    MAX_BUF / 1024 / 1024
                );
                let drain_len = buf.len() - MAX_BUF;
                buf.drain(..drain_len);
            }

            let _ = ts_tx.try_send((frame.send_ts_us, Some(frame.recv_time)));

            let should_flush = if !initial_flushed {
                buf.len() >= 65536
            } else {
                true
            };

            if should_flush && !buf.is_empty() {
                if stdin.write_all(&buf).is_err() {
                    warn!("ffmpeg stdin write error");
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

        info!("Restarting ffmpeg decoder...");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    info!("Decoder thread exiting");
}

fn open_fifo_with_timeout(
    path: &std::path::Path,
    _timeout: std::time::Duration,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::io::FromRawFd;

    let c_path =
        std::ffi::CString::new(path.to_str().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid path")
        })?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

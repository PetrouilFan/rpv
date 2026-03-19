use std::io::{Read, Write, BufReader};
use std::net::UdpSocket;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const GROUND_IP: &str = "192.168.100.116";
const VIDEO_PORT: u16 = 5600;
const TELEMETRY_PORT: u16 = 5601;
const RC_PORT: u16 = 5602;

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    tracing::info!("rpv-cam starting on Raspberry Pi Zero 2W");

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        tracing::info!("Shutting down...");
        r.store(false, Ordering::SeqCst);
    }).ok();

    // Start video capture and streaming
    let video_running = running.clone();
    let video_handle = thread::spawn(move || {
        video_loop(video_running);
    });

    // Start RC receiver
    let rc_running = running.clone();
    let rc_handle = thread::spawn(move || {
        rc_receiver(rc_running);
    });

    // Start telemetry sender (dummy for now)
    let telem_running = running.clone();
    let telem_handle = thread::spawn(move || {
        telemetry_sender(telem_running);
    });

    video_handle.join().ok();
    rc_handle.join().ok();
    telem_handle.join().ok();

    tracing::info!("rpv-cam stopped");
}

fn video_loop(running: Arc<AtomicBool>) {
    let target_addr = format!("{}:{}", GROUND_IP, VIDEO_PORT);
    let socket = UdpSocket::bind("0.0.0.0:0").expect("Failed to bind video socket");
    tracing::info!("Video sender -> {}", target_addr);

    while running.load(Ordering::SeqCst) {
        tracing::info!("Starting rpicam-vid...");

        let child = Command::new("rpicam-vid")
            .args(&[
                "--width", "1280",
                "--height", "720",
                "--framerate", "30",
                "--codec", "h264",
                "--bitrate", "4000000",
                "--inline",
                "--nopreview",
                "-t", "0",
                "-o", "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to start rpicam-vid: {}", e);
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        let stdout = child.stdout.take().expect("No stdout");
        let mut reader = BufReader::new(stdout);

        // Read stderr in background
        let stderr = child.stderr.take();
        thread::spawn(move || {
            if let Some(mut stderr) = stderr {
                let mut buf = Vec::new();
                let _ = stderr.read_to_end(&mut buf);
            }
        });

        tracing::info!("rpicam-vid started, streaming H.264...");

        let mut buf = vec![0u8; 65536];
        let mut total_bytes = 0u64;

        while running.load(Ordering::SeqCst) {
            match reader.read(&mut buf) {
                Ok(0) => {
                    tracing::info!("rpicam-vid stdout closed");
                    break;
                }
                Ok(n) => {
                    // Send H.264 data over UDP
                    // Split into chunks if larger than MTU
                    let mut offset = 0;
                    while offset < n {
                        let chunk_size = (n - offset).min(1300);
                        let _ = socket.send_to(&buf[offset..offset + chunk_size], &target_addr);
                        offset += chunk_size;
                    }
                    total_bytes += n as u64;
                }
                Err(e) => {
                    tracing::error!("Read error: {}", e);
                    break;
                }
            }
        }

        let _ = child.kill();
        let _ = child.wait();

        tracing::info!(
            "rpicam-vid stopped, sent {:.1} MB total",
            total_bytes as f64 / 1_048_576.0
        );

        if running.load(Ordering::SeqCst) {
            tracing::info!("Restarting in 2 seconds...");
            thread::sleep(Duration::from_secs(2));
        }
    }
}

fn rc_receiver(running: Arc<AtomicBool>) {
    let bind_addr = format!("0.0.0.0:{}", RC_PORT);
    let socket = match UdpSocket::bind(&bind_addr) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Failed to bind RC socket on {}: {}", bind_addr, e);
            return;
        }
    };

    tracing::info!("RC receiver listening on {}", bind_addr);

    let mut buf = [0u8; 256];
    let mut rc_file_path = "/tmp/rpv_rc_channels";

    while running.load(Ordering::SeqCst) {
        match socket.recv_from(&mut buf) {
            Ok((len, _addr)) => {
                if len < 4 {
                    continue;
                }

                // Parse RC channels: [4B count][N*2B channels]
                let count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                let expected = 4 + count * 2;

                if len < expected {
                    continue;
                }

                let mut channels = Vec::with_capacity(count);
                for i in 0..count {
                    let offset = 4 + i * 2;
                    let ch = u16::from_le_bytes([buf[offset], buf[offset + 1]]);
                    channels.push(ch);
                }

                // Write channels to file for external integration
                let ch_str: Vec<String> = channels.iter().map(|c| c.to_string()).collect();
                let _ = std::fs::write(rc_file_path, ch_str.join(","));
            }
            Err(e) => {
                tracing::warn!("RC recv error: {}", e);
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn telemetry_sender(running: Arc<AtomicBool>) {
    let target_addr = format!("{}:{}", GROUND_IP, TELEMETRY_PORT);
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Failed to bind telemetry socket: {}", e);
            return;
        }
    };

    tracing::info!("Telemetry sender -> {}", target_addr);

    let mut interval = Duration::from_millis(200); // 5Hz

    while running.load(Ordering::SeqCst) {
        let telem = serde_json::json!({
            "lat": 0.0,
            "lon": 0.0,
            "alt": 0.0,
            "heading": 0.0,
            "speed": 0.0,
            "satellites": 0,
            "battery_v": 0.0,
            "battery_pct": 0,
            "mode": "UNKNOWN",
            "armed": false,
        });

        if let Ok(data) = serde_json::to_string(&telem) {
            let _ = socket.send_to(data.as_bytes(), &target_addr);
        }

        thread::sleep(interval);
    }
}

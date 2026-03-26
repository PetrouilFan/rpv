mod config;
mod link;
mod link_state;
mod rawsock;
mod telemetry;
mod video {
    pub mod decoder;
    pub mod receiver;
}
pub mod rc {
    pub mod joystick;
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use egui::{ColorImage, TextureHandle, Vec2};

use crate::config::Config;
use crate::link_state::{LinkStateHandle, LinkStatus};
use crate::rawsock::RawSocket;
use crate::telemetry::{Telemetry, TelemetryReceiver};
use crate::video::decoder::{DecodedFrame as DecodedYUV, VideoDecoder};
use crate::video::receiver::VideoReceiver;

pub struct AppState {
    pub texture: Option<TextureHandle>,
    pub last_frame_time: Instant,
    pub frame_count: u64,
    pub fps_timer: Instant,
    pub fps: f64,
    pub link_status: LinkStatus,
    pub link_state: LinkStateHandle,
    pub config: Config,
    pub telemetry: Arc<Mutex<Telemetry>>,
    pub running: Arc<AtomicBool>,
    pub rssi: Arc<Mutex<Option<i8>>>,
    pub channels: Arc<Mutex<Vec<u16>>>,
}

pub struct RpvApp {
    state: AppState,
    frame_rx: crossbeam_channel::Receiver<DecodedYUV>,
    rgba_buf: Vec<u8>,
    needs_repaint: bool,
    has_ever_had_frame: bool,
}

impl RpvApp {
    pub fn new(
        config: Config,
        frame_rx: crossbeam_channel::Receiver<DecodedYUV>,
        telemetry: Arc<Mutex<Telemetry>>,
        running: Arc<AtomicBool>,
        link_state: LinkStateHandle,
        rssi: Arc<Mutex<Option<i8>>>,
        channels: Arc<Mutex<Vec<u16>>>,
    ) -> Self {
        let w = config.video_width as usize;
        let h = config.video_height as usize;
        Self {
            state: AppState {
                texture: None,
                last_frame_time: Instant::now(),
                fps_timer: Instant::now(),
                frame_count: 0,
                fps: 0.0,
                link_status: LinkStatus::Searching,
                link_state,
                config,
                telemetry,
                running,
                rssi,
                channels,
            },
            frame_rx,
            rgba_buf: vec![0u8; w * h * 4],
            needs_repaint: false,
            has_ever_had_frame: false,
        }
    }

    fn update_texture(&mut self, ctx: &egui::Context) -> bool {
        let mut latest = None;
        let mut recv_count = 0;
        while let Ok(frame) = self.frame_rx.try_recv() {
            latest = Some(frame);
            recv_count += 1;
        }
        if recv_count > 1 {
            tracing::debug!("UI frame queue drain: {} frames dropped", recv_count - 1);
        }
        let frame_data = latest;

        let mut had_frame = false;
        if let Some(frame) = frame_data {
            let w = frame.width as usize;
            let h = frame.height as usize;
            let stride = frame.stride as usize;

            let y_size = stride * h;
            let uv_size = stride * h / 2;

            if frame.nv12_data.len() >= y_size + uv_size {
                let y_plane = &frame.nv12_data[0..y_size];
                let uv_plane = &frame.nv12_data[y_size..y_size + uv_size];

                crate::video::decoder::nv12_to_rgba(
                    y_plane,
                    uv_plane,
                    stride,
                    w,
                    h,
                    &mut self.rgba_buf,
                );

                let image = ColorImage::from_rgba_unmultiplied([w, h], &self.rgba_buf);

                if let Some(ref mut tex) = self.state.texture {
                    tex.set(image, egui::TextureOptions::LINEAR);
                } else {
                    self.state.texture =
                        Some(ctx.load_texture("video", image, egui::TextureOptions::LINEAR));
                }

                self.state.frame_count += 1;
                self.state.last_frame_time = Instant::now();

                if self.state.frame_count == 60 {
                    self.state.fps = 60.0 / self.state.fps_timer.elapsed().as_secs_f64();
                    self.state.frame_count = 0;
                    self.state.fps_timer = Instant::now();
                }

                // Notify link state machine that video is flowing.
                // This only transitions Searching->Connected, never overrides SignalLost.
                self.state.link_state.video_frame_decoded();
                self.has_ever_had_frame = true;
                had_frame = true;
            }
        }
        had_frame
    }
}

impl eframe::App for RpvApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.state.running.load(Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        let had_frame = self.update_texture(ctx);
        self.needs_repaint = had_frame;

        if !self.has_ever_had_frame {
            let telem = self.state.telemetry.lock().unwrap();
            if !telem.camera_ok && self.state.link_status != LinkStatus::NoCamera {
                self.state.link_state.camera_unavailable();
                self.needs_repaint = true;
            } else if telem.camera_ok && self.state.link_status == LinkStatus::NoCamera {
                self.state.link_state.camera_available();
                self.needs_repaint = true;
            }
        }

        // Read authoritative link state from the state machine.
        let new_status = self.state.link_state.get();
        if new_status != self.state.link_status {
            self.state.link_status = new_status;
            self.needs_repaint = true;
        }

        if self.needs_repaint {
            ctx.request_repaint();
        } else if self.state.link_status == LinkStatus::Connected {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(egui::Color32::BLACK)
                    .inner_margin(0.0),
            )
            .show(ctx, |ui| {
                let available = ui.available_size();

                if let Some(ref tex) = self.state.texture {
                    let tex_size = Vec2::new(
                        self.state.config.video_width as f32,
                        self.state.config.video_height as f32,
                    );
                    let scale_x = available.x / tex_size.x;
                    let scale_y = available.y / tex_size.y;
                    let scale = scale_x.min(scale_y);
                    let display_size = tex_size * scale;

                    let rect = egui::Rect::from_center_size(
                        ui.available_rect_before_wrap().center(),
                        display_size,
                    );

                    ui.painter().rect_filled(
                        ui.available_rect_before_wrap(),
                        0.0,
                        egui::Color32::BLACK,
                    );

                    egui::Image::from_texture(tex).paint_at(ui, rect);
                } else {
                    ui.painter().rect_filled(
                        ui.available_rect_before_wrap(),
                        0.0,
                        egui::Color32::from_gray(20),
                    );
                    let (wait_text, wait_color) = match self.state.link_status {
                        LinkStatus::Searching => ("Searching for camera...", egui::Color32::YELLOW),
                        LinkStatus::SignalLost => {
                            ("Signal lost — reconnecting...", egui::Color32::RED)
                        }
                        LinkStatus::Connected => ("Waiting for video...", egui::Color32::GRAY),
                        LinkStatus::NoCamera => {
                            ("No camera detected", egui::Color32::from_rgb(255, 160, 0))
                        }
                    };
                    ui.centered_and_justified(|ui| {
                        ui.label(egui::RichText::new(wait_text).size(32.0).color(wait_color));
                    });
                }

                draw_osd(ui, &self.state);
            });
    }
}

fn draw_osd(ui: &mut egui::Ui, state: &AppState) {
    let screen = ui.available_rect_before_wrap();

    let telem = state.telemetry.lock().unwrap().clone();

    egui::Area::new(egui::Id::new("osd_top_left"))
        .fixed_pos(egui::pos2(10.0, 10.0))
        .show(ui.ctx(), |ui| {
            ui.vertical(|ui| {
                let (label, color, show_dot) = match state.link_status {
                    LinkStatus::Connected => ("LINK OK", egui::Color32::GREEN, true),
                    LinkStatus::Searching => ("SEARCHING", egui::Color32::YELLOW, false),
                    LinkStatus::SignalLost => ("SIGNAL LOST", egui::Color32::RED, false),
                    LinkStatus::NoCamera => {
                        ("NO CAMERA", egui::Color32::from_rgb(255, 160, 0), false)
                    }
                };

                ui.horizontal(|ui| {
                    if show_dot {
                        let dot_size = 10.0;
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(dot_size, dot_size),
                            egui::Sense::hover(),
                        );
                        ui.painter()
                            .circle_filled(rect.center(), dot_size / 2.0, color);
                    } else {
                        let t = ui.ctx().input(|i| i.time);
                        let blink = (t * 2.0) as u64 % 2 == 0;
                        if blink {
                            let dot_size = 10.0;
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(dot_size, dot_size),
                                egui::Sense::hover(),
                            );
                            ui.painter()
                                .circle_filled(rect.center(), dot_size / 2.0, color);
                        } else {
                            ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                        }
                    }
                    ui.label(egui::RichText::new(label).size(14.0).color(color));
                });

                let fps_color = match state.link_status {
                    LinkStatus::Connected => egui::Color32::from_gray(200),
                    _ => egui::Color32::from_gray(100),
                };
                ui.label(
                    egui::RichText::new(format!("FPS: {:.1}", state.fps))
                        .size(12.0)
                        .color(fps_color),
                );

                // RSSI display
                let rssi_val = state.rssi.lock().unwrap().clone();
                if let Some(rssi_dbm) = rssi_val {
                    let (rssi_text, rssi_color) = if rssi_dbm >= -50 {
                        (
                            format!("SIG: {} dBm (excellent)", rssi_dbm),
                            egui::Color32::GREEN,
                        )
                    } else if rssi_dbm >= -70 {
                        (
                            format!("SIG: {} dBm (good)", rssi_dbm),
                            egui::Color32::from_rgb(100, 255, 100),
                        )
                    } else if rssi_dbm >= -80 {
                        (
                            format!("SIG: {} dBm (weak)", rssi_dbm),
                            egui::Color32::YELLOW,
                        )
                    } else {
                        (format!("SIG: {} dBm (poor)", rssi_dbm), egui::Color32::RED)
                    };
                    ui.label(egui::RichText::new(rssi_text).size(12.0).color(rssi_color));
                }
            });
        });

    egui::Area::new(egui::Id::new("osd_top_right"))
        .fixed_pos(egui::pos2(screen.max.x - 170.0, 10.0))
        .show(ui.ctx(), |ui| {
            ui.vertical(|ui| {
                let pct = telem.battery_pct as f32;
                let bar_color = if pct > 30.0 {
                    egui::Color32::from_rgb(0, 200, 0)
                } else if pct > 15.0 {
                    egui::Color32::YELLOW
                } else {
                    egui::Color32::RED
                };

                let bar_width = 120.0;
                let bar_height = 14.0;
                let rect =
                    egui::Rect::from_min_size(ui.cursor().min, egui::vec2(bar_width, bar_height));
                ui.advance_cursor_after_rect(rect);

                ui.painter()
                    .rect_filled(rect, 2.0, egui::Color32::from_gray(40));
                let fill_width = (bar_width * pct / 100.0).max(0.0);
                let fill_rect =
                    egui::Rect::from_min_size(rect.min, egui::vec2(fill_width, bar_height));
                ui.painter().rect_filled(fill_rect, 2.0, bar_color);

                ui.label(
                    egui::RichText::new(format!("{:.1}V  {}%", telem.battery_v, telem.battery_pct))
                        .size(12.0)
                        .color(egui::Color32::WHITE),
                );

                ui.label(
                    egui::RichText::new(format!("MODE: {}", telem.mode))
                        .size(12.0)
                        .color(if telem.armed {
                            egui::Color32::from_rgb(255, 100, 100)
                        } else {
                            egui::Color32::from_rgb(100, 255, 100)
                        }),
                );
            });
        });

    egui::Area::new(egui::Id::new("osd_bottom_left"))
        .fixed_pos(egui::pos2(10.0, screen.max.y - 70.0))
        .show(ui.ctx(), |ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(format!("SPD: {:.1} m/s", telem.speed))
                        .size(16.0)
                        .color(egui::Color32::WHITE),
                );
                ui.label(
                    egui::RichText::new(format!("ALT: {:.1} m", telem.alt))
                        .size(16.0)
                        .color(egui::Color32::WHITE),
                );
            });
        });

    egui::Area::new(egui::Id::new("osd_bottom_right"))
        .fixed_pos(egui::pos2(screen.max.x - 210.0, screen.max.y - 70.0))
        .show(ui.ctx(), |ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(format!("HDG: {:.0}deg", telem.heading))
                        .size(16.0)
                        .color(egui::Color32::WHITE),
                );
                ui.label(
                    egui::RichText::new(format!("SAT: {}", telem.satellites))
                        .size(14.0)
                        .color(egui::Color32::WHITE),
                );
                ui.label(
                    egui::RichText::new(format!("{:.6}, {:.6}", telem.lat, telem.lon))
                        .size(11.0)
                        .color(egui::Color32::from_gray(180)),
                );
            });
        });

    // Joystick RC channels OSD (middle-bottom)
    let channels = state.channels.lock().unwrap().clone();
    egui::Area::new(egui::Id::new("osd_joystick"))
        .fixed_pos(egui::pos2(screen.center().x - 150.0, screen.max.y - 90.0))
        .show(ui.ctx(), |ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new("RC").size(12.0).color(egui::Color32::YELLOW),
                );
                ui.horizontal(|ui| {
                    for (_i, ch) in channels.iter().enumerate().take(4) {
                        let normalized = (*ch as f32 - 1000.0) / 1000.0;
                        let bar_height = 30.0;
                        let bar_width = 20.0;
                        let rect = egui::Rect::from_min_size(
                            ui.cursor().min,
                            egui::vec2(bar_width, bar_height),
                        );
                        ui.advance_cursor_after_rect(egui::Rect::from_min_max(
                            rect.min,
                            egui::pos2(rect.max.x + 4.0, rect.max.y),
                        ));
                        ui.painter()
                            .rect_filled(rect, 2.0, egui::Color32::from_gray(40));
                        let fill_h = bar_height * normalized.clamp(0.0, 1.0);
                        let fill_rect = egui::Rect::from_min_size(
                            egui::pos2(rect.min.x, rect.max.y - fill_h),
                            egui::vec2(bar_width, fill_h),
                        );
                        ui.painter().rect_filled(fill_rect, 2.0, egui::Color32::GREEN);
                    }
                });
            });
        });
}

fn main() -> Result<(), eframe::Error> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    tracing::info!("rpv ground station starting (monitor mode)");

    let (config, was_default) = Config::load();
    tracing::info!("Config: {:?}", config);

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        tracing::info!("Ctrl+C received, shutting down...");
        r.store(false, Ordering::SeqCst);
    })
    .expect("Failed to set ctrl+c handler");

    // Open raw AF_PACKET socket on the configured interface (must be in monitor mode)
    let socket = match RawSocket::new(&config.interface) {
        Ok(s) => {
            tracing::info!("Raw socket bound to {} (monitor mode)", config.interface);
            Arc::new(s)
        }
        Err(e) => {
            tracing::error!("Failed to open raw socket on {}: {}", config.interface, e);
            tracing::error!(
                "Make sure the interface is in monitor mode: iw dev {} set type monitor",
                config.interface
            );
            std::process::exit(1);
        }
    };

    // Shared link state machine
    let link_state = LinkStateHandle::new();

    // Shared RSSI from camera (dBm, updated by RX dispatcher)
    let rssi_shared: Arc<Mutex<Option<i8>>> = Arc::new(Mutex::new(None));

    // Channel for video NAL data: RX dispatcher -> VideoReceiver
    let (video_payload_tx, video_payload_rx) = crossbeam_channel::bounded::<Vec<u8>>(1024);
    // Channel for decoded video frames: VideoReceiver -> VideoDecoder
    // Bounded(4) + blocking send forces FEC thread to pace the pipeline naturally
    let (video_frame_tx, video_frame_rx_decoder) = crossbeam_channel::bounded::<Vec<u8>>(4);
    // Channel for telemetry JSON: RX dispatcher -> TelemetryReceiver
    let (telem_payload_tx, telem_payload_rx) = crossbeam_channel::bounded::<Vec<u8>>(16);

    // Create the decoder pipeline
    let decoder = VideoDecoder::new(config.video_width, config.video_height);
    let ui_frame_rx = decoder.get_rx();
    decoder.spawn(video_frame_rx_decoder);

    // Create telemetry receiver
    let telemetry = TelemetryReceiver::new(link_state.clone(), telem_payload_rx);
    let telemetry_state = telemetry.get_state();

    if was_default {
        config.save();
    }

    // ---- Background threads ----

    let last_heartbeat: Arc<Mutex<Instant>> = Arc::new(Mutex::new(Instant::now()));

    // RX dispatcher: single reader from raw socket, strips Radiotap, dispatches by L2 type
    let rx_running = running.clone();
    let rx_socket = Arc::clone(&socket);
    let rx_video_tx = video_payload_tx;
    let rx_telem_tx = telem_payload_tx;
    let rx_drone_id = config.drone_id;
    let rx_last_hb = Arc::clone(&last_heartbeat);
    let rx_rssi = Arc::clone(&rssi_shared);
    let _rx_handle = std::thread::spawn(move || {
        rx_dispatcher(
            rx_running,
            rx_socket,
            rx_drone_id,
            rx_video_tx,
            rx_telem_tx,
            rx_last_hb,
            rx_rssi,
        );
    });

    // Video receiver thread: FEC reassembly from video payload channel
    let vr = VideoReceiver::new(video_frame_tx, video_payload_rx);
    let _vr_handle = std::thread::spawn(move || {
        vr.run();
    });

    // Telemetry receiver thread
    let _telem_handle = std::thread::spawn(move || {
        telemetry.run();
    });

    // RC joystick thread
    let rc_socket = Arc::clone(&socket);
    let rc_drone_id = config.drone_id;
    let rc_running = running.clone();
    let mut rc = crate::rc::joystick::RCTx::new(rc_socket, rc_drone_id, rc_running);
    let channels_shared = rc.channels();
    let _rc_handle = std::thread::spawn(move || {
        rc.run();
    });

    // Heartbeat sender thread
    let hb_running = running.clone();
    let hb_socket = Arc::clone(&socket);
    let hb_drone_id = config.drone_id;
    let _hb_handle = std::thread::spawn(move || {
        heartbeat_sender(hb_running, hb_socket, hb_drone_id);
    });

    // Heartbeat monitor thread: detects when camera stops sending heartbeats
    let hm_running = running.clone();
    let hm_last = Arc::clone(&last_heartbeat);
    let hm_link_state = link_state.clone();
    let _hm_handle = std::thread::spawn(move || {
        heartbeat_monitor(hm_running, hm_last, hm_link_state);
    });

    // Run egui on main thread with custom wgpu limits for Pi 5 V3D GPU
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_fullscreen(true)
            .with_title("rpv ground station"),
        wgpu_options: egui_wgpu::WgpuConfiguration {
            present_mode: wgpu::PresentMode::Mailbox,
            device_descriptor: std::sync::Arc::new(|_adapter| {
                let limits = wgpu::Limits {
                    max_texture_dimension_1d: 4096,
                    max_texture_dimension_2d: 4096,
                    max_texture_dimension_3d: 2048,
                    ..wgpu::Limits::default()
                };
                wgpu::DeviceDescriptor {
                    label: Some("rpv device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: limits,
                    ..Default::default()
                }
            }),
            ..Default::default()
        },
        ..Default::default()
    };

    let app_running = running.clone();
    eframe::run_native(
        "rpv ground station",
        native_options,
        Box::new(|_cc| {
            Ok(Box::new(RpvApp::new(
                config,
                ui_frame_rx,
                telemetry_state,
                app_running,
                link_state,
                rssi_shared,
                channels_shared,
            )))
        }),
    )
}

/// Single raw socket RX dispatcher.
/// Reads all incoming frames, strips Radiotap, filters by L2 magic+drone_id,
/// then dispatches by payload type to the appropriate channels.
fn rx_dispatcher(
    running: Arc<AtomicBool>,
    socket: Arc<RawSocket>,
    drone_id: u8,
    video_tx: crossbeam_channel::Sender<Vec<u8>>,
    telem_tx: crossbeam_channel::Sender<Vec<u8>>,
    last_heartbeat: Arc<Mutex<Instant>>,
    rssi: Arc<Mutex<Option<i8>>>,
) {
    tracing::info!("RX dispatcher started (raw socket)");
    let mut buf = vec![0u8; 65536];
    let mut reject_count: u64 = 0;

    while running.load(Ordering::SeqCst) {
        let len = match socket.recv(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("RX recv error: {}", e);
                continue;
            }
        };

        // Strip Radiotap + 802.11 header, extract RSSI
        let (payload, frame_rssi) = match rawsock::recv_extract(&buf[..len], reject_count < 10) {
            Some(p) => p,
            None => {
                reject_count += 1;
                if reject_count <= 5 {
                    tracing::debug!(
                        "RX: rejected frame ({}B), first 8 bytes: {:02x?}",
                        len,
                        &buf[..8.min(len)]
                    );
                }
                continue;
            }
        };

        // Update RSSI if available
        if let Some(rssi_val) = frame_rssi {
            *rssi.lock().unwrap() = Some(rssi_val);
        }

        // Check magic and drone_id
        if !link::L2Header::matches_magic(payload) {
            reject_count += 1;
            if reject_count <= 5 {
                tracing::debug!(
                    "RX: magic mismatch, payload first 8 bytes: {:02x?}",
                    &payload[..8.min(payload.len())]
                );
            }
            continue;
        }
        let (header, data) = match link::L2Header::decode(payload) {
            Some(h) => h,
            None => continue,
        };

        if header.drone_id != drone_id {
            continue; // different swarm
        }

        match header.payload_type {
            link::PAYLOAD_VIDEO => {
                if video_tx.try_send(data.to_vec()).is_err() {
                    tracing::warn!("Video queue dropped (backpressure)");
                }
            }
            link::PAYLOAD_TELEMETRY => {
                if telem_tx.try_send(data.to_vec()).is_err() {
                    tracing::warn!("Telemetry queue dropped");
                }
            }
            link::PAYLOAD_HEARTBEAT => {
                *last_heartbeat.lock().unwrap() = Instant::now();
            }
            _ => {}
        }
    }

    tracing::info!("RX dispatcher stopped");
}

/// Heartbeat sender — sends heartbeat packets via raw socket at 10Hz.
fn heartbeat_sender(running: Arc<AtomicBool>, socket: Arc<RawSocket>, drone_id: u8) {
    tracing::info!("Heartbeat sender ready (L2 broadcast, 10Hz)");
    let mut l2_seq: u32 = 0;
    let mut payload_buf: Vec<u8> = Vec::with_capacity(19);
    let mut l2_buf: Vec<u8> = Vec::with_capacity(link::HEADER_LEN + 19);
    let mut send_buf: Vec<u8> = Vec::with_capacity(8 + 24 + link::HEADER_LEN + 19);

    while running.load(Ordering::SeqCst) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // Heartbeat payload: [7B "rpv-bea"][4B seq][8B timestamp]
        payload_buf.clear();
        payload_buf.extend_from_slice(b"rpv-bea");
        payload_buf.extend_from_slice(&l2_seq.to_le_bytes());
        payload_buf.extend_from_slice(&ts.to_le_bytes());

        let header = link::L2Header {
            drone_id,
            payload_type: link::PAYLOAD_HEARTBEAT,
            seq: l2_seq,
        };
        header.encode_into(&payload_buf, &mut l2_buf);
        let _ = socket.send_with_buf(&l2_buf, &mut send_buf);

        l2_seq = l2_seq.wrapping_add(1);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Heartbeat monitor — detects when camera stops sending heartbeats.
/// This is the PRIMARY link liveness source. Only heartbeat transitions
/// can override SignalLost back to Connected.
fn heartbeat_monitor(
    running: Arc<AtomicBool>,
    last_heartbeat: Arc<Mutex<Instant>>,
    link_state: LinkStateHandle,
) {
    tracing::info!("Heartbeat monitor started (timeout: 0.5s)");
    std::thread::sleep(std::time::Duration::from_secs(1)); // initial grace period

    let mut ever_connected = false;

    while running.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(100));

        let elapsed = last_heartbeat.lock().unwrap().elapsed();
        if elapsed > std::time::Duration::from_millis(500) {
            if ever_connected {
                link_state.heartbeat_lost();
            }
        } else {
            ever_connected = true;
            link_state.heartbeat_restored();
        }
    }
}

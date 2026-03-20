mod config;
mod discovery;
mod telemetry;
mod video {
    pub mod receiver;
    pub mod decoder;
}
mod rc {
    pub mod joystick;
}

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use egui::{ColorImage, TextureHandle, Vec2};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::telemetry::{Telemetry, TelemetryReceiver};
use crate::video::receiver::VideoReceiver;
use crate::video::decoder::{VideoDecoder, DecodedYUV};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkStatus {
    Searching,
    Connected,
    SignalLost,
    NoCamera,
}

pub struct AppState {
    pub texture: Option<TextureHandle>,
    pub last_frame_time: Instant,
    pub frame_count: u64,
    pub fps_timer: Instant,
    pub fps: f64,
    pub link_status: LinkStatus,
    pub link_status_shared: Arc<Mutex<LinkStatus>>,
    pub config: Config,
    pub telemetry: Arc<Mutex<Telemetry>>,
    pub running: Arc<AtomicBool>,
}

pub struct RpvApp {
    state: AppState,
    video_rx: Arc<Mutex<Option<DecodedYUV>>>,
    rgba_buf: Vec<u8>,
    needs_repaint: bool,
    has_ever_had_frame: bool,
}

impl RpvApp {
    pub fn new(
        config: Config,
        video_rx: Arc<Mutex<Option<DecodedYUV>>>,
        telemetry: Arc<Mutex<Telemetry>>,
        running: Arc<AtomicBool>,
        link_status_shared: Arc<Mutex<LinkStatus>>,
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
                link_status_shared,
                config,
                telemetry,
                running,
            },
            video_rx,
            rgba_buf: vec![0u8; w * h * 4],
            needs_repaint: false,
            has_ever_had_frame: false,
        }
    }

    fn update_texture(&mut self, ctx: &egui::Context) -> bool {
        let frame_data = {
            let mut lock = self.video_rx.lock().unwrap();
            lock.take()
        };

        let mut had_frame = false;
        if let Some(yuv) = frame_data {
            let w = self.state.config.video_width as usize;
            let h = self.state.config.video_height as usize;

            if yuv.y_data.len() == w * h
                && yuv.u_data.len() == (w / 2) * (h / 2)
                && yuv.v_data.len() == (w / 2) * (h / 2)
            {
                yuv420p_to_rgba_inplace(&yuv.y_data, &yuv.u_data, &yuv.v_data, w, h, &mut self.rgba_buf);
                let image = ColorImage::from_rgba_unmultiplied([w, h], &self.rgba_buf);

                if let Some(ref mut tex) = self.state.texture {
                    tex.set(image, egui::TextureOptions::default());
                } else {
                    self.state.texture = Some(ctx.load_texture(
                        "video",
                        image,
                        egui::TextureOptions::default(),
                    ));
                }

                self.state.frame_count += 1;
                self.state.last_frame_time = Instant::now();

                if self.state.frame_count == 30 {
                    self.state.fps = 30.0 / self.state.fps_timer.elapsed().as_secs_f64();
                    self.state.frame_count = 0;
                    self.state.fps_timer = Instant::now();
                }

                if self.state.link_status != LinkStatus::Connected {
                    self.state.link_status = LinkStatus::Connected;
                    tracing::info!("Video: decoded frame received, link status = Connected");
                }
                self.has_ever_had_frame = true;
                had_frame = true;
            }
        }
        had_frame
    }
}

fn yuv420p_to_rgba_inplace(y: &[u8], u: &[u8], v: &[u8], w: usize, h: usize, rgba: &mut [u8]) {

    let mut y_idx = 0usize;
    let mut uv_idx = 0usize;
    let mut rgba_idx = 0usize;

    for _ in 0..h {
        for _ in 0..w {
            let y_val = y[y_idx] as i32;
            let u_val = u[uv_idx] as i32 - 128;
            let v_val = v[uv_idx] as i32 - 128;

            rgba[rgba_idx] = (y_val + ((359 * v_val) >> 8)).clamp(0, 255) as u8;
            rgba[rgba_idx + 1] = (y_val - ((88 * u_val + 183 * v_val) >> 8)).clamp(0, 255) as u8;
            rgba[rgba_idx + 2] = (y_val + ((454 * u_val) >> 8)).clamp(0, 255) as u8;
            rgba[rgba_idx + 3] = 255;

            y_idx += 1;
            rgba_idx += 4;

            if (y_idx % w) == 0 {
                uv_idx += 1;
            }
        }
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

        // Detect "no camera module" state
        if !self.has_ever_had_frame {
            let telem = self.state.telemetry.lock().unwrap();
            if !telem.camera_ok && self.state.link_status != LinkStatus::NoCamera {
                self.state.link_status = LinkStatus::NoCamera;
                self.needs_repaint = true;
            } else if telem.camera_ok && self.state.link_status == LinkStatus::NoCamera {
                self.state.link_status = LinkStatus::Searching;
                self.needs_repaint = true;
            }
        }

        if self.state.link_status == LinkStatus::Connected
            && self.state.last_frame_time.elapsed().as_secs() >= 2
        {
            self.state.link_status = LinkStatus::SignalLost;
            self.needs_repaint = true;
        }

        if let Ok(shared) = self.state.link_status_shared.lock() {
            if *shared == LinkStatus::Connected && self.state.link_status == LinkStatus::Searching {
                self.state.link_status = LinkStatus::Connected;
                self.needs_repaint = true;
            }
        }

        if self.needs_repaint || self.state.link_status != LinkStatus::Connected {
            ctx.request_repaint();
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(egui::Color32::BLACK).inner_margin(0.0))
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

                    egui::Image::from_texture(tex)
                        .paint_at(ui, rect);
                } else {
                    ui.painter().rect_filled(
                        ui.available_rect_before_wrap(),
                        0.0,
                        egui::Color32::from_gray(20),
                    );
                    let (wait_text, wait_color) = match self.state.link_status {
                        LinkStatus::Searching => ("Searching for camera...", egui::Color32::YELLOW),
                        LinkStatus::SignalLost => ("Signal lost — reconnecting...", egui::Color32::RED),
                        LinkStatus::Connected => ("Waiting for video...", egui::Color32::GRAY),
                        LinkStatus::NoCamera => ("No camera detected", egui::Color32::from_rgb(255, 160, 0)),
                    };
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new(wait_text)
                                .size(32.0)
                                .color(wait_color),
                        );
                    });
                }

                draw_osd(ui, &self.state);
            });
    }
}

fn draw_osd(ui: &mut egui::Ui, state: &AppState) {
    let screen = ui.available_rect_before_wrap();

    egui::Area::new(egui::Id::new("osd_top_left"))
        .fixed_pos(egui::pos2(10.0, 10.0))
        .show(ui.ctx(), |ui| {
            ui.vertical(|ui| {
                let (label, color, show_dot) = match state.link_status {
                    LinkStatus::Connected => ("LINK OK", egui::Color32::GREEN, true),
                    LinkStatus::Searching => ("SEARCHING", egui::Color32::YELLOW, false),
                    LinkStatus::SignalLost => ("SIGNAL LOST", egui::Color32::RED, false),
                    LinkStatus::NoCamera => ("NO CAMERA", egui::Color32::from_rgb(255, 160, 0), false),
                };

                // Status dot + text row
                ui.horizontal(|ui| {
                    if show_dot {
                        let dot_size = 10.0;
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(dot_size, dot_size),
                            egui::Sense::hover(),
                        );
                        ui.painter().circle_filled(rect.center(), dot_size / 2.0, color);
                    } else {
                        // Blinking dot for searching/lost
                        let ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis();
                        let blink = (ms / 500) % 2 == 0;
                        if blink {
                            let dot_size = 10.0;
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(dot_size, dot_size),
                                egui::Sense::hover(),
                            );
                            ui.painter().circle_filled(rect.center(), dot_size / 2.0, color);
                        } else {
                            ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                        }
                    }
                    ui.label(
                        egui::RichText::new(label)
                            .size(14.0)
                            .color(color),
                    );
                });

                // FPS line
                let fps_color = match state.link_status {
                    LinkStatus::Connected => egui::Color32::from_gray(200),
                    _ => egui::Color32::from_gray(100),
                };
                ui.label(
                    egui::RichText::new(format!("FPS: {:.1}", state.fps))
                        .size(12.0)
                        .color(fps_color),
                );
            });
        });

    egui::Area::new(egui::Id::new("osd_top_right"))
        .fixed_pos(egui::pos2(screen.max.x - 170.0, 10.0))
        .show(ui.ctx(), |ui| {
            let telem = state.telemetry.lock().unwrap();
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
                let rect = egui::Rect::from_min_size(
                    ui.cursor().min,
                    egui::vec2(bar_width, bar_height),
                );
                ui.advance_cursor_after_rect(rect);

                ui.painter().rect_filled(rect, 2.0, egui::Color32::from_gray(40));
                let fill_width = (bar_width * pct / 100.0).max(0.0);
                let fill_rect = egui::Rect::from_min_size(
                    rect.min,
                    egui::vec2(fill_width, bar_height),
                );
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
            let telem = state.telemetry.lock().unwrap();
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
            let telem = state.telemetry.lock().unwrap();
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
}

fn main() -> Result<(), eframe::Error> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    tracing::info!("rpv ground station starting");

    let config = Config::load();
    tracing::info!("Config: {:?}", config);

    // Shared running flag for ctrl+c
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        tracing::info!("Ctrl+C received, shutting down...");
        r.store(false, Ordering::SeqCst);
    }).expect("Failed to set ctrl+c handler");

    let (video_frame_tx, video_frame_rx) = mpsc::unbounded_channel();

    // Shared link status between video receiver and UI
    let link_status_shared: Arc<Mutex<LinkStatus>> = Arc::new(Mutex::new(LinkStatus::Searching));

    // Shared camera IP for RC transmitter and heartbeat sender
    let cam_ip: Arc<Mutex<Option<IpAddr>>> = Arc::new(Mutex::new(None));

    let discovery_running = running.clone();
    let bg_link_status = Arc::clone(&link_status_shared);
    let bg_cam_ip = Arc::clone(&cam_ip);
    std::thread::spawn(move || {
        discovery::run(discovery_running, bg_link_status, bg_cam_ip);
    });

    let decoder = VideoDecoder::new(config.video_width, config.video_height);
    let decoded_frame = decoder.get_frame();
    decoder.spawn(video_frame_rx);

    let telemetry = TelemetryReceiver::new();
    let telemetry_state = telemetry.get_state();

    config.save();

    let bg_video_frame_tx = video_frame_tx;
    let bg_telemetry = telemetry;
    let bg_config = config.clone();
    let bg_running = running.clone();
    let bg_cam_ip2 = Arc::clone(&cam_ip);

    // Spawn tokio runtime in background thread
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let receiver = VideoReceiver::new(bg_config.video_port, bg_video_frame_tx).await
                .expect("Failed to create video receiver");

            let telem_port = bg_config.telemetry_port;
            let rc_port = bg_config.rc_port;

            tokio::spawn(async move {
                receiver.run().await;
            });

            tokio::spawn(async move {
                bg_telemetry.run(telem_port).await;
            });

            // RC transmitter with dynamic cam IP (discovery provides it)
            let rc_cam_ip = Arc::clone(&bg_cam_ip2);
            let mut rc = crate::rc::joystick::RCTx::new(rc_cam_ip, rc_port);
            tokio::spawn(async move {
                rc.run().await;
            });

            // Heartbeat sender (ground → camera)
            let hb_cam_ip = Arc::clone(&bg_cam_ip2);
            tokio::spawn(async move {
                let socket = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("Failed to bind heartbeat sender socket: {}", e);
                        return;
                    }
                };
                let hb_port = 5603u16;
                let mut seq: u32 = 0;
                tracing::info!("Heartbeat sender ready on port {}", hb_port);

                loop {
                    let target = {
                        let locked = hb_cam_ip.lock().unwrap();
                        locked.map(|ip| format!("{}:{}", ip, hb_port))
                    };
                    if let Some(addr) = target {
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;

                        let mut pkt = [0u8; 19];
                        pkt[..7].copy_from_slice(b"rpv-bea");
                        pkt[7..11].copy_from_slice(&seq.to_le_bytes());
                        pkt[11..19].copy_from_slice(&ts.to_le_bytes());

                        let _ = socket.send_to(&pkt, &addr).await;
                        seq = seq.wrapping_add(1);
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            });

            // Stay alive until ctrl+c
            while bg_running.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            tracing::info!("Background tasks shutting down");
        });
    });

    // Run egui on main thread with custom wgpu limits for Pi 5 V3D GPU
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_fullscreen(true)
            .with_title("rpv ground station"),
        wgpu_options: egui_wgpu::WgpuConfiguration {
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
            Ok(Box::new(RpvApp::new(config, decoded_frame, telemetry_state, app_running, link_status_shared)))
        }),
    )
}

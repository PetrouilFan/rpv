mod config;
mod telemetry;
mod video {
    pub mod receiver;
    pub mod decoder;
}
mod rc {
    pub mod joystick;
}

use std::sync::{Arc, Mutex};

use egui::{ColorImage, TextureHandle, Vec2};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::telemetry::{Telemetry, TelemetryReceiver};
use crate::video::receiver::VideoReceiver;
use crate::video::decoder::{VideoDecoder, DecodedYUV};

pub struct AppState {
    pub texture: Option<TextureHandle>,
    pub last_frame_time: std::time::Instant,
    pub frame_count: u64,
    pub fps: f64,
    pub connected: bool,
    pub config: Config,
    pub telemetry: Arc<Mutex<Telemetry>>,
}

pub struct RpvApp {
    state: AppState,
    video_rx: Arc<Mutex<Option<DecodedYUV>>>,
}

impl RpvApp {
    pub fn new(
        config: Config,
        video_rx: Arc<Mutex<Option<DecodedYUV>>>,
        telemetry: Arc<Mutex<Telemetry>>,
    ) -> Self {
        Self {
            state: AppState {
                texture: None,
                last_frame_time: std::time::Instant::now(),
                frame_count: 0,
                fps: 0.0,
                connected: false,
                config,
                telemetry,
            },
            video_rx,
        }
    }

    fn update_texture(&mut self, ctx: &egui::Context) {
        let frame_data = {
            let lock = self.video_rx.lock().unwrap();
            lock.clone()
        };

        if let Some(yuv) = frame_data {
            let w = self.state.config.video_width as usize;
            let h = self.state.config.video_height as usize;

            if yuv.y_data.len() == w * h
                && yuv.u_data.len() == (w / 2) * (h / 2)
                && yuv.v_data.len() == (w / 2) * (h / 2)
            {
                let rgba = yuv420p_to_rgba(&yuv.y_data, &yuv.u_data, &yuv.v_data, w, h);
                let image = ColorImage::from_rgba_unmultiplied([w, h], &rgba);

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
                self.state.connected = true;

                let elapsed = self.state.last_frame_time.elapsed().as_secs_f64();
                if elapsed >= 1.0 {
                    self.state.fps = self.state.frame_count as f64 / elapsed;
                    self.state.frame_count = 0;
                    self.state.last_frame_time = std::time::Instant::now();
                }
            }
        }
    }
}

fn yuv420p_to_rgba(y: &[u8], u: &[u8], v: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut rgba = vec![0u8; w * h * 4];
    for row in 0..h {
        for col in 0..w {
            let y_val = y[row * w + col] as i32;
            let uv_row = row / 2;
            let uv_col = col / 2;
            let u_val = u[uv_row * (w / 2) + uv_col] as i32 - 128;
            let v_val = v[uv_row * (w / 2) + uv_col] as i32 - 128;

            let r = (y_val + ((359 * v_val) >> 8)).clamp(0, 255) as u8;
            let g = (y_val - ((88 * u_val + 183 * v_val) >> 8)).clamp(0, 255) as u8;
            let b = (y_val + ((454 * u_val) >> 8)).clamp(0, 255) as u8;

            let idx = (row * w + col) * 4;
            rgba[idx] = r;
            rgba[idx + 1] = g;
            rgba[idx + 2] = b;
            rgba[idx + 3] = 255;
        }
    }
    rgba
}

impl eframe::App for RpvApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.update_texture(ctx);

        ctx.request_repaint();

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
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Waiting for video...")
                                .size(32.0)
                                .color(egui::Color32::GRAY),
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
                let status_color = if state.connected {
                    egui::Color32::GREEN
                } else {
                    egui::Color32::RED
                };
                ui.label(
                    egui::RichText::new(format!("FPS: {:.1}", state.fps))
                        .size(14.0)
                        .color(status_color),
                );
                if state.connected {
                    ui.label(
                        egui::RichText::new("CONNECTED")
                            .size(14.0)
                            .color(egui::Color32::GREEN),
                    );
                }
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
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();

    tracing::info!("rpv ground station starting");

    let config = Config::load();
    tracing::info!("Config: {:?}", config);

    let (video_frame_tx, video_frame_rx) = mpsc::unbounded_channel();

    // Create video decoder
    let decoder = VideoDecoder::new(config.video_width, config.video_height);
    let decoded_frame = decoder.get_frame();
    decoder.spawn(video_frame_rx);

    // Create telemetry receiver
    let telemetry = TelemetryReceiver::new();
    let telemetry_state = telemetry.get_state();

    config.save();

    let bg_config = config.clone();
    let bg_video_frame_tx = video_frame_tx;
    let bg_telemetry = telemetry;

    // Spawn tokio runtime in background thread
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let receiver = VideoReceiver::new(bg_config.video_port, bg_video_frame_tx).await
                .expect("Failed to create video receiver");

            let telem_port = bg_config.telemetry_port;
            let camera_ip = bg_config.camera_ip.clone();
            let rc_port = bg_config.rc_port;

            tokio::spawn(async move {
                receiver.run().await;
            });

            tokio::spawn(async move {
                bg_telemetry.run(telem_port).await;
            });

            let mut rc = crate::rc::joystick::RCTx::new(&camera_ip, rc_port);
            tokio::spawn(async move {
                rc.run().await;
            });

            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    });

    // Run egui on main thread with custom wgpu limits for Pi 5 V3D GPU
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
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

    eframe::run_native(
        "rpv ground station",
        native_options,
        Box::new(|_cc| {
            Ok(Box::new(RpvApp::new(config, decoded_frame, telemetry_state)))
        }),
    )
}

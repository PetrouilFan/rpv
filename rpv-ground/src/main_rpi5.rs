mod config;
mod link;
mod link_state;
mod rawsock;
mod telemetry;
mod video {
    pub mod decoder;
    pub mod receiver;
}
mod rc {
    pub mod joystick;
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use egui::Vec2;

use crate::config::Config;
use crate::link_state::{LinkStateHandle, LinkStatus};
use crate::rawsock::RawSocket;
use crate::telemetry::{Telemetry, TelemetryReceiver};
use crate::video::decoder::{DecodedFrame as DecodedYUV, VideoDecoder};
use crate::video::receiver::VideoReceiver;

// ── GPU YUV→RGB conversion via egui PaintCallback ───────────────────

const YUV_SHADER: &str = "
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VertexOutput {
    // Fullscreen triangle (3 vertices, no vertex buffer needed)
    let x = f32((idx & 1u) << 2u) - 1.0;
    let y = 1.0 - f32((idx & 2u) << 1u);
    var out: VertexOutput;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>(x * 0.5 + 0.5, y * 0.5 + 0.5);
    return out;
}

@group(0) @binding(0) var t_y: texture_2d<f32>;
@group(0) @binding(1) var t_uv: texture_2d<f32>;
@group(0) @binding(2) var s: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let y_val = textureSample(t_y, s, in.uv).r * 255.0;
    let uv_val = textureSample(t_uv, s, in.uv).rg * 255.0;
    let c = y_val - 16.0;
    let u = uv_val.r - 128.0;
    let v = uv_val.g - 128.0;
    let r = (298.0 * c + 409.0 * v + 128.0) / 256.0;
    let g = (298.0 * c - 100.0 * u - 208.0 * v + 128.0) / 256.0;
    let b = (298.0 * c + 517.0 * u + 128.0) / 256.0;
    return vec4<f32>(
        clamp(r / 255.0, 0.0, 1.0),
        clamp(g / 255.0, 0.0, 1.0),
        clamp(b / 255.0, 0.0, 1.0),
        1.0
    );
}
";

/// GPU resources for YUV→RGB rendering. Shared between app and paint callback via Arc<Mutex>.
struct YuvGpuResources {
    #[allow(dead_code)]
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    y_texture: wgpu::Texture,
    uv_texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
    video_width: u32,
    video_height: u32,
}

impl YuvGpuResources {
    fn new(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        video_width: u32,
        video_height: u32,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        let y_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("y_plane"),
            size: wgpu::Extent3d {
                width: video_width,
                height: video_height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let uv_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("uv_plane"),
            size: wgpu::Extent3d {
                width: video_width / 2,
                height: video_height / 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let uv_view = uv_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("yuv_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("yuv_bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&uv_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv_shader"),
            source: wgpu::ShaderSource::Wgsl(YUV_SHADER.into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuv_pl"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("yuv_rp"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview: None,
            cache: None,
        });

        Self {
            device,
            queue,
            y_texture,
            uv_texture,
            bind_group,
            pipeline,
            video_width,
            video_height,
        }
    }

    /// Upload NV12 planes to GPU textures (called from update, before paint)
    fn upload(&self, nv12_data: &[u8], stride: u32) {
        let w = self.video_width as usize;
        let h = self.video_height as usize;
        let stride = stride as usize;
        let y_size = stride * h;

        if stride == w {
            // Fast path: stride matches width, upload entire planes in single calls
            let y_data = &nv12_data[..y_size.min(nv12_data.len())];
            self.queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &self.y_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y: 0, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                y_data,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(w as u32),
                    rows_per_image: Some(h as u32),
                },
                wgpu::Extent3d {
                    width: w as u32,
                    height: h as u32,
                    depth_or_array_layers: 1,
                },
            );

            let uv_h = h / 2;
            if y_size < nv12_data.len() {
                let uv_data = &nv12_data[y_size..y_size + w * uv_h];
                self.queue.write_texture(
                    wgpu::ImageCopyTexture {
                        texture: &self.uv_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: 0, y: 0, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    uv_data,
                    wgpu::ImageDataLayout {
                        offset: 0,
                        bytes_per_row: Some(w as u32),
                        rows_per_image: Some(uv_h as u32),
                    },
                    wgpu::Extent3d {
                        width: w as u32 / 2,
                        height: uv_h as u32,
                        depth_or_array_layers: 1,
                    },
                );
            }
        } else {
            // Slow path: stride != width, must copy row by row
            let y_data = &nv12_data[..y_size.min(nv12_data.len())];
            for row in 0..h {
                let src_start = row * stride;
                let src_end = src_start + w;
                if src_end > y_data.len() {
                    break;
                }
                self.queue.write_texture(
                    wgpu::ImageCopyTexture {
                        texture: &self.y_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: 0,
                            y: row as u32,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &y_data[src_start..src_end],
                    wgpu::ImageDataLayout {
                        offset: 0,
                        bytes_per_row: Some(w as u32),
                        rows_per_image: Some(1),
                    },
                    wgpu::Extent3d {
                        width: w as u32,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                );
            }

            let uv_h = h / 2;
            if y_size < nv12_data.len() {
                let uv_data = &nv12_data[y_size..];
                for row in 0..uv_h {
                    let src_start = row * stride;
                    let src_end = src_start + w;
                    if src_end > uv_data.len() {
                        break;
                    }
                    self.queue.write_texture(
                        wgpu::ImageCopyTexture {
                            texture: &self.uv_texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d {
                                x: 0,
                                y: row as u32,
                                z: 0,
                            },
                            aspect: wgpu::TextureAspect::All,
                        },
                        &uv_data[src_start..src_end],
                        wgpu::ImageDataLayout {
                            offset: 0,
                            bytes_per_row: Some(w as u32),
                            rows_per_image: Some(1),
                        },
                        wgpu::Extent3d {
                            width: w as u32 / 2,
                            height: 1,
                            depth_or_array_layers: 1,
                        },
                    );
                }
            }
        }
    }
}

/// egui PaintCallback that renders a fullscreen YUV→RGB quad
struct YuvRenderCallback {
    resources: Arc<Mutex<YuvGpuResources>>,
}

impl egui_wgpu::CallbackTrait for YuvRenderCallback {
    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        _callback_resources: &egui_wgpu::CallbackResources,
    ) {
        let res = self.resources.lock().unwrap();
        render_pass.set_pipeline(&res.pipeline);
        render_pass.set_bind_group(0, &res.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}

// ── App state ───────────────────────────────────────────────────────

pub struct AppState {
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
    needs_repaint: bool,
    has_ever_had_frame: bool,
    yuv_gpu: Option<Arc<Mutex<YuvGpuResources>>>,
    update_logged: bool,
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
        Self {
            state: AppState {
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
            needs_repaint: false,
            has_ever_had_frame: false,
            yuv_gpu: None,
            update_logged: false,
        }
    }

    fn ensure_gpu_resources(&mut self, frame: &eframe::Frame) {
        if self.yuv_gpu.is_some() {
            return;
        }
        if let Some(rs) = frame.wgpu_render_state() {
            tracing::info!(
                "Initializing GPU YUV→RGB pipeline ({}x{})",
                self.state.config.video_width,
                self.state.config.video_height
            );
            self.yuv_gpu = Some(Arc::new(Mutex::new(YuvGpuResources::new(
                rs.device.clone(),
                rs.queue.clone(),
                self.state.config.video_width,
                self.state.config.video_height,
                rs.target_format,
            ))));
        } else {
            tracing::warn!("wgpu render state not available yet");
        }
    }

    fn process_frames(&mut self) -> bool {
        let mut latest = None;
        let mut recv_count = 0usize;
        while let Ok(frame) = self.frame_rx.try_recv() {
            latest = Some(frame);
            recv_count += 1;
        }

        if recv_count > 0 {
            if self.yuv_gpu.is_some() {
                tracing::info!("process_frames: received {} frames, GPU ready", recv_count);
            } else {
                tracing::warn!("process_frames: received {} frames but GPU resources NOT initialized (yuv_gpu=None)", recv_count);
            }
        }

        let mut had_frame = false;
        if let (Some(frame), Some(ref gpu)) = (latest, &self.yuv_gpu) {
            let h = frame.height as usize;
            let stride = frame.stride as usize;
            let y_size = stride * h;
            let uv_size = stride * h / 2;

            if frame.nv12_data.len() >= y_size + uv_size {
                let res = gpu.lock().unwrap();
                res.upload(&frame.nv12_data, frame.stride);
                drop(res);

                self.state.frame_count += 1;
                self.state.last_frame_time = Instant::now();

                if self.state.frame_count == 30 {
                    self.state.fps = 30.0 / self.state.fps_timer.elapsed().as_secs_f64();
                    self.state.frame_count = 0;
                    self.state.fps_timer = Instant::now();
                }

                self.state.link_state.video_frame_decoded();
                self.has_ever_had_frame = true;
                had_frame = true;
            }
        }
        had_frame
    }
}

impl eframe::App for RpvApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if !self.state.running.load(Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        if !self.update_logged {
            tracing::info!(
                "update() called, wgpu_render_state available: {}",
                frame.wgpu_render_state().is_some()
            );
            self.update_logged = true;
        }

        self.ensure_gpu_resources(frame);
        let had_frame = self.process_frames();
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

                if self.has_ever_had_frame {
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

                    // GPU YUV→RGB via egui PaintCallback
                    // Draw directly in the CentralPanel (no Area wrapper) so the
                    // video renders in the correct sequence relative to the OSD.
                    if let Some(ref gpu) = self.yuv_gpu {
                        let callback = YuvRenderCallback {
                            resources: gpu.clone(),
                        };
                        let paint_cb = egui_wgpu::Callback::new_paint_callback(rect, callback);
                        ui.painter().add(paint_cb);
                    }
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
    let p = ui.painter();

    // ── Top-left: link status, FPS, RSSI ──
    let mut y = 10.0;

    let (label, color, show_dot) = match state.link_status {
        LinkStatus::Connected => ("LINK OK", egui::Color32::GREEN, true),
        LinkStatus::Searching => ("SEARCHING", egui::Color32::YELLOW, false),
        LinkStatus::SignalLost => ("SIGNAL LOST", egui::Color32::RED, false),
        LinkStatus::NoCamera => ("NO CAMERA", egui::Color32::from_rgb(255, 160, 0), false),
    };

    // Blinking dot
    let dot_visible = if show_dot {
        true
    } else {
        let t = ui.ctx().input(|i| i.time);
        (t * 2.0) as u64 % 2 == 0
    };
    if dot_visible {
        p.circle_filled(egui::pos2(15.0, y + 7.0), 5.0, color);
    }

    p.text(
        egui::pos2(26.0, y),
        egui::Align2::LEFT_TOP,
        label,
        egui::FontId::proportional(14.0),
        color,
    );
    y += 20.0;

    let fps_color = match state.link_status {
        LinkStatus::Connected => egui::Color32::from_gray(200),
        _ => egui::Color32::from_gray(100),
    };
    p.text(
        egui::pos2(10.0, y),
        egui::Align2::LEFT_TOP,
        format!("FPS: {:.1}", state.fps),
        egui::FontId::proportional(12.0),
        fps_color,
    );
    y += 16.0;

    let rssi_val = state.rssi.lock().unwrap().clone();
    let (rssi_text, rssi_color) = match rssi_val {
        Some(dbm) if dbm >= -50 => (
            format!("SIG: {} dBm (excellent)", dbm),
            egui::Color32::GREEN,
        ),
        Some(dbm) if dbm >= -70 => (
            format!("SIG: {} dBm (good)", dbm),
            egui::Color32::from_rgb(100, 255, 100),
        ),
        Some(dbm) if dbm >= -80 => (format!("SIG: {} dBm (weak)", dbm), egui::Color32::YELLOW),
        Some(dbm) => (format!("SIG: {} dBm (poor)", dbm), egui::Color32::RED),
        None => ("SIG: ---".to_string(), egui::Color32::from_gray(100)),
    };
    p.text(
        egui::pos2(10.0, y),
        egui::Align2::LEFT_TOP,
        rssi_text,
        egui::FontId::proportional(12.0),
        rssi_color,
    );

    // ── Top-right: battery bar + voltage + mode ──
    let right_x = screen.max.x - 10.0;
    let mut y = 10.0;

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
    let bar_rect = egui::Rect::from_min_size(
        egui::pos2(right_x - bar_width, y),
        egui::vec2(bar_width, bar_height),
    );
    p.rect_filled(bar_rect, 2.0, egui::Color32::from_gray(40));
    let fill_width = (bar_width * pct / 100.0).max(0.0);
    let fill_rect = egui::Rect::from_min_size(bar_rect.min, egui::vec2(fill_width, bar_height));
    p.rect_filled(fill_rect, 2.0, bar_color);
    y += bar_height + 4.0;

    p.text(
        egui::pos2(right_x, y),
        egui::Align2::RIGHT_TOP,
        format!("{:.1}V  {}%", telem.battery_v, telem.battery_pct),
        egui::FontId::proportional(12.0),
        egui::Color32::WHITE,
    );
    y += 16.0;

    let mode_color = if telem.armed {
        egui::Color32::from_rgb(255, 100, 100)
    } else {
        egui::Color32::from_rgb(100, 255, 100)
    };
    p.text(
        egui::pos2(right_x, y),
        egui::Align2::RIGHT_TOP,
        format!("MODE: {}", telem.mode),
        egui::FontId::proportional(12.0),
        mode_color,
    );

    // ── Bottom-left: speed + altitude ──
    let mut y = screen.max.y - 70.0;
    p.text(
        egui::pos2(10.0, y),
        egui::Align2::LEFT_TOP,
        format!("SPD: {:.1} m/s", telem.speed),
        egui::FontId::proportional(16.0),
        egui::Color32::WHITE,
    );
    y += 22.0;
    p.text(
        egui::pos2(10.0, y),
        egui::Align2::LEFT_TOP,
        format!("ALT: {:.1} m", telem.alt),
        egui::FontId::proportional(16.0),
        egui::Color32::WHITE,
    );

    // ── Bottom-right: heading, satellites, GPS ──
    let right_x = screen.max.x - 10.0;
    let mut y = screen.max.y - 70.0;
    p.text(
        egui::pos2(right_x, y),
        egui::Align2::RIGHT_TOP,
        format!("HDG: {:.0}deg", telem.heading),
        egui::FontId::proportional(16.0),
        egui::Color32::WHITE,
    );
    y += 22.0;
    p.text(
        egui::pos2(right_x, y),
        egui::Align2::RIGHT_TOP,
        format!("SAT: {}", telem.satellites),
        egui::FontId::proportional(14.0),
        egui::Color32::WHITE,
    );
    y += 18.0;
    p.text(
        egui::pos2(right_x, y),
        egui::Align2::RIGHT_TOP,
        format!("{:.6}, {:.6}", telem.lat, telem.lon),
        egui::FontId::proportional(11.0),
        egui::Color32::from_gray(180),
    );

    // ── Center-bottom: RC channel bars ──
    let channels = state.channels.lock().unwrap().clone();
    let bar_width = 20.0;
    let bar_height = 30.0;
    let bar_gap = 4.0;
    let total_width = 4.0 * bar_width + 3.0 * bar_gap;
    let start_x = screen.center().x - total_width / 2.0;
    let bars_y = screen.max.y - 90.0;

    p.text(
        egui::pos2(screen.center().x, bars_y - 14.0),
        egui::Align2::CENTER_TOP,
        "RC",
        egui::FontId::proportional(12.0),
        egui::Color32::YELLOW,
    );

    for (i, ch) in channels.iter().enumerate().take(4) {
        let normalized = (*ch as f32 - 1000.0) / 1000.0;
        let x = start_x + i as f32 * (bar_width + bar_gap);
        let rect =
            egui::Rect::from_min_size(egui::pos2(x, bars_y), egui::vec2(bar_width, bar_height));
        p.rect_filled(rect, 2.0, egui::Color32::from_gray(40));
        let fill_h = bar_height * normalized.clamp(0.0, 1.0);
        let fill_rect = egui::Rect::from_min_size(
            egui::pos2(x, bars_y + bar_height - fill_h),
            egui::vec2(bar_width, fill_h),
        );
        p.rect_filled(fill_rect, 2.0, egui::Color32::GREEN);
    }
}

fn main() -> Result<(), eframe::Error> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    tracing::info!("rpv ground station starting (Pi 5, monitor mode)");

    let (config, was_default) = Config::load();
    tracing::info!("Config: {:?}", config);

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        tracing::info!("Ctrl+C received, shutting down...");
        r.store(false, Ordering::SeqCst);
    })
    .expect("Failed to set ctrl+c handler");

    let socket = match RawSocket::new(&config.interface) {
        Ok(s) => {
            tracing::info!("Raw socket bound to {} (monitor mode)", config.interface);
            Arc::new(s)
        }
        Err(e) => {
            tracing::error!("Failed to open raw socket on {}: {}", config.interface, e);
            std::process::exit(1);
        }
    };

    let link_state = LinkStateHandle::new();

    let (video_payload_tx, video_payload_rx) = crossbeam_channel::bounded::<Vec<u8>>(1024);
    let (video_frame_tx, video_frame_rx_decoder) = crossbeam_channel::bounded::<Vec<u8>>(4);
    let (telem_payload_tx, telem_payload_rx) = crossbeam_channel::bounded::<Vec<u8>>(16);

    // Test the video channel works
    let test_data = vec![0x52, 0x50, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00];
    video_payload_tx
        .try_send(test_data)
        .expect("video channel test failed");
    tracing::info!("Video channel test OK");

    let decoder = VideoDecoder::new(config.video_width, config.video_height);
    let ui_frame_rx = decoder.get_rx();
    decoder.spawn(video_frame_rx_decoder);

    let telemetry = TelemetryReceiver::new(link_state.clone(), telem_payload_rx);
    let telemetry_state = telemetry.get_state();

    if was_default {
        config.save();
    }

    let last_heartbeat: Arc<Mutex<Instant>> = Arc::new(Mutex::new(Instant::now()));
    let rssi_shared: Arc<Mutex<Option<i8>>> = Arc::new(Mutex::new(None));

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

    let vr = VideoReceiver::new(video_frame_tx, video_payload_rx);
    let _vr_handle = std::thread::spawn(move || {
        vr.run();
    });

    let _telem_handle = std::thread::spawn(move || {
        telemetry.run();
    });

    let rc_socket = Arc::clone(&socket);
    let rc_drone_id = config.drone_id;
    let rc_running = running.clone();
    let mut rc = crate::rc::joystick::RCTx::new(rc_socket, rc_drone_id, rc_running);
    let channels_shared = rc.channels();
    let _rc_handle = std::thread::spawn(move || {
        rc.run();
    });

    let hb_running = running.clone();
    let hb_socket = Arc::clone(&socket);
    let hb_drone_id = config.drone_id;
    let _hb_handle = std::thread::spawn(move || {
        heartbeat_sender(hb_running, hb_socket, hb_drone_id);
    });

    let hm_running = running.clone();
    let hm_last = Arc::clone(&last_heartbeat);
    let hm_link_state = link_state.clone();
    let _hm_handle = std::thread::spawn(move || {
        heartbeat_monitor(hm_running, hm_last, hm_link_state);
    });

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_fullscreen(true)
            .with_title("rpv ground station"),
        wgpu_options: egui_wgpu::WgpuConfiguration {
            present_mode: wgpu::PresentMode::Fifo,
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
    let mut video_count: u64 = 0;
    let mut telemetry_count: u64 = 0;
    let mut heartbeat_count: u64 = 0;
    let mut total_frames: u64 = 0;

    while running.load(Ordering::SeqCst) {
        let len = match socket.recv(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("RX recv error: {}", e);
                continue;
            }
        };

        let (payload, frame_rssi) = match rawsock::recv_extract(&buf[..len], reject_count < 10) {
            Some(p) => p,
            None => {
                reject_count += 1;
                if reject_count <= 10 || reject_count % 500 == 0 {
                    tracing::warn!(
                        "RX: rejected frame #{} ({}B), first 16 bytes: {:02x?}",
                        reject_count,
                        len,
                        &buf[..16.min(len)]
                    );
                }
                continue;
            }
        };

        if let Some(rssi_val) = frame_rssi {
            *rssi.lock().unwrap() = Some(rssi_val);
        }

        if !link::L2Header::matches_magic(payload) {
            reject_count += 1;
            if reject_count <= 10 || reject_count % 500 == 0 {
                tracing::warn!(
                    "RX: magic mismatch #{}, payload first 16 bytes: {:02x?}",
                    reject_count,
                    &payload[..16.min(payload.len())]
                );
            }
            continue;
        }
        let (header, data) = match link::L2Header::decode(payload) {
            Some(h) => h,
            None => continue,
        };

        if header.drone_id != drone_id {
            continue;
        }

        total_frames += 1;
        if total_frames % 500 == 0 {
            tracing::info!(
                "RX stats: total={} video={} telem={} hb={} rejected={}",
                total_frames,
                video_count,
                telemetry_count,
                heartbeat_count,
                reject_count
            );
        }

        match header.payload_type {
            link::PAYLOAD_VIDEO => {
                video_count += 1;
                if video_tx.try_send(data.to_vec()).is_err() {
                    tracing::warn!("Video queue dropped (backpressure)");
                }
            }
            link::PAYLOAD_TELEMETRY => {
                telemetry_count += 1;
                if telem_tx.try_send(data.to_vec()).is_err() {
                    tracing::warn!("Telemetry queue dropped");
                }
            }
            link::PAYLOAD_HEARTBEAT => {
                heartbeat_count += 1;
                *last_heartbeat.lock().unwrap() = Instant::now();
            }
            _ => {
                tracing::debug!("RX: unknown payload type 0x{:02x}", header.payload_type);
            }
        }
    }
}

fn heartbeat_monitor(
    running: Arc<AtomicBool>,
    last_heartbeat: Arc<Mutex<Instant>>,
    link_state: LinkStateHandle,
) {
    tracing::info!("Heartbeat monitor started (timeout: 0.5s)");
    std::thread::sleep(std::time::Duration::from_secs(1));

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

mod config;
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

use std::sync::atomic::{AtomicBool, AtomicI8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use egui::Vec2;

use rpv_proto::discovery;
use rpv_proto::link;
use rpv_proto::socket_trait::SocketTrait;
use rpv_proto::tcpsock::TcpSocket;
use rpv_proto::udpsock::UdpSocket;
use rpv_proto::rawsock_common;

use crate::config::Config;
use crate::link_state::{LinkStateHandle, LinkStatus};
use crate::rawsock::RawSocket;
use crate::telemetry::{Telemetry, TelemetryReceiver};
use crate::video::decoder::{DecodedFrame as DecodedYUV, VideoDecoder};
use crate::video::receiver::VideoReceiver;

const STATUS_FILE: &str = "/tmp/rpv_link_status";

/// #24: Pin current thread to core + optional SCHED_FIFO
fn pin_thread_to_core(core_id: usize, fifo_priority: Option<i32>) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(core_id, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        if let Some(prio) = fifo_priority {
            let param = libc::sched_param {
                sched_priority: prio,
            };
            libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
        }
    }
}

fn write_link_status(status: &str) {
    if let Err(e) = std::fs::write(STATUS_FILE, status) {
        tracing::warn!("Failed to write link status file: {}", e);
    }
}

fn join_log(name: &str, handle: std::thread::JoinHandle<()>) {
    match handle.join() {
        Ok(()) => {}
        Err(e) => tracing::error!("Thread '{}' panicked: {:?}", name, e),
    }
}

// ── GPU YUV→RGB conversion via egui PaintCallback ───────────────────

const YUV_SHADER: &str = "
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VertexOutput {
    let x = f32((idx & 1u) << 2u) - 1.0;
    let y = 1.0 - f32((idx & 2u) << 1u);
    var out: VertexOutput;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>(x * 0.5 + 0.5, 0.5 - y * 0.5);
    return out;
}

@group(0) @binding(0) var t_y: texture_2d<f32>;
@group(0) @binding(1) var t_u: texture_2d<f32>;
@group(0) @binding(2) var t_v: texture_2d<f32>;
@group(0) @binding(3) var s: sampler;

    @fragment
    fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
        // Use same UV for all planes - sampler handles half-resolution chroma via linear filtering
        let uv = in.uv;
        
        // BT.601 YCbCr (limited range 16-235 for Y, 16-240 for U/V)
        // Convert from 0-255 to proper ranges
        // Y at full resolution, U/V at half resolution -> use uv*0.5 for correct chroma coordinates
        // Use textureSampleLevel(..., 0.0) to force nearest-neighbor (no interpolation)
        let y_val = (textureSample(t_y, s, uv).r * 255.0 - 16.0) * (255.0 / 219.0);
        let u_val = (textureSampleLevel(t_u, s, uv * 0.5, 0.0).r * 255.0 - 128.0) * (255.0 / 224.0);
        let v_val = (textureSampleLevel(t_v, s, uv * 0.5, 0.0).r * 255.0 - 128.0) * (255.0 / 224.0);
        
        // BT.601 YCbCr -> RGB
        let r = y_val + 1.402 * v_val;
        let g = y_val - 0.344 * u_val - 0.714 * v_val;
        let b = y_val + 1.772 * u_val;
        
        return vec4<f32>(
            clamp(r / 255.0, 0.0, 1.0),
            clamp(g / 255.0, 0.0, 1.0),
            clamp(b / 255.0, 0.0, 1.0),
            1.0
        );
    }
";

/// GPU resources for planar YUV→RGB rendering.
struct YuvGpuResources {
    #[allow(dead_code)]
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    y_texture: wgpu::Texture,
    u_texture: wgpu::Texture,
    v_texture: wgpu::Texture,
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

        let uv_extent = wgpu::Extent3d {
            width: video_width / 2,
            height: video_height / 2,
            depth_or_array_layers: 1,
        };
        let u_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("u_plane"),
            size: uv_extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let u_view = u_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let v_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("v_plane"),
            size: uv_extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let v_view = v_texture.create_view(&wgpu::TextureViewDescriptor::default());

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
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
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
                    resource: wgpu::BindingResource::TextureView(&u_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&v_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
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
                    blend: Some(wgpu::BlendState::REPLACE),
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
            u_texture,
            v_texture,
            bind_group,
            pipeline,
            video_width,
            video_height,
        }
    }

    fn upload(&self, y_data: &[u8], u_data: &[u8], v_data: &[u8]) {
        let w = self.video_width as usize;
        let h = self.video_height as usize;
        let uv_w = w / 2;
        let uv_h = h / 2;

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

        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.u_texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: 0, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            u_data,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(uv_w as u32),
                rows_per_image: Some(uv_h as u32),
            },
            wgpu::Extent3d {
                width: uv_w as u32,
                height: uv_h as u32,
                depth_or_array_layers: 1,
            },
        );

        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.v_texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: 0, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            v_data,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(uv_w as u32),
                rows_per_image: Some(uv_h as u32),
            },
            wgpu::Extent3d {
                width: uv_w as u32,
                height: uv_h as u32,
                depth_or_array_layers: 1,
            },
        );
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
    pub telemetry: Arc<ArcSwap<Telemetry>>,
    pub running: Arc<AtomicBool>,
    pub rssi: Arc<AtomicI8>,
    pub channels: Arc<ArcSwap<[u16; 16]>>,
}

pub struct RpvApp {
    state: AppState,
    frame_rx: crossbeam_channel::Receiver<DecodedYUV>,
    needs_repaint: bool,
    has_ever_had_frame: bool,
    yuv_gpu: Option<Arc<Mutex<YuvGpuResources>>>,
    update_logged: bool,
    last_rc_channels: [u16; 16],
}

impl RpvApp {
    pub fn new(
        config: Config,
        frame_rx: crossbeam_channel::Receiver<DecodedYUV>,
        telemetry: Arc<ArcSwap<Telemetry>>,
        running: Arc<AtomicBool>,
        link_state: LinkStateHandle,
        rssi: Arc<AtomicI8>,
        channels: Arc<ArcSwap<[u16; 16]>>,
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
            last_rc_channels: [1500; 16],
        }
    }

    fn ensure_gpu_resources(&mut self, frame: &eframe::Frame) {
        if self.yuv_gpu.is_some() {
            return;
        }
        if let Some(rs) = frame.wgpu_render_state() {
            tracing::info!(
                "Initializing GPU YUV→RGB pipeline ({}x{})",
                self.state.config.common.video_width,
                self.state.config.common.video_height
            );
            self.yuv_gpu = Some(Arc::new(Mutex::new(YuvGpuResources::new(
                rs.device.clone(),
                rs.queue.clone(),
                self.state.config.common.video_width,
                self.state.config.common.video_height,
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
            if self.yuv_gpu.is_none() {
                tracing::warn!(
                    "process_frames: received {} frames but GPU resources NOT initialized",
                    recv_count
                );
            }
        }

        let mut had_frame = false;
        if let (Some(frame), Some(ref gpu)) = (latest, &self.yuv_gpu) {
            {
                let res = gpu.lock().unwrap();
                if res.video_width != frame.width || res.video_height != frame.height {
                    tracing::warn!(
                        "Resolution change detected ({}x{} -> {}x{}), reinitializing GPU",
                        res.video_width,
                        res.video_height,
                        frame.width,
                        frame.height
                    );
                    drop(res);
                    self.yuv_gpu = None;
                    self.state.config.common.video_width = frame.width;
                    self.state.config.common.video_height = frame.height;
                    self.state.frame_count = 0;
                    self.state.fps_timer = Instant::now();
                    self.has_ever_had_frame = false;
                    self.needs_repaint = true;
                    return false;
                }
            }
            if let Some(recv_time) = frame.recv_time {
                let latency_ms = recv_time.elapsed().as_millis();
                if self.state.frame_count % 60 == 0 {
                    tracing::info!(
                        "decode-to-display latency: {}ms, dropped {} stale frames",
                        latency_ms,
                        recv_count.saturating_sub(1)
                    );
                }
            }

            let res = gpu.lock().unwrap();
            res.upload(&frame.y_data, &frame.u_data, &frame.v_data);
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
            let telem = self.state.telemetry.load();
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

        let channels = **self.state.channels.load();
        let rc_changed =
            channels.iter().zip(self.last_rc_channels.iter()).any(|(a, b)| a != b);
        if rc_changed {
            self.last_rc_channels.copy_from_slice(&channels);
            self.needs_repaint = true;
        }

        if self.needs_repaint {
            ctx.request_repaint();
        } else if self.state.link_status == LinkStatus::Connected {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
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
                        self.state.config.common.video_width as f32,
                        self.state.config.common.video_height as f32,
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
    let telem = state.telemetry.load();
    let p = ui.painter();

    // ── Top-left: link status, FPS, RSSI ──
    let mut y = 10.0;

    let (label, color, show_dot) = match state.link_status {
        LinkStatus::Connected => ("LINK OK", egui::Color32::GREEN, true),
        LinkStatus::Searching => ("SEARCHING", egui::Color32::YELLOW, false),
        LinkStatus::SignalLost => ("SIGNAL LOST", egui::Color32::RED, false),
        LinkStatus::NoCamera => ("NO CAMERA", egui::Color32::from_rgb(255, 160, 0), false),
    };

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
    let mono = || egui::FontId::monospace(12.0);
    let mono_big = || egui::FontId::monospace(16.0);
    let mono_sm = || egui::FontId::monospace(11.0);

    p.text(
        egui::pos2(10.0, y),
        egui::Align2::LEFT_TOP,
        format!("FPS: {:.1}", state.fps),
        mono(),
        fps_color,
    );
    y += 16.0;

    let rssi_raw = state.rssi.load(Ordering::Relaxed);
    let (rssi_text, rssi_color) = if rssi_raw == -128i8 {
        ("SIG: ---".to_string(), egui::Color32::from_gray(100))
    } else if rssi_raw >= -50 {
        (
            format!("SIG: {} dBm (excellent)", rssi_raw),
            egui::Color32::GREEN,
        )
    } else if rssi_raw >= -70 {
        (
            format!("SIG: {} dBm (good)", rssi_raw),
            egui::Color32::from_rgb(100, 255, 100),
        )
    } else if rssi_raw >= -80 {
        (
            format!("SIG: {} dBm (weak)", rssi_raw),
            egui::Color32::YELLOW,
        )
    } else {
        (format!("SIG: {} dBm (poor)", rssi_raw), egui::Color32::RED)
    };
    p.text(
        egui::pos2(10.0, y),
        egui::Align2::LEFT_TOP,
        rssi_text,
        mono(),
        rssi_color,
    );

    // ── Top-right: battery bar + voltage + mode ──
    let right_x = screen.max.x - 10.0;
    let mut y = 10.0;

    let (pct, bar_color, pct_text) = match telem.battery_pct {
        None => (0.0, egui::Color32::from_gray(80), "---%".to_string()),
        Some(0) => (0.0, egui::Color32::RED, "0%".to_string()),
        Some(p) => {
            let bar_color = if p > 30 {
                egui::Color32::from_rgb(0, 200, 0)
            } else if p > 15 {
                egui::Color32::YELLOW
            } else {
                egui::Color32::RED
            };
            (p as f32, bar_color, format!("{}%", p))
        }
    };

    let bar_width = 120.0;
    let bar_height = 14.0;
    let bar_rect = egui::Rect::from_min_size(
        egui::pos2(right_x - bar_width, y),
        egui::vec2(bar_width, bar_height),
    );
    p.rect_filled(bar_rect, 2.0, egui::Color32::from_gray(40));
    if telem.battery_pct.is_some() {
        let fill_width = (bar_width * pct / 100.0).max(0.0);
        let fill_rect = egui::Rect::from_min_size(bar_rect.min, egui::vec2(fill_width, bar_height));
        p.rect_filled(fill_rect, 2.0, bar_color);
    } else {
        // No battery data — show "N/A" text centered in bar
        p.text(
            bar_rect.center(),
            egui::Align2::CENTER_CENTER,
            "N/A",
            egui::FontId::monospace(10.0),
            egui::Color32::from_gray(150),
        );
    }
    y += bar_height + 4.0;

    p.text(
        egui::pos2(right_x, y),
        egui::Align2::RIGHT_TOP,
        format!("{:.1}V  {}", telem.battery_v, pct_text),
        mono(),
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
        mono_big(),
        egui::Color32::WHITE,
    );
    y += 22.0;
    p.text(
        egui::pos2(10.0, y),
        egui::Align2::LEFT_TOP,
        format!("ALT: {:.1} m", telem.alt),
        mono_big(),
        egui::Color32::WHITE,
    );

    // ── Bottom-right: heading, satellites, GPS ──
    let right_x = screen.max.x - 10.0;
    let mut y = screen.max.y - 70.0;
    p.text(
        egui::pos2(right_x, y),
        egui::Align2::RIGHT_TOP,
        format!("HDG: {:.0}deg", telem.heading),
        mono_big(),
        egui::Color32::WHITE,
    );
    y += 22.0;
    p.text(
        egui::pos2(right_x, y),
        egui::Align2::RIGHT_TOP,
        format!("SAT: {}", telem.satellites),
        mono(),
        egui::Color32::WHITE,
    );
    y += 18.0;
    p.text(
        egui::pos2(right_x, y),
        egui::Align2::RIGHT_TOP,
        format!("{:.6}, {:.6}", telem.lat, telem.lon),
        mono_sm(),
        egui::Color32::from_gray(180),
    );

    // ── Center-bottom: RC channel bars ──
    let channels = **state.channels.load();
    let bar_width = 24.0;
    let bar_height = 36.0;
    let bar_gap = 6.0;
    let total_width = 4.0 * bar_width + 3.0 * bar_gap;
    let start_x = screen.center().x - total_width / 2.0;
    let bars_y = screen.max.y - 100.0;
    let labels = ["Ail", "Ele", "Thr", "Rud"];

    for (i, ch) in channels.iter().enumerate().take(4) {
        let normalized = (*ch as f32 - 1000.0) / 1000.0;
        let x = start_x + i as f32 * (bar_width + bar_gap);

        p.text(
            egui::pos2(x + bar_width / 2.0, bars_y - 12.0),
            egui::Align2::CENTER_TOP,
            labels[i],
            egui::FontId::proportional(10.0),
            egui::Color32::from_gray(160),
        );

        let rect =
            egui::Rect::from_min_size(egui::pos2(x, bars_y), egui::vec2(bar_width, bar_height));
        p.rect_filled(rect, 2.0, egui::Color32::from_gray(40));

        let fill_h = bar_height * normalized.clamp(0.0, 1.0);
        let fill_color = if *ch < 1200 || *ch > 1800 {
            egui::Color32::from_rgb(255, 100, 100)
        } else {
            egui::Color32::from_rgb(100, 255, 100)
        };
        let fill_rect = egui::Rect::from_min_size(
            egui::pos2(x, bars_y + bar_height - fill_h),
            egui::vec2(bar_width, fill_h),
        );
        p.rect_filled(fill_rect, 2.0, fill_color);
    }
}

fn main() -> Result<(), eframe::Error> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,wgpu=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let (config, was_default) = Config::load();
    tracing::debug!("Config: {:?}", config);
    tracing::info!(
        "rpv ground station starting ({} mode)",
        config.common.transport
    );

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        tracing::info!("Ctrl+C received, shutting down...");
        r.store(false, Ordering::SeqCst);
    }) {
        tracing::error!("Failed to install Ctrl-C handler: {}. Clean shutdown may not work.", e);
    }

    let is_udp = config.common.transport == "udp";
    let is_tcp = config.common.transport == "tcp";

    let socket: Arc<dyn SocketTrait> = if is_udp {
        // If peer_addr is pre-configured, set it directly; otherwise use discovery
        let preconfigured_addr: Option<std::net::SocketAddr> = if let Some(ref peer) = config.common.peer_addr {
            match peer.parse() {
                Ok(addr) => {
                    tracing::info!("Using configured camera address: {}", addr);
                    Some(addr)
                }
                Err(e) => {
                    tracing::warn!("Invalid peer_addr '{}': {}, falling back to discovery", peer, e);
                    None
                }
            }
        } else {
            None
        };

        let (_discovery, peer_addr) =
            discovery::Discovery::spawn(0x02, config.common.drone_id, config.common.udp_port)
                .unwrap_or_else(|e| {
                    tracing::error!("Failed to start discovery: {}", e);
                    std::process::exit(1);
                });

        // If pre-configured, set the address directly; otherwise wait for discovery
        if let Some(addr) = preconfigured_addr {
            peer_addr.store(Arc::new(Some(addr)));
        } else {
            let mut waited = std::time::Duration::ZERO;
            let wait_timeout = std::time::Duration::from_secs(30);
            while peer_addr.load().is_none() && waited < wait_timeout {
                std::thread::sleep(std::time::Duration::from_millis(200));
                waited += std::time::Duration::from_millis(200);
                if (waited.as_millis() as u64) % 2000 < 200 {
                    tracing::info!("Searching for camera... ({}s elapsed)", waited.as_secs());
                }
            }

            if peer_addr.load().is_none() {
                tracing::warn!(
                    "No camera discovered after {}s — continuing anyway",
                    wait_timeout.as_secs()
                );
            } else {
                let discovered_addr = peer_addr.load();
                tracing::info!("Camera discovered at {}", discovered_addr.as_ref().unwrap());
            }
        };

        let std_socket = std::net::UdpSocket::bind(format!("0.0.0.0:{}", config.common.udp_port))
            .map_err(|e| {
                tracing::error!("Failed to bind UDP socket: {}", e);
                std::process::exit(1);
            })
            .unwrap();
        std_socket.set_broadcast(true).unwrap();
        std_socket
            .set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .unwrap();
        let std_socket = Arc::new(std_socket);

        match UdpSocket::new(std_socket, peer_addr, config.common.udp_port) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::error!("Failed to create UDP socket: {}", e);
                std::process::exit(1);
            }
        }
    } else if is_tcp {
        // TCP mode: ground station acts as server, camera connects to us
        let tcp_port = config.common.tcp_port.unwrap_or(9003);
        let listen_addr = format!("0.0.0.0:{}", tcp_port);
        tracing::info!("Starting TCP server on {}", listen_addr);
        
        match TcpSocket::new_server(&listen_addr, 1000) {
            Ok(s) => {
                tracing::info!("TCP connection established with camera");
                Arc::new(s)
            }
            Err(e) => {
                tracing::error!("Failed to start TCP server: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        match RawSocket::new(&config.common.interface) {
            Ok(s) => {
                tracing::info!(
                    "Raw socket bound to {} (monitor mode)",
                    config.common.interface
                );
                Arc::new(s)
            }
            Err(e) => {
                tracing::error!(
                    "Failed to open raw socket on {}: {}",
                    config.common.interface,
                    e
                );
                std::process::exit(1);
            }
        }
    };

    let link_state = LinkStateHandle::new();

    let (video_payload_tx, video_payload_rx) = crossbeam_channel::bounded::<Vec<u8>>(256);
    let (video_frame_tx, video_frame_rx_decoder) = crossbeam_channel::bounded::<Vec<u8>>(4);
    let (telem_payload_tx, telem_payload_rx) = crossbeam_channel::bounded::<Vec<u8>>(4);

    // QGC UDP bridge
    use std::net::UdpSocket as StdUdpSocket;
    let gcs_bridge_sock = StdUdpSocket::bind(format!("127.0.0.1:{}", config.gcs_uplink_port))
        .expect("Failed to bind QGC bridge socket");
    gcs_bridge_sock
        .set_read_timeout(Some(std::time::Duration::from_millis(100)))
        .expect("set_read_timeout failed");
    let gcs_qgc_addr: std::net::SocketAddr = format!("127.0.0.1:{}", config.gcs_downlink_port)
        .parse()
        .unwrap();

    let (mavlink_down_tx, mavlink_down_rx) = crossbeam_channel::bounded::<Vec<u8>>(256);

    let decoder = VideoDecoder::new(config.common.video_width, config.common.video_height);
    let ui_frame_rx = decoder.get_rx();
    decoder.spawn(video_frame_rx_decoder);

    let telemetry = TelemetryReceiver::new(link_state.clone(), telem_payload_rx);
    let telemetry_state = telemetry.get_state();

    if was_default {
        config.save();
    }

    let last_heartbeat: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let rssi_shared: Arc<AtomicI8> = Arc::new(AtomicI8::new(-128i8));

    let rx_running = running.clone();
    let rx_socket = Arc::clone(&socket);
    let rx_video_tx = video_payload_tx;
    let rx_telem_tx = telem_payload_tx;
    let rx_drone_id = config.common.drone_id;
    let rx_last_hb = Arc::clone(&last_heartbeat);
    let rx_rssi = Arc::clone(&rssi_shared);
    let rx_mavlink_tx = mavlink_down_tx.clone();
    let rx_handle = std::thread::spawn(move || {
        pin_thread_to_core(0, Some(50));
        rx_dispatcher(
            rx_running,
            rx_socket,
            rx_drone_id,
            rx_video_tx,
            rx_telem_tx,
            rx_last_hb,
            rx_rssi,
            rx_mavlink_tx,
        );
    });

    let mut vr = VideoReceiver::new(video_frame_tx, video_payload_rx);
    let vr_handle = std::thread::spawn(move || {
        vr.run();
    });

    let telem_handle = std::thread::spawn(move || {
        telemetry.run();
    });

    let rc_socket = Arc::clone(&socket);
    let rc_drone_id = config.common.drone_id;
    let rc_running = running.clone();
    let rc = crate::rc::joystick::RCTx::new(rc_socket, rc_drone_id, rc_running);
    let channels_shared = rc.channels();
    let rc_handle = std::thread::spawn(move || {
        rc.run();
    });

    let hb_running = running.clone();
    let hb_socket = Arc::clone(&socket);
    let hb_drone_id = config.common.drone_id;
    let hb_handle = std::thread::spawn(move || {
        heartbeat_sender(hb_running, hb_socket, hb_drone_id);
    });

    let hm_running = running.clone();
    let hm_last = Arc::clone(&last_heartbeat);
    let hm_link_state = link_state.clone();
    let hm_handle = std::thread::spawn(move || {
        heartbeat_monitor(hm_running, hm_last, hm_link_state);
    });

    // MAVLink bridge — downlink: radio → QGC
    let bridge_down_sock = gcs_bridge_sock
        .try_clone()
        .expect("Failed to clone GCS bridge socket for downlink");
    let bridge_down_running = running.clone();
    let mavlink_down_handle = std::thread::spawn(move || {
        tracing::info!(
            "MAVLink bridge ready → UDP 127.0.0.1:{} (QGC downlink)",
            config.gcs_downlink_port
        );
        while bridge_down_running.load(Ordering::SeqCst) {
            match mavlink_down_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(frame_bytes) => {
                    if let Err(e) = bridge_down_sock.send_to(&frame_bytes, gcs_qgc_addr) {
                        if e.raw_os_error() != Some(libc::ECONNREFUSED) {
                            tracing::warn!("MAVLink bridge sendto error: {}", e);
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        tracing::info!("MAVLink bridge downlink thread exiting");
    });

    // MAVLink bridge — uplink: QGC → radio
    let bridge_up_sock = gcs_bridge_sock;
    let bridge_up_running = running.clone();
    let bridge_up_socket = Arc::clone(&socket);
    let bridge_up_drone_id = config.common.drone_id;
    let mavlink_up_handle = std::thread::spawn(move || {
        tracing::info!(
            "MAVLink bridge ready ← UDP 127.0.0.1:{} (QGC uplink)",
            config.gcs_uplink_port
        );
        let mut recv_buf = [0u8; 1400];
        let mut l2_seq: u32 = 0;
        let mut l2_buf = Vec::with_capacity(link::MAX_PAYLOAD);
        let mut send_buf = Vec::with_capacity(8 + 24 + link::MAX_PAYLOAD);

        while bridge_up_running.load(Ordering::SeqCst) {
            match bridge_up_sock.recv_from(&mut recv_buf) {
                Ok((len, _src)) if len > 0 => {
                    let header = link::L2Header {
                        drone_id: bridge_up_drone_id,
                        payload_type: link::PAYLOAD_MAVLINK,
                        seq: l2_seq,
                    };
                    header.encode_into(&recv_buf[..len], &mut l2_buf);
                    let _ = bridge_up_socket.send_with_buf(&l2_buf, &mut send_buf);
                    l2_seq = l2_seq.wrapping_add(1);
                }
                Ok(_) => {}
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    tracing::warn!("MAVLink uplink recv error: {}", e);
                }
            }
        }
        tracing::info!("MAVLink bridge uplink thread exiting");
    });

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_fullscreen(true)
            .with_title("rpv ground station"),
        wgpu_options: egui_wgpu::WgpuConfiguration {
            present_mode: wgpu::PresentMode::AutoNoVsync,
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
    let result = eframe::run_native(
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
    );

    write_link_status("disconnected");

    join_log("rx_dispatcher", rx_handle);
    join_log("video_receiver", vr_handle);
    join_log("telemetry", telem_handle);
    join_log("rc_joystick", rc_handle);
    join_log("heartbeat_sender", hb_handle);
    join_log("heartbeat_monitor", hm_handle);
    join_log("mavlink_downlink", mavlink_down_handle);
    join_log("mavlink_uplink", mavlink_up_handle);

    result
}

fn rx_dispatcher(
    running: Arc<AtomicBool>,
    socket: Arc<dyn SocketTrait>,
    drone_id: u8,
    video_tx: crossbeam_channel::Sender<Vec<u8>>,
    telem_tx: crossbeam_channel::Sender<Vec<u8>>,
    last_heartbeat: Arc<AtomicU64>,
    _rssi: Arc<AtomicI8>,
    mavlink_tx: crossbeam_channel::Sender<Vec<u8>>,
) {
    tracing::info!("RX dispatcher started");
    let mut buf = vec![0u8; 65536];
    let mut reject_count: u64 = 0;
    let mut video_count: u64 = 0;
    let mut telemetry_count: u64 = 0;
    let mut heartbeat_count: u64 = 0;
    let mut mavlink_count: u64 = 0;
    let mut total_frames: u64 = 0;
    let mut retry_count: u32 = 0; // Track consecutive failures for backoff

    while running.load(Ordering::SeqCst) {
        let len = match socket.recv(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("RX recv error: {}", e);
                tracing::info!("Attempting to reconnect TCP socket...");
                loop {
                    retry_count += 1;
                    match socket.reconnect() {
                        Ok(()) => {
                            tracing::info!("Reconnected successfully");
                            retry_count = 0;
                            break;
                        }
                        Err(re) => {
                            let backoff_secs = 2u64.pow(retry_count.min(5));
                            let backoff_secs = backoff_secs.min(60);
                            tracing::error!(
                                "Reconnect failed (attempt {}): {}, retrying in {}s...",
                                retry_count,
                                re,
                                backoff_secs
                            );
                            std::thread::sleep(Duration::from_secs(backoff_secs));
                        }
                    }
                }
                continue;
            }
        };

        let payload = &buf[..len];

        let (actual_payload, maybe_rssi) = match rawsock_common::recv_extract(payload, false) {
            Some((p, r)) => (p, r),
            None => (payload, None),
        };
        if let Some(rssi) = maybe_rssi {
            _rssi.store(rssi, Ordering::Relaxed);
        }

        if !link::L2Header::matches_magic(actual_payload) {
            reject_count += 1;
            if reject_count <= 10 || reject_count % 500 == 0 {
                tracing::warn!(
                    "RX: magic mismatch #{}, payload first 16 bytes: {:02x?}",
                    reject_count,
                    &actual_payload[..16.min(actual_payload.len())]
                );
            }
            continue;
        }
        let (header, data) = match link::L2Header::decode(actual_payload) {
            Some(h) => h,
            None => continue,
        };

        if header.drone_id != drone_id {
            reject_count += 1;
            if reject_count <= 5 {
                tracing::warn!(
                    "RX: drone_id mismatch: expected {}, got {}, first16={:02x?}",
                    drone_id,
                    header.drone_id,
                    &payload[..16.min(payload.len())]
                );
            }
            continue;
        }

        if header.payload_type == link::PAYLOAD_RC {
            continue;
        }

        total_frames += 1;
        if total_frames % 500 == 0 {
            tracing::info!(
                "RX stats: total={} video={} telem={} hb={} mavlink={} rejected={}",
                total_frames,
                video_count,
                telemetry_count,
                heartbeat_count,
                mavlink_count,
                reject_count
            );
        }

        match header.payload_type {
            link::PAYLOAD_VIDEO => {
                video_count += 1;
                if video_tx.try_send(data.to_vec()).is_err() {
                    tracing::warn!("Video queue dropped (backpressure)");
                }
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                last_heartbeat.store(now_ms, Ordering::Relaxed);
            }
            link::PAYLOAD_TELEMETRY => {
                telemetry_count += 1;
                if telem_tx.try_send(data.to_vec()).is_err() {
                    tracing::warn!("Telemetry queue dropped");
                }
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                last_heartbeat.store(now_ms, Ordering::Relaxed);
            }
            link::PAYLOAD_HEARTBEAT => {
                heartbeat_count += 1;
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                last_heartbeat.store(now_ms, Ordering::Relaxed);
            }
            link::PAYLOAD_MAVLINK => {
                mavlink_count += 1;
                if mavlink_tx.try_send(data.to_vec()).is_err() {
                    tracing::warn!("MAVLink downlink queue full — frame dropped");
                }
            }
            _ => {
                tracing::debug!("RX: unknown payload type 0x{:02x}", header.payload_type);
            }
        }
    }
}

fn heartbeat_monitor(
    running: Arc<AtomicBool>,
    last_heartbeat: Arc<AtomicU64>,
    link_state: LinkStateHandle,
) {
    tracing::info!("Heartbeat monitor started (timeout: 5.0s)");
    std::thread::sleep(std::time::Duration::from_secs(1));

    let mut ever_connected = false;

    while running.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(100));

        let last_ms = last_heartbeat.load(Ordering::Relaxed);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let elapsed_ms = now_ms.saturating_sub(last_ms);

        if last_ms == 0 || elapsed_ms > 5000 {
            if ever_connected {
                link_state.heartbeat_lost();
                write_link_status("signal_lost");
            }
        } else {
            ever_connected = true;
            link_state.heartbeat_restored();
            write_link_status("connected");
        }
    }
}

fn heartbeat_sender(running: Arc<AtomicBool>, socket: Arc<dyn SocketTrait>, drone_id: u8) {
    tracing::info!("Heartbeat sender ready (L2 broadcast, 10Hz)");
    let mut l2_seq: u32 = 0;
    let mut payload_buf: Vec<u8> = Vec::with_capacity(19);
    let mut l2_buf: Vec<u8> = Vec::with_capacity(link::HEADER_LEN + 19);
    let mut send_buf: Vec<u8> = Vec::with_capacity(8 + 24 + link::HEADER_LEN + 19);

    while running.load(Ordering::SeqCst) {
        // NOTE: Using wall-clock SystemTime for heartbeat timestamp. Backward clock jumps
        // can make timestamps non-monotonic even when the link is healthy. This is a
        // trade-off: simple implementation vs. monotonic timestamps. If strict monotonic
        // timestamps are needed, consider using a monotonic counter instead.
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

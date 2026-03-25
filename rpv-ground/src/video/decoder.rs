use std::ffi::CString;
use tracing::{error, info, warn};

// ── Minimal libavcodec FFI (FFmpeg 6.x layout) ─────────────────────

/// Opaque codec context
#[repr(C)]
struct AvCodecContext {
    _private: [u8; 0],
}

/// Opaque codec descriptor
#[repr(C)]
struct AvCodec {
    _private: [u8; 0],
}

/// Opaque parser context
#[repr(C)]
struct AvCodecParserContext {
    _private: [u8; 0],
}

/// AVPacket — fields must match FFmpeg 6.x AVPacket struct layout.
/// We only touch data/size/buf; the rest are zeroed by av_packet_alloc.
#[repr(C)]
struct AvPacket {
    _buf: *mut u8, // AVBufferRef*
    pts: i64,
    dts: i64,
    data: *mut u8,
    size: i32,
    stream_index: i32,
    flags: i32,
    _side_data: *mut u8, // AVPacketSideData*
    _side_data_elems: i32,
    duration: i64,
    pos: i64,
    _opaque: *mut u8, // void* opaque_ref
    _opaque2: i64,
}

/// AVFrame — fields must match FFmpeg 6.x AVFrame struct layout.
#[repr(C)]
struct AvFrame {
    data: [*mut u8; 8],
    linesize: [i32; 8],
    _extended_data: *mut *mut u8,
    width: i32,
    height: i32,
    nb_samples: i32,
    format: i32,
    _key_frame: i32,
    _pict_type: i32,
    _sample_aspect_ratio_num: i32,
    _sample_aspect_ratio_den: i32,
    pts: i64,
    _pkt_pts: i64,
    _pkt_dts: i64,
    _coded_picture_number: i32,
    _display_picture_number: i32,
    quality: i32,
    _opaque: *mut u8,
    _repeat_pict: i32,
    _interlaced_frame: i32,
    _top_field_first: i32,
    _palette_has_changed: i32,
    _reordered_opaque: i64,
    _sample_rate: i32,
    _channel_layout: u64,
    _buf: [*mut u8; 8],
    _extended_buf: *mut *mut u8,
    _nb_extended_buf: i32,
    _side_data: *mut u8,
    _nb_side_data: i32,
    _flags: i32,
    _color_range: i32,
    _color_primaries: i32,
    _color_trc: i32,
    _colorspace: i32,
    _chroma_location: i32,
    _best_effort_timestamp: i64,
    _pkt_pos: i64,
    _pkt_duration: i64,
    _metadata: *mut u8,
    _decode_error_flags: i32,
    _channels: i32,
    _pkt_size: i32,
    _qscale_table: *mut i8,
    _qstride: i32,
    _qscale_type: i32,
    _qp_table_buf: *mut u8,
    _hw_frames_ctx: *mut u8,
    _opaque_ref: *mut u8,
    _crop_top: usize,
    _crop_bottom: usize,
    _crop_left: usize,
    _crop_right: usize,
    _private_ref: *mut u8,
    _hwaccel_picture_private: *mut u8,
}

// FFmpeg extern declarations
extern "C" {
    fn avcodec_alloc_context3(codec: *const AvCodec) -> *mut AvCodecContext;
    fn avcodec_free_context(ctx: *mut *mut AvCodecContext);
    fn avcodec_find_decoder_by_name(name: *const u8) -> *const AvCodec;
    fn avcodec_open2(ctx: *mut AvCodecContext, codec: *const AvCodec, options: *mut u8) -> i32;
    fn av_parser_init(codec_id: i32) -> *mut AvCodecParserContext;
    fn av_parser_close(parser: *mut AvCodecParserContext);
    fn av_parser_parse2(
        parser: *mut AvCodecParserContext,
        ctx: *mut AvCodecContext,
        poutbuf: *mut *mut u8,
        poutbuf_size: *mut i32,
        buf: *const u8,
        buf_size: i32,
        pts: i64,
        dts: i64,
        pos: i64,
    ) -> i32;
    fn avcodec_send_packet(ctx: *mut AvCodecContext, pkt: *const AvPacket) -> i32;
    fn avcodec_receive_frame(ctx: *mut AvCodecContext, frame: *mut AvFrame) -> i32;
    fn av_packet_alloc() -> *mut AvPacket;
    fn av_packet_free(pkt: *mut *mut AvPacket);
    fn av_packet_unref(pkt: *mut AvPacket);
    fn av_frame_alloc() -> *mut AvFrame;
    fn av_frame_free(frame: *mut *mut AvFrame);
}

const AV_CODEC_ID_H264: i32 = 27;
const AVERROR_EOF: i32 = -541478725; // FFERRTAG('E','O','F',' ')
const AVERROR_EAGAIN: i32 = -11;

// ── Public types ────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DecodedFrame {
    pub nv12_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub send_ts_us: Option<u64>,
    pub recv_time: Option<std::time::Instant>,
}

pub struct VideoDecoder {
    frame_tx: crossbeam_channel::Sender<DecodedFrame>,
    frame_rx: crossbeam_channel::Receiver<DecodedFrame>,
    width: u32,
    height: u32,
}

impl VideoDecoder {
    pub fn new(width: u32, height: u32) -> Self {
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

    pub fn spawn(&self, rx: crossbeam_channel::Receiver<Vec<u8>>) {
        let frame_tx = self.frame_tx.clone();
        let width = self.width;
        let height = self.height;
        std::thread::spawn(move || {
            decode_loop_libavcodec(frame_tx, rx, width, height);
        });
    }
}

// ── NV12 → RGBA (CPU fallback, kept for main.rs compat) ────────────

#[allow(dead_code)]
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

            let uv_idx = uv_row * stride + (col & !1);
            if uv_idx + 1 >= uv_plane.len() {
                i += 4;
                continue;
            }
            let u_val = uv_plane[uv_idx] as i32 - 128;
            let v_val = uv_plane[uv_idx + 1] as i32 - 128;

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

// ── In-process libavcodec decode loop ──────────────────────────────

fn decode_loop_libavcodec(
    frame_tx: crossbeam_channel::Sender<DecodedFrame>,
    rx: crossbeam_channel::Receiver<Vec<u8>>,
    width: u32,
    height: u32,
) {
    let stride = ((width + 31) / 32) * 32;
    let y_size = (stride * height) as usize;
    let uv_size = (stride * height / 2) as usize;
    let total_size = y_size + uv_size;

    info!(
        "libavcodec H.264 decoder initialized: {}x{} stride={} NV12",
        width, height, stride
    );

    loop {
        let codec_name = CString::new("h264").unwrap();
        let codec = unsafe { avcodec_find_decoder_by_name(codec_name.as_ptr() as *const u8) };
        if codec.is_null() {
            error!("libavcodec: h264 decoder not found");
            return;
        }

        let codec_ctx = unsafe { avcodec_alloc_context3(codec) };
        if codec_ctx.is_null() {
            error!("libavcodec: failed to alloc context");
            return;
        }

        let ret = unsafe { avcodec_open2(codec_ctx, codec, std::ptr::null_mut()) };
        if ret < 0 {
            error!("libavcodec: failed to open h264 decoder (err {})", ret);
            unsafe { avcodec_free_context(&mut { codec_ctx }) };
            return;
        }

        let parser = unsafe { av_parser_init(AV_CODEC_ID_H264) };
        if parser.is_null() {
            error!("libavcodec: failed to init H.264 parser");
            unsafe { avcodec_free_context(&mut { codec_ctx }) };
            return;
        }

        let pkt = unsafe { av_packet_alloc() };
        let frame = unsafe { av_frame_alloc() };

        info!(
            "libavcodec H.264 decoder started: {}x{} NV12 (stride={})",
            width, height, stride
        );

        let mut frame_count: u64 = 0;
        let mut _decode_err_count: u32 = 0;

        'decode_loop: loop {
            let nal_data = match rx.recv() {
                Ok(d) => d,
                Err(_) => {
                    info!("Decoder input channel closed");
                    break 'decode_loop;
                }
            };

            let mut parse_offset = 0usize;
            while parse_offset < nal_data.len() {
                let mut out_buf: *mut u8 = std::ptr::null_mut();
                let mut out_size: i32 = 0;

                let consumed = unsafe {
                    av_parser_parse2(
                        parser,
                        codec_ctx,
                        &mut out_buf,
                        &mut out_size,
                        nal_data[parse_offset..].as_ptr(),
                        (nal_data.len() - parse_offset) as i32,
                        -1,
                        -1,
                        0,
                    )
                };

                if consumed < 0 {
                    warn!("libavcodec: parser error {}", consumed);
                    break;
                }
                parse_offset += consumed as usize;

                if out_size > 0 && !out_buf.is_null() {
                    unsafe {
                        (*pkt).data = out_buf;
                        (*pkt).size = out_size;
                    }

                    let send_ret = unsafe { avcodec_send_packet(codec_ctx, pkt) };
                    if send_ret < 0 {
                        _decode_err_count += 1;
                        if _decode_err_count <= 5 {
                            warn!("libavcodec: send_packet error {}", send_ret);
                        }
                        continue;
                    }

                    // Drain all available frames
                    loop {
                        let recv_ret = unsafe { avcodec_receive_frame(codec_ctx, frame) };
                        if recv_ret == AVERROR_EAGAIN || recv_ret == AVERROR_EOF {
                            break;
                        }
                        if recv_ret < 0 {
                            _decode_err_count += 1;
                            if _decode_err_count <= 5 {
                                warn!("libavcodec: receive_frame error {}", recv_ret);
                            }
                            break;
                        }

                        let linesize0 = unsafe { (*frame).linesize[0] } as usize;
                        let linesize1 = unsafe { (*frame).linesize[1] } as usize;
                        let data0 = unsafe { (*frame).data[0] };
                        let data1 = unsafe { (*frame).data[1] };
                        let fw = unsafe { (*frame).width } as usize;
                        let fh = unsafe { (*frame).height } as usize;

                        if data0.is_null() || data1.is_null() {
                            continue;
                        }

                        let mut nv12 = vec![0u8; total_size];

                        // Copy Y plane (row by row for stride mismatch)
                        let copy_w = fw.min(stride as usize);
                        for row in 0..fh {
                            let src = unsafe {
                                std::slice::from_raw_parts(data0.add(row * linesize0), copy_w)
                            };
                            let dst_start = row * stride as usize;
                            nv12[dst_start..dst_start + copy_w].copy_from_slice(src);
                        }

                        // Copy UV plane (NV12: interleaved U/V, half height)
                        let uv_h = fh / 2;
                        let uv_copy_w = copy_w.min(stride as usize);
                        for row in 0..uv_h {
                            let src = unsafe {
                                std::slice::from_raw_parts(data1.add(row * linesize1), uv_copy_w)
                            };
                            let dst_start = y_size + row * stride as usize;
                            nv12[dst_start..dst_start + uv_copy_w].copy_from_slice(src);
                        }

                        let decoded = DecodedFrame {
                            nv12_data: nv12,
                            width: fw as u32,
                            height: fh as u32,
                            stride,
                            send_ts_us: None,
                            recv_time: None,
                        };

                        if frame_tx.try_send(decoded).is_err() {
                            // Queue full, drop frame for low latency
                        }

                        frame_count += 1;
                        if frame_count % 30 == 0 {
                            info!("Decoded {} frames (NV12, libavcodec)", frame_count);
                        }
                    }

                    unsafe {
                        av_packet_unref(pkt);
                    }
                }
            }
        }

        // Cleanup
        unsafe {
            av_parser_close(parser);
            avcodec_free_context(&mut { codec_ctx });
            av_packet_free(&mut { pkt });
            av_frame_free(&mut { frame });
        }

        info!(
            "libavcodec decoder stopped after {} frames, restarting...",
            frame_count
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

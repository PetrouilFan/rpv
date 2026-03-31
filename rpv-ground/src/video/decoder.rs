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
    /// #11: Separate Y/U/V planes — no CPU interleaving, GPU does conversion
    pub y_data: Vec<u8>,
    pub u_data: Vec<u8>,
    pub v_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
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
            // #24: Pin decoder to core 2 for consistent decode latency
            unsafe {
                let mut set: libc::cpu_set_t = std::mem::zeroed();
                libc::CPU_ZERO(&mut set);
                libc::CPU_SET(2, &mut set);
                libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
            }
            decode_loop_libavcodec(frame_tx, rx, width, height);
        });
    }
}

// ── In-process libavcodec decode loop ──────────────────────────────

/// Process a decoded AVFrame into separate Y/U/V planes and send it.
/// #11: No CPU interleaving — output planar YUV for direct GPU upload.
fn process_decoded_frame(
    frame: *mut AvFrame,
    frame_tx: crossbeam_channel::Sender<DecodedFrame>,
    width: u32,
    height: u32,
    frame_count: &mut u64,
) {
    let linesize0 = unsafe { (*frame).linesize[0] } as usize;
    let linesize1 = unsafe { (*frame).linesize[1] } as usize;
    let linesize2 = unsafe { (*frame).linesize[2] } as usize;
    let data0 = unsafe { (*frame).data[0] };
    let data1 = unsafe { (*frame).data[1] };
    let data2 = unsafe { (*frame).data[2] };
    let fw = unsafe { (*frame).width } as usize;
    let fh = unsafe { (*frame).height } as usize;
    let pix_fmt = unsafe { (*frame).format };

    if data0.is_null() || data1.is_null() {
        return;
    }

    if *frame_count == 0 {
        info!(
            "Decoded frame: {}x{}, format={}, linesize=[{},{},{}], data2_null={}",
            fw,
            fh,
            pix_fmt,
            linesize0,
            linesize1,
            linesize2,
            data2.is_null()
        );
    }

    if fw == 0 || fh == 0 || fw > width as usize * 2 || fh > height as usize * 2 {
        return;
    }

    let w = width as usize;
    let h = height as usize;

    // Copy Y plane — row by row if linesize != width
    let mut y_data = vec![0u8; w * h];
    let copy_w = fw.min(w).min(linesize0);
    let copy_h = fh.min(h);
    for row in 0..copy_h {
        let src = unsafe { std::slice::from_raw_parts(data0.add(row * linesize0), copy_w) };
        y_data[row * w..row * w + copy_w].copy_from_slice(src);
    }

    // Copy U and V planes — separate planes, no interleaving
    let uv_w = w / 2;
    let uv_h = h / 2;
    let uv_src_h = if pix_fmt == 1 || pix_fmt == 13 {
        fh
    } else {
        fh / 2
    };
    let uv_copy_h = uv_src_h.min(uv_h);
    let uv_copy_w = (fw / 2).min(uv_w).min(linesize1);
    let step = if uv_src_h > uv_h { 2 } else { 1 };

    let mut u_data = vec![0u8; uv_w * uv_h];
    let mut v_data = vec![0u8; uv_w * uv_h];

    if pix_fmt == 23 {
        // NV12 input: UV is interleaved (U,V,U,V...) — deinterleave into separate planes
        let uv_interleaved_w = fw.min(linesize1);
        for row in 0..uv_copy_h {
            let src =
                unsafe { std::slice::from_raw_parts(data1.add(row * linesize1), uv_interleaved_w) };
            for col in 0..uv_copy_w.min(uv_interleaved_w / 2) {
                u_data[row * uv_w + col] = src[col * 2];
                v_data[row * uv_w + col] = src[col * 2 + 1];
            }
        }
    } else {
        // YUV420P/YUV422P: U and V are in separate planes
        for out_row in 0..uv_copy_h {
            let src_row = out_row * step;
            if src_row * linesize1 < fw * fh {
                let u_src = unsafe {
                    std::slice::from_raw_parts(data1.add(src_row * linesize1), uv_copy_w)
                };
                u_data[out_row * uv_w..out_row * uv_w + uv_copy_w].copy_from_slice(u_src);
            }
            if !data2.is_null() && src_row * linesize2 < fw * fh {
                let v_src = unsafe {
                    std::slice::from_raw_parts(data2.add(src_row * linesize2), uv_copy_w)
                };
                v_data[out_row * uv_w..out_row * uv_w + uv_copy_w].copy_from_slice(v_src);
            }
        }
    }

    let decoded = DecodedFrame {
        y_data,
        u_data,
        v_data,
        width,
        height,
        send_ts_us: None,
        recv_time: Some(std::time::Instant::now()),
    };

    if frame_tx.try_send(decoded).is_err() {}

    *frame_count += 1;
    if *frame_count % 30 == 0 {
        info!("Decoded {} frames (planar YUV, libavcodec)", *frame_count);
    }
}

fn decode_loop_libavcodec(
    frame_tx: crossbeam_channel::Sender<DecodedFrame>,
    rx: crossbeam_channel::Receiver<Vec<u8>>,
    width: u32,
    height: u32,
) {
    info!(
        "libavcodec H.264 decoder initialized: {}x{} planar YUV",
        width, height
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
            "libavcodec H.264 decoder started: {}x{} planar YUV",
            width, height
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
                        // #16: EAGAIN means decoder buffer is full — drain frames, then retry
                        if send_ret == AVERROR_EAGAIN {
                            // Drain all available frames before retrying
                            loop {
                                let recv_ret = unsafe { avcodec_receive_frame(codec_ctx, frame) };
                                if recv_ret == AVERROR_EAGAIN || recv_ret == AVERROR_EOF {
                                    break;
                                }
                                if recv_ret < 0 {
                                    break;
                                }
                                // Process the drained frame (same as below)
                                process_decoded_frame(
                                    frame,
                                    frame_tx.clone(),
                                    width,
                                    height,
                                    &mut frame_count,
                                );
                            }
                            // Retry sending the same packet now that buffer is drained
                            let retry_ret = unsafe { avcodec_send_packet(codec_ctx, pkt) };
                            if retry_ret < 0 {
                                _decode_err_count += 1;
                                if _decode_err_count <= 5 {
                                    warn!("libavcodec: send_packet retry error {}", retry_ret);
                                }
                                continue;
                            }
                        } else {
                            _decode_err_count += 1;
                            if _decode_err_count <= 5 {
                                warn!("libavcodec: send_packet error {}", send_ret);
                            }
                            continue;
                        }
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

                        process_decoded_frame(
                            frame,
                            frame_tx.clone(),
                            width,
                            height,
                            &mut frame_count,
                        );
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

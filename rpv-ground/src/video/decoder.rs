use std::ffi::CString;
use tracing::{error, info, warn};

use ffmpeg_sys_next as ffi;

// FFmpeg constants
const AV_PIX_FMT_NV12: i32 = 23;
const AV_CODEC_ID_H264: i32 = 27;
const AVERROR_EOF: i32 = -0x5445_4f46; // FFERRTAG('E','O','F',' ')
const AVERROR_EAGAIN: i32 = -11;

// ── Public types ────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DecodedFrame {
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
        let (frame_tx, frame_rx) = crossbeam_channel::bounded::<DecodedFrame>(2);
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

// ── Frame processing ────────────────────────────────────────────────

fn process_decoded_frame(
    frame: *mut ffi::AVFrame,
    frame_tx: &crossbeam_channel::Sender<DecodedFrame>,
    width: u32,
    height: u32,
    frame_count: &mut u64,
    y_buf: &mut Vec<u8>,
    u_buf: &mut Vec<u8>,
    v_buf: &mut Vec<u8>,
) {
    let fw = unsafe { (*frame).width } as usize;
    let fh = unsafe { (*frame).height } as usize;
    let pix_fmt = unsafe { (*frame).format };
    let linesize0 = unsafe { (*frame).linesize[0] } as usize;
    let linesize1 = unsafe { (*frame).linesize[1] } as usize;
    let linesize2 = unsafe { (*frame).linesize[2] } as usize;
    let data0 = unsafe { (*frame).data[0] };
    let data1 = unsafe { (*frame).data[1] };
    let data2 = unsafe { (*frame).data[2] };

    if data0.is_null() || data1.is_null() {
        return;
    }
    if fw == 0 || fh == 0 || fw > width as usize * 2 || fh > height as usize * 2 {
        return;
    }

    if *frame_count == 0 {
        info!(
            "Decoded frame: {}x{}, fmt={}, ls=[{},{},{}], d2_null={}",
            fw,
            fh,
            pix_fmt,
            linesize0,
            linesize1,
            linesize2,
            data2.is_null()
        );
    }

    let w = width as usize;
    let h = height as usize;

    // Zero-fill pre-allocated buffers and copy Y plane
    y_buf.fill(0);
    let copy_w = fw.min(w).min(linesize0);
    let copy_h = fh.min(h);
    for row in 0..copy_h {
        let src = unsafe { std::slice::from_raw_parts(data0.add(row * linesize0), copy_w) };
        y_buf[row * w..row * w + copy_w].copy_from_slice(src);
    }

    // U/V planes
    let uv_w = w / 2;
    let uv_h = h / 2;
    u_buf.fill(0);
    v_buf.fill(0);
    let uv_copy_w = (fw / 2).min(uv_w).min(linesize1);
    let uv_v_copy_w = (fw / 2).min(uv_w).min(linesize2);

    if pix_fmt == AV_PIX_FMT_NV12 as i32 {
        let uv_interleaved_w = fw.min(linesize1);
        let uv_copy_h = fh.min(uv_h);
        for row in 0..uv_copy_h {
            let src =
                unsafe { std::slice::from_raw_parts(data1.add(row * linesize1), uv_interleaved_w) };
            for col in 0..uv_copy_w.min(uv_interleaved_w / 2) {
                u_buf[row * uv_w + col] = src[col * 2];
                v_buf[row * uv_w + col] = src[col * 2 + 1];
            }
        }
    } else {
        // YUV420P or similar planar
        let uv_src_h = fh / 2;
        let uv_copy_h = uv_src_h.min(uv_h);
        for row in 0..uv_copy_h {
            if !data1.is_null() {
                let u_src =
                    unsafe { std::slice::from_raw_parts(data1.add(row * linesize1), uv_copy_w) };
                u_buf[row * uv_w..row * uv_w + uv_copy_w].copy_from_slice(u_src);
            }
            if !data2.is_null() {
                let v_src =
                    unsafe { std::slice::from_raw_parts(data2.add(row * linesize2), uv_v_copy_w) };
                v_buf[row * uv_w..row * uv_w + uv_v_copy_w].copy_from_slice(v_src);
            }
        }
    }

    let decoded = DecodedFrame {
        y_data: y_buf.clone(),
        u_data: u_buf.clone(),
        v_data: v_buf.clone(),
        width,
        height,
        send_ts_us: None,
        recv_time: Some(std::time::Instant::now()),
    };

    let _ = frame_tx.try_send(decoded);

    *frame_count += 1;
    if *frame_count % 30 == 0 {
        info!("Decoded {} frames (planar YUV, libavcodec)", *frame_count);
    }
}

// ── Decode loop ─────────────────────────────────────────────────────

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
        let codec = unsafe {
            let name = CString::new("h264").unwrap();
            ffi::avcodec_find_decoder_by_name(name.as_ptr() as *const _)
        };
        if codec.is_null() {
            error!("libavcodec: h264 decoder not found");
            return;
        }

        let codec_ctx = unsafe { ffi::avcodec_alloc_context3(codec) };
        if codec_ctx.is_null() {
            error!("libavcodec: failed to alloc context");
            return;
        }

        // Low-latency: disable B-frame reordering and frame threading
        unsafe {
            (*codec_ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*codec_ctx).thread_count = 1;
            (*codec_ctx).thread_type = 0;
        }

        let ret = unsafe { ffi::avcodec_open2(codec_ctx, codec, std::ptr::null_mut()) };
        if ret < 0 {
            error!("libavcodec: failed to open h264 decoder (err {})", ret);
            unsafe { ffi::avcodec_free_context(&mut { codec_ctx }) };
            return;
        }

        let mut parser = unsafe { ffi::av_parser_init(AV_CODEC_ID_H264) };
        if parser.is_null() {
            error!("libavcodec: failed to init H.264 parser");
            unsafe { ffi::avcodec_free_context(&mut { codec_ctx }) };
            return;
        }

        let pkt = unsafe { ffi::av_packet_alloc() };
        let frame = unsafe { ffi::av_frame_alloc() };

        info!(
            "libavcodec H.264 decoder started: {}x{} planar YUV",
            width, height
        );

        let mut frame_count: u64 = 0;
        let mut nal_recv_count: u64 = 0;

        // Pre-allocate YUV buffers to avoid 3 allocations per frame at 30fps
        let w = width as usize;
        let h = height as usize;
        let mut y_buf = vec![0u8; w * h];
        let mut u_buf = vec![0u8; (w / 2) * (h / 2)];
        let mut v_buf = vec![0u8; (w / 2) * (h / 2)];

        'decode_loop: loop {
            let nal_data = match rx.recv() {
                Ok(d) => d,
                Err(_) => {
                    info!("Decoder input channel closed");
                    break 'decode_loop;
                }
            };

            nal_recv_count += 1;
            if nal_recv_count <= 5 {
                info!(
                    "DECODER chunk #{}: {} bytes, first8={:02x?}",
                    nal_recv_count,
                    nal_data.len(),
                    &nal_data[..8.min(nal_data.len())]
                );
            }

            let mut parse_offset = 0usize;
            while parse_offset < nal_data.len() {
                let mut out_buf: *mut u8 = std::ptr::null_mut();
                let mut out_size: i32 = 0;

                let consumed = unsafe {
                    ffi::av_parser_parse2(
                        parser,
                        codec_ctx,
                        &mut out_buf,
                        &mut out_size,
                        nal_data[parse_offset..].as_ptr(),
                        (nal_data.len() - parse_offset) as i32,
                        ffi::AV_NOPTS_VALUE as _,
                        ffi::AV_NOPTS_VALUE as _,
                        -1,
                    )
                };

                if nal_recv_count <= 5 && (consumed != 0 || out_size != 0) {
                    info!(
                        "PARSE: consumed={}, out_size={}, in_len={}",
                        consumed,
                        out_size,
                        nal_data.len() - parse_offset
                    );
                }

                if consumed < 0 {
                    warn!("libavcodec: parser error {}, resetting", consumed);
                    unsafe { ffi::av_parser_close(parser) };
                    parser = unsafe { ffi::av_parser_init(AV_CODEC_ID_H264) };
                    if parser.is_null() {
                        error!("libavcodec: failed to re-init parser");
                        break 'decode_loop;
                    }
                    break;
                }

                if consumed == 0 && out_size == 0 {
                    // Parser needs more data
                    break;
                }

                parse_offset += consumed as usize;

                if out_size > 0 && !out_buf.is_null() {
                    unsafe {
                        (*pkt).data = out_buf;
                        (*pkt).size = out_size;
                    }

                    if nal_recv_count <= 5 {
                        info!(
                            "SEND pkt: size={}, out_buf_null={}",
                            out_size,
                            out_buf.is_null()
                        );
                    }

                    let send_ret = unsafe { ffi::avcodec_send_packet(codec_ctx, pkt) };
                    if send_ret < 0 {
                        if send_ret == AVERROR_EAGAIN {
                            // Drain frames, then retry
                            loop {
                                let r = unsafe { ffi::avcodec_receive_frame(codec_ctx, frame) };
                                if r == AVERROR_EAGAIN || r == AVERROR_EOF || r < 0 {
                                    break;
                                }
                                process_decoded_frame(
                                    frame,
                                    &frame_tx,
                                    width,
                                    height,
                                    &mut frame_count,
                                    &mut y_buf,
                                    &mut u_buf,
                                    &mut v_buf,
                                );
                            }
                            let retry = unsafe { ffi::avcodec_send_packet(codec_ctx, pkt) };
                            if retry < 0 {
                                continue;
                            }
                        } else {
                            continue;
                        }
                    }

                    // Drain decoded frames
                    loop {
                        let recv_ret = unsafe { ffi::avcodec_receive_frame(codec_ctx, frame) };
                        if recv_ret == AVERROR_EAGAIN || recv_ret == AVERROR_EOF {
                            break;
                        }
                        if recv_ret < 0 {
                            warn!("libavcodec: receive_frame error {}", recv_ret);
                            break;
                        }
                        if nal_recv_count <= 5 {
                            info!(
                                "RECV frame: ret={}, width={}, height={}",
                                recv_ret,
                                unsafe { (*frame).width },
                                unsafe { (*frame).height }
                            );
                        }
                        process_decoded_frame(
                            frame,
                            &frame_tx,
                            width,
                            height,
                            &mut frame_count,
                            &mut y_buf,
                            &mut u_buf,
                            &mut v_buf,
                        );
                    }

                    unsafe {
                        ffi::av_packet_unref(pkt);
                    }
                }
            }
        }

        // Cleanup and restart
        unsafe {
            ffi::av_parser_close(parser);
            ffi::avcodec_free_context(&mut { codec_ctx });
            ffi::av_packet_free(&mut { pkt });
            ffi::av_frame_free(&mut { frame });
        }

        info!(
            "libavcodec decoder stopped after {} frames, restarting...",
            frame_count
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

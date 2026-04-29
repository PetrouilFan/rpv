use tracing::{error, info, warn};

use ffmpeg_sys_next as ffi;

// NOTE: These FFmpeg constants are hardcoded because ffmpeg_sys_next doesn't re-export them.
// These values are stable across FFmpeg versions:
// - AV_PIX_FMT_YUV420P = 0 (standard YUV 4:2:0 planar)
// - AV_PIX_FMT_NV12 = 23 (YUV 4:2:0 with interleaved UV)
// - AVERROR_EAGAIN = -11 (resource temporarily unavailable)
// - AVERROR_EOF = -0x54454F46 = -1414545062 (end of file)
//
// Using bindgen to generate these from FFmpeg headers would be more robust,
// but requires ffbuild or manual header installation.
const AV_PIX_FMT_NV12: i32 = 23;
const AV_PIX_FMT_YUV420P: i32 = 0;
const AVERROR_EOF: i32 = -0x5445_4f46;
const AVERROR_EAGAIN: i32 = -11;

// ── Public types ────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DecodedFrame {
    pub y_data: Vec<u8>,
    pub u_data: Vec<u8>,
    pub v_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub y_stride: u32,
    pub u_stride: u32,
    pub v_stride: u32,
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
    // linesize can be negative (indicating padding direction) - check before casting
    let ls0 = unsafe { (*frame).linesize[0] };
    let ls1 = unsafe { (*frame).linesize[1] };
    let ls2 = unsafe { (*frame).linesize[2] };
    if ls2 < 0 {
        tracing::warn!("Negative linesize for V plane: {}", ls2);
        return;
    }
    if ls0 < 0 || ls1 < 0 || ls2 < 0 {
        return;
    }
    let linesize0 = ls0 as usize;
    let linesize1 = ls1 as usize;
    let linesize2 = ls2 as usize;
    let data0 = unsafe { (*frame).data[0] };
    let data1 = unsafe { (*frame).data[1] };
    let data2 = unsafe { (*frame).data[2] };

    if data0.is_null() || data1.is_null() || data2.is_null() {
        return;
    }
    if fw == 0 || fh == 0 || fw > width as usize * 2 || fh > height as usize * 2 {
        return;
    }

    if *frame_count == 0 {
        info!(
            "DECODED: {}x{}, fmt={} (0=YUV420P, 23=NV12), ls=[{},{},{}], d2_null={}",
            fw,
            fh,
            pix_fmt,
            linesize0,
            linesize1,
            linesize2,
            data2.is_null()
        );
    }

    // Detailed diagnostics for first few frames
    if *frame_count < 5 && !y_buf.is_empty() {
        let y1 = y_buf[0];
        let y2 = y_buf[1];
        let y3 = y_buf[2];
        let y4 = if fw < y_buf.len() { y_buf[fw] } else { 0 };
        info!("FRAME {}: Y=[{},{},{},{}], U=[{},{},{},{}], V=[{},{},{},{}]",
            *frame_count, y1, y2, y3, y4,
            u_buf.get(0).copied().unwrap_or(128), u_buf.get(1).copied().unwrap_or(128),
            u_buf.get(2).copied().unwrap_or(128), u_buf.get(3).copied().unwrap_or(128),
            v_buf.get(0).copied().unwrap_or(128), v_buf.get(1).copied().unwrap_or(128),
            v_buf.get(2).copied().unwrap_or(128), v_buf.get(3).copied().unwrap_or(128));
    }

    // Use actual decoded frame dimensions, not config dimensions
    let w = fw;
    let h = fh;

    // Resize buffers to include padding
    let y_size = linesize0 * h;
    y_buf.resize(y_size, 0);
    // Copy Y plane with padding
    for row in 0..h {
        let src = unsafe { std::slice::from_raw_parts(data0.add(row * linesize0), linesize0) };
        let dst_start = row * linesize0;
        y_buf[dst_start..dst_start + linesize0].copy_from_slice(src);
    }

    let uv_w = w / 2;
    let uv_h = h / 2;

    if pix_fmt == AV_PIX_FMT_NV12 as i32 {
        // NV12: copy interleaved UV with padding, then deinterleave
        let uv_size = linesize1 * uv_h;
        let mut uv_buf = vec![0u8; uv_size];
        for row in 0..uv_h {
            let src = unsafe { std::slice::from_raw_parts(data1.add(row * linesize1), linesize1) };
            let dst_start = row * linesize1;
            uv_buf[dst_start..dst_start + linesize1].copy_from_slice(src);
        }
        // Deinterleave into padded U and V
        let u_size = linesize1 * uv_h;
        u_buf.resize(u_size, 0);
        v_buf.resize(u_size, 0);
        for row in 0..uv_h {
            let src = &uv_buf[row * linesize1..(row + 1) * linesize1];
            for col in 0..uv_w {
                u_buf[row * linesize1 + col] = src[col * 2];
                v_buf[row * linesize1 + col] = src[col * 2 + 1];
            }
        }
    } else {
        // YUV420P or fallback: copy U and V with padding
        let u_size = linesize1 * uv_h;
        u_buf.resize(u_size, 0);
        for row in 0..uv_h {
            let src = unsafe { std::slice::from_raw_parts(data1.add(row * linesize1), linesize1) };
            let dst_start = row * linesize1;
            u_buf[dst_start..dst_start + linesize1].copy_from_slice(src);
        }
        let v_size = match linesize2.checked_mul(uv_h) {
            Some(val) => val,
            None => {
                tracing::warn!("Integer overflow in V plane size calculation: linesize2={}, uv_h={}", linesize2, uv_h);
                return;
            }
        };
        v_buf.resize(v_size, 0);
        for row in 0..uv_h {
            let src = unsafe { std::slice::from_raw_parts(data2.add(row * linesize2), linesize2) };
            let dst_start = row * linesize2;
            v_buf[dst_start..dst_start + linesize2].copy_from_slice(src);
        }
    }

    // Clone the data
    let y_data = y_buf.clone();
    let u_data = u_buf.clone();
    let v_data = v_buf.clone();

    let decoded = DecodedFrame {
        y_data,
        u_data,
        v_data,
        width: fw as u32,
        height: fh as u32,
        y_stride: linesize0 as u32,
        u_stride: linesize1 as u32,
        v_stride: if pix_fmt == AV_PIX_FMT_YUV420P as i32 { linesize2 as u32 } else { linesize1 as u32 },
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
    let codec_name = std::ffi::CString::new("h264").unwrap();
    // Try hardware-accelerated decoders first
    let hw_decoder_names = ["h264_vaapi", "h264_v4l2m2m", "h264_videotoolbox", "h264_cuvid", "h264_qsv"];
    let mut codec = std::ptr::null();
    let mut selected_decoder = "h264";

    for hw_name in &hw_decoder_names {
        let cstr = std::ffi::CString::new(*hw_name).unwrap();
        codec = unsafe { ffi::avcodec_find_decoder_by_name(cstr.as_ptr()) };
        if !codec.is_null() {
            selected_decoder = *hw_name;
            info!("Using hardware decoder: {}", selected_decoder);
            break;
        }
    }

    if codec.is_null() {
        // Fallback to software decoder
        let codec_name = std::ffi::CString::new("h264").unwrap();
        codec = unsafe { ffi::avcodec_find_decoder_by_name(codec_name.as_ptr() as *const _) };
        selected_decoder = "h264 (software)";
    }

    if codec.is_null() {
        error!("libavcodec: h264 decoder not found");
        return;
    }

    let codec_ctx = unsafe { ffi::avcodec_alloc_context3(codec) };
    if codec_ctx.is_null() {
        error!("libavcodec: failed to alloc context");
        return;
    }
    unsafe {
        (*codec_ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
        (*codec_ctx).thread_count = 1;
        (*codec_ctx).thread_type = 0;
        (*codec_ctx).err_recognition = 1;
        (*codec_ctx).flags2 |= ffi::AV_CODEC_FLAG2_SHOW_ALL as i32;
    }

    let ret = unsafe { ffi::avcodec_open2(codec_ctx, codec, std::ptr::null_mut()) };
    if ret < 0 {
        error!("libavcodec: failed to open {} decoder (err {})", selected_decoder, ret);
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

    let w = width as usize;
    let h = height as usize;
    let mut y_buf = vec![0u8; w * h];
    let mut u_buf = vec![0u8; (w / 2) * (h / 2)];
    let mut v_buf = vec![0u8; (w / 2) * (h / 2)];

    loop {
        let mut nal_data = match rx.recv() {
            Ok(d) => d,
            Err(_) => {
                info!("Decoder input channel closed");
                break;
            }
        };

        let mut nal_start = 0usize;
        for i in 0..nal_data.len().saturating_sub(3) {
            if nal_data[i] == 0x00 && nal_data[i + 1] == 0x00 {
                if i + 2 < nal_data.len() && nal_data[i + 2] == 0x01 {
                    nal_start = i;
                    break;
                } else if i + 3 < nal_data.len()
                    && nal_data[i + 2] == 0x00
                    && nal_data[i + 3] == 0x01
                {
                    nal_start = i;
                    break;
                }
            }
        }
        if nal_start > 0 {
            nal_data = nal_data[nal_start..].to_vec();
        }

        nal_recv_count += 1;
        if nal_recv_count <= 5 {
            info!(
                "DECODER chunk #{}: {} bytes, first8={:02x?}",
                nal_recv_count,
                nal_data.len(),
                &nal_data[..8.min(nal_data.len())]
            );
        }

        unsafe {
            // Allocate buffer with FFmpeg's allocator to avoid mismatched free
            let buf_len = nal_data.len();
            let buffer = ffi::av_malloc(buf_len) as *mut u8;
            if buffer.is_null() {
                error!("av_malloc failed for packet data (size {})", buf_len);
                continue;
            }
            std::ptr::copy_nonoverlapping(nal_data.as_ptr(), buffer, buf_len);
            ffi::av_packet_from_data(pkt, buffer, buf_len as i32);
        }

        let send_ret = unsafe { ffi::avcodec_send_packet(codec_ctx, pkt) };
        if send_ret < 0 {
            if send_ret == AVERROR_EAGAIN {
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
                    unsafe {
                        ffi::av_frame_unref(frame);
                    }
                }
                let retry = unsafe { ffi::avcodec_send_packet(codec_ctx, pkt) };
                if retry < 0 {
                    warn!("libavcodec: avcodec_send_packet retry failed with {}", retry);
                    unsafe {
                        ffi::av_packet_unref(pkt);
                    }
                    continue;
                }
            } else {
                warn!("libavcodec: avcodec_send_packet failed with {}", send_ret);
                unsafe {
                    ffi::av_packet_unref(pkt);
                }
                continue;
            }
        }

        loop {
            let recv_ret = unsafe { ffi::avcodec_receive_frame(codec_ctx, frame) };
            if recv_ret == AVERROR_EAGAIN || recv_ret == AVERROR_EOF {
                break;
            }
            if recv_ret < 0 {
                warn!("libavcodec: receive_frame error {}", recv_ret);
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
            unsafe {
                ffi::av_frame_unref(frame);
            }
        }

        unsafe {
            ffi::av_packet_unref(pkt);
        }
    }

    unsafe {
        ffi::av_packet_unref(pkt);
        ffi::avcodec_free_context(&mut { codec_ctx });
        ffi::av_packet_free(&mut { pkt });
        ffi::av_frame_free(&mut { frame });
    }

    info!("libavcodec decoder stopped after {} frames", frame_count);
}

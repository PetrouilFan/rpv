use tracing::{error, info, warn};

use ffmpeg_sys_next as ffi;

const AV_PIX_FMT_NV12: i32 = 23;
const AV_PIX_FMT_YUV420P: i32 = 0; // YUV420P will be detected at runtime
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

    // Resize buffers if actual frame is larger than current allocation
    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);
    if y_buf.len() < y_size {
        y_buf.resize(y_size, 0);
    }
    if u_buf.len() < uv_size {
        u_buf.resize(uv_size, 0);
    }
    if v_buf.len() < uv_size {
        v_buf.resize(uv_size, 0);
    }

    // Zero-fill and copy Y plane
    y_buf[..y_size].fill(0);
    for row in 0..h {
        let src_len = w.min(linesize0 as usize);
        let src =
            unsafe { std::slice::from_raw_parts(data0.add(row * linesize0 as usize), src_len) };
        let dst_start = row * w;
        let dst_end = dst_start + w.min(src_len);
        y_buf[dst_start..dst_end].copy_from_slice(src);
    }

    // U/V planes - fix stride handling
    let uv_w = w / 2;
    let uv_h = h / 2;
    u_buf[..uv_size].fill(0);
    v_buf[..uv_size].fill(0);
    
    // For YUV420P: linesize is bytes per row (row stride in bytes)
    // For NV12: linesize1 is bytes for interleaved UV, so UV pixels = linesize1 / 2
    let uv_copy_w = if pix_fmt == AV_PIX_FMT_NV12 as i32 {
        (fw / 2).min(uv_w).min(linesize1 as usize / 2)
    } else {
        (fw / 2).min(uv_w).min(linesize1 as usize)
    };
    let uv_v_copy_w = if pix_fmt == AV_PIX_FMT_NV12 as i32 {
        (fw / 2).min(uv_w).min(linesize2 as usize / 2)
    } else {
        (fw / 2).min(uv_w).min(linesize2 as usize)
    };

    if pix_fmt == AV_PIX_FMT_NV12 as i32 {
        // NV12: UV interleaved, linesize1 is bytes, pixels = linesize1 / 2
        let uv_interleaved_bytes = fw.min(linesize1 as usize);
        let uv_interleaved_pixels = uv_interleaved_bytes / 2;
        let uv_copy_h = (fh / 2).min(uv_h);
        for row in 0..uv_copy_h {
            let src =
                unsafe { std::slice::from_raw_parts(data1.add(row * linesize1 as usize), uv_interleaved_bytes) };
            for col in 0..uv_copy_w.min(uv_interleaved_pixels) {
                if col * 2 + 1 < uv_interleaved_bytes {
                    u_buf[row * uv_w + col] = src[col * 2];
                    v_buf[row * uv_w + col] = src[col * 2 + 1];
                }
            }
        }
    } else if pix_fmt == AV_PIX_FMT_YUV420P as i32 {
        // Standard YUV420P planar format
        let uv_src_h = fh / 2;
        let uv_copy_h = uv_src_h.min(uv_h as usize);
        for row in 0..uv_copy_h {
            if !data1.is_null() {
                let u_src_len = uv_copy_w.min(linesize1 as usize);
                let u_src =
                    unsafe { std::slice::from_raw_parts(data1.add(row * linesize1 as usize), u_src_len) };
                let dst_start = row * uv_w;
                let dst_end = dst_start + u_src_len;
                u_buf[dst_start..dst_end].copy_from_slice(u_src);
            }
            if !data2.is_null() {
                let v_src_len = uv_v_copy_w.min(linesize2 as usize);
                let v_src =
                    unsafe { std::slice::from_raw_parts(data2.add(row * linesize2 as usize), v_src_len) };
                let dst_start = row * uv_w;
                let dst_end = dst_start + v_src_len;
                v_buf[dst_start..dst_end].copy_from_slice(v_src);
            }
        }
    } else {
        // Fallback for other formats - log warning
        warn!("Unknown pixel format: {}, attempting YUV420P fallback", pix_fmt);
        let uv_src_h = fh / 2;
        let uv_copy_h = uv_src_h.min(uv_h as usize);
        for row in 0..uv_copy_h {
            if !data1.is_null() {
                let u_src_len = uv_copy_w.min(linesize1 as usize);
                let u_src =
                    unsafe { std::slice::from_raw_parts(data1.add(row * linesize1 as usize), u_src_len) };
                let dst_start = row * uv_w;
                let dst_end = dst_start + u_src_len;
                u_buf[dst_start..dst_end].copy_from_slice(u_src);
            }
            if !data2.is_null() {
                let v_src_len = uv_v_copy_w.min(linesize2 as usize);
                let v_src =
                    unsafe { std::slice::from_raw_parts(data2.add(row * linesize2 as usize), v_src_len) };
                let dst_start = row * uv_w;
                let dst_end = dst_start + v_src_len;
                v_buf[dst_start..dst_end].copy_from_slice(v_src);
            }
        }
    }

    // Create output frames, transferring ownership of the YUV data
    // without extra copying. We use Vec::from_raw_parts to take the existing
    // allocation and leave a new buffer for reuse.
    let y_data = {
        let old_y = std::mem::replace(&mut *y_buf, Vec::new());
        let ptr = old_y.as_ptr() as *mut u8;
        let len = y_size;
        let cap = old_y.capacity();
        std::mem::forget(old_y);
        unsafe { Vec::from_raw_parts(ptr, len, cap) }
    };
    let u_data = {
        let old_u = std::mem::replace(&mut *u_buf, Vec::new());
        let ptr = old_u.as_ptr() as *mut u8;
        let len = uv_size;
        let cap = old_u.capacity();
        std::mem::forget(old_u);
        unsafe { Vec::from_raw_parts(ptr, len, cap) }
    };
    let v_data = {
        let old_v = std::mem::replace(&mut *v_buf, Vec::new());
        let ptr = old_v.as_ptr() as *mut u8;
        let len = uv_size;
        let cap = old_v.capacity();
        std::mem::forget(old_v);
        unsafe { Vec::from_raw_parts(ptr, len, cap) }
    };

    let decoded = DecodedFrame {
        y_data,
        u_data,
        v_data,
        width: fw as u32,
        height: fh as u32,
        send_ts_us: None,
        recv_time: Some(std::time::Instant::now()),
    };

    let _ = frame_tx.try_send(decoded);

    // Buffers are now empty. Restore capacity for next frame.
    if y_buf.capacity() < y_size {
        y_buf.reserve(y_size);
    }
    if u_buf.capacity() < uv_size {
        u_buf.reserve(uv_size);
    }
    if v_buf.capacity() < uv_size {
        v_buf.reserve(uv_size);
    }

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
    let codec = unsafe { ffi::avcodec_find_decoder_by_name(codec_name.as_ptr() as *const _) };
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
        error!("libavcodec: failed to open h264 decoder (err {})", ret);
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
            ffi::av_packet_unref(pkt);
            ffi::av_new_packet(pkt, nal_data.len() as i32);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(nal_data.as_ptr(), (*pkt).data, nal_data.len());
            (*pkt).size = nal_data.len() as i32;
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
                    unsafe {
                        ffi::av_packet_unref(pkt);
                    }
                    continue;
                }
            } else {
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
        ffi::avcodec_free_context(&mut { codec_ctx });
        ffi::av_packet_free(&mut { pkt });
        ffi::av_frame_free(&mut { frame });
    }

    info!("libavcodec decoder stopped after {} frames", frame_count);
}

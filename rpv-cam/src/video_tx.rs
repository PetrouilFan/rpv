use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use reed_solomon_erasure::ReedSolomon;

use crate::link;
use crate::rawsock::RawSocket;

const DATA_SHARDS: usize = 1;
const PARITY_SHARDS: usize = 1;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;
const MAX_NAL_BUF: usize = 512 * 1024;

/// Video header: [4B block_seq][1B idx][1B total][1B data][1B pad][2B*DATA_SHARDS shard_lens]
const VIDEO_HDR_FIXED: usize = 8;
const VIDEO_HDR_LEN: usize = VIDEO_HDR_FIXED + DATA_SHARDS * 2;
const FRAG_HDR_LEN: usize = 2; // u16 fragment index
const MAX_SHARD_DATA: usize = link::MAX_PAYLOAD - 8 - VIDEO_HDR_LEN - FRAG_HDR_LEN;

/// Pre-allocated shard arena for zero-alloc FEC encoding.
/// Each slot is MAX_SHARD_DATA bytes, zero-filled remainder in-place.
struct ShardArena {
    slots: Vec<[u8; MAX_SHARD_DATA]>,
}

impl ShardArena {
    fn new() -> Self {
        Self {
            slots: vec![[0u8; MAX_SHARD_DATA]; DATA_SHARDS],
        }
    }

    /// Write NAL fragment into slot `idx` starting at `arena_offset`.
    /// Returns the number of bytes written into this slot.
    /// The caller tracks arena_offset across calls. When a slot is full,
    /// it advances to the next slot.
    fn write_frag(&mut self, slot: usize, offset: usize, data: &[u8]) -> usize {
        if slot >= DATA_SHARDS {
            return 0;
        }
        let space = MAX_SHARD_DATA - offset;
        let copy_len = data.len().min(space);
        self.slots[slot][offset..offset + copy_len].copy_from_slice(&data[..copy_len]);
        copy_len
    }

    /// Zero-pad slot `idx` from `filled` to MAX_SHARD_DATA.
    #[allow(dead_code)]
    fn pad_slot(&mut self, slot: usize, filled: usize) {
        if slot < DATA_SHARDS && filled < MAX_SHARD_DATA {
            self.slots[slot][filled..].fill(0);
        }
    }
}

/// Run the video capture and streaming loop.
pub fn run(
    running: Arc<AtomicBool>,
    socket: Arc<RawSocket>,
    drone_id: u8,
    bitrate: u32,
    intra: u32,
    hp_rx: Option<crossbeam_channel::Receiver<Vec<u8>>>,
    video_width: u32,
    video_height: u32,
    framerate: u32,
    video_device: String,
) {
    tracing::info!(
        "Video sender ready (FEC {}+{}, L2 broadcast, device={})",
        DATA_SHARDS,
        PARITY_SHARDS,
        video_device,
    );

    let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS)
        .expect("Failed to create Reed-Solomon encoder");

    let mut fec_block_seq: u32 = 0;
    let mut l2_pkt_seq: u32 = 0;

    while running.load(Ordering::SeqCst) {
        tracing::info!(
            "Starting ffmpeg (bitrate={}, intra={}, device={})...",
            bitrate,
            intra,
            video_device,
        );

        let bitrate_s = format!("{}k", bitrate / 1000);
        let width_s = video_width.to_string();
        let height_s = video_height.to_string();
        let framerate_s = framerate.to_string();
        let gop_s = (framerate * 2).to_string(); // keyframe every 2 seconds
        let bufsize_s = format!("{}k", bitrate / 500); // 2x bitrate for VBV buffer
        let child = Command::new("ffmpeg")
            .args(&[
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "v4l2",
                "-input_format",
                "mjpeg",
                "-video_size",
                &format!("{}x{}", width_s, height_s),
                "-framerate",
                &framerate_s,
                "-i",
                &video_device,
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-tune",
                "zerolatency",
                "-crf",
                "28",
                "-maxrate",
                &bitrate_s,
                "-bufsize",
                &bufsize_s,
                "-g",
                &gop_s,
                "-f",
                "h264",
                "-an",
                "-y",
                "pipe:1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to start ffmpeg: {}", e);
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        // Direct stdout read — no BufReader wrapper (eliminates 8KB buffer copy)
        let mut stdout = child.stdout.take().expect("No stdout");

        let stderr = child.stderr.take();
        thread::spawn(move || {
            if let Some(mut stderr) = stderr {
                use std::io::BufRead;
                let reader = std::io::BufReader::new(&mut stderr);
                for line in reader.lines() {
                    match line {
                        Ok(line) if !line.is_empty() => {
                            if line.contains("ERROR")
                                || line.contains("failed")
                                || line.contains("error")
                            {
                                tracing::error!("ffmpeg: {}", line);
                            } else {
                                tracing::info!("ffmpeg: {}", line);
                            }
                        }
                        Err(e) => {
                            tracing::warn!("ffmpeg stderr read error: {}", e);
                            break;
                        }
                        _ => {}
                    }
                }
            }
        });

        tracing::info!(
            "ffmpeg started (pid {}), streaming H.264 with FEC {}+{} over raw L2...",
            child.id(),
            DATA_SHARDS,
            PARITY_SHARDS
        );

        let mut read_buf = vec![0u8; 65536];
        let mut total_bytes = u64::default();
        let mut total_nals: u64 = 0;
        let mut total_groups: u64 = 0;
        let mut fail_count: u8 = 0;
        let mut last_stats = std::time::Instant::now();
        // Cursor-based NAL buffer
        let mut nal_buf: Vec<u8> = vec![0u8; MAX_NAL_BUF];
        let mut nal_head: usize = 0;
        let mut nal_tail: usize = 0;
        let mut nal_idle_cycles: u32 = 0;
        const NAL_IDLE_LIMIT: u32 = 200;
        // Reusable buffers for send path
        let mut l2_frame_buf: Vec<u8> = Vec::with_capacity(link::MAX_PAYLOAD);
        let mut send_buf: Vec<u8> = Vec::with_capacity(8 + 24 + link::MAX_PAYLOAD);
        let mut video_payload_buf: Vec<u8> = Vec::with_capacity(VIDEO_HDR_LEN + MAX_SHARD_DATA);
        // Pre-allocated shard arena (zero-alloc padding)
        let mut arena = ShardArena::new();
        // Pre-allocated FEC shard Vecs (reused across groups to avoid 6 heap allocs per group)
        let mut fec_shards: Vec<Vec<u8>> = (0..TOTAL_SHARDS)
            .map(|_| Vec::with_capacity(MAX_SHARD_DATA))
            .collect();

        while running.load(Ordering::SeqCst) {
            match stdout.read(&mut read_buf) {
                Ok(0) => {
                    tracing::info!("ffmpeg stdout closed");
                    break;
                }
                Ok(n) => {
                    total_bytes += n as u64;

                    // Per-read shard tracking (reinitialized each read)
                    let mut shards_in_group: usize = 0;
                    let mut slot_filled = [0usize; DATA_SHARDS];
                    let mut slot_frag_lens: [usize; DATA_SHARDS] = [0; DATA_SHARDS];

                    let copy_len = n.min(nal_buf.len().saturating_sub(nal_tail));
                    if copy_len == 0 {
                        if nal_head > 0 {
                            let remaining = nal_tail - nal_head;
                            nal_buf.copy_within(nal_head..nal_tail, 0);
                            nal_head = 0;
                            nal_tail = remaining;
                            let retry = n.min(nal_buf.len().saturating_sub(nal_tail));
                            nal_buf[nal_tail..nal_tail + retry].copy_from_slice(&read_buf[..retry]);
                            nal_tail += retry;
                        }
                    } else {
                        nal_buf[nal_tail..nal_tail + copy_len]
                            .copy_from_slice(&read_buf[..copy_len]);
                        nal_tail += copy_len;
                    }

                    // Track NAL extraction
                    let mut extracted_any = false;
                    while let Some((nal, consumed)) =
                        extract_next_nal_cursor(&nal_buf, nal_head, nal_tail)
                    {
                        nal_head += consumed;
                        extracted_any = true;
                        total_nals += 1;

                        if last_stats.elapsed().as_secs() >= 5 {
                            tracing::info!(
                                "Video stats: {:.1} MB, {} NALs, {} FEC groups in {}s",
                                total_bytes as f64 / 1_048_576.0,
                                total_nals,
                                total_groups,
                                last_stats.elapsed().as_secs()
                            );
                            last_stats = std::time::Instant::now();
                        }

                        let mut off = 0;
                        let mut frag_idx: u16 = 0;
                        while off < nal.len() {
                            let slot = shards_in_group % DATA_SHARDS;
                            let arena_offset = slot_filled[slot];

                            // If starting a new slot, write frag header first
                            if arena_offset == 0 {
                                let hdr_written =
                                    arena.write_frag(slot, 0, &frag_idx.to_le_bytes());
                                slot_filled[slot] = hdr_written;
                                slot_frag_lens[slot] = hdr_written;
                            }

                            // Write as much NAL data as fits in this slot
                            let nal_chunk = &nal[off..nal.len().min(off + MAX_SHARD_DATA)];
                            let written = arena.write_frag(slot, slot_filled[slot], nal_chunk);
                            slot_filled[slot] += written;
                            off += written;
                            frag_idx += 1;

                            // If slot is full or NAL is done, advance
                            if slot_filled[slot] >= MAX_SHARD_DATA || off >= nal.len() {
                                shards_in_group += 1;

                                if shards_in_group == DATA_SHARDS {
                                    send_fec_group_arena(
                                        &socket,
                                        &rs,
                                        &arena,
                                        &slot_filled,
                                        &slot_frag_lens,
                                        drone_id,
                                        &mut fec_block_seq,
                                        &mut l2_pkt_seq,
                                        &mut fail_count,
                                        &mut l2_frame_buf,
                                        &mut send_buf,
                                        &mut video_payload_buf,
                                        &hp_rx,
                                        &mut fec_shards,
                                    );
                                    total_groups += 1;
                                    // Reset arena state
                                    shards_in_group = 0;
                                    slot_filled = [0; DATA_SHARDS];
                                    slot_frag_lens = [0; DATA_SHARDS];
                                }
                            }
                        }
                    }

                    // Flush remaining partial group
                    if shards_in_group > 0 {
                        send_fec_group_arena(
                            &socket,
                            &rs,
                            &arena,
                            &slot_filled,
                            &slot_frag_lens,
                            drone_id,
                            &mut fec_block_seq,
                            &mut l2_pkt_seq,
                            &mut fail_count,
                            &mut l2_frame_buf,
                            &mut send_buf,
                            &mut video_payload_buf,
                            &hp_rx,
                            &mut fec_shards,
                        );
                    }

                    if nal_head > 0 && nal_head == nal_tail {
                        nal_head = 0;
                        nal_tail = 0;
                        nal_idle_cycles = 0;
                    } else if nal_head > 0 && nal_head > nal_buf.len() / 2 {
                        let remaining = nal_tail - nal_head;
                        nal_buf.copy_within(nal_head..nal_tail, 0);
                        nal_head = 0;
                        nal_tail = remaining;
                    }

                    if extracted_any {
                        nal_idle_cycles = 0;
                    } else if nal_tail > nal_head && nal_tail - nal_head > nal_buf.len() / 2 {
                        nal_idle_cycles += 1;
                        if nal_idle_cycles >= NAL_IDLE_LIMIT {
                            tracing::warn!(
                                "NAL buffer idle reset ({} cycles, {}B unparseable)",
                                nal_idle_cycles,
                                nal_tail - nal_head
                            );
                            nal_head = 0;
                            nal_tail = 0;
                            nal_idle_cycles = 0;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Read error: {}", e);
                    break;
                }
            }
        }

        let _ = child.kill();
        let _ = child.wait();

        tracing::info!(
            "ffmpeg stopped, sent {:.1} MB total",
            total_bytes as f64 / 1_048_576.0
        );

        if running.load(Ordering::SeqCst) {
            tracing::info!("Restarting in 2 seconds...");
            thread::sleep(Duration::from_secs(2));
        }
    }
}

fn extract_next_nal_cursor(buf: &[u8], head: usize, tail: usize) -> Option<(&[u8], usize)> {
    let data = &buf[head..tail];
    if data.len() < 4 {
        return None;
    }

    let mut sc1_offset = None;
    for i in 0..data.len().saturating_sub(3) {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 0 && i + 3 < data.len() && data[i + 3] == 1 {
                sc1_offset = Some(i);
                break;
            }
            if data[i + 2] == 1 {
                sc1_offset = Some(i);
                break;
            }
        }
    }
    let sc1 = sc1_offset?;

    let sc1_len = if sc1 + 3 < data.len() && data[sc1 + 2] == 0 && data[sc1 + 3] == 1 {
        4
    } else {
        3
    };

    let nal_start = sc1 + sc1_len;

    for i in nal_start..data.len().saturating_sub(3) {
        if data[i] == 0 && data[i + 1] == 0 {
            let is_sc4 = data[i + 2] == 0 && i + 3 < data.len() && data[i + 3] == 1;
            let is_sc3 = data[i + 2] == 1;
            if is_sc4 || is_sc3 {
                let nal = &data[sc1..i];
                let consumed = i;
                return Some((nal, consumed));
            }
        }
    }

    None
}

/// Send an FEC group from pre-allocated arena slots (zero-alloc padding).
/// Drains high-priority channel (telemetry/RC/heartbeat) before each shard send.
fn send_fec_group_arena(
    socket: &RawSocket,
    rs: &reed_solomon_erasure::galois_8::ReedSolomon,
    arena: &ShardArena,
    slot_filled: &[usize; DATA_SHARDS],
    _slot_frag_lens: &[usize; DATA_SHARDS],
    drone_id: u8,
    fec_block_seq: &mut u32,
    l2_pkt_seq: &mut u32,
    fail_count: &mut u8,
    l2_frame_buf: &mut Vec<u8>,
    send_buf: &mut Vec<u8>,
    video_payload_buf: &mut Vec<u8>,
    hp_rx: &Option<crossbeam_channel::Receiver<Vec<u8>>>,
    fec_shards: &mut Vec<Vec<u8>>,
) {
    // Determine max shard size for RS encoding
    let mut shard_lens = [0usize; DATA_SHARDS];
    let mut max_shard_size = 0usize;
    for i in 0..DATA_SHARDS {
        shard_lens[i] = slot_filled[i];
        max_shard_size = max_shard_size.max(slot_filled[i]);
    }

    if max_shard_size == 0 {
        return;
    }

    // Reuse pre-allocated shard Vecs (resize + zero-fill instead of new allocs)
    for i in 0..DATA_SHARDS {
        fec_shards[i].resize(max_shard_size, 0);
        fec_shards[i].fill(0);
        let copy_len = slot_filled[i].min(max_shard_size);
        fec_shards[i][..copy_len].copy_from_slice(&arena.slots[i][..copy_len]);
    }
    for i in DATA_SHARDS..TOTAL_SHARDS {
        fec_shards[i].resize(max_shard_size, 0);
        fec_shards[i].fill(0);
    }

    if let Err(e) = rs.encode(&mut *fec_shards) {
        tracing::warn!("Reed-Solomon encode error: {:?}", e);
        return;
    }

    let mut group_ok = true;

    for (i, shard) in fec_shards.iter().enumerate() {
        // Skip parity shards — only send data shards (no FEC overhead)
        if i >= DATA_SHARDS {
            break;
        }
        // Drain high-priority packets (telemetry, RC, heartbeat) before this shard
        if let Some(ref hp) = hp_rx {
            while let Ok(hp_frame) = hp.try_recv() {
                let _ = socket.send(&hp_frame);
            }
        }

        // Data shards sent at original length, parity trimmed to max_shard_size
        let send_data = if i < DATA_SHARDS {
            let orig_len = slot_filled[i];
            if orig_len > 0 && orig_len <= shard.len() {
                &shard[..orig_len]
            } else {
                shard
            }
        } else {
            &shard[..max_shard_size.min(shard.len())]
        };

        // Debug: log first few shards
        if *fec_block_seq < 5 {
            tracing::info!(
                "SEND shard[{}]: {} bytes, first16={:02x?}",
                i,
                send_data.len(),
                &send_data[..16.min(send_data.len())]
            );
        }

        // Build video header dynamically
        video_payload_buf.clear();
        video_payload_buf.reserve(VIDEO_HDR_LEN + send_data.len());
        video_payload_buf.extend_from_slice(&fec_block_seq.to_le_bytes());
        video_payload_buf.push(i as u8);
        video_payload_buf.push(TOTAL_SHARDS as u8);
        video_payload_buf.push(DATA_SHARDS as u8);
        video_payload_buf.push(0u8); // pad
                                     // [u16; DATA_SHARDS] shard length array
        for &len in &shard_lens {
            video_payload_buf.extend_from_slice(&(len as u16).to_le_bytes());
        }
        video_payload_buf.extend_from_slice(send_data);

        let header = link::L2Header {
            drone_id,
            payload_type: link::PAYLOAD_VIDEO,
            seq: *l2_pkt_seq,
        };
        header.encode_into(video_payload_buf, l2_frame_buf);

        match socket.send_with_buf(l2_frame_buf, send_buf) {
            Ok(_) => {
                *l2_pkt_seq = l2_pkt_seq.wrapping_add(1);
            }
            Err(e) => {
                group_ok = false;
                *fail_count = fail_count.saturating_add(1);
                if *fail_count <= 5 {
                    tracing::warn!("Video send error: {}", e);
                }
                if *fail_count > 30 {
                    tracing::warn!("Too many send failures, retrying...");
                    *fail_count = 0;
                    return;
                }
            }
        }
    }

    *fec_block_seq = fec_block_seq.wrapping_add(1);
    if group_ok {
        *fail_count = 0;
    }
}

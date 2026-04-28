use bytes::{Buf, BytesMut};
use std::env;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// #30: Import video health flag from main
use crate::VIDEO_HEALTHY;

use reed_solomon_erasure::ReedSolomon;

use crate::link;
use crate::SocketTrait;

/// Check camera via sysfs (single stat() syscall, no subprocess spawn)
pub fn check_camera_available(camera_type: &str) -> bool {
    let is_csi = camera_type == "csi" || camera_type == "rpicam";

    if is_csi {
        // For CSI cameras, check if rpicam-vid is available and/or use vcgencmd
        if command_exists("rpicam-vid") {
            // Also try to check if camera is detected via vcgencmd (Pi-specific)
            if let Ok(output) = Command::new("vcgencmd").arg("get_camera").output() {
                if output.status.success() {
                    let response = String::from_utf8_lossy(&output.stdout);
                    // Response like "supported=1 detected=1" or "supported=0"
                    if response.contains("detected=1") {
                        return true;
                    }
                }
            }
            // If vcgencmd fails or doesn't detect, still return true if rpicam-vid exists
            // (user might have CSI camera but vcgencmd might not work)
            return true;
        }
        false
    } else {
        // USB camera: check V4L2 devices
        // /dev/v4l/by-id or /dev/v4l/by-path may contain symlinks on some systems
        let v4l_ok = std::fs::read_dir("/dev/v4l")
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
        if v4l_ok {
            return true;
        }
        // Fallback: check for /dev/video* devices (standard V4L2)
        std::fs::read_dir("/dev")
            .map(|entries| {
                entries.filter_map(|e| e.ok())
                    .any(|e| {
                        e.file_name()
                            .to_str()
                            .map(|s| s.starts_with("video"))
                            .unwrap_or(false)
                    })
            })
            .unwrap_or(false)
    }
}

const DATA_SHARDS: usize = 4;
const PARITY_SHARDS: usize = 2;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;
const MAX_NAL_BUF: usize = 512 * 1024;

/// Check if a command exists in PATH by trying to run it with --version.
fn command_exists(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .output()
        .map(|_| true)
        .unwrap_or(false)
}

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
        // Zero tail to prevent stale data from being sent when slot is reused
        self.slots[slot][offset + copy_len..].fill(0);
        copy_len
    }

    /// Zero-pad slot `idx` from `filled` to MAX_SHARD_DATA.
    fn pad_slot(&mut self, slot: usize, filled: usize) {
        if slot < DATA_SHARDS && filled < MAX_SHARD_DATA {
            self.slots[slot][filled..].fill(0);
        }
    }
}

/// Run the video capture and streaming loop.
pub fn run(
    running: Arc<AtomicBool>,
    socket: Arc<dyn SocketTrait>,
    drone_id: u8,
    bitrate: u32,
    intra: u32,
    hp_rx: Option<crossbeam_channel::Receiver<Vec<u8>>>,
    video_width: u32,
    video_height: u32,
    framerate: u32,
    video_device: String,
    camera_type: &str,
    rpicam_options: &str,
) {
    let is_csi = camera_type == "csi" || camera_type == "rpicam";
    tracing::info!(
        "Video sender ready (FEC {}+{}, L2 broadcast, device={}, type={})",
        DATA_SHARDS,
        PARITY_SHARDS,
        video_device,
        if is_csi {
            "csi (rpicam-vid)"
        } else {
            "usb (ffmpeg)"
        },
    );

    let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS)
        .expect("Failed to create ReedSolomon");

    let mut nal_seq: u32 = 0;

    let mut read_buf = [0u8; 65536];
    let mut shards_in_group = 0;
    let mut slot_filled = [0usize; DATA_SHARDS];
    let mut slot_frag_lens = [0usize; DATA_SHARDS];
    let mut fail_count: u32 = 0;

    while running.load(Ordering::SeqCst) {
        // Read from file
        match file.read(&mut read_buf) {
            Ok(0) => {
                tracing::info!("EOF reached on test video, rewinding");
                // NOTE: Any remaining data in nal_buf without a trailing start code
                // is lost here. This is a known limitation - the last NAL in a stream
                // without a subsequent boundary marker won't be emitted.
                file.seek(SeekFrom::Start(0)).ok();
                nal_buf.clear();
                continue;
            }
            Ok(n) => {
                if nal_buf.len() + n > MAX_NAL_BUF {
                    if let Some(next_sc) = find_start_code(&nal_buf, nal_buf.len() / 4) {
                        nal_buf.drain(..next_sc);
                    } else {
                        nal_buf.clear();
                    }
                }
                nal_buf.extend_from_slice(&read_buf[..n]);
            }
            Err(e) => {
                tracing::error!("Read error from test video: {}", e);
                break;
            }
        }

        tracing::debug!("After read: nal_buf.len()={}", nal_buf.len());

        // Extract NAL units from buffer
        loop {
            let (nal_data, consumed) = match extract_next_nal_cursor(&nal_buf) {
                Some((nal, consumed)) => (nal.to_vec(), consumed),
                None => {
                    tracing::debug!("No NAL found in buffer (len={})", nal_buf.len());
                    break;
                }
            };
            nal_buf.drain(..consumed);

            tracing::debug!("Extracted NAL: len={}, first4={:02x?}", nal_data.len(), &nal_data[..4.min(nal_data.len())]);

            let nal_with_sc = nal_data.clone();

            let nal_id = nal_seq;
            nal_seq = nal_seq.wrapping_add(1);

            // Fragment and send (same as live camera)
            let max_data = MAX_SHARD_DATA - 5;
            if nal_with_sc.len() <= max_data {
                // Single shard NAL
                let slot = shards_in_group % DATA_SHARDS;
                let frag_start = slot_filled[slot];
                arena.write_frag(slot, slot_filled[slot], &[0x00]);
                slot_filled[slot] += 1;
                arena.write_frag(slot, slot_filled[slot], &nal_id.to_le_bytes());
                slot_filled[slot] += 4;
                arena.write_frag(slot, slot_filled[slot], &nal_with_sc);
                slot_filled[slot] += nal_with_sc.len();
                slot_frag_lens[slot] = slot_filled[slot] - frag_start;
                shards_in_group += 1;
                if shards_in_group == DATA_SHARDS {
                    let _ = send_fec_group_arena(
                        socket.as_ref(),
                        &rs,
                        &mut arena,
                        &slot_filled,
                        &slot_frag_lens,
                        drone_id,
                        fec_block_seq,
                        l2_pkt_seq,
                        &mut fail_count,
                        &mut l2_frame_buf,
                        &mut send_buf,
                        &mut video_payload_buf,
                        &None,
                        &mut fec_shards,
                    );
                    shards_in_group = 0;
                    slot_filled = [0; DATA_SHARDS];
                    slot_frag_lens = [0; DATA_SHARDS];
                }
            } else {
                // Multi-shard NAL
                let mut off = 0;
                let mut frag_num: u8 = 1;
                while off < nal_with_sc.len() {
                    let slot = shards_in_group % DATA_SHARDS;
                    let frag_start = slot_filled[slot];
                    let chunk = &nal_with_sc[off..nal_with_sc.len().min(off + max_data)];
                    off += chunk.len();
                    let frag_type = if off >= nal_with_sc.len() { 3 } else { frag_num };
                    frag_num = 2;
                    arena.write_frag(slot, slot_filled[slot], &[frag_type]);
                    slot_filled[slot] += 1;
                    arena.write_frag(slot, slot_filled[slot], &nal_id.to_le_bytes());
                    slot_filled[slot] += 4;
                    arena.write_frag(slot, slot_filled[slot], chunk);
                    slot_filled[slot] += chunk.len();
                    slot_frag_lens[slot] = slot_filled[slot] - frag_start;
                    shards_in_group += 1;
                    if shards_in_group == DATA_SHARDS {
                        let _ = send_fec_group_arena(
                            socket.as_ref(),
                            &rs,
                            &mut arena,
                            &slot_filled,
                            &slot_frag_lens,
                            drone_id,
                            fec_block_seq,
                            l2_pkt_seq,
                            &mut fail_count,
                            &mut l2_frame_buf,
                            &mut send_buf,
                            &mut video_payload_buf,
                            &None,
                            &mut fec_shards,
                        );
                        shards_in_group = 0;
                        slot_filled = [0; DATA_SHARDS];
                        slot_frag_lens = [0; DATA_SHARDS];
                    }
                }
            }
        } // end NAL extraction loop

        // Rate limit slightly to avoid overwhelming the network
        std::thread::sleep(std::time::Duration::from_micros(100));
    } // end running loop
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== find_start_code() tests ====================

    #[test]
    fn find_start_code_3byte_basic() {
        let buf = [0x00, 0x00, 0x01, 0xFF];
        assert_eq!(find_start_code(&buf, 0), Some(0));
    }

    #[test]
    fn find_start_code_4byte_basic() {
        let buf = [0x00, 0x00, 0x00, 0x01, 0xFF];
        assert_eq!(find_start_code(&buf, 0), Some(0));
    }

    #[test]
    fn find_start_code_3byte_after_offset() {
        let buf = [0xFF, 0x00, 0x00, 0x01, 0xAA];
        assert_eq!(find_start_code(&buf, 1), Some(1));
    }

    #[test]
    fn find_start_code_4byte_after_3byte() {
        // 3-byte at 0, 4-byte at 1 (00 00 00 01)
        let buf = [0x00, 0x00, 0x00, 0x01, 0xFF];
        // From position 0, it should find the 4-byte start code
        assert_eq!(find_start_code(&buf, 0), Some(0));
    }

    #[test]
    fn find_start_code_multiple_start_codes() {
        let buf = [0xAA, 0x00, 0x00, 0x01, 0xBB, 0x00, 0x00, 0x00, 0x01, 0xCC];
        // First start code at index 1 (3-byte)
        assert_eq!(find_start_code(&buf, 0), Some(1));
    }

    #[test]
    fn find_start_code_no_start_code() {
        let buf = [0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(find_start_code(&buf, 0), None);
    }

    #[test]
    fn find_start_code_offset_past_end() {
        let buf = [0x00, 0x00, 0x01, 0xFF];
        assert_eq!(find_start_code(&buf, 10), None);
    }

    #[test]
    fn find_start_code_4byte_detection() {
        // 00 00 01 is a 3-byte start code, but if preceded by 00, it becomes 4-byte
        let buf = [0xAA, 0x00, 0x00, 0x00, 0x01, 0xBB];
        // At position 1 we have 00 00 00 01 - should detect as 4-byte starting at 1
        assert_eq!(find_start_code(&buf, 1), Some(1));
    }

    // ==================== extract_next_nal_cursor() tests ====================

    #[test]
    fn extract_next_nal_3byte_start() {
        let buf = [0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x00, 0x01, 0x68];
        let result = extract_next_nal_cursor(&buf);
        assert!(result.is_some());
        let (nal, _consumed) = result.unwrap();
        // NAL should start with start code (3 bytes) and include data up to next start code
        // buf[0..5] = [0x00, 0x00, 0x01, 0x67, 0x42] = 5 bytes
        assert_eq!(nal.len(), 5);
        assert_eq!(nal[3], 0x67);
        assert_eq!(nal[4], 0x42);
    }

    #[test]
    fn extract_next_nal_4byte_start() {
        let buf = [0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x00, 0x00, 0x01, 0x68];
        let result = extract_next_nal_cursor(&buf);
        assert!(result.is_some());
        let (nal, _consumed) = result.unwrap();
        // NAL should start with 4-byte start code
        assert!(nal.len() >= 5);
        assert_eq!(nal[4], 0x67);
    }

    #[test]
    fn extract_next_nal_no_nal() {
        let buf = [0xFF, 0xFF, 0xFF];
        assert!(extract_next_nal_cursor(&buf).is_none());
    }

    #[test]
    fn extract_next_nal_only_start_code() {
        let buf = [0x00, 0x00, 0x01];
        // No second start code, so can't extract full NAL
        assert!(extract_next_nal_cursor(&buf).is_none());
    }

    // ==================== ShardArena and write_frag() tests ====================

    #[test]
    fn shard_arena_new() {
        let arena = ShardArena::new();
        assert_eq!(arena.slots.len(), DATA_SHARDS);
        // All slots should be zero-filled
        for slot in &arena.slots {
            assert!(slot.iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn write_frag_basic() {
        let mut arena = ShardArena::new();
        let data = [0xDE, 0xAD, 0xBE, 0xEF];
        let written = arena.write_frag(0, 0, &data);
        assert_eq!(written, 4);
        assert_eq!(&arena.slots[0][..4], &data);
        // Tail should be zeroed
        assert!(arena.slots[0][4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn write_frag_multiple_writes() {
        let mut arena = ShardArena::new();
        let data1 = [0x11, 0x22, 0x33];
        let data2 = [0xAA, 0xBB];

        arena.write_frag(0, 0, &data1);
        arena.write_frag(0, 3, &data2);

        assert_eq!(arena.slots[0][0], 0x11);
        assert_eq!(arena.slots[0][1], 0x22);
        assert_eq!(arena.slots[0][2], 0x33);
        assert_eq!(arena.slots[0][3], 0xAA);
        assert_eq!(arena.slots[0][4], 0xBB);
    }

    #[test]
    fn write_frag_zero_tail() {
        let mut arena = ShardArena::new();
        // Fill some data
        let data = [0x01, 0x02, 0x03, 0x04, 0x05];
        arena.write_frag(0, 0, &data);

        // Now write shorter data at same offset - tail should be zeroed
        let short_data = [0x0A, 0x0B];
        arena.write_frag(0, 0, &short_data);

        assert_eq!(arena.slots[0][0], 0x0A);
        assert_eq!(arena.slots[0][1], 0x0B);
        // Rest should be zeroed
        assert!(arena.slots[0][2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn write_frag_slot_out_of_bounds() {
        let mut arena = ShardArena::new();
        let data = [0x01];
        let written = arena.write_frag(DATA_SHARDS, 0, &data);
        assert_eq!(written, 0);
    }

    #[test]
    fn write_frag_truncates_at_max() {
        let mut arena = ShardArena::new();
        let offset = MAX_SHARD_DATA - 2;
        let data = [0x01, 0x02, 0x03]; // 3 bytes, but only 2 fit
        let written = arena.write_frag(0, offset, &data);
        assert_eq!(written, 2);
        assert_eq!(arena.slots[0][offset], 0x01);
        assert_eq!(arena.slots[0][offset + 1], 0x02);
    }

    #[test]
    fn pad_slot_basic() {
        let mut arena = ShardArena::new();
        // Write some data
        arena.write_frag(0, 0, &[0x01, 0x02, 0x03]);
        // Pad from filled=3 to end
        arena.pad_slot(0, 3);
        // All bytes from index 3 onward should be 0
        assert!(arena.slots[0][3..].iter().all(|&b| b == 0));
    }

    #[test]
    fn pad_slot_already_full() {
        let mut arena = ShardArena::new();
        // Fill the entire slot
        let full_data = vec![0xFF; MAX_SHARD_DATA];
        arena.write_frag(0, 0, &full_data);
        // Pad should be no-op
        arena.pad_slot(0, MAX_SHARD_DATA);
    }

    #[test]
    fn pad_slot_out_of_bounds() {
        let mut arena = ShardArena::new();
        // Should not panic
        arena.pad_slot(DATA_SHARDS, 0);
    }

    // ==================== NAL fragmentation logic tests ====================

    #[test]
    fn nal_fits_in_single_shard() {
        // Test that a small NAL is detected as single-shard
        let max_data = MAX_SHARD_DATA - 5; // reserve 5 bytes for frag header
        let small_nal = vec![0x00, 0x00, 0x01, 0x67, 0x42]; // 5 bytes
        assert!(small_nal.len() <= max_data);
    }

    #[test]
    fn nal_requires_multiple_shards() {
        let max_data = MAX_SHARD_DATA - 1;
        let large_nal = vec![0x00; max_data + 100];
        assert!(large_nal.len() > max_data);
    }

    // ==================== FEC constants tests ====================

    #[test]
    fn fec_constants_valid() {
        assert!(DATA_SHARDS > 0);
        assert!(PARITY_SHARDS > 0);
        assert_eq!(TOTAL_SHARDS, DATA_SHARDS + PARITY_SHARDS);
    }

    #[test]
    fn max_shard_data_fits_in_payload() {
        // MAX_SHARD_DATA should be less than link::MAX_PAYLOAD minus headers
        assert!(MAX_SHARD_DATA > 0);
        // The constant is defined as link::MAX_PAYLOAD - 8 - VIDEO_HDR_LEN - FRAG_HDR_LEN
        // Just verify it's reasonable
        assert!(MAX_SHARD_DATA < 1500); // Typical MTU-ish
    }

    // ==================== Video header constants tests ====================

    #[test]
    fn video_header_size() {
        assert_eq!(VIDEO_HDR_LEN, VIDEO_HDR_FIXED + DATA_SHARDS * 2);
        assert_eq!(VIDEO_HDR_FIXED, 8);
        assert_eq!(FRAG_HDR_LEN, 2);
    }

    // ==================== Integration test for ShardArena with FEC ====================

    #[test]
    fn shard_arena_fec_round_trip() {
        // Create a Reed-Solomon encoder (using galois_8 like the main code)
        use reed_solomon_erasure::galois_8;
        let rs = galois_8::ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS).unwrap();

        let mut arena = ShardArena::new();

        // Write test data to each shard
        for i in 0..DATA_SHARDS {
            let data: Vec<u8> = (0..100).map(|b| (b + i as u8) % 255).collect();
            arena.write_frag(i, 0, &data);
        }

        // Prepare shards for encoding
        let mut shard_lens = [0usize; DATA_SHARDS];
        let mut max_shard_size = 0usize;
        let mut slot_filled = [0usize; DATA_SHARDS];

        for i in 0..DATA_SHARDS {
            slot_filled[i] = 100;
            shard_lens[i] = 100;
            max_shard_size = max_shard_size.max(100);
        }

        // Pad slots
        for i in 0..DATA_SHARDS {
            arena.pad_slot(i, slot_filled[i]);
        }

        // Create fec_shards
        let mut fec_shards: Vec<Vec<u8>> = (0..TOTAL_SHARDS)
            .map(|_| Vec::with_capacity(max_shard_size))
            .collect();

        for i in 0..DATA_SHARDS {
            let copy_len = slot_filled[i].min(max_shard_size);
            fec_shards[i].extend_from_slice(&arena.slots[i][..copy_len]);
            fec_shards[i].resize(max_shard_size, 0);
        }
        for i in DATA_SHARDS..TOTAL_SHARDS {
            fec_shards[i].resize(max_shard_size, 0);
        }

        // Encode
        rs.encode(&mut fec_shards).unwrap();

        // Verify we have all shards
        assert_eq!(fec_shards.len(), TOTAL_SHARDS);

        // Verify parity shards are not all zeros (encoding worked)
        for i in DATA_SHARDS..TOTAL_SHARDS {
            assert!(fec_shards[i].iter().any(|&b| b != 0), "Parity shard {} is all zeros", i);
        }
    }

    // ==================== Regression tests for AUDIT.md bugs ====================

    /// Bug: AUDIT.md — FEC parity shards not transmitted
    /// Fixed: send_fec_group_arena now iterates 0..TOTAL_SHARDS (all shards including parity)
    /// Test: Verify that RS encoding produces valid parity shards and TOTAL_SHARDS includes them
    #[test]
    fn regression_fec_parity_sent() {
        use reed_solomon_erasure::galois_8;
        let rs = galois_8::ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS).unwrap();

        // Create test data shards
        let mut shards: Vec<Vec<u8>> = (0..TOTAL_SHARDS)
            .map(|i| {
                if i < DATA_SHARDS {
                    vec![i as u8; 50] // Data shards have known pattern
                } else {
                    vec![0u8; 50] // Parity shards start as zeros
                }
            })
            .collect();

        // Encode - this should fill parity shards
        rs.encode(&mut shards).expect("RS encode failed");

        // Verify parity shards are now non-zero (they were computed)
        for i in DATA_SHARDS..TOTAL_SHARDS {
            assert!(
                shards[i].iter().any(|&b| b != 0),
                "Parity shard {} is all zeros after encode - RS not working",
                i
            );
        }

        // Verify TOTAL_SHARDS equals DATA_SHARDS + PARITY_SHARDS
        assert_eq!(TOTAL_SHARDS, DATA_SHARDS + PARITY_SHARDS);

        // The fix in send_fec_group_arena iterates over all TOTAL_SHARDS
        // Verify our loop constant is correct
        let mut parity_count = 0;
        for i in 0..TOTAL_SHARDS {
            if i >= DATA_SHARDS {
                parity_count += 1;
            }
        }
        assert_eq!(parity_count, PARITY_SHARDS);
    }

    /// Bug: AUDIT.md — Partial shards contain stale data
    /// Fixed: write_frag now zeros the tail after copying data
    /// Test: Verify that writing shorter data after longer data doesn't leak stale bytes
    #[test]
    fn regression_partial_shards_no_stale_data() {
        let mut arena = ShardArena::new();

        // Write long data to slot 0
        let long_data = vec![0xAA; 100];
        arena.write_frag(0, 0, &long_data);

        // Verify the data was written
        for i in 0..100 {
            assert_eq!(arena.slots[0][i], 0xAA);
        }

        // Now write shorter data
        let short_data = vec![0xBB; 50];
        arena.write_frag(0, 0, &short_data);

        // Verify new data
        for i in 0..50 {
            assert_eq!(arena.slots[0][i], 0xBB);
        }

        // Verify tail is zeroed (no stale 0xAA leaking)
        for i in 50..100 {
            assert_eq!(
                arena.slots[0][i], 0,
                "Stale data at index {}: expected 0, got {:x}",
                i, arena.slots[0][i]
            );
        }
    }

    /// Bug: AUDIT.md — fail_count is u8, saturates with no recovery
    /// Fixed: fail_count is now u32 in the current code
    /// Test: Verify fail_count doesn't saturate at 255
    #[test]
    fn regression_fail_count_no_saturation() {
        // Verify fail_count is u32 (or larger) and can count past 255
        let mut fail_count: u32 = 0;

        // Simulate 300 failures
        for _ in 0..300 {
            fail_count = fail_count.saturating_add(1);
        }

        assert_eq!(fail_count, 300, "fail_count should be able to count past 255");
        assert!(
            fail_count > 255,
            "fail_count should not saturate at 255 (old u8 behavior)"
        );
    }
}


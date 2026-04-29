/// Shared raw socket functions for stripping radiotap + 802.11 headers
/// and parsing RSSI from monitor-mode WiFi frames.

/// Fixed 802.11 QoS Data header size (26 bytes with QoS Control field).
const IEEE80211_HDR_LEN: usize = 26;
/// 9-byte radiotap: Rate (bit 2) only.
pub const RADIOTAP_LEN: usize = 9;
pub const HEADER_TOTAL: usize = RADIOTAP_LEN + IEEE80211_HDR_LEN; // 35 bytes

/// Static radiotap header with TX rate.
/// Present bit 2 (Rate) set. Rate byte: 0x30 = 24 Mbps (48 * 500kbps).
static RADIOTAP: [u8; RADIOTAP_LEN] = [
    0x00, 0x00, // version=0, pad=0
    0x09, 0x00, // hdr_len=9 (LE)
    0x04, 0x00, 0x00, 0x00, // present: Rate (bit 2)
    0x30, // Rate: 24 Mbps
];

/// Static 802.11 QoS Data broadcast header (pre-computed).
static DATA_FRAME_HDR: [u8; IEEE80211_HDR_LEN] = {
    let mut hdr = [0u8; IEEE80211_HDR_LEN];
    hdr[0] = 0x88; // QoS Data frame (type=2, subtype=0x08 -> 0x88)
    hdr[1] = 0x00; // No flags
    hdr[4] = 0xFF;
    hdr[5] = 0xFF;
    hdr[6] = 0xFF;
    hdr[7] = 0xFF;
    hdr[8] = 0xFF;
    hdr[9] = 0xFF; // DA: broadcast
    hdr[10] = 0xFF;
    hdr[11] = 0xFF;
    hdr[12] = 0xFF;
    hdr[13] = 0xFF;
    hdr[14] = 0xFF;
    hdr[15] = 0xFF; // SA: broadcast
    hdr[16] = 0xFF;
    hdr[17] = 0xFF;
    hdr[18] = 0xFF;
    hdr[19] = 0xFF;
    hdr[20] = 0xFF;
    hdr[21] = 0xFF; // BSSID: broadcast
                    // Bytes 22-23: Sequence Control — updated per send in RawSocket
                    // Bytes 24-25: QoS Control — 0x00 = best effort AC
    hdr
};

/// Get the static radiotap header bytes (for TX frame construction).
pub fn radiotap_header() -> &'static [u8; RADIOTAP_LEN] {
    &RADIOTAP
}

/// Get the static 802.11 data frame header bytes (for TX frame construction).
pub fn data_frame_header() -> &'static [u8; IEEE80211_HDR_LEN] {
    &DATA_FRAME_HDR
}

/// Walk radiotap header properly, handling extended present bitmaps.
/// Returns the offset where the 802.11 frame starts.
pub fn radiotap_hdr_len(frame: &[u8]) -> Option<usize> {
    if frame.len() < 8 {
        return None;
    }
    let version = frame[0];
    if version != 0 {
        return None;
    }
    let hdr_len = u16::from_le_bytes([frame[2], frame[3]]) as usize;
    if hdr_len < 8 || hdr_len > frame.len() {
        return None;
    }
    Some(hdr_len)
}

pub fn strip_radiotap(frame: &[u8]) -> Option<&[u8]> {
    let hdr_len = radiotap_hdr_len(frame)?;
    Some(&frame[hdr_len..])
}

/// Parse the 802.11 header length from the Frame Control field.
pub fn ieee80211_hdr_len(frame: &[u8]) -> Option<usize> {
    if frame.len() < 2 {
        return None;
    }
    let fc = u16::from_le_bytes([frame[0], frame[1]]);
    let frame_type = (fc >> 2) & 0x3;
    let to_ds = (fc >> 8) & 1;
    let from_ds = (fc >> 9) & 1;
    let subtype = (fc >> 4) & 0xF;

    if frame_type != 2 {
        return if frame.len() >= 24 { Some(24) } else { None };
    }

    let base_len = if to_ds == 1 && from_ds == 1 { 30 } else { 24 };
    let qos_bit = subtype & 0x8 != 0;
    let hdr_len = if qos_bit { base_len + 2 } else { base_len };

    if frame.len() < hdr_len {
        None
    } else {
        Some(hdr_len)
    }
}

/// Strip Radiotap + 802.11 header, returning the L2 payload and optional RSSI (dBm).
pub fn recv_extract(frame: &[u8], _log_rejections: bool) -> Option<(&[u8], Option<i8>)> {
    let rssi = parse_radiotap_rssi(frame);
    let hdr_len = radiotap_hdr_len(frame)?;

    let after_radiotap = &frame[hdr_len..];
    let ieee_hdr_len = ieee80211_hdr_len(after_radiotap)?;
    let after_80211 = &after_radiotap[ieee_hdr_len..];

    // Skip LLC/SNAP header (8 bytes: 3-byte LLC + 5-byte SNAP)
    if after_80211.len() >= 8
        && after_80211[0] == 0xAA
        && after_80211[1] == 0xAA
        && after_80211[2] == 0x03
    {
        let payload = &after_80211[8..];
        if !payload.is_empty() {
            return Some((payload, rssi));
        }
        return None;
    }

    if after_80211.is_empty() {
        None
    } else {
        Some((after_80211, rssi))
    }
}

/// Parse antenna signal (RSSI in dBm) from the Radiotap header if present.
/// Properly handles extended present bitmasks and natural field alignment.
pub fn parse_radiotap_rssi(frame: &[u8]) -> Option<i8> {
    let hdr_len = radiotap_hdr_len(frame)?;

    // Walk present bitmaps (handle extended bit 31 chaining)
    let mut present_words: Vec<u32> = Vec::new();
    let mut pres_offset = 4usize;
    loop {
        if pres_offset + 4 > hdr_len || pres_offset + 4 > frame.len() {
            return None;
        }
        let word = u32::from_le_bytes([
            frame[pres_offset],
            frame[pres_offset + 1],
            frame[pres_offset + 2],
            frame[pres_offset + 3],
        ]);
        present_words.push(word);
        pres_offset += 4;
        if word & (1 << 31) == 0 {
            break;
        }
    }

    // Check if RSSI (bit 5: dBm antenna signal) is present
    let has_rssi = present_words.iter().any(|w| w & (1 << 5) != 0);
    if !has_rssi {
        return None;
    }

    // Natural alignment for each radiotap field per the spec
    const FIELD_ALIGN: [usize; 32] = [
        8, 1, 1, 4, 2, 1, 1, 2, 2, 2, 1, 1, 1, 1, 2, 4, 4, 4, 4, 2, 2, 2, 2, 1, 1, 1, 1, 8, 2, 4,
        2, 1,
    ];
    const FIELD_SIZE: [usize; 32] = [
        8, 1, 1, 4, 2, 1, 1, 2, 2, 2, 1, 1, 1, 1, 2, 4, 4, 4, 4, 2, 2, 2, 2, 1, 1, 1, 1, 8, 2, 4,
        2, 1,
    ];

    let mut offset = pres_offset;
    for (word_idx, &present) in present_words.iter().enumerate() {
        let base_bit = word_idx * 32;
        for bit in 0..32u32 {
            if present & (1 << bit) != 0 {
                let field_idx = base_bit + bit as usize;
                if field_idx >= 32 {
                    continue;
                }
                let align = FIELD_ALIGN[field_idx];
                if align > 1 {
                    offset = (offset + align - 1) & !(align - 1);
                }
                if field_idx == 5 {
                    // dBm antenna signal
                    if offset >= hdr_len || offset >= frame.len() {
                        return None;
                    }
                    return Some(frame[offset] as i8);
                }
                offset += FIELD_SIZE[field_idx];
                if offset > hdr_len {
                    return None;
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to build a radiotap header with given present flags and field data
    // The radiotap header format is:
    // - 1 byte: version
    // - 1 byte: pad
    // - 2 bytes: header length (LE)
    // - present words (4 bytes each)
    // - field data
    fn build_radiotap_header(present_words: &[u32], field_data: &[u8]) -> Vec<u8> {
        let present_bytes: Vec<u8> = present_words
            .iter()
            .flat_map(|w| w.to_le_bytes().to_vec())
            .collect();
        // Header length = 4 (version + pad + hdr_len) + present_bytes + field_data
        let hdr_len = 4 + present_bytes.len() + field_data.len();
        let mut buf = Vec::new();
        buf.push(0x00); // version
        buf.push(0x00); // pad
        buf.extend_from_slice(&(hdr_len as u16).to_le_bytes()); // header length
        buf.extend_from_slice(&present_bytes);
        buf.extend_from_slice(field_data);
        buf
    }

    // ==================== Regression tests for AUDIT.md bugs ====================

    /// Bug: AUDIT.md rawsock.rs — Radiotap RSSI offset is wrong on real hardware
    /// Fixed: parse_radiotap_rssi now uses proper field alignment (TSFT=8-byte align)
    /// Test: Build a radiotap header with TSFT field (8-byte aligned) followed by RSSI
    /// and verify RSSI is read from the correct offset
    #[test]
    fn regression_radiotap_rssi_alignment_with_tsft() {
        // Radiotap header with TSFT (bit 0) and dBm antenna signal (bit 5)
        // Base header: version(1) + pad(1) + hdr_len(2) = 4 bytes
        // Present word: 4 bytes
        // TSFT field: 8 bytes (8-byte aligned, starts at offset 8)
        // RSSI field: 1 byte (1-byte aligned, starts at offset 16)

        let present: u32 = (1 << 0) | (1 << 5); // TSFT + RSSI

        // Build the frame
        let mut frame = vec![];
        frame.push(0x00); // version
        frame.push(0x00); // pad
                          // hdr_len will be set later
        frame.extend_from_slice(&0u16.to_le_bytes());

        // Present word
        frame.extend_from_slice(&present.to_le_bytes());

        // TSFT field (8 bytes, at offset 8 which is 8-byte aligned)
        frame.extend_from_slice(&12345678901234567u64.to_le_bytes());

        // RSSI field (1 byte) - no alignment needed after TSFT since offset 16 is 1-byte aligned
        let rssi_value: i8 = -65;
        let rssi_offset = frame.len();
        frame.push(rssi_value as u8);

        // Now set the correct hdr_len (total header length)
        let hdr_len = frame.len() as u16;
        frame[2..4].copy_from_slice(&hdr_len.to_le_bytes());

        // Verify the header length calculation
        let parsed_hdr_len = radiotap_hdr_len(&frame).expect("Should parse header");
        assert_eq!(parsed_hdr_len, hdr_len as usize);

        // Parse RSSI - should get the correct value
        let rssi = parse_radiotap_rssi(&frame);
        assert_eq!(
            rssi,
            Some(rssi_value),
            "RSSI should be {} with proper alignment, got {:?}",
            rssi_value,
            rssi
        );
    }

    /// Bug: AUDIT.md rawsock.rs — Extended Radiotap present bitmasks are ignored
    /// Fixed: parse_radiotap_rssi now walks extended present bitmasks (bit 31 chaining)
    /// Test: Build a radiotap header with extended present bitmask (bit 31 set)
    /// and verify RSSI is found in the first present word
    #[test]
    fn regression_radiotap_extended_present_bitmask() {
        // First present word: bit 5 (RSSI) + bit 31 (extended)
        // Second present word: no bits set
        let present1: u32 = (1 << 5) | (1 << 31); // RSSI + Extended bit
        let present2: u32 = 0; // No additional fields

        // Build the frame
        let mut frame = vec![];
        frame.push(0x00); // version
        frame.push(0x00); // pad
                          // hdr_len will be set later
        frame.extend_from_slice(&0u16.to_le_bytes());
        // Present words (8 bytes total)
        frame.extend_from_slice(&present1.to_le_bytes());
        frame.extend_from_slice(&present2.to_le_bytes());
        // RSSI value (1 byte, immediately after present words)
        let rssi_value: i8 = -42;
        frame.push(rssi_value as u8);
        // Set the correct hdr_len (total header length)
        let hdr_len = frame.len() as u16;
        frame[2..4].copy_from_slice(&hdr_len.to_le_bytes());

        // Verify the header length
        let parsed_hdr_len = radiotap_hdr_len(&frame).expect("Should parse header");
        assert_eq!(parsed_hdr_len, hdr_len as usize);

        // Parse RSSI - should find it in the first present word
        let rssi = parse_radiotap_rssi(&frame);
        assert_eq!(
            rssi,
            Some(rssi_value),
            "RSSI should be found via extended present bitmask"
        );
    }

    /// Test: Radiotap header without RSSI bit should return None
    #[test]
    fn regression_radiotap_no_rssi_returns_none() {
        // Present word: bit 0 (TSFT) only, no RSSI
        let present: u32 = 1 << 0;
        let hdr_len: u16 = 8 + 4 + 8; // base + present + TSFT

        let mut frame = vec![
            0x00, 0x00, // version=0, pad=0
        ];
        frame.extend_from_slice(&hdr_len.to_le_bytes());
        frame.extend_from_slice(&present.to_le_bytes());
        frame.extend_from_slice(&1234567890u64.to_le_bytes()); // TSFT

        let rssi = parse_radiotap_rssi(&frame);
        assert_eq!(rssi, None, "Should return None when RSSI bit not set");
    }

    /// Test: Verify LLC/SNAP header skipping works correctly
    #[test]
    fn regression_llc_snap_header_skip() {
        // Build a frame: radiotap + 802.11 header + LLC/SNAP + payload
        let rt_len: u16 = 8; // minimal radiotap (no fields)
        let ieee_len: usize = 24; // minimal 802.11 header

        let mut frame = vec![];

        // Radiotap header
        frame.extend_from_slice(&[0x00, 0x00]); // version, pad
        frame.extend_from_slice(&rt_len.to_le_bytes());
        frame.extend_from_slice(&0u32.to_le_bytes()); // no present bits

        // 802.11 header (24 bytes, minimal)
        frame.extend_from_slice(&[0x08, 0x00]); // Data frame
        frame.extend_from_slice(&[0x00; 22]); // rest of header

        // LLC/SNAP header (8 bytes: 3 LLC + 5 SNAP)
        frame.extend_from_slice(&[0xAA, 0xAA, 0x03]); // LLC
        frame.extend_from_slice(&[0x00, 0x00, 0x00]); // OUI
        frame.extend_from_slice(&[0x08, 0x00]); // EtherType (IPv4)

        // Payload
        let payload = b"Hello RPV";
        frame.extend_from_slice(payload);

        // Use recv_extract to parse
        let result = recv_extract(&frame, false);
        assert!(result.is_some(), "Should extract payload after LLC/SNAP");
        let (extracted_payload, _) = result.unwrap();
        assert_eq!(
            extracted_payload, payload,
            "Payload should match after LLC/SNAP skip"
        );
    }

    /// Test: strip_radiotap returns correct offset
    #[test]
    fn regression_strip_radiotap() {
        let rt_len: u16 = 8;
        let mut frame = vec![];
        frame.extend_from_slice(&[0x00, 0x00]);
        frame.extend_from_slice(&rt_len.to_le_bytes());
        frame.extend_from_slice(&0u32.to_le_bytes());

        // Add some data after radiotap
        frame.extend_from_slice(b"payload");

        let stripped = strip_radiotap(&frame).expect("Should strip radiotap");
        assert_eq!(stripped, b"payload", "Should return data after radiotap");
    }

    // ==================== Additional tests for P4 TEST INFRASTRUCTURE ====================

    #[test]
    fn radiotap_hdr_len_basic() {
        let hdr = build_radiotap_header(&[0x04], &[0x30]); // Rate only
        assert_eq!(radiotap_hdr_len(&hdr), Some(9));
    }

    #[test]
    fn radiotap_hdr_len_with_extended_present() {
        // First present word has bit 31 set, so second present word exists
        let present1 = 1u32 << 31; // Extended present bit set
        let present2 = 0u32; // No additional fields in second word
        let hdr = build_radiotap_header(&[present1, present2], &[]);
        let result = radiotap_hdr_len(&hdr);
        assert!(result.is_some());
        // Header: 4 bytes (version+pad+hdr_len) + 8 bytes (2 present words) = 12
        assert_eq!(result.unwrap(), 12);
    }

    #[test]
    fn radiotap_hdr_len_too_short() {
        let buf = vec![0x00, 0x00, 0x04, 0x00]; // hdr_len=4, but buffer only 4 bytes
        assert_eq!(radiotap_hdr_len(&buf), None);
    }

    #[test]
    fn radiotap_hdr_len_invalid_version() {
        let mut hdr = build_radiotap_header(&[0x04], &[0x30]);
        hdr[0] = 0x01; // Invalid version
        assert_eq!(radiotap_hdr_len(&hdr), None);
    }

    #[test]
    fn parse_radiotap_rssi_simple() {
        // Present: bit 5 (dBm antenna signal)
        let present = 1u32 << 5;
        let rssi_value: i8 = -65;
        let hdr = build_radiotap_header(&[present], &[rssi_value as u8]);
        assert_eq!(parse_radiotap_rssi(&hdr), Some(rssi_value));
    }

    #[test]
    fn parse_radiotap_rssi_with_tsft() {
        // Present: bit 0 (TSFT, 8 bytes) + bit 5 (RSSI)
        // After present word (4 bytes), fields start at offset 8
        // TSFT is 8 bytes, 8-byte aligned: offset 8-15
        // RSSI is 1 byte at offset 16
        let present = (1u32 << 0) | (1u32 << 5);
        let tsft: u64 = 123456789;
        let rssi_value: i8 = -50;

        let mut field_data = Vec::new();
        field_data.extend_from_slice(&tsft.to_le_bytes());
        field_data.push(rssi_value as u8);

        let hdr = build_radiotap_header(&[present], &field_data);

        // RSSI should be at offset 16 (4 hdr + 4 present + 8 TSFT)
        assert_eq!(hdr[16], rssi_value as u8);
        assert_eq!(parse_radiotap_rssi(&hdr), Some(rssi_value));
    }

    #[test]
    fn parse_radiotap_rssi_with_channel() {
        // Present: bit 3 (Channel, 4 bytes) + bit 5 (RSSI)
        let present = (1u32 << 3) | (1u32 << 5);
        let channel_freq: u16 = 2412;
        let channel_flags: u16 = 0x00A0; // 2.4 GHz, active
        let rssi_value: i8 = -70;

        let mut field_data = Vec::new();
        field_data.extend_from_slice(&channel_freq.to_le_bytes());
        field_data.extend_from_slice(&channel_flags.to_le_bytes());
        field_data.push(rssi_value as u8);

        let hdr = build_radiotap_header(&[present], &field_data);
        assert_eq!(parse_radiotap_rssi(&hdr), Some(rssi_value));
    }

    #[test]
    fn parse_radiotap_rssi_with_tsft_and_channel() {
        // Present: bit 0 (TSFT, 8 bytes), bit 3 (Channel, 4 bytes), bit 5 (RSSI)
        let present = (1u32 << 0) | (1u32 << 3) | (1u32 << 5);
        let tsft: u64 = 987654321;
        let channel_freq: u16 = 5180;
        let channel_flags: u16 = 0x00C0; // 5 GHz
        let rssi_value: i8 = -55;

        let mut field_data = Vec::new();
        field_data.extend_from_slice(&tsft.to_le_bytes());
        field_data.extend_from_slice(&channel_freq.to_le_bytes());
        field_data.extend_from_slice(&channel_flags.to_le_bytes());
        field_data.push(rssi_value as u8);

        let hdr = build_radiotap_header(&[present], &field_data);
        assert_eq!(parse_radiotap_rssi(&hdr), Some(rssi_value));
    }

    #[test]
    fn parse_radiotap_rssi_with_extended_present_bit() {
        // Test that RSSI is found when present in first word, even if extended bit is set
        let present1 = (1u32 << 5) | (1u32 << 31); // RSSI + extended bit
        let present2 = 0u32; // No additional fields
        let rssi_value: i8 = -80;

        let hdr = build_radiotap_header(&[present1, present2], &[rssi_value as u8]);
        assert_eq!(parse_radiotap_rssi(&hdr), Some(rssi_value));
    }

    #[test]
    fn parse_radiotap_rssi_not_present() {
        let present = 1u32 << 2; // Rate only, no RSSI
        let hdr = build_radiotap_header(&[present], &[0x30]);
        assert_eq!(parse_radiotap_rssi(&hdr), None);
    }

    #[test]
    fn parse_radiotap_rssi_empty_frame() {
        assert_eq!(parse_radiotap_rssi(&[]), None);
    }

    #[test]
    fn ieee80211_hdr_len_tods() {
        let mut frame = [0u8; 30];
        frame[0] = 0x08; // Data frame
        frame[1] = 0x01; // ToDS=1
        assert_eq!(ieee80211_hdr_len(&frame), Some(24));
    }

    #[test]
    fn ieee80211_hdr_len_fromds() {
        let mut frame = [0u8; 30];
        frame[0] = 0x08;
        frame[1] = 0x02; // FromDS=1
        assert_eq!(ieee80211_hdr_len(&frame), Some(24));
    }

    #[test]
    fn ieee80211_hdr_len_tods_fromds() {
        let mut frame = [0u8; 36];
        frame[0] = 0x08;
        frame[1] = 0x03; // ToDS=1, FromDS=1
        assert_eq!(ieee80211_hdr_len(&frame), Some(30));
    }

    #[test]
    fn ieee80211_hdr_len_qos() {
        let mut frame = [0u8; 32];
        frame[0] = 0x88; // QoS Data (type=2, subtype with QoS)
        frame[1] = 0x00;
        assert_eq!(ieee80211_hdr_len(&frame), Some(26));
    }

    #[test]
    fn ieee80211_hdr_len_qos_tods_fromds() {
        let mut frame = [0u8; 38];
        frame[0] = 0x88; // QoS Data
        frame[1] = 0x03; // ToDS=1, FromDS=1
        assert_eq!(ieee80211_hdr_len(&frame), Some(32));
    }

    #[test]
    fn ieee80211_hdr_len_non_data_frame() {
        let mut frame = [0u8; 30];
        frame[0] = 0x80; // Beacon frame (management)
        frame[1] = 0x00;
        assert_eq!(ieee80211_hdr_len(&frame), Some(24));
    }

    #[test]
    fn ieee80211_hdr_len_too_short() {
        let frame = [0u8; 10];
        assert_eq!(ieee80211_hdr_len(&frame), None);
    }

    #[test]
    fn strip_radiotap_basic() {
        let hdr = build_radiotap_header(&[0x04], &[0x30]);
        let mut frame = hdr.clone();
        frame.extend_from_slice(&[0x88, 0x00]); // 802.11 QoS Data header start
        frame.extend_from_slice(&[0xFF; 20]); // rest of 802.11 header

        let stripped = strip_radiotap(&frame).unwrap();
        assert_eq!(stripped.len(), 22);
        assert_eq!(stripped[0], 0x88);
    }

    #[test]
    fn recv_extract_basic() {
        let rssi_value: i8 = -60;
        let present = 1u32 << 5; // RSSI present
        let radiotap = build_radiotap_header(&[present], &[rssi_value as u8]);

        let mut frame = radiotap;
        // Add 802.11 QoS Data header (26 bytes)
        frame.extend_from_slice(&[
            0x88, 0x00, // QoS Data
            0x00, 0x00, // Duration
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // DA
            0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, // SA
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, // BSSID
            0x00, 0x00, // Seq Ctrl
            0x00, 0x00, // QoS Ctrl
        ]);
        frame.extend_from_slice(b"payload");

        let (payload, rssi) = recv_extract(&frame, false).unwrap();
        assert_eq!(payload, b"payload");
        assert_eq!(rssi, Some(rssi_value));
    }

    #[test]
    fn recv_extract_with_llc_snap() {
        let present = 1u32 << 5;
        let rssi_value: i8 = -45;
        let radiotap = build_radiotap_header(&[present], &[rssi_value as u8]);

        let mut frame = radiotap;
        // 802.11 QoS Data header (26 bytes)
        frame.extend_from_slice(&[
            0x88, 0x00, // QoS Data
            0x00, 0x00, // Duration
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // DA
            0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, // SA
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, // BSSID
            0x00, 0x00, // Seq Ctrl
            0x00, 0x00, // QoS Ctrl
        ]);
        // LLC/SNAP header (8 bytes)
        frame.extend_from_slice(&[
            0xAA, 0xAA, 0x03, // LLC
            0x00, 0x00, 0x00, // SNAP OUI
            0x08, 0x00, // EtherType
        ]);
        frame.extend_from_slice(b"payload");

        let result = recv_extract(&frame, false);
        assert!(result.is_some());
        let (payload, rssi) = result.unwrap();
        assert_eq!(payload, b"payload");
        assert_eq!(rssi, Some(rssi_value));
    }

    #[test]
    fn recv_extract_rssi_not_present() {
        let radiotap = build_radiotap_header(&[0x04], &[0x30]); // Rate only
        let mut frame = radiotap;
        frame.extend_from_slice(&[0x88, 0x00]);
        frame.extend_from_slice(&[0xFF; 24]);
        frame.extend_from_slice(b"data");

        let (payload, rssi) = recv_extract(&frame, false).unwrap();
        assert_eq!(payload, b"data");
        assert_eq!(rssi, None);
    }
}

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
    // Debug: log first 3 raw frames to diagnose radiotap format
    static DEBUG_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let count = DEBUG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if count < 3 {
        tracing::info!(
            "RADIOTAP raw: len={}, first16={:02x?}",
            frame.len(),
            &frame[..16.min(frame.len())]
        );
    }

    let rssi = parse_radiotap_rssi(frame);
    let hdr_len = radiotap_hdr_len(frame)?;

    let after_radiotap = &frame[hdr_len..];
    let ieee_hdr_len = ieee80211_hdr_len(after_radiotap)?;

    if count < 3 {
        tracing::info!(
            "AFTER_RT: len={}, fc={:02x?}, ieee_hdr_len={}, payload_start={:02x?}",
            after_radiotap.len(),
            &after_radiotap[..2.min(after_radiotap.len())],
            ieee_hdr_len,
            &after_radiotap[ieee_hdr_len..(ieee_hdr_len + 8).min(after_radiotap.len())]
        );
    }
    let after_80211 = &after_radiotap[ieee_hdr_len..];

    // LLC/SNAP header: DSAP=0xAA, SSAP=0xAA, Control=0x03
    // Then 3-byte OUI + 2-byte EtherType (8 bytes total)
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
        8, 1, 1, 4, 2, 1, 1, 2, 2, 2, 1, 1, 1, 1, 2, 4, 4, 4, 4, 2, 2, 2, 2, 1, 1, 1, 1, 8,
        2, 4, 2, 1,
    ];
    const FIELD_SIZE: [usize; 32] = [
        8, 1, 1, 4, 2, 1, 1, 2, 2, 2, 1, 1, 1, 1, 2, 4, 4, 4, 4, 2, 2, 2, 2, 1, 1, 1, 1, 8,
        2, 4, 2, 1,
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
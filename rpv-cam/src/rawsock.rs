/// Raw AF_PACKET socket for send/receive on a WiFi interface in monitor mode.
///
/// This bypasses the entire IP stack. On send, the module constructs a minimal
/// 802.11 broadcast data frame with our L2 protocol payload as the frame body.
/// On receive, it strips the Radiotap header + 802.11 MAC header to extract
/// the L2 protocol payload.
use std::io;

/// Fixed 802.11 QoS Data header size (26 bytes with QoS Control field).
/// #9: QoS Data frames enable HT/VHT MCS rates instead of legacy 1-6 Mbps.
const IEEE80211_HDR_LEN: usize = 26;
/// 11-byte radiotap with MCS field to force HT20 rate (MCS7 = 65 Mbps)
const RADIOTAP_LEN: usize = 9;
const HEADER_TOTAL: usize = RADIOTAP_LEN + IEEE80211_HDR_LEN; // 35 bytes

/// Static radiotap header with TX rate for AR9271 (ath9k_htc).
/// Present bit 2 (Rate) set. Rate byte: 0x30 = 48 * 500kbps = 24 Mbps.
static RADIOTAP: [u8; RADIOTAP_LEN] = [
    0x00, 0x00, // version=0, pad=0
    0x09, 0x00, // hdr_len=9 (LE)
    0x04, 0x00, 0x00, 0x00, // present: bit 2 (Rate)
    0x30, // Rate: 24 Mbps (48 * 500kbps)
];

/// Static 802.11 QoS Data broadcast header (pre-computed).
/// #9: QoS Data (subtype 0x88) enables HT/VHT rates.
/// #10: Sequence control at bytes 22-23, updated per send.
static DATA_FRAME_HDR: [u8; IEEE80211_HDR_LEN] = {
    let mut hdr = [0u8; IEEE80211_HDR_LEN];
    hdr[0] = 0x88; // #9: QoS Data frame (type=2, subtype=0x08 -> 0x88)
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
                    // Bytes 22-23: Sequence Control — updated per send in send_with_buf
                    // Bytes 24-25: QoS Control — 0x00 = best effort AC
    hdr
};

pub struct RawSocket {
    fd: i32,
    // #10: 802.11 sequence counter — must increment per frame
    seq_control: std::sync::atomic::AtomicU16,
}

impl RawSocket {
    /// Open a raw AF_PACKET socket bound to the given interface.
    /// The interface must already be in monitor mode.
    pub fn new(iface: &str) -> io::Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW,
                libc::ETH_P_ALL.to_be() as i32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let iface_c = std::ffi::CString::new(iface)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad interface name"))?;
        let ifindex = unsafe { libc::if_nametoindex(iface_c.as_ptr()) };
        if ifindex == 0 {
            unsafe {
                libc::close(fd);
            }
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("interface '{}' not found", iface),
            ));
        }

        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        addr.sll_family = libc::AF_PACKET as u16;
        addr.sll_protocol = (libc::ETH_P_ALL.to_be()) as u16;
        addr.sll_ifindex = ifindex as i32;
        // #8: Set hardware type for monitor mode radiotap
        addr.sll_hatype = libc::ARPHRD_IEEE80211_RADIOTAP as u16;

        let ret = unsafe {
            libc::bind(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            unsafe {
                libc::close(fd);
            }
            return Err(io::Error::last_os_error());
        }

        // 100ms receive timeout for responsive shutdown
        let tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 100_000,
        };
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            tracing::warn!("Failed to set SO_RCVTIMEO: {}", io::Error::last_os_error());
        }

        // 8MB send/receive buffers — requires net.core.rmem_max/wmem_max >= 8388608
        let sndbuf: libc::c_int = 8 * 1024 * 1024;
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &sndbuf as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            tracing::warn!(
                "Failed to set SO_SNDBUF to 8MB (check sysctl net.core.wmem_max): {}",
                io::Error::last_os_error()
            );
        }
        let rcvbuf: libc::c_int = 8 * 1024 * 1024;
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &rcvbuf as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            tracing::warn!(
                "Failed to set SO_RCVBUF to 8MB (check sysctl net.core.rmem_max): {}",
                io::Error::last_os_error()
            );
        }

        // #2: Set O_NONBLOCK so send() never blocks on a full TX ring
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
        if flags >= 0 {
            unsafe {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }

        Ok(Self {
            fd,
            seq_control: std::sync::atomic::AtomicU16::new(0),
        })
    }

    /// Send a raw 802.11 frame.
    /// Prepends a minimal Radiotap header + broadcast data frame header.
    #[allow(dead_code)]
    pub fn send(&self, payload: &[u8]) -> io::Result<usize> {
        let mut frame = Vec::with_capacity(HEADER_TOTAL + payload.len());
        frame.extend_from_slice(&RADIOTAP);
        frame.extend_from_slice(&DATA_FRAME_HDR);
        frame.extend_from_slice(payload);

        let ret = unsafe {
            libc::send(
                self.fd,
                frame.as_ptr() as *const libc::c_void,
                frame.len(),
                0,
            )
        };
        if ret < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EAGAIN) || e.raw_os_error() == Some(libc::EWOULDBLOCK)
            {
                return Ok(0); // TX ring full, frame dropped
            }
            Err(e)
        } else {
            Ok(ret as usize)
        }
    }

    /// Send using a reusable buffer. Non-blocking: returns Ok(0) if TX ring is full.
    /// #10: Increments 802.11 sequence control to prevent duplicate drops.
    pub fn send_with_buf(&self, payload: &[u8], buf: &mut Vec<u8>) -> io::Result<usize> {
        let total = HEADER_TOTAL + payload.len();
        buf.clear();
        buf.reserve(total);
        buf.extend_from_slice(&RADIOTAP);
        buf.extend_from_slice(&DATA_FRAME_HDR);

        // #10: Write sequence control field (bytes 22-23 in the 802.11 header)
        let seq = self
            .seq_control
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let seq_bytes = seq.to_le_bytes();
        // Offset: RADIOTAP_LEN(8) + 22 = byte 30, 31
        buf[RADIOTAP_LEN + 22] = seq_bytes[0];
        buf[RADIOTAP_LEN + 23] = seq_bytes[1];

        buf.extend_from_slice(payload);

        let ret = unsafe { libc::send(self.fd, buf.as_ptr() as *const libc::c_void, buf.len(), 0) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EAGAIN) || e.raw_os_error() == Some(libc::EWOULDBLOCK)
            {
                return Ok(0);
            }
            // ENXIO means interface state changed (driver invalidated socket)
            // Caller should reopen socket
            if e.raw_os_error() == Some(libc::ENXIO) || e.raw_os_error() == Some(libc::ENODEV) {
                return Err(e);
            }
            Err(e)
        } else {
            Ok(ret as usize)
        }
    }

    /// Receive a raw frame. Returns bytes read or 0 on timeout.
    /// #3: Uses SO_RCVTIMEO directly (no redundant poll()).
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let ret =
            unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock || err.kind() == io::ErrorKind::TimedOut {
                Ok(0)
            } else if err.raw_os_error() == Some(libc::ENXIO)
                || err.raw_os_error() == Some(libc::ENODEV)
            {
                Err(err)
            } else {
                Err(err)
            }
        } else {
            Ok(ret as usize)
        }
    }
}

impl Drop for RawSocket {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

/// Strip the Radiotap header from a received monitor-mode frame.
/// Walk radiotap header properly, handling extended present bitmaps.
fn radiotap_hdr_len(frame: &[u8]) -> Option<usize> {
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

/// Strip Radiotap + 802.11 header from a received monitor-mode frame.
pub fn recv_strip_headers(frame: &[u8], log_rejections: bool) -> Option<&[u8]> {
    recv_extract(frame, log_rejections).map(|(payload, _rssi)| payload)
}

/// Strip Radiotap + 802.11 header, returning the L2 payload and optional RSSI (dBm).
pub fn recv_extract(frame: &[u8], _log_rejections: bool) -> Option<(&[u8], Option<i8>)> {
    let rssi = parse_radiotap_rssi(frame);
    let after_radiotap = strip_radiotap(frame)?;
    let hdr_len = ieee80211_hdr_len(after_radiotap)?;

    let after_80211 = &after_radiotap[hdr_len..];
    // LLC/SNAP: DSAP=0xAA, SSAP=0xAA, Control=0x03, then 3-byte OUI + 2-byte EtherType
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
        return None;
    }
    Some((after_80211, rssi))
}

/// Parse antenna signal (RSSI in dBm) from the Radiotap header if present.
fn parse_radiotap_rssi(frame: &[u8]) -> Option<i8> {
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

    let has_rssi = present_words.iter().any(|w| w & (1 << 5) != 0);
    if !has_rssi {
        return None;
    }

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

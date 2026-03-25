/// Raw AF_PACKET socket for send/receive on a WiFi interface in monitor mode.
///
/// This bypasses the entire IP stack. On send, the module constructs a minimal
/// 802.11 broadcast data frame with our L2 protocol payload as the frame body.
/// On receive, it strips the Radiotap header + 802.11 MAC header to extract
/// the L2 protocol payload.
use std::io;

/// Fixed 802.11 header size for a broadcast data frame (no QoS, no 4th address).
const IEEE80211_HDR_LEN: usize = 24;

pub struct RawSocket {
    fd: i32,
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

        // 100ms receive timeout for responsive shutdown (was 500ms)
        let tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 100_000,
        };
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
        }

        // 8MB send/receive buffers
        let sndbuf: libc::c_int = 8 * 1024 * 1024;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &sndbuf as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        let rcvbuf: libc::c_int = 8 * 1024 * 1024;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &rcvbuf as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }

        Ok(Self { fd })
    }

    /// Send a raw 802.11 frame.
    /// Prepends a minimal Radiotap header + broadcast data frame header.
    /// The Radiotap header is required by mac80211 for injected frames in monitor mode.
    #[allow(dead_code)]
    pub fn send(&self, payload: &[u8]) -> io::Result<usize> {
        // Minimal Radiotap header: version=0, pad=0, hdr_len=8, present=0
        let radiotap: [u8; 8] = [0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mut frame = Vec::with_capacity(radiotap.len() + IEEE80211_HDR_LEN + payload.len());
        frame.extend_from_slice(&radiotap);
        frame.extend_from_slice(&build_data_frame_header());
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
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    /// Send using a reusable buffer to avoid per-call heap allocation.
    /// Prepends Radiotap + 802.11 header to `payload`, sends, then returns
    /// the buffer for reuse. The buffer is cleared before use.
    pub fn send_with_buf(&self, payload: &[u8], buf: &mut Vec<u8>) -> io::Result<usize> {
        static RADIOTAP: [u8; 8] = [0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00];
        let total = RADIOTAP.len() + IEEE80211_HDR_LEN + payload.len();
        buf.clear();
        buf.reserve(total);
        buf.extend_from_slice(&RADIOTAP);
        buf.extend_from_slice(&build_data_frame_header());
        buf.extend_from_slice(payload);

        let ret = unsafe { libc::send(self.fd, buf.as_ptr() as *const libc::c_void, buf.len(), 0) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    /// Receive a raw frame and extract the L2 protocol payload.
    /// Strips Radiotap header + 802.11 MAC header.
    /// Returns the number of bytes written into `buf`, or 0 on timeout.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let ret =
            unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock || err.kind() == io::ErrorKind::TimedOut {
                Ok(0)
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

/// Build a minimal 802.11 data frame header for a broadcast frame.
/// No QoS, no 4th address, no WEP. Frame body follows immediately.
fn build_data_frame_header() -> [u8; IEEE80211_HDR_LEN] {
    let mut hdr = [0u8; IEEE80211_HDR_LEN];
    // Frame Control: Data frame (0x0008), To DS=0, From DS=1 (IBSS/ad-hoc style broadcast)
    hdr[0] = 0x08; // Type=Data, Subtype=0
    hdr[1] = 0x00; // No flags (no WEP, no retry, no more data)
                   // Duration: 0
    hdr[2] = 0x00;
    hdr[3] = 0x00;
    // Address 1 (DA): broadcast
    hdr[4..10].fill(0xFF);
    // Address 2 (SA): broadcast (source doesn't matter for filtering, we use L2 header)
    hdr[10..16].fill(0xFF);
    // Address 3 (BSSID): broadcast
    hdr[16..22].fill(0xFF);
    // Sequence Control: 0
    hdr[22] = 0x00;
    hdr[23] = 0x00;
    hdr
}

/// Strip the Radiotap header from a received monitor-mode frame.
/// Returns a slice starting at the 802.11 header, or None if too short.
///
/// Radiotap header length is in bytes 2..4 (little-endian u16).
pub fn strip_radiotap(frame: &[u8]) -> Option<&[u8]> {
    if frame.len() < 4 {
        return None;
    }
    let hdr_len = u16::from_le_bytes([frame[2], frame[3]]) as usize;
    if hdr_len >= frame.len() || hdr_len < 8 {
        return None;
    }
    Some(&frame[hdr_len..])
}

/// Parse the 802.11 header length from the Frame Control field.
/// This handles variable-length headers (QoS data, 4-address frames, etc).
/// Returns None if the frame is too short to contain a valid header.
pub fn ieee80211_hdr_len(frame: &[u8]) -> Option<usize> {
    if frame.len() < 2 {
        return None;
    }
    let fc = u16::from_le_bytes([frame[0], frame[1]]);
    let frame_type = (fc >> 2) & 0x3; // 0=Management, 1=Control, 2=Data
    let to_ds = (fc >> 8) & 1;
    let from_ds = (fc >> 9) & 1;
    let subtype = (fc >> 4) & 0xF;

    // Only Data frames (type=2) have variable-length headers with QoS
    if frame_type != 2 {
        // Management and Control frames: 24 bytes (no QoS, no 4-addr)
        return if frame.len() >= 24 { Some(24) } else { None };
    }

    let base_len = if to_ds == 1 && from_ds == 1 {
        30 // 4-address frame
    } else {
        24 // standard 3-address frame
    };

    // QoS data frames (subtype bit 3 set) have an extra 2-byte QoS Control field
    let qos_bit = subtype & 0x8 != 0;
    let hdr_len = if qos_bit { base_len + 2 } else { base_len };

    if frame.len() < hdr_len {
        None
    } else {
        Some(hdr_len)
    }
}

/// Strip Radiotap + 802.11 header from a received monitor-mode frame.
/// Returns the L2 protocol payload (our link.rs header + application data).
/// Returns None if parsing fails.
pub fn recv_strip_headers(frame: &[u8], log_rejections: bool) -> Option<&[u8]> {
    recv_extract(frame, log_rejections).map(|(payload, _rssi)| payload)
}

/// Strip Radiotap + 802.11 header, returning the L2 payload and optional RSSI (dBm).
pub fn recv_extract(frame: &[u8], _log_rejections: bool) -> Option<(&[u8], Option<i8>)> {
    let rssi = parse_radiotap_rssi(frame);
    let after_radiotap = strip_radiotap(frame)?;
    let hdr_len = ieee80211_hdr_len(after_radiotap)?;

    let after_80211 = &after_radiotap[hdr_len..];
    if after_80211.len() >= 8
        && after_80211[0] == 0xAA
        && after_80211[1] == 0xAA
        && after_80211[2] == 0x03
    {
        let payload = &after_80211[8..];
        if payload.is_empty() {
            return None;
        }
        return Some((payload, rssi));
    }

    if after_80211.is_empty() {
        return None;
    }
    Some((after_80211, rssi))
}

/// Parse antenna signal (RSSI in dBm) from the Radiotap header if present.
/// Radiotap fields are variable-length and ordered by the present bitmask.
/// Antenna signal is present bit 5. Field sizes (bytes):
///   0:TSFT(8) 1:FLAGS(1) 2:RATE(1) 3:CHANNEL(4) 4:FHSS(2) 5:SIGNAL(1) 6:NOISE(1)
fn parse_radiotap_rssi(frame: &[u8]) -> Option<i8> {
    if frame.len() < 8 {
        return None;
    }
    let hdr_len = u16::from_le_bytes([frame[2], frame[3]]) as usize;
    if hdr_len < 8 || hdr_len > frame.len() {
        return None;
    }

    let present = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);

    // Field sizes for present bits 0..31 (only bits 0-6 are common)
    const FIELD_SIZES: [u8; 32] = [
        8, // 0: TSFT
        1, // 1: FLAGS
        1, // 2: RATE
        4, // 3: CHANNEL
        2, // 4: FHSS
        1, // 5: ANTENNA SIGNAL (this is what we want)
        1, // 6: ANTENNA NOISE
        2, // 7: LOCK QUALITY
        2, // 8: TX ATTENUATION
        2, // 9: dB TX ATTENUATION
        1, // 10: dBm TX POWER
        1, // 11: ANTENNA
        1, // 12: dB ANTENNA SIGNAL
        1, // 13: dB ANTENNA NOISE
        2, // 14: RX FLAGS
        4, 4, 4, 4, // 15-18: various
        2, 2, 2, 2, // 19-22
        1, 1, 1, 1, // 23-26
        8, 2, 4, 2, 1, // 27-31
    ];

    // Check if bit 5 (antenna signal) is present
    if present & (1 << 5) == 0 {
        return None;
    }

    // Walk through all present bits before bit 5 to find offset
    let mut offset = 8; // start after the first present word
    for bit in 0..5u32 {
        if present & (1 << bit) != 0 {
            offset += FIELD_SIZES[bit as usize] as usize;
        }
    }

    // Antenna signal is at `offset` within the Radiotap header
    if offset >= hdr_len {
        return None;
    }
    let signal = frame[offset] as i8; // signed dBm value
    Some(signal)
}

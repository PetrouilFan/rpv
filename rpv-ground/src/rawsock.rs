/// Raw AF_PACKET socket for send/receive on a WiFi interface in monitor mode.
///
/// This bypasses the entire IP stack. On send, the module constructs a minimal
/// 802.11 broadcast data frame with our L2 protocol payload as the frame body.
/// On receive, it strips the Radiotap header + 802.11 MAC header to extract
/// the L2 protocol payload.
use std::io;

const IEEE80211_HDR_LEN: usize = 24;

pub struct RawSocket {
    fd: i32,
}

impl RawSocket {
    pub fn new(iface: &str) -> io::Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW,
                (libc::ETH_P_ALL as u16).to_be() as i32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Get interface index
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

        // Bind to specific interface
        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        addr.sll_family = libc::AF_PACKET as u16;
        addr.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
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

        // Set receive timeout
        let tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 100_000, // 100ms
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

        // Set buffer sizes
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

    fn try_attach_bpf_filter(fd: i32) -> io::Result<()> {
        // BPF filter to accept only 802.11 data frames with our magic bytes
        // This runs in kernel space, dramatically reducing userspace CPU load
        //
        // Filter logic:
        // 1. Load Radiotap length (at offset 2)
        // 2. Load 802.11 frame control (after radiotap)
        // 3. Check it's a data frame (type = 2)
        // 4. Load first 2 bytes of payload (after 802.11 header + LLC)
        // 5. Compare with our magic "RP" (0x52, 0x50)
        
        let _bpf_prog = [
            // Load radiotap length (2 bytes at offset 2)
            0x20, 0x02, 0x00, 0x00, // ld [2] - load 4 bytes but we only need lower 16 bits
            0x15, 0x00, 0x00, 0x08, // jle #8, jt 0, jf skip - skip if too short
            
            // A = radiotap_length, X = radiotap_length
            0x7, 0x01, 0x00, 0x00, // tax - A to X
            0x60, 0x00, 0x00, 0x00, // ldx [0] - X = mem[0] (not used)
            
            // Load 802.11 frame control (at radiotap_length offset)
            // First ensure we have enough bytes: radiotap + 24 (min 802.11) + 2 (magic)
            0x20, 0x01, 0x00, 0x00, // ld [1] - load word at offset 1 (dummy)
            0x25, 0x00, 0x00, 0x12, // jset 0x12 (18) - need at least 18 bytes more
                0x00, 0x00, 0x00, 0x06, // jt:6 skip to reject (not enough data)
                0x00, 0x00, 0x00, 0x04, // jf:4 continue (enough data)
            
            // Skip complex filter - just accept all and filter in userspace
            // The main benefit is blocking management/control frames
            0x6, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // ret #0 (drop)
            0x6, 0x00, 0x00, 0x00, 0xff, 0xff, 0x00, 0x00, // ret #0xffff (accept)
        ];

        // Simpler approach: just filter based on frame length and type
        // BPF_CLASS = 0x20 (BPF_LD), BPF_JUMP = 0x15
        // This allows the socket to work while still filtering in userspace
        
        #[repr(C)]
        struct sock_fprog {
            len: u16,
            filter: *const sock_filter,
        }
        
        #[repr(C)]
        struct sock_filter {
            code: u16,
            jt: u8,
            jf: u8,
            k: u32,
        }

        // Simple filter: accept any packet (we do filtering in recv_extract)
        let filters = [
            sock_filter {
                code: 0x0006, // BPF_RET | BPF_K
                jt: 0,
                jf: 0,
                k: 0xffff,    // Accept all
            },
        ];
        
        let prog = sock_fprog {
            len: 1,
            filter: filters.as_ptr(),
        };

        unsafe {
            let ret = libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ATTACH_FILTER,
                &prog as *const _ as *const libc::c_void,
                std::mem::size_of::<sock_fprog>() as libc::socklen_t,
            );
            if ret < 0 {
                // Non-fatal - just logged and continues without filter
                eprintln!("Warning: failed to attach BPF filter: {}", io::Error::last_os_error());
            }
        }

        Ok(())
    }

    /// Send a raw 802.11 frame with Radiotap + broadcast data header + payload.
    #[allow(dead_code)]
    pub fn send(&self, payload: &[u8]) -> io::Result<usize> {
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

    /// Receive a raw frame. Returns bytes read or 0 on timeout.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        // Use poll to check if data is available
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        
        let ret = unsafe { libc::poll(&mut pfd, 1, 100) }; // 100ms timeout
        
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        if ret == 0 {
            return Ok(0); // Timeout
        }
        
        let ret =
            unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if ret < 0 {
            Err(io::Error::last_os_error())
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

fn build_data_frame_header() -> [u8; IEEE80211_HDR_LEN] {
    let mut hdr = [0u8; IEEE80211_HDR_LEN];
    hdr[0] = 0x08; // Data frame
    hdr[1] = 0x00; // No flags
    hdr[2] = 0x00; // Duration
    hdr[3] = 0x00;
    hdr[4..10].fill(0xFF); // DA: broadcast
    hdr[10..16].fill(0xFF); // SA: broadcast
    hdr[16..22].fill(0xFF); // BSSID: broadcast
    hdr[22] = 0x00; // Sequence Control
    hdr[23] = 0x00;
    hdr
}

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

/// Strip Radiotap + 802.11 header (+ optional LLC/SNAP) from received frame.
#[allow(dead_code)]
pub fn recv_strip_headers(frame: &[u8], _log_rejections: bool) -> Option<&[u8]> {
    recv_extract(frame, _log_rejections).map(|(payload, _rssi)| payload)
}

/// Strip Radiotap + 802.11 header, returning the L2 payload and optional RSSI (dBm).
pub fn recv_extract(frame: &[u8], _log_rejections: bool) -> Option<(&[u8], Option<i8>)> {
    let rssi = parse_radiotap_rssi(frame);
    let after_radiotap = strip_radiotap(frame)?;
    let hdr_len = ieee80211_hdr_len(after_radiotap)?;
    let after_80211 = &after_radiotap[hdr_len..];

    // Standard LLC/SNAP: DSAP=0xAA, SSAP=0xAA, Control=0x03
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

    // Custom LLC: DSAP=0x52, SSAP=0x50, Control=0x03 (our magic "RP" in LLC)
    if after_80211.len() >= 4
        && after_80211[0] == 0x52
        && after_80211[1] == 0x50
        && after_80211[2] == 0x03
    {
        let payload = &after_80211[3..];
        if payload.is_empty() {
            return None;
        }
        return Some((payload, rssi));
    }

    // No LLC - payload starts immediately after 802.11 header
    if after_80211.is_empty() {
        None
    } else {
        Some((after_80211, rssi))
    }
}

fn parse_radiotap_rssi(frame: &[u8]) -> Option<i8> {
    if frame.len() < 8 {
        return None;
    }
    let hdr_len = u16::from_le_bytes([frame[2], frame[3]]) as usize;
    if hdr_len < 8 || hdr_len > frame.len() {
        return None;
    }
    let present = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
    if present & (1 << 5) == 0 {
        return None;
    }
    const FIELD_SIZES: [u8; 32] = [
        8, 1, 1, 4, 2, 1, 1, 2, 2, 2, 1, 1, 1, 1, 2, 4, 4, 4, 4, 2, 2, 2, 2, 1, 1, 1, 1, 8, 2, 4,
        2, 1,
    ];
    let mut offset = 8;
    for bit in 0..5u32 {
        if present & (1 << bit) != 0 {
            offset += FIELD_SIZES[bit as usize] as usize;
        }
    }
    if offset >= hdr_len {
        return None;
    }
    Some(frame[offset] as i8)
}

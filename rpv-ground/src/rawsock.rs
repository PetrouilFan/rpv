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

        let tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 500_000,
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

        let sndbuf: libc::c_int = 4 * 1024 * 1024;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &sndbuf as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        let rcvbuf: libc::c_int = 4 * 1024 * 1024;
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

    /// Send a raw 802.11 frame with broadcast data header + payload.
    pub fn send(&self, payload: &[u8]) -> io::Result<usize> {
        let mut frame = Vec::with_capacity(IEEE80211_HDR_LEN + payload.len());
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

    /// Receive a raw frame. Returns bytes read or 0 on timeout.
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
    let to_ds = (fc >> 8) & 1;
    let from_ds = (fc >> 9) & 1;
    let subtype = (fc >> 4) & 0xF;

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
pub fn recv_strip_headers(frame: &[u8], _log_rejections: bool) -> Option<&[u8]> {
    let after_radiotap = strip_radiotap(frame)?;
    let hdr_len = ieee80211_hdr_len(after_radiotap)?;
    let after_80211 = &after_radiotap[hdr_len..];

    // Some drivers insert LLC/SNAP (AA AA 03 ...) for data frames
    if after_80211.len() >= 8
        && after_80211[0] == 0xAA
        && after_80211[1] == 0xAA
        && after_80211[2] == 0x03
    {
        return Some(&after_80211[8..]);
    }

    if after_80211.is_empty() {
        None
    } else {
        Some(after_80211)
    }
}

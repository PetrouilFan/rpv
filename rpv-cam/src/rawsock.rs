/// Raw AF_PACKET socket for send/receive on a WiFi interface in monitor mode.
///
/// This bypasses the entire IP stack. Frames are sent and received as raw
/// 802.11 payloads (the L2 header defined in `link.rs` is the application-layer
/// content carried inside the raw frame).
use std::io;

pub struct RawSocket {
    fd: i32,
}

impl RawSocket {
    /// Open a raw AF_PACKET socket bound to the given interface.
    /// The interface must already be in monitor mode (use `iw dev wlan0 set type monitor`).
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

        // Bind to interface index
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

        // Set a 500ms receive timeout so RX loop is interruptible
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

        // Increase send buffer to 4MB
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

        // Increase receive buffer to 4MB
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

    /// Send a raw frame (payload will be the L2 header + application data).
    /// In monitor mode the kernel wraps this in a Radiotap + 802.11 header.
    pub fn send(&self, data: &[u8]) -> io::Result<usize> {
        let ret =
            unsafe { libc::send(self.fd, data.as_ptr() as *const libc::c_void, data.len(), 0) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    /// Receive a raw frame into the buffer. Returns the number of bytes read.
    /// In monitor mode, received frames include a Radiotap header prefix.
    /// The caller must strip Radiotap before parsing the L2 protocol header.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let ret =
            unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock || err.kind() == io::ErrorKind::TimedOut {
                Ok(0) // timeout, no data
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
/// Returns a slice starting at the 802.11 header, or None if the frame
/// is too short or malformed.
///
/// Radiotap header length is in bytes 2..4 (little-endian u16).
pub fn strip_radiotap(frame: &[u8]) -> Option<&[u8]> {
    if frame.len() < 4 {
        return None;
    }
    // Radiotap version byte is at 0, skip/version check omitted for speed
    let hdr_len = u16::from_le_bytes([frame[2], frame[3]]) as usize;
    if hdr_len >= frame.len() || hdr_len < 8 {
        return None;
    }
    Some(&frame[hdr_len..])
}

use std::io;
use std::sync::atomic::{AtomicU16, Ordering};
use crate::rawsock_common;

/// Raw AF_PACKET socket base for send/receive on a WiFi interface in monitor mode.
/// The `hatype` parameter specifies the hardware address type (e.g., ARPHRD_IEEE80211
/// or ARPHRD_IEEE80211_RADIOTAP).
pub struct RawSocketBase {
    fd: i32,
    seq_control: AtomicU16,
    iface: String,
}

impl RawSocketBase {
    /// Create a new raw socket bound to the given interface.
    pub fn new(iface: &str, hatype: i32) -> io::Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW,
                libc::ETH_P_ALL.to_be(),
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

        // SAFETY: Zero-initializing sockaddr_ll is safe; we set all required fields before use
        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        addr.sll_family = libc::AF_PACKET as u16;
        addr.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
        addr.sll_ifindex = ifindex as i32;
        addr.sll_hatype = hatype as u16;

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

        // Set O_NONBLOCK so send() never blocks on a full TX ring
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
        if flags >= 0 {
            unsafe {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }

        Ok(Self {
            fd,
            seq_control: AtomicU16::new(0),
            iface: iface.to_string(),
        })
    }

    /// Send using a reusable buffer. Non-blocking: returns Ok(0) if TX ring is full.
    pub fn send_with_buf(&self, payload: &[u8], buf: &mut Vec<u8>) -> io::Result<usize> {
        let total = rawsock_common::HEADER_TOTAL + payload.len();
        buf.clear();
        buf.reserve(total);
        buf.extend_from_slice(rawsock_common::radiotap_header());
        buf.extend_from_slice(rawsock_common::data_frame_header());

        let seq = self
            .seq_control
            .fetch_add(1, Ordering::Relaxed) & 0x0FFF;
        let seq_bytes = seq.to_le_bytes();
        buf[rawsock_common::RADIOTAP_LEN + 22] = seq_bytes[0];
        buf[rawsock_common::RADIOTAP_LEN + 23] = seq_bytes[1];

        buf.extend_from_slice(payload);

        let ret = unsafe { libc::send(self.fd, buf.as_ptr() as *const libc::c_void, buf.len(), 0) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EAGAIN) || e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(io::Error::new(std::io::ErrorKind::WouldBlock, "TX ring full"));
            }
            if e.raw_os_error() == Some(libc::ENXIO) || e.raw_os_error() == Some(libc::ENODEV) {
                return Err(e);
            }
            Err(e)
        } else {
            Ok(ret as usize)
        }
    }

    /// Receive a raw frame. Returns bytes read or 0 on timeout.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let ret = unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
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

    pub fn iface(&self) -> &str {
        &self.iface
    }
}

impl Drop for RawSocketBase {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe {
                libc::close(self.fd);
            }
            self.fd = -1;
        }
    }
}

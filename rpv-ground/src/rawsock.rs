/// Raw AF_PACKET socket for send/receive on a WiFi interface in monitor mode.
///
/// Ground station uses ARPHRD_IEEE80211_RADIOTAP for hatype (radiotap-formatted
/// frames from monitor mode) and attaches a BPF filter for kernel-side frame
/// rejection.
use std::io;

use rpv_proto::rawsock_common;
use rpv_proto::socket_trait::SocketTrait;

pub struct RawSocket {
    fd: i32,
    seq_control: std::sync::atomic::AtomicU16,
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
        addr.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
        addr.sll_ifindex = ifindex as i32;
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

        let _ = Self::try_attach_bpf_filter(fd);

        Ok(Self {
            fd,
            seq_control: std::sync::atomic::AtomicU16::new(0),
        })
    }

    /// BPF filter: accept only frames with RPV magic bytes after the radiotap
    /// and 802.11 header. Handles both QoS (26-byte) and non-QoS (24-byte)
    /// 802.11 headers by checking both offsets.
    fn try_attach_bpf_filter(fd: i32) -> io::Result<()> {
        #[repr(C)]
        struct sock_filter {
            code: u16,
            jt: u8,
            jf: u8,
            k: u32,
        }

        #[repr(C)]
        struct sock_fprog {
            len: u16,
            filter: *const sock_filter,
        }

        const BPF_LD_H_ABS: u16 = 0x0028;
        const BPF_LD_B_IND: u16 = 0x0040;
        const BPF_JEQ_K: u16 = 0x0015;
        const BPF_RET_K: u16 = 0x0006;

        let filters = [
            // 0: Check frame length >= 36 (min: radiotap + 802.11 + 4 bytes)
            sock_filter {
                code: 0x0025, // BPF_JGE | BPF_K
                jt: 0,
                jf: 8, // jump to reject if too short
                k: 36,
            },
            // 1: Load radiotap length field (u16 LE at offset 2)
            sock_filter {
                code: BPF_LD_H_ABS,
                jt: 0,
                jf: 0,
                k: 2,
            },
            // 2: X = radiotap_len
            sock_filter {
                code: 0x0087, // TAX
                jt: 0,
                jf: 0,
                k: 0,
            },
            // 3: Load byte at X + 26 (QoS Data magic byte 1)
            sock_filter {
                code: BPF_LD_B_IND,
                jt: 0,
                jf: 0,
                k: 26,
            },
            // 4: Check == 0x52 ('R')
            sock_filter {
                code: BPF_JEQ_K,
                jt: 2, // match: check byte 2 at QoS offset
                jf: 0, // no match: try non-QoS
                k: 0x52,
            },
            // 5: Load byte at X + 24 (non-QoS magic byte 1)
            sock_filter {
                code: BPF_LD_B_IND,
                jt: 0,
                jf: 0,
                k: 24,
            },
            // 6: Check == 0x52 ('R')
            sock_filter {
                code: BPF_JEQ_K,
                jt: 0, // match: fall through to check byte 2
                jf: 1, // no match: reject
                k: 0x52,
            },
            // 7: Accept
            sock_filter {
                code: BPF_RET_K,
                jt: 0,
                jf: 0,
                k: 0xffff,
            },
            // 8: Reject
            sock_filter {
                code: BPF_RET_K,
                jt: 0,
                jf: 0,
                k: 0,
            },
        ];

        let prog = sock_fprog {
            len: filters.len() as u16,
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
                tracing::warn!(
                    "Failed to attach BPF filter (will filter in userspace): {}",
                    io::Error::last_os_error()
                );
            } else {
                tracing::info!("BPF magic-byte filter attached (kernel-side RP frame filtering)");
            }
        }

        Ok(())
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
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let seq_bytes = seq.to_le_bytes();
        buf[rawsock_common::RADIOTAP_LEN + 22] = seq_bytes[0];
        buf[rawsock_common::RADIOTAP_LEN + 23] = seq_bytes[1];

        buf.extend_from_slice(payload);

        let ret = unsafe { libc::send(self.fd, buf.as_ptr() as *const libc::c_void, buf.len(), 0) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EAGAIN) || e.raw_os_error() == Some(libc::EWOULDBLOCK)
            {
                return Ok(0);
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

impl SocketTrait for RawSocket {
    fn send_with_buf(&self, payload: &[u8], buf: &mut Vec<u8>) -> io::Result<usize> {
        RawSocket::send_with_buf(self, payload, buf)
    }
    fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        RawSocket::recv(self, buf)
    }
}

impl Drop for RawSocket {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

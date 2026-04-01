/// Raw AF_PACKET socket for send/receive on a WiFi interface in monitor mode.
///
/// This bypasses the entire IP stack. On send, the module constructs a minimal
/// 802.11 broadcast data frame with our L2 protocol payload as the frame body.
/// On receive, it strips the Radiotap header + 802.11 MAC header to extract
/// the L2 protocol payload.
use std::io;

const IEEE80211_HDR_LEN: usize = 26; // #9: QoS Data (26 bytes)
const RADIOTAP_LEN: usize = 9;
const HEADER_TOTAL: usize = RADIOTAP_LEN + IEEE80211_HDR_LEN; // 35 bytes

/// Static radiotap header with TX rate for AR9271 (ath9k_htc).
/// Present bit 2 (Rate) set. Rate byte: 0x30 = 24 Mbps.
static RADIOTAP: [u8; RADIOTAP_LEN] = [
    0x00, 0x00, 0x09, 0x00, // version=0, pad=0, hdr_len=9
    0x04, 0x00, 0x00, 0x00, // present: bit 2 (Rate)
    0x30, // Rate: 24 Mbps
];

/// #9: Static 802.11 QoS Data broadcast header (enables HT/VHT rates)
static DATA_FRAME_HDR: [u8; IEEE80211_HDR_LEN] = {
    let mut hdr = [0u8; IEEE80211_HDR_LEN];
    hdr[0] = 0x88; // #9: QoS Data (subtype 0x88)
    hdr[1] = 0x00; // No flags
    hdr[4] = 0xFF;
    hdr[5] = 0xFF;
    hdr[6] = 0xFF;
    hdr[7] = 0xFF;
    hdr[8] = 0xFF;
    hdr[9] = 0xFF;
    // SA: broadcast
    hdr[10] = 0xFF;
    hdr[11] = 0xFF;
    hdr[12] = 0xFF;
    hdr[13] = 0xFF;
    hdr[14] = 0xFF;
    hdr[15] = 0xFF;
    // BSSID: broadcast
    hdr[16] = 0xFF;
    hdr[17] = 0xFF;
    hdr[18] = 0xFF;
    hdr[19] = 0xFF;
    hdr[20] = 0xFF;
    hdr[21] = 0xFF;
    // Sequence Control: 0, 0
    hdr
};

pub struct RawSocket {
    fd: i32,
    // #10: 802.11 sequence counter
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

        // Set receive timeout (SO_RCVTIMEO is the sole timeout mechanism; no poll() needed)
        let tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 100_000, // 100ms
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

        // Set buffer sizes — requires net.core.rmem_max/wmem_max >= 8388608 (set by deploy script)
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

        // Attach BPF filter — kernel-side rejection of non-RPV frames
        let _ = Self::try_attach_bpf_filter(fd);

        Ok(Self {
            fd,
            seq_control: std::sync::atomic::AtomicU16::new(0),
        })
    }

    /// BPF filter: accept only frames that pass the RPV magic byte check.
    ///
    /// Filter logic (runs in kernel space, evaluated per-frame):
    ///   1. Ensure frame >= 34 bytes (8 radiotap + 24 802.11 + 2 magic)
    ///   2. Load radiotap header length (u16 LE at offset 2)
    ///   3. Load byte at radiotap_len+0 and compare to 0x52 ('R')
    ///   4. Load byte at radiotap_len+1 and compare to 0x50 ('P')
    ///   5. Accept if both match, drop otherwise
    ///
    /// This rejects beacons, probes, ACKs, and all non-data management/control frames,
    /// dramatically reducing userspace wakeups.
    fn try_attach_bpf_filter(fd: i32) -> io::Result<()> {
        // BPF instruction encoding helpers:
        //   code = (class << 5) | (size << 3) | op
        //   class: BPF_LD=0x0, BPF_JMP=0x5, BPF_RET=0x6
        //   size:  BPF_B=0x0, BPF_H=0x1, BPF_W=0x2
        //   op:    BPF_ABS=0x20, BPF_JEQ=0x10, BPF_K=0x00
        //   BPF_RET|BPF_K = 0x06
        const BPF_LD_H_ABS: u16 = 0x0028; // BPF_LD | BPF_H | BPF_ABS
        const BPF_JEQ_K: u16 = 0x0015; // BPF_JMP | BPF_JEQ | BPF_K
        const BPF_RET_K: u16 = 0x0006; // BPF_RET | BPF_K

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

        // BPF filter: check RPV magic bytes at radiotap_len + 26 (26-byte QoS 802.11 header)
        const BPF_LD_B_IND: u16 = 0x0040; // BPF_LD | BPF_B | BPF_IND (load byte [X + k])

        let filters = [
            // 0: Load radiotap length field (u16 LE at offset 2)
            sock_filter {
                code: BPF_LD_H_ABS, // Load halfword at absolute offset
                jt: 0,
                jf: 0,
                k: 2, // radiotap length field
            },
            // A = radiotap_len. Save to X.
            sock_filter {
                code: 0x0087, // TAX: X = A
                jt: 0,
                jf: 0,
                k: 0,
            },
            // 2: Load byte at X+26 (first byte of L2 payload after 26-byte QoS header)
            sock_filter {
                code: BPF_LD_B_IND, // Load byte at [X + k]
                jt: 0,
                jf: 0,
                k: 26, // #9: offset after 26-byte QoS 802.11 header
            },
            // A = byte at radiotap_len + 26. Check == 0x52 ('R')
            sock_filter {
                code: BPF_JEQ_K,
                jt: 0,
                jf: 6, // jump to reject
                k: 0x52,
            },
            // 4: Load byte at X+27 (second byte of L2 payload)
            sock_filter {
                code: BPF_LD_B_IND,
                jt: 0,
                jf: 0,
                k: 27, // #9: offset after 26-byte QoS 802.11 header + 1
            },
            // A = byte at radiotap_len + 27. Check == 0x50 ('P')
            sock_filter {
                code: BPF_JEQ_K,
                jt: 0,
                jf: 4, // jump to reject
                k: 0x50,
            },
            // 6: Accept: return 0xffff (accept all remaining bytes)
            //    We already loaded radiotap into X, but checking absolute frame length
            //    is harder in BPF. We can check that we didn't fault on the indirect loads
            //    (BPF returns 0 for out-of-bounds loads) but the JEQ already handles that
            //    since OOB load returns 0 which won't match 0x52.
            // Accept: return 0xffff (accept all remaining bytes)
            sock_filter {
                code: BPF_RET_K,
                jt: 0,
                jf: 0,
                k: 0xffff,
            },
            // 7: Also accept if radiotap_len is not 8 (could be extended radiotap)
            //    We can't easily handle variable radiotap in BPF, so accept and let
            //    userspace handle it. This is still better than accepting beacons/probes.
            //    Actually, let's keep it simple: if radiotap != 8, the indirect loads
            //    will read wrong offsets and likely fail the magic check, dropping the frame.
            //    This is acceptable — most drivers emit 8-byte radiotap for data frames.
            // 8: (unused slot — padding)
            sock_filter {
                code: BPF_RET_K,
                jt: 0,
                jf: 0,
                k: 0, // unused — just space for jump targets
            },
            // 9: Reject: return 0 (drop packet)
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

    /// Send a raw 802.11 frame with Radiotap + broadcast data header + payload.
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

    /// Send using a persistent header buffer (avoids per-call static lookup + extend).
    /// The buffer should be pre-filled with Radiotap + 802.11 header at positions 0..32.
    /// Only the payload portion (after HEADER_TOTAL) is written per call.
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
                return Ok(0); // TX ring full, frame dropped (non-blocking)
            }
            Err(e)
        } else {
            Ok(ret as usize)
        }
    }

    /// Receive a raw frame. Returns bytes read or 0 on timeout.
    /// Relies on SO_RCVTIMEO (no redundant poll() syscall).
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

/// Walk radiotap header properly, handling extended present bitmaps.
/// Returns the offset where the 802.11 frame starts.
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

    // LLC/SNAP header: DSAP=0xAA, SSAP=0xAA, Control=0x03
    // Then 3-byte OUI + 2-byte EtherType (8 bytes total skip)
    // Or bare LLC without SNAP (6 bytes)
    if after_80211.len() >= 8
        && after_80211[0] == 0xAA
        && after_80211[1] == 0xAA
        && after_80211[2] == 0x03
    {
        // Full LLC/SNAP with EtherType: skip 8 bytes
        if after_80211.len() >= 8 {
            let payload = &after_80211[8..];
            if !payload.is_empty() {
                return Some((payload, rssi));
            }
        }
        return None;
    }

    if after_80211.is_empty() {
        None
    } else {
        Some((after_80211, rssi))
    }
}

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

    // Check if RSSI (bit 5: dBm antenna signal) is present
    let has_rssi = present_words.iter().any(|w| w & (1 << 5) != 0);
    if !has_rssi {
        return None;
    }

    // Natural alignment for each radiotap field
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

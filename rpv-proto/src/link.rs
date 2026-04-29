/// Layer 2 protocol header for raw 802.11 frame multiplexing.
///
/// Layout (8 bytes fixed header):
///   [0..2]  Magic: 0x52 0x50 ("RP")
///   [2]     Drone ID: filters frames from other swarms (1-255)
///   [3]     Payload Type: 0x01=Video, 0x02=Telemetry, 0x03=RC, 0x04=Heartbeat, 0x05=MAVLink
///   [4..8]  Sequence number (u32 LE) - enables frame ordering and loss detection
pub const MAGIC: [u8; 2] = [0x52, 0x50];
pub const HEADER_LEN: usize = 8;

/// Video data: RS-encoded FEC shards with video header
pub const PAYLOAD_VIDEO: u8 = 0x01;
/// Telemetry: JSON format from camera (lat, lon, alt, etc.)
pub const PAYLOAD_TELEMETRY: u8 = 0x02;
/// RC commands: 16-channel PWM values (1000-2000)
pub const PAYLOAD_RC: u8 = 0x03;
/// Heartbeat: link health indicator (rpv-bea + seq + timestamp)
pub const PAYLOAD_HEARTBEAT: u8 = 0x04;
/// MAVLink: raw FC communication frames
pub const PAYLOAD_MAVLINK: u8 = 0x05;

/// Maximum safe payload size for 802.11 frame without fragmentation.
pub const MAX_PAYLOAD: usize = 1400;

#[derive(Debug, Clone, Copy)]
pub struct L2Header {
    pub drone_id: u8,
    pub payload_type: u8,
    pub seq: u32,
}

impl L2Header {
    /// Encode header + payload into a reusable buffer (avoids per-call allocation).
    /// Clears the buffer first, then writes: MAGIC | drone_id | payload_type | seq | payload.
    pub fn encode_into(&self, payload: &[u8], buf: &mut Vec<u8>) {
        buf.clear();
        buf.reserve(HEADER_LEN + payload.len());
        buf.extend_from_slice(&MAGIC);
        buf.push(self.drone_id);
        buf.push(self.payload_type);
        buf.extend_from_slice(&self.seq.to_le_bytes());
        buf.extend_from_slice(payload);
    }

    /// Decode a framed L2 header from raw bytes.
    /// Returns Some((header, payload)) if valid, None otherwise.
    #[inline]
    pub fn decode(frame: &[u8]) -> Option<(L2Header, &[u8])> {
        if frame.len() < HEADER_LEN {
            return None;
        }
        if frame[0] != MAGIC[0] || frame[1] != MAGIC[1] {
            return None;
        }
        let header = L2Header {
            drone_id: frame[2],
            payload_type: frame[3],
            seq: u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]),
        };
        Some((header, &frame[HEADER_LEN..]))
    }

    /// Fast magic check — avoids full HEADER_LEN check since rawsock
    /// already verified frame size.
    ///
    /// Use this as a filter before calling `decode()` to avoid
    /// unnecessary processing for non-RPV frames.
    #[inline]
    pub fn matches_magic(frame: &[u8]) -> bool {
        frame.len() >= 2 && frame[0] == MAGIC[0] && frame[1] == MAGIC[1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_into_produces_correct_bytes() {
        let hdr = L2Header {
            seq: 123,
            payload_type: 1,
            drone_id: 42,
        };
        let mut buf = Vec::new();
        hdr.encode_into(&[], &mut buf);
        assert_eq!(buf[0], MAGIC[0]);
        assert_eq!(buf[1], MAGIC[1]);
        assert_eq!(buf[2], 42);
        assert_eq!(buf[3], 1);
        assert_eq!(buf[4..8], 123u32.to_le_bytes());
    }

    #[test]
    fn decode_valid_header() {
        let mut frame = vec![];
        frame.extend_from_slice(&MAGIC);
        frame.push(42);
        frame.push(1);
        frame.extend_from_slice(&123u32.to_le_bytes());
        frame.extend_from_slice(b"payload");
        let result = L2Header::decode(&frame);
        assert!(result.is_some());
        let (hdr, payload) = result.unwrap();
        assert_eq!(hdr.drone_id, 42);
        assert_eq!(hdr.payload_type, 1);
        assert_eq!(hdr.seq, 123);
        assert_eq!(payload, b"payload");
    }

    #[test]
    fn decode_invalid_magic() {
        let frame = [0x00, 0x00, 42, 1, 0, 0, 0, 0];
        assert!(L2Header::decode(&frame).is_none());
    }

    #[test]
    fn decode_too_short() {
        let frame = [0x52, 0x50, 42, 1, 0, 0, 0];
        assert!(L2Header::decode(&frame).is_none());
    }

    #[test]
    fn matches_magic_valid() {
        let frame = [0x52, 0x50, 0, 0, 0, 0, 0, 0];
        assert!(L2Header::matches_magic(&frame));
    }

    #[test]
    fn matches_magic_invalid() {
        let frame = [0x00, 0x00, 0, 0, 0, 0, 0, 0];
        assert!(!L2Header::matches_magic(&frame));
    }

    #[test]
    fn matches_magic_too_short() {
        let frame = [0x52];
        assert!(!L2Header::matches_magic(&frame));
    }

    #[test]
    fn round_trip_encode_decode() {
        let original = L2Header {
            seq: 999,
            payload_type: PAYLOAD_VIDEO,
            drone_id: 100,
        };
        let mut buf = Vec::new();
        let payload = b"test data";
        original.encode_into(payload, &mut buf);
        let result = L2Header::decode(&buf).unwrap();
        assert_eq!(result.0.seq, original.seq);
        assert_eq!(result.0.payload_type, original.payload_type);
        assert_eq!(result.0.drone_id, original.drone_id);
        assert_eq!(result.1, payload);
    }

    #[test]
    fn edge_case_max_seq() {
        let hdr = L2Header {
            seq: u32::MAX,
            payload_type: 1,
            drone_id: 42,
        };
        let mut buf = Vec::new();
        hdr.encode_into(&[], &mut buf);
        let (decoded, _) = L2Header::decode(&buf).unwrap();
        assert_eq!(decoded.seq, u32::MAX);
    }

    #[test]
    fn edge_case_zero_drone_id() {
        let hdr = L2Header {
            seq: 0,
            payload_type: 1,
            drone_id: 0,
        };
        let mut buf = Vec::new();
        hdr.encode_into(&[], &mut buf);
        let (decoded, _) = L2Header::decode(&buf).unwrap();
        assert_eq!(decoded.drone_id, 0);
    }

    #[test]
    fn various_payload_types() {
        for &pt in &[
            PAYLOAD_VIDEO,
            PAYLOAD_TELEMETRY,
            PAYLOAD_RC,
            PAYLOAD_HEARTBEAT,
            PAYLOAD_MAVLINK,
        ] {
            let hdr = L2Header {
                seq: 1,
                payload_type: pt,
                drone_id: 1,
            };
            let mut buf = Vec::new();
            hdr.encode_into(&[], &mut buf);
            let (decoded, _) = L2Header::decode(&buf).unwrap();
            assert_eq!(decoded.payload_type, pt);
        }
    }
}

/// Layer 2 protocol header for raw 802.11 frame multiplexing.
///
/// Layout (8 bytes fixed header):
///   [0..2]  Magic: 0x52 0x50 ("RP")
///   [2]     Drone ID: filters frames from other swarms
///   [3]     Payload Type: 0x01=Video, 0x02=Telemetry, 0x03=RC, 0x04=Heartbeat
///   [4..8]  Sequence number (u32 LE)
///   [8..]   Payload

pub const MAGIC: [u8; 2] = [0x52, 0x50];
pub const HEADER_LEN: usize = 8;

pub const PAYLOAD_VIDEO: u8 = 0x01;
pub const PAYLOAD_TELEMETRY: u8 = 0x02;
pub const PAYLOAD_RC: u8 = 0x03;
pub const PAYLOAD_HEARTBEAT: u8 = 0x04;

/// Maximum safe payload size for 802.11 frame without fragmentation.
/// Conservative limit: ~1400 bytes after accounting for L2 headers.
#[allow(dead_code)]
pub const MAX_PAYLOAD: usize = 1400;

#[derive(Debug, Clone, Copy)]
pub struct L2Header {
    pub drone_id: u8,
    pub payload_type: u8,
    pub seq: u32,
}

impl L2Header {
    pub fn encode(&self, payload: &[u8]) -> Vec<u8> {
        let total = HEADER_LEN + payload.len();
        let mut buf = Vec::with_capacity(total);
        buf.extend_from_slice(&MAGIC);
        buf.push(self.drone_id);
        buf.push(self.payload_type);
        buf.extend_from_slice(&self.seq.to_le_bytes());
        buf.extend_from_slice(payload);
        buf
    }

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

    /// Check if a buffer starts with the RPV magic bytes (fast filter).
    pub fn matches_magic(frame: &[u8]) -> bool {
        frame.len() >= HEADER_LEN && frame[0] == MAGIC[0] && frame[1] == MAGIC[1]
    }
}

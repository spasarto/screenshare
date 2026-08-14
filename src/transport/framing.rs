/// Message chunking for WebRTC data channels.
///
/// SCTP (underlying WebRTC data channels) has a max message size (~64KB).
/// H264 keyframes at high resolutions can exceed this. This module splits
/// large messages into chunks and reassembles them on the receiving end.
///
/// Protocol (data channels are ordered + reliable by default):
/// - Single message:    [0x00][4-byte BE total_len][payload]
/// - Chunked start:     [0x01][4-byte BE total_len]
/// - Chunked data:      [0x02][chunk_payload]
/// - Signaling message: [0x03][payload]  (JSON-serialized SignalingBlob)
const MAX_CHUNK_PAYLOAD: usize = 15000; // Well under 16KB SCTP limit

/// Upper bound on a reassembled message's declared length. Guards the
/// peer-supplied 32-bit length in the chunked-start frame: without this, a
/// hostile or malformed 5-byte message can claim a length up to 4 GiB and
/// drive an allocation of that size via `Vec::reserve`.
const MAX_MESSAGE_SIZE: usize = 8 * 1024 * 1024; // 8 MiB, comfortably above any keyframe we produce

pub const MSG_SIGNALING: u8 = 0x03;

/// Encode a signaling payload (JSON bytes) into a framed message.
pub fn encode_signaling(json: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(1 + json.len());
    msg.push(MSG_SIGNALING);
    msg.extend_from_slice(json);
    msg
}

/// Incoming message classification.
pub enum IncomingMessage {
    /// Video data (complete reassembled frame)
    VideoData(Vec<u8>),
    /// Signaling message (raw JSON bytes)
    Signaling(Vec<u8>),
    /// More chunks needed, or invalid
    Pending,
}

/// Split a message into data channel frames.
pub fn encode_chunked(data: &[u8]) -> Vec<Vec<u8>> {
    if data.len() <= MAX_CHUNK_PAYLOAD {
        // Fits in a single message
        let mut msg = Vec::with_capacity(5 + data.len());
        msg.push(0x00);
        msg.extend_from_slice(&(data.len() as u32).to_be_bytes());
        msg.extend_from_slice(data);
        vec![msg]
    } else {
        // Need chunking
        let mut frames = Vec::new();

        // Header frame
        let mut header = Vec::with_capacity(5);
        header.push(0x01);
        header.extend_from_slice(&(data.len() as u32).to_be_bytes());
        frames.push(header);

        // Data frames
        for chunk in data.chunks(MAX_CHUNK_PAYLOAD) {
            let mut msg = Vec::with_capacity(1 + chunk.len());
            msg.push(0x02);
            msg.extend_from_slice(chunk);
            frames.push(msg);
        }

        frames
    }
}

/// Reassembly state machine for incoming chunks.
pub struct Reassembler {
    expected: usize,
    buffer: Vec<u8>,
    active: bool,
}

impl Reassembler {
    pub fn new() -> Self {
        Self {
            expected: 0,
            buffer: Vec::new(),
            active: false,
        }
    }

    /// Reset reassembly state, discarding any partial message.
    pub fn reset(&mut self) {
        self.expected = 0;
        self.buffer.clear();
        self.active = false;
    }

    /// Feed a raw data channel message. Returns `Some(complete_message)` when
    /// a full message has been reassembled, or `None` if more chunks are needed.
    pub fn feed(&mut self, msg: &[u8]) -> Option<Vec<u8>> {
        if msg.is_empty() {
            return None;
        }

        match msg[0] {
            0x00 => {
                // Complete single message
                if msg.len() < 5 {
                    return None;
                }
                let len = u32::from_be_bytes([msg[1], msg[2], msg[3], msg[4]]) as usize;
                if msg.len() < 5 + len {
                    return None;
                }
                self.active = false;
                Some(msg[5..5 + len].to_vec())
            }
            0x01 => {
                // Start of chunked message
                if msg.len() < 5 {
                    return None;
                }
                let len = u32::from_be_bytes([msg[1], msg[2], msg[3], msg[4]]) as usize;
                if len > MAX_MESSAGE_SIZE {
                    // Refuse to reserve for an implausible length. Leave `active`
                    // false so any continuation chunks for this bogus start are
                    // dropped rather than accumulated.
                    self.active = false;
                    return None;
                }
                self.expected = len;
                self.buffer.clear();
                self.buffer.reserve(len);
                self.active = true;
                None
            }
            0x02 => {
                // Continuation chunk
                if !self.active {
                    return None;
                }
                self.buffer.extend_from_slice(&msg[1..]);
                if self.buffer.len() >= self.expected {
                    self.active = false;
                    self.buffer.truncate(self.expected);
                    Some(std::mem::take(&mut self.buffer))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Feed a raw message and classify it as video data, signaling, or pending.
    pub fn feed_classified(&mut self, msg: &[u8]) -> IncomingMessage {
        if msg.is_empty() {
            return IncomingMessage::Pending;
        }
        if msg[0] == MSG_SIGNALING {
            return IncomingMessage::Signaling(msg[1..].to_vec());
        }
        match self.feed(msg) {
            Some(data) => IncomingMessage::VideoData(data),
            None => IncomingMessage::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(size: usize) {
        let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let frames = encode_chunked(&data);
        let mut reassembler = Reassembler::new();

        let mut result = None;
        let last = frames.len() - 1;
        for (i, frame) in frames.iter().enumerate() {
            let out = reassembler.feed(frame);
            if i == last {
                result = out;
            } else {
                assert!(out.is_none(), "frame {i}/{last} produced output early");
            }
        }

        assert_eq!(result, Some(data), "roundtrip failed for size {size}");
    }

    #[test]
    fn roundtrip_one_byte() {
        roundtrip(1);
    }

    #[test]
    fn roundtrip_exactly_max_chunk_payload() {
        roundtrip(MAX_CHUNK_PAYLOAD);
    }

    #[test]
    fn roundtrip_one_over_max_chunk_payload() {
        roundtrip(MAX_CHUNK_PAYLOAD + 1);
    }

    #[test]
    fn roundtrip_several_chunks() {
        roundtrip(MAX_CHUNK_PAYLOAD * 2);
        roundtrip(MAX_CHUNK_PAYLOAD * 2 + 1);
        roundtrip(MAX_CHUNK_PAYLOAD * 3 + 123);
    }

    #[test]
    fn truncated_single_message_header() {
        let mut reassembler = Reassembler::new();
        assert!(reassembler.feed(&[0x00]).is_none());
    }

    #[test]
    fn truncated_chunk_start_header() {
        let mut reassembler = Reassembler::new();
        assert!(reassembler.feed(&[0x01, 0xFF]).is_none());
    }

    #[test]
    fn continuation_chunk_without_start_is_dropped() {
        let mut reassembler = Reassembler::new();
        assert!(reassembler.feed(&[0x02, 1, 2, 3]).is_none());

        // A stray 0x02 must not leave the reassembler in a bad state: a
        // proper start + chunk still reassembles correctly afterwards.
        let data = vec![9u8; 10];
        let frames = encode_chunked(&data);
        let mut result = None;
        for frame in &frames {
            result = reassembler.feed(frame);
        }
        assert_eq!(result, Some(data));
    }

    #[test]
    fn declared_length_far_larger_than_data_stays_pending() {
        let mut reassembler = Reassembler::new();
        let mut start = vec![0x01];
        start.extend_from_slice(&(1_000_000u32).to_be_bytes());
        assert!(reassembler.feed(&start).is_none());

        // Only a handful of bytes actually show up; must not complete or panic.
        let mut chunk = vec![0x02];
        chunk.extend_from_slice(&[1, 2, 3, 4, 5]);
        assert!(reassembler.feed(&chunk).is_none());
    }

    #[test]
    fn declared_length_above_max_message_size_is_rejected() {
        let mut reassembler = Reassembler::new();
        let mut start = vec![0x01];
        start.extend_from_slice(&(0xFFFFFFFFu32).to_be_bytes());
        assert!(reassembler.feed(&start).is_none());

        // The bogus start must not have armed reassembly: a follow-up chunk
        // is dropped rather than accumulated toward a 4 GiB buffer.
        assert!(reassembler.feed(&[0x02, 1, 2, 3]).is_none());
    }

    #[test]
    fn signaling_frames_interleaved_with_video_chunks() {
        let data = vec![7u8; MAX_CHUNK_PAYLOAD * 2 + 50];
        let frames = encode_chunked(&data);
        assert!(frames.len() >= 3, "test needs a multi-chunk message");

        let mut reassembler = Reassembler::new();
        let signaling_payload = br#"{"type":"ice-restart"}"#;
        let signaling_frame = encode_signaling(signaling_payload);

        let mut video_result = None;
        for (i, frame) in frames.iter().enumerate() {
            match reassembler.feed_classified(&signaling_frame) {
                IncomingMessage::Signaling(payload) => {
                    assert_eq!(payload, signaling_payload);
                }
                _ => panic!("signaling frame misclassified"),
            }

            match reassembler.feed_classified(frame) {
                IncomingMessage::VideoData(d) => video_result = Some(d),
                IncomingMessage::Pending => {}
                IncomingMessage::Signaling(_) => panic!("video frame {i} misclassified as signaling"),
            }
        }

        assert_eq!(video_result, Some(data));
    }
}

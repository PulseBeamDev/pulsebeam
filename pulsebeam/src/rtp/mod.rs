pub mod jitter_buffer;
pub mod monitor;
pub mod rtp_rewriter;

use std::ops::{Deref, DerefMut};

use str0m::{media::MediaTime, rtp::SeqNo};
use tokio::time::Instant;

#[derive(Debug)]
pub struct RtpPacket {
    inner: str0m::rtp::RtpPacket,
}

impl Deref for RtpPacket {
    type Target = str0m::rtp::RtpPacket;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for RtpPacket {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl From<str0m::rtp::RtpPacket> for RtpPacket {
    fn from(value: str0m::rtp::RtpPacket) -> Self {
        Self { inner: value }
    }
}

impl AsRef<str0m::rtp::RtpPacket> for RtpPacket {
    fn as_ref(&self) -> &str0m::rtp::RtpPacket {
        &self.inner
    }
}

impl RtpPacket {
    /// Heuristically detect whether this RTP packet appears to start a full intra (keyframe).
    ///
    /// Notes:
    /// - Stateless: only inspects this RTP packet.
    /// - For H.264, detects IDR slices or the start fragment (FU-A with NAL type 5).
    /// - For VP8/VP9, checks the "keyframe" and "start of frame" bits.
    /// - Out-of-order packets are fine: detection is opportunistic, not sequential.
    ///
    /// Reference specs:
    /// - H.264: RFC 6184 §5.2
    /// - VP8:  RFC 7741 §4.2
    /// - VP9:  W3C WebRTC VP9 RTP Payload Format
    pub fn is_keyframe(&self) -> bool {
        let payload = &self.payload;
        if payload.is_empty() {
            return false;
        }

        // You might have a payload type map if you know what codec is in use.
        // But most WebRTC or RTP demuxers heuristically detect codec by payload content.
        if looks_like_h264(payload) {
            return is_h264_keyframe_start(payload);
        }

        if looks_like_vp8(payload) {
            return is_vp8_keyframe_start(payload);
        }

        if looks_like_vp9(payload) {
            return is_vp9_keyframe_start(payload);
        }

        false
    }
}

/// Roughly guess H.264 by looking for NALU start patterns.
fn looks_like_h264(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }
    let nal_type = payload[0] & 0x1F;
    (1..=23).contains(&nal_type) || nal_type == 28
}

/// Detects if an H264 payload is a keyframe.
/// This code was taken from https://github.com/jech/galene/blob/codecs/rtpconn/rtpreader.go#L45
/// All credits belong to Juliusz Chroboczek @jech and the awesome Galene SFU.
fn is_h264_keyframe_start(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }

    let nalu = payload[0] & 0x1F;

    match nalu {
        0 => {
            // reserved
            false
        }
        1..=23 => {
            // Simple NALU
            // NALU type 7 is a Sequence Parameter Set (SPS), which indicates a keyframe.
            nalu == 7
        }
        24..=27 => {
            // STAP-A, STAP-B, MTAP16, or MTAP24
            let mut i = 1;
            if nalu >= 25 {
                // Skip DON (Decoding Order Number)
                i += 2;
            }

            while i < payload.len() {
                if i + 2 > payload.len() {
                    return false;
                }
                // Read the 16-bit length of the NAL unit
                let length = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
                i += 2;

                if i + length > payload.len() {
                    return false;
                }

                let mut offset = 0;
                if nalu == 26 {
                    // MTAP16
                    offset = 3;
                } else if nalu == 27 {
                    // MTAP24
                    offset = 4;
                }

                if offset >= length {
                    return false;
                }

                let n = payload[i + offset] & 0x1F;
                if n == 7 {
                    return true;
                }
                // The original Go code has a debug log here for n >= 24.

                i += length;
            }

            // If we've processed all aggregated NALUs and found no keyframe indicator
            false
        }
        28 | 29 => {
            // FU-A or FU-B
            if payload.len() < 2 {
                return false;
            }
            // Check for the start bit
            if (payload[1] & 0x80) == 0 {
                // Not a starting fragment
                return false;
            }
            // Check if the fragmented NALU type is an SPS
            (payload[1] & 0x1F) == 7
        }
        _ => false, // 30, 31 are reserved or undefined
    }
}

/// Rough VP8 detection — first byte pattern with 0x10 (start of partition).
fn looks_like_vp8(payload: &[u8]) -> bool {
    let b0 = payload[0];
    // For VP8, bits 0..2 are usually 0b000 or 0b001 for normal streams.
    b0 & 0b1110_0000 == 0b0000_0000 || b0 & 0b1110_0000 == 0b0010_0000
}

/// Detect if this VP8 RTP packet appears to start a keyframe.
///
/// VP8 RTP Payload (RFC 7741):
/// - Bit 0 (P): 0 = keyframe, 1 = interframe.
/// - Bit 4..7 (PartID): 0 means start of a new frame.
/// - Optional X field may follow.
fn is_vp8_keyframe_start(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }

    let b0 = payload[0];
    let x_bit = (b0 & 0x80) != 0;
    let p_bit = (b0 & 0x01) != 0; // 0 = keyframe
    let part_id = (b0 >> 4) & 0x0F;
    let start_of_frame = part_id == 0;

    if !start_of_frame || p_bit {
        return false;
    }

    // Optionally skip extended control fields (rare but valid)
    let mut idx = 1;
    if x_bit && payload.len() > 1 {
        let x_field = payload[idx];
        idx += 1;
        if x_field & 0x80 != 0 {
            // PictureID
            if idx >= payload.len() {
                return false;
            }
            let picid = payload[idx];
            idx += 1;
            if picid & 0x80 != 0 {
                idx += 1; // long PictureID
            }
        }
        if x_field & 0x40 != 0 {
            idx += 1; // TL0PICIDX
        }
        if x_field & 0x20 != 0 {
            idx += 1; // TID/Y/KEYIDX
        }
    }

    // If we reached here, it's the first packet of a keyframe
    true
}

/// Rough VP9 detection — first byte often starts with I|P|L|F|B|E|V|Z bits.
fn looks_like_vp9(payload: &[u8]) -> bool {
    let b0 = payload[0];
    // I bit is MSB; P bit next. B bit marks beginning of frame.
    (b0 & 0xE0) == 0x80 || (b0 & 0xC0) == 0x00
}

/// Detect if this VP9 RTP packet appears to start a keyframe.
///
/// VP9 RTP Payload:
/// - I bit (MSB): 1 if PictureID present
/// - P bit: 0 = keyframe, 1 = interframe
/// - B bit: 1 = beginning of frame
fn is_vp9_keyframe_start(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }

    let b0 = payload[0];
    let b_bit = (b0 & 0x08) != 0; // beginning of frame
    let p_bit = (b0 & 0x40) != 0; // inter-frame indicator (1 = inter, 0 = keyframe)

    b_bit && !p_bit
}

pub trait PacketTiming {
    fn seq_no(&self) -> SeqNo;
    fn rtp_timestamp(&self) -> MediaTime;
    fn arrival_timestamp(&self) -> Instant;
}

impl PacketTiming for RtpPacket {
    #[inline]
    fn seq_no(&self) -> SeqNo {
        self.inner.seq_no
    }

    #[inline]
    fn rtp_timestamp(&self) -> MediaTime {
        self.inner.time
    }

    #[inline]
    fn arrival_timestamp(&self) -> Instant {
        self.inner.timestamp.into()
    }
}

pub trait Packet: PacketTiming {
    /// True if this is the last packet of a video frame.
    fn marker(&self) -> bool;

    /// Heuristically detect whether this RTP packet appears to start a keyframe.
    fn is_keyframe(&self) -> bool;
}

impl Packet for RtpPacket {
    #[inline]
    fn marker(&self) -> bool {
        self.inner.header.marker
    }

    #[inline]
    fn is_keyframe(&self) -> bool {
        self.is_keyframe()
    }
}

#[derive(Clone, Copy)]
pub struct TimingHeader {
    pub seq_no: SeqNo,
    pub rtp_ts: MediaTime,
    pub server_ts: Instant,
    pub marker: bool,
    pub is_keyframe: bool,
}

impl PacketTiming for TimingHeader {
    fn seq_no(&self) -> SeqNo {
        self.seq_no
    }

    fn rtp_timestamp(&self) -> MediaTime {
        self.rtp_ts
    }

    fn arrival_timestamp(&self) -> Instant {
        self.server_ts
    }
}

impl<T: Packet> From<&T> for TimingHeader {
    fn from(value: &T) -> Self {
        Self {
            seq_no: value.seq_no(),
            rtp_ts: value.rtp_timestamp(),
            server_ts: value.arrival_timestamp(),
            marker: value.marker(),
            is_keyframe: value.is_keyframe(),
        }
    }
}

impl TimingHeader {
    pub fn new(seq_no: SeqNo, rtp_ts: MediaTime, arrival_ts: Instant) -> Self {
        Self {
            seq_no,
            rtp_ts,
            server_ts: arrival_ts,
            marker: false,
            is_keyframe: false,
        }
    }
}

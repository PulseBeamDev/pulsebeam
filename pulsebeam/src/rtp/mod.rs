pub mod buffer;
pub mod monitor;
pub mod switcher;
pub mod sync;
pub mod timeline;

use bytes::Bytes;
use str0m::media::{Frequency, MediaTime};
use str0m::rtp::rtcp::SenderInfo;
use str0m::rtp::{RtpHeader, SeqNo};
use tokio::time::Instant;

/// The standard 90kHz clock rate for video RTP, used for all internal timestamp math.
/// TODO: get these clocks from SDP instead.
pub const VIDEO_FREQUENCY: Frequency = Frequency::NINETY_KHZ;
pub const AUDIO_FREQUENCY: Frequency = Frequency::FORTY_EIGHT_KHZ;

#[derive(Debug, Clone, Copy)]
pub enum Codec {
    H264,
    VP8,
    VP9,
    Opus,
}

/// Unified internal RTP packet representation used across the SFU.
/// This struct is designed for mutability and composition in middleware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpPacket {
    pub raw_header: RtpHeader,
    pub seq_no: SeqNo,
    pub rtp_ts: MediaTime,
    pub arrival_ts: Instant,

    /// Scheduled playout time for the packet, in the server's monotonic clock domain.
    /// Since all streams in this process share the same monotonic clock, this time can
    /// be compared directly between unrelated streams for scheduling or synchronization.
    pub playout_time: Instant,
    pub is_keyframe_start: bool,
    pub last_sender_info: Option<SenderInfo>,
    pub payload: Bytes,
}

impl Default for RtpPacket {
    fn default() -> Self {
        let header = RtpHeader {
            sequence_number: 1,
            timestamp: 0,
            ssrc: 1234.into(),
            ..RtpHeader::default()
        };

        Self {
            raw_header: header.clone(),
            seq_no: SeqNo::from(header.sequence_number as u64),
            rtp_ts: MediaTime::new(header.timestamp as u64, VIDEO_FREQUENCY),
            arrival_ts: Instant::now(),
            playout_time: Instant::now(),
            is_keyframe_start: false,
            last_sender_info: None,
            payload: Bytes::from_static(&[0u8; 1200]), // 1.2KB payload for test realism
        }
    }
}

impl RtpPacket {
    pub fn from_str0m(rtp: str0m::rtp::RtpPacket, codec: Codec) -> Self {
        let is_keyframe_start = match codec {
            Codec::H264 => is_h264_keyframe_start(&rtp.payload),
            Codec::VP8 => is_vp8_keyframe_start(&rtp.payload),
            Codec::VP9 => is_vp9_keyframe_start(&rtp.payload),
            Codec::Opus => true, // audio frame has not dependencies,
        };

        Self {
            raw_header: rtp.header,
            seq_no: rtp.seq_no,
            rtp_ts: rtp.time,
            arrival_ts: rtp.timestamp.into(),
            playout_time: rtp.timestamp.into(),
            is_keyframe_start,
            last_sender_info: None,
            payload: Bytes::from(rtp.payload),
        }
    }

    pub fn with_playout_time(mut self, playout_time: Instant) -> Self {
        self.playout_time = playout_time;
        self
    }
}

// adapted from Galene, https://github.com/jech/galene/blob/e09b135cd2ff240626b8619913bb9b2b3bba242b/codecs/codecs.go
fn is_h264_keyframe_start(payload: &[u8]) -> bool {
    let nalu = payload[0] & 0x1F;

    if nalu == 0 {
        // Reserved
        return false;
    } else if nalu <= 23 {
        // Simple NALU.
        return nalu == 7;
    } else if nalu >= 24 && nalu <= 27 {
        // STAP-A (24), STAP-B (25), MTAP16 (26), MTAP24 (27)
        let mut i = 1;

        // Skip DON (Decoding Order Number) for STAP-B, MTAP16, MTAP24
        if nalu == 25 || nalu == 26 || nalu == 27 {
            i += 2;
        }

        while i < payload.len() {
            // Need at least 2 bytes for length
            if i + 2 > payload.len() {
                return false;
            }

            // Read big-endian u16 length
            let length = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
            i += 2;

            if i + length > payload.len() {
                return false;
            }

            let mut offset = 0;
            if nalu == 26 {
                offset = 3; // MTAP16 TS offset
            } else if nalu == 27 {
                offset = 4; // MTAP24 TS offset
            }

            if offset >= length {
                // Malformed or empty NALU inside
                return false;
            }

            let inner_nalu_header = payload[i + offset];
            let n = inner_nalu_header & 0x1F;

            // STRICT PORT: Only checking for SPS (7)
            if n == 7 {
                return true;
            } else if n >= 24 {
                // Illegal nested aggregation
                return false;
            }

            i += length;
        }
        return false;
    } else if nalu == 28 || nalu == 29 {
        // FU-A (28) or FU-B (29)
        if payload.len() < 2 {
            return false;
        }
        let fu_header = payload[1];

        // Check Start Bit (0x80)
        if (fu_header & 0x80) == 0 {
            return false;
        }

        // STRICT PORT: Check if the original NALU type was 7 (SPS)
        return (fu_header & 0x1F) == 7;
    }

    false
}

fn is_vp8_keyframe_start(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }
    let b0 = payload[0];
    let p_bit = b0 & 0x01 != 0;
    let part_id = (b0 >> 4) & 0x0F;
    !p_bit && part_id == 0
}

fn is_vp9_keyframe_start(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }
    let b0 = payload[0];
    let b_bit = b0 & 0x08 != 0;
    let p_bit = b0 & 0x40 != 0;
    b_bit && !p_bit
}

#[cfg(test)]
pub mod test_utils {
    use std::time::Duration;

    use super::*;

    impl RtpPacket {
        fn next_seq(&self) -> Self {
            let mut new_packet = self.clone();
            new_packet.seq_no = self.seq_no.wrapping_add(1).into();
            new_packet.raw_header.sequence_number = *new_packet.seq_no as u16;
            new_packet
        }

        fn next_frame(&self) -> Self {
            let mut new_packet = self.next_seq();

            // Assuming 30fps video for test purposes.
            let rtp_ts_delta = 90_000 / 30; // 3000 ticks per frame
            let playout_time_delta = Duration::from_millis(1000 / 30);

            new_packet.rtp_ts = MediaTime::new(
                new_packet.rtp_ts.numer().wrapping_add(rtp_ts_delta),
                new_packet.rtp_ts.frequency(),
            );
            new_packet.raw_header.timestamp = new_packet.rtp_ts.numer() as u32;

            new_packet.playout_time = new_packet.playout_time + playout_time_delta;
            if let Some(at) = new_packet.arrival_ts.checked_add(playout_time_delta) {
                new_packet.arrival_ts = at;
            }

            new_packet
        }
    }

    pub type ScenarioStep = Box<dyn Fn(&RtpPacket) -> RtpPacket>;

    pub fn next_seq() -> ScenarioStep {
        Box::new(RtpPacket::next_seq)
    }
    pub fn next_frame() -> ScenarioStep {
        Box::new(RtpPacket::next_frame)
    }
    pub fn keyframe() -> ScenarioStep {
        Box::new(|prev| {
            let mut next = prev.next_frame();
            next.is_keyframe_start = true;
            next
        })
    }
    pub fn marker() -> ScenarioStep {
        Box::new(|prev| {
            let mut next = prev.next_seq();
            next.raw_header.marker = true;
            next
        })
    }
    pub fn simulcast_switch(new_ssrc: u32, start_seq: u16, start_ts: u32) -> ScenarioStep {
        Box::new(move |prev| {
            let mut prev = prev.clone();
            prev.raw_header.ssrc = new_ssrc.into();
            prev.seq_no = SeqNo::from(start_seq as u64);
            prev.rtp_ts = MediaTime::new(start_ts as u64, Frequency::NINETY_KHZ);
            prev.raw_header.marker = false;
            prev.is_keyframe_start = false;
            prev
        })
    }

    /// Generates a `Vec<RtpPacket>` from a series of steps.
    pub fn generate(initial: RtpPacket, steps: Vec<ScenarioStep>) -> Vec<RtpPacket> {
        let mut packets = Vec::with_capacity(steps.len());
        let mut current = initial;
        packets.push(current.clone());
        for step in steps {
            current = step(&current);
            packets.push(current.clone());
        }
        packets
    }
}

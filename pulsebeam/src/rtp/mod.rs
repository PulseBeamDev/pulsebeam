pub mod monitor;
pub mod sequencer;
pub mod switcher;
pub mod sync;

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
    pub playout_time: Option<Instant>,
    pub is_keyframe_start: bool,
    pub last_sender_info: Option<SenderInfo>,
    pub payload: Vec<u8>,
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
            playout_time: None,
            is_keyframe_start: false,
            last_sender_info: None,
            payload: vec![0u8; 1200], // 1.2KB payload for test realism
        }
    }
}

impl RtpPacket {
    pub fn from_str0m(rtp: str0m::rtp::RtpPacket, codec: Codec) -> Self {
        let is_keyframe_start = match codec {
            Codec::H264 => is_h264_keyframe_start(&rtp.payload),
            Codec::VP8 => is_vp8_keyframe_start(&rtp.payload),
            Codec::VP9 => is_vp9_keyframe_start(&rtp.payload),
        };

        Self {
            raw_header: rtp.header,
            seq_no: rtp.seq_no,
            rtp_ts: rtp.time,
            arrival_ts: rtp.timestamp.into(),
            playout_time: None,
            is_keyframe_start,
            last_sender_info: None,
            payload: rtp.payload,
        }
    }

    pub fn with_playout_time(mut self, playout_time: Instant) -> Self {
        self.playout_time = Some(playout_time);
        self
    }
}

/// Checks if an H.264 payload represents the start of a keyframe.
///
/// This function is hardened to prevent panics by using safe slicing and `get()`
/// methods, ensuring it can handle malformed or unexpected RTP payloads gracefully.
///
/// It supports:
/// - **Single NAL Units (SPS):** NALU type 7 is a Sequence Parameter Set, a key component of a keyframe.
/// - **Aggregation Packets (STAP-A, STAP-B):** Scans aggregated NALUs for an SPS.
/// - **Fragmentation Units (FU-A):** Checks if a starting fragment contains an SPS.
fn is_h264_keyframe_start(payload: &[u8]) -> bool {
    let Some(first_byte) = payload.first() else {
        return false;
    };

    let nalu_type = first_byte & 0x1F;

    match nalu_type {
        // NALU types 1-23 are single NAL units.
        1..=23 => {
            // Type 7 (Sequence Parameter Set) and Type 5 (IDR Slice) are indicators of a keyframe.
            // While SPS is the most reliable signal, IDR is also a definitive start.
            nalu_type == 7 || nalu_type == 5
        }

        // STAP-A (24) and STAP-B (25) are aggregation packets, containing multiple NALUs.
        24 | 25 => {
            // We need to parse the series of NALUs within this single RTP packet.
            // The format is: [STAP Header (1 byte)] [NALU 1 Size (2 bytes)] [NALU 1 Data] [NALU 2 Size] ...
            let mut remaining_payload = &payload[1..];

            // For STAP-B, there's an additional 16-bit DON (Decoding Order Number). Skip it.
            if nalu_type == 25 {
                remaining_payload = remaining_payload.get(2..).unwrap_or(&[]);
            }

            while !remaining_payload.is_empty() {
                // Each NALU is preceded by a 2-byte length field.
                // Safely check if we have enough bytes for the length field.
                let Some(nalu_size_bytes) = remaining_payload.get(0..2) else {
                    // Malformed packet: not enough bytes for a NALU size.
                    return false;
                };
                let nalu_size =
                    u16::from_be_bytes([nalu_size_bytes[0], nalu_size_bytes[1]]) as usize;

                // Move slice past the size field.
                remaining_payload = &remaining_payload[2..];

                // Safely get the NALU data itself.
                let Some(nalu_data) = remaining_payload.get(0..nalu_size) else {
                    // Malformed packet: declared NALU size is larger than the remaining payload.
                    return false;
                };

                // Check the NALU type of the inner packet.
                if let Some(nalu_header) = nalu_data.first() {
                    let inner_nalu_type = nalu_header & 0x1F;
                    if inner_nalu_type == 7 || inner_nalu_type == 5 {
                        return true;
                    }
                }

                // Move the slice to the start of the next NALU.
                remaining_payload = &remaining_payload[nalu_size..];
            }
            false
        }

        // FU-A (28) and FU-B (29) are for fragmentation.
        // A large NALU is split across multiple RTP packets.
        28 | 29 => {
            // We need at least two bytes: [FU Indicator] + [FU Header].
            let Some(fu_header) = payload.get(1) else {
                return false;
            };

            // The Start bit (S) must be 1 to indicate the first fragment of a NALU.
            let is_start_fragment = (fu_header & 0x80) != 0;

            if is_start_fragment {
                // The NALU type is contained in the FU header.
                let fragmented_nalu_type = fu_header & 0x1F;
                if fragmented_nalu_type == 7 || fragmented_nalu_type == 5 {
                    return true;
                }
            }
            false
        }

        // Other NALU types are not considered keyframe starts.
        _ => false,
    }
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
            // Assuming 30fps, so 90000 / 30 = 3000 per frame
            new_packet.rtp_ts = MediaTime::new(
                new_packet.rtp_ts.numer().wrapping_add(3000),
                new_packet.rtp_ts.frequency(),
            );
            new_packet.raw_header.timestamp = new_packet.rtp_ts.numer() as u32;
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
        for step in steps {
            current = step(&current);
            packets.push(current.clone());
        }
        packets
    }
}

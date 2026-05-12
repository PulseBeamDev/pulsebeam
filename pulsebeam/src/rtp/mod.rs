pub mod buffer;
pub mod monitor;
pub mod switcher;
pub mod sync;
pub mod timeline;

use str0m::media::{Frequency, MediaTime};
use str0m::rtp::rtcp::SenderInfo;
use str0m::rtp::{ExtensionValues, SeqNo, Ssrc};
use tokio::time::Instant;

use crate::entity::{ParticipantId, TrackId};

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

#[derive(Clone, Debug)]
pub struct AudioRtpPacket {
    pub participant_id: ParticipantId,
    pub track_id: TrackId,
    pub packet: RtpPacket,
}

/// Unified internal RTP packet representation used across the SFU.
/// This struct is designed for mutability and composition in middleware.
/// Only the fields actually consumed by the forwarding pipeline are kept here;
/// redundant header data (sequence_number, timestamp, csrc list, etc.) is dropped
/// at ingress so every ring-slot stays as small as possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpPacket {
    pub ssrc: Ssrc,
    pub marker: bool,
    pub ext_vals: ExtensionValues,
    pub header_len: usize,
    pub seq_no: SeqNo,
    pub rtp_ts: MediaTime,
    pub arrival_ts: Instant,

    /// Scheduled playout time for the packet, in the server's monotonic clock domain.
    /// Since all streams in this process share the same monotonic clock, this time can
    /// be compared directly between unrelated streams for scheduling or synchronization.
    pub playout_time: Instant,
    pub is_keyframe_start: bool,
    pub payload: Vec<u8>,
}

impl Default for RtpPacket {
    fn default() -> Self {
        Self {
            ssrc: 1234.into(),
            marker: false,
            ext_vals: ExtensionValues::default(),
            header_len: 12,
            seq_no: SeqNo::from(1u64),
            rtp_ts: MediaTime::new(0, VIDEO_FREQUENCY),
            arrival_ts: Instant::now(),
            playout_time: Instant::now(),
            is_keyframe_start: false,
            payload: vec![0u8; 1200], // 1.2KB payload for test realism
        }
    }
}

impl RtpPacket {
    /// Converts a str0m `RtpPacket` into the internal representation.
    ///
    /// Returns `(packet, sr)` where `sr` is the most recent Sender Report piggybacked
    /// on this packet by str0m (present on ~1/30 packets). The caller must thread `sr`
    /// to the `Synchronizer` so it never has to live in the ring struct.
    pub fn from_str0m(rtp: str0m::rtp::RtpPacket, codec: Codec) -> (Self, Option<SenderInfo>) {
        let is_keyframe_start = match codec {
            Codec::H264 => is_h264_keyframe_start(&rtp.payload),
            Codec::VP8 => is_vp8_keyframe_start(&rtp.payload),
            Codec::VP9 => is_vp9_keyframe_start(&rtp.payload),
            Codec::Opus => true, // audio frame has not dependencies,
        };

        let sr = rtp.last_sender_info;
        let pkt = Self {
            ssrc: rtp.header.ssrc,
            marker: rtp.header.marker,
            ext_vals: rtp.header.ext_vals,
            header_len: rtp.header.header_len,
            seq_no: rtp.seq_no,
            rtp_ts: rtp.time,
            arrival_ts: rtp.timestamp.into(),
            playout_time: rtp.timestamp.into(),
            is_keyframe_start,
            payload: rtp.payload,
        };
        (pkt, sr)
    }

    pub fn with_playout_time(mut self, playout_time: Instant) -> Self {
        self.playout_time = playout_time;
        self
    }
}

/// Checks if an H.264 RTP payload contains the start of a keyframe sequence.
///
/// Input: `payload` is the raw RTP payload (excluding the RTP header).
///
/// This function returns `true` if the payload contains any of the following NAL types,
/// ensuring we detect the start of a keyframe regardless of packet bundling:
/// - IDR (5): Instantaneous Decoding Refresh (Keyframe pixel data)
/// - SPS (7): Sequence Parameter Set (Critical configuration)
/// - PPS (8): Picture Parameter Set (Critical configuration)
///
/// AUD (9) and SEI (6) are intentionally excluded: both can appear before any access
/// unit (I-frame or P-frame).  Treating them as keyframe markers causes P-frames to be
/// forwarded without a preceding IDR during simulcast layer switches, corrupting the
/// downstream H.264 decoder.
pub fn is_h264_keyframe_start(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }

    // --- Constants (RFC 6184) ---
    const NAL_IDR: u8 = 5;
    const NAL_SPS: u8 = 7;
    const NAL_PPS: u8 = 8;

    const NAL_STAPA: u8 = 24;
    const NAL_FUA: u8 = 28;

    const NAL_TYPE_MASK: u8 = 0x1F;
    const FU_START_BITMASK: u8 = 0x80;

    // Only IDR/SPS/PPS are exclusive to keyframe access units.
    // AUD (9) and SEI (6) appear before *every* frame on many encoders.
    let is_anchor = |t: u8| -> bool { matches!(t, NAL_IDR | NAL_SPS | NAL_PPS) };

    let b0 = payload[0];
    let nal_type = b0 & NAL_TYPE_MASK;

    // 1. Single NAL Unit Case
    // If the packet contains just a raw SPS, PPS, or IDR.
    if is_anchor(nal_type) {
        return true;
    }

    // 2. STAP-A Case (Aggregation) - The Chrome WebRTC Fix
    // Format: [STAP Header (1)] [Size (2)] [NAL Header (1)] [Data...] [Size (2)] ...
    // We must iterate the entire packet. Chrome sometimes places filler NALs or
    // timing SEIs *before* the SPS/IDR in the same packet.
    if nal_type == NAL_STAPA {
        // Start after the STAP-A header (1 byte)
        let mut offset = 1;
        let len = payload.len();

        // Loop as long as we have at least 2 bytes for the NAL size
        while offset + 2 <= len {
            // Read 16-bit NAL unit size (Big Endian)
            let nalu_size = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
            offset += 2; // Advance past size field

            // Safety check: Ensure the NAL unit data exists in the buffer
            if offset + nalu_size > len {
                // Malformed packet: claimed size exceeds remaining buffer.
                // Stop processing to prevent panic.
                break;
            }

            // If the NAL has content, check its header
            if nalu_size > 0 {
                let nalu_header = payload[offset];
                let nalu_type = nalu_header & NAL_TYPE_MASK;

                if is_anchor(nalu_type) {
                    return true;
                }
            }

            // Advance to the next NAL unit
            offset += nalu_size;
        }

        return false;
    }

    // 3. FU-A Case (Fragmentation)
    // Format: [FU Indicator (1)] [FU Header (1)] [Data...]
    if nal_type == NAL_FUA {
        if payload.len() < 2 {
            return false;
        }

        let fu_header = payload[1];
        let is_start = (fu_header & FU_START_BITMASK) != 0;

        // We only consider it a "start" if this is the FIRST fragment
        if is_start {
            let original_nal_type = fu_header & NAL_TYPE_MASK;
            if is_anchor(original_nal_type) {
                return true;
            }
        }
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
            next.marker = true;
            next
        })
    }
    pub fn simulcast_switch(new_ssrc: u32, start_seq: u16, start_ts: u32) -> ScenarioStep {
        Box::new(move |prev| {
            let mut prev = prev.clone();
            prev.ssrc = new_ssrc.into();
            prev.seq_no = SeqNo::from(start_seq as u64);
            prev.rtp_ts = MediaTime::new(start_ts as u64, Frequency::NINETY_KHZ);
            prev.marker = false;
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

#[cfg(test)]
mod tests {
    use super::is_h264_keyframe_start;

    fn single_nal(nal_type: u8) -> Vec<u8> {
        // Forbidden bit = 0, NRI = 3, type = nal_type
        vec![0x60 | (nal_type & 0x1F), 0xDE, 0xAD]
    }

    fn stap_a(nal_types: &[u8]) -> Vec<u8> {
        // STAP-A header byte
        let mut payload = vec![0x60 | 24u8];
        for &nt in nal_types {
            let nal = [0x60 | (nt & 0x1F), 0xDE, 0xAD];
            payload.push(0);
            payload.push(nal.len() as u8);
            payload.extend_from_slice(&nal);
        }
        payload
    }

    fn fua(nal_type: u8, start: bool) -> Vec<u8> {
        // FU indicator: NRI=3, type=28
        let fu_indicator = 0x60 | 28u8;
        let s_bit = if start { 0x80 } else { 0x00 };
        let fu_header = s_bit | (nal_type & 0x1F);
        vec![fu_indicator, fu_header, 0xDE, 0xAD]
    }

    // --- True keyframe cases ---

    #[test]
    fn idr_is_keyframe() {
        assert!(is_h264_keyframe_start(&single_nal(5)));
    }

    #[test]
    fn sps_is_keyframe() {
        assert!(is_h264_keyframe_start(&single_nal(7)));
    }

    #[test]
    fn pps_is_keyframe() {
        assert!(is_h264_keyframe_start(&single_nal(8)));
    }

    #[test]
    fn stap_a_with_sps_pps_is_keyframe() {
        assert!(is_h264_keyframe_start(&stap_a(&[7, 8])));
    }

    #[test]
    fn stap_a_sei_before_sps_is_keyframe() {
        // Chrome sometimes bundles a timing SEI before SPS in a STAP-A for keyframes.
        // The loop must continue past the SEI and find SPS.
        assert!(is_h264_keyframe_start(&stap_a(&[6, 7])));
    }

    #[test]
    fn fua_idr_start_is_keyframe() {
        assert!(is_h264_keyframe_start(&fua(5, true)));
    }

    #[test]
    fn fua_idr_continuation_is_not_keyframe() {
        assert!(!is_h264_keyframe_start(&fua(5, false)));
    }

    // --- False positive guard: P-frame NAL types must NOT be keyframes ---

    #[test]
    fn sei_single_nal_is_not_keyframe() {
        // SEI (type 6) appears before P-frames on many encoders; must not be treated as keyframe.
        assert!(!is_h264_keyframe_start(&single_nal(6)));
    }

    #[test]
    fn aud_single_nal_is_not_keyframe() {
        // AUD (type 9) marks ANY access unit boundary, not only IDR frames.
        assert!(!is_h264_keyframe_start(&single_nal(9)));
    }

    #[test]
    fn p_frame_nal_is_not_keyframe() {
        assert!(!is_h264_keyframe_start(&single_nal(1)));
    }

    #[test]
    fn stap_a_with_only_sei_is_not_keyframe() {
        assert!(!is_h264_keyframe_start(&stap_a(&[6])));
    }

    #[test]
    fn stap_a_with_aud_and_p_frame_is_not_keyframe() {
        // AUD + P-frame in a STAP-A should NOT be a keyframe.
        assert!(!is_h264_keyframe_start(&stap_a(&[9, 1])));
    }

    #[test]
    fn fua_p_frame_start_is_not_keyframe() {
        // FU-A carrying a P-frame (NAL type 1) is not a keyframe even at start bit.
        assert!(!is_h264_keyframe_start(&fua(1, true)));
    }
}

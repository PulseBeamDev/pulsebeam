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
    pub is_keyframe: bool,
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
            is_keyframe: false,
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
            Codec::H264 => str0m::format::detect_h264_keyframe(&rtp.payload),
            Codec::VP8 => str0m::format::detect_vp8_keyframe(&rtp.payload),
            Codec::VP9 => str0m::format::detect_vp9_keyframe(&rtp.payload),
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
            is_keyframe: is_keyframe_start,
            payload: rtp.payload,
        };
        (pkt, sr)
    }

    pub fn with_playout_time(mut self, playout_time: Instant) -> Self {
        self.playout_time = playout_time;
        self
    }
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
            next.is_keyframe = true;
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
            prev.is_keyframe = false;
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

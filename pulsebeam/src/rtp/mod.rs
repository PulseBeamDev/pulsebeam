pub mod cache;
#[cfg(debug_assertions)]
pub mod egress_guard;
pub mod frame_selector;
pub mod h264;
pub mod monitor;
pub mod normalize;
pub mod switcher;
pub mod sync;
pub mod timeline;
pub mod types;

#[cfg(test)]
pub mod conformance;

use tokio::time::Instant;

pub use types::{
    EncodingId, Frequency, KeyframeRequest, KeyframeRequestKind, MediaKind, MediaSectionId,
    MediaTime, PacketDerivedFacts, PayloadType, ResolutionAndFramerate, SenderReport,
    SequenceNumber, SimulcastEncoding, SimulcastStreamAllocation, SpatialLayerAllocation, Ssrc,
    TemporalLayerAllocation, VideoLayersAllocation,
};

/// The standard 90kHz clock rate for video RTP, used for all internal timestamp math.
/// TODO: get these clocks from SDP instead.
pub const VIDEO_FREQUENCY: Frequency = Frequency::NINETY_KHZ;
pub const AUDIO_FREQUENCY: Frequency = Frequency::FORTY_EIGHT_KHZ;
pub const ABS_CAPTURE_TIME_EXTENSION_URI: &str =
    "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Opus,
}

impl Codec {
    pub fn from_name(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("h264") {
            Some(Self::H264)
        } else if name.eq_ignore_ascii_case("opus") {
            Some(Self::Opus)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodecPayloadTypes {
    h264: Option<PayloadType>,
    opus: Option<PayloadType>,
}

impl CodecPayloadTypes {
    pub fn insert(&mut self, codec: Codec, payload_type: PayloadType) {
        match codec {
            Codec::H264 => self.h264 = Some(payload_type),
            Codec::Opus => self.opus = Some(payload_type),
        }
    }

    pub fn get(self, codec: Codec) -> Option<PayloadType> {
        match codec {
            Codec::H264 => self.h264,
            Codec::Opus => self.opus,
        }
    }

    pub fn is_empty(self) -> bool {
        self.h264.is_none() && self.opus.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketForwardingState {
    pub marker: bool,
    pub derived: PacketDerivedFacts,
    pub size_bytes: usize,
    pub seq_no: SequenceNumber,
    pub rtp_ts: MediaTime,
    pub arrival_ts: Instant,
    pub packet_id: u64,

    pub playout_time: Instant,
    /// Whether this packet begins a frame that can be decoded without any
    /// preceding frame (an H.264 IDR, or any Opus packet).
    pub is_keyframe: bool,
    /// Whether this packet is the first of its frame.
    ///
    /// A Dependency Descriptor rides on every packet of a frame and carries the
    /// template structure on the first packet of a *keyframe*, so `is_keyframe`
    /// alone does not identify the packet a receiver can start assembling from.
    /// Replaying a keyframe from anywhere else hands over a frame with no
    /// beginning, which the receiver discards — along with the structure, which
    /// only keyframes carry, leaving the stream permanently unparseable.
    pub is_frame_start: bool,
    /// Which switch-relevant H.264 NAL units this payload carries. Always empty
    /// for audio.
    pub nal: h264::NalFlags,
    #[cfg(test)]
    pub codec: Codec,
    #[cfg(test)]
    pub ssrc: Ssrc,
    #[cfg(test)]
    pub rid: Option<EncodingId>,
    #[cfg(test)]
    pub test_audio_level: Option<i8>,
    #[cfg(test)]
    pub payload: Vec<u8>,
}

#[cfg(test)]
pub type RtpPacket = PacketForwardingState;

#[cfg(test)]
impl Default for PacketForwardingState {
    fn default() -> Self {
        Self {
            marker: false,
            derived: PacketDerivedFacts::default(),
            size_bytes: 1212,
            seq_no: SequenceNumber::from(1u64),
            rtp_ts: MediaTime::new(0, VIDEO_FREQUENCY),
            arrival_ts: Instant::now(),
            packet_id: 0,
            playout_time: Instant::now(),
            is_keyframe: false,
            is_frame_start: true,
            nal: h264::NalFlags::empty(),
            #[cfg(test)]
            codec: Codec::H264,
            #[cfg(test)]
            ssrc: 1234.into(),
            #[cfg(test)]
            rid: None,
            #[cfg(test)]
            test_audio_level: None,
            #[cfg(test)]
            payload: vec![0u8; 1200],
        }
    }
}

impl PacketForwardingState {
    pub fn validate_derived(&mut self) {
        debug_assert!(
            self.derived.dependency_descriptor.is_none()
                || self.derived.raw_dependency_descriptor.is_some()
        );
    }

    pub fn with_playout_time(mut self, playout_time: Instant) -> Self {
        self.playout_time = playout_time;
        self
    }
}

#[cfg(test)]
mod structural_tests {
    use super::*;
    #[test]
    fn codec_lookup_accepts_only_h264_and_opus() {
        assert_eq!(Codec::from_name("h264"), Some(Codec::H264));
        assert_eq!(Codec::from_name("opus"), Some(Codec::Opus));
        assert_eq!(Codec::from_name("vp8"), None);
        assert_eq!(Codec::from_name("vp9"), None);
        assert_eq!(Codec::from_name("av1"), None);
    }
}

#[cfg(test)]
pub mod test_utils {
    // A fixture that overflows should fail the test, not clamp into a pass.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use std::time::Duration;

    use super::*;

    impl PacketForwardingState {
        fn next_seq(&self) -> Self {
            let mut new_packet = self.clone();
            new_packet.seq_no = self.seq_no.wrapping_add(1);
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

            new_packet.playout_time += playout_time_delta;
            if let Some(at) = new_packet.arrival_ts.checked_add(playout_time_delta) {
                new_packet.arrival_ts = at;
            }

            new_packet
        }
    }

    pub type ScenarioStep = Box<dyn Fn(&PacketForwardingState) -> PacketForwardingState>;

    pub fn next_seq() -> ScenarioStep {
        Box::new(PacketForwardingState::next_seq)
    }
    pub fn next_frame() -> ScenarioStep {
        Box::new(PacketForwardingState::next_frame)
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
            prev.seq_no = SequenceNumber::from(start_seq as u64);
            prev.rtp_ts = MediaTime::new(start_ts as u64, Frequency::NINETY_KHZ);
            prev.marker = false;
            prev.is_keyframe = false;
            prev
        })
    }

    pub fn generate(
        initial: PacketForwardingState,
        steps: Vec<ScenarioStep>,
    ) -> Vec<PacketForwardingState> {
        let mut packets = Vec::with_capacity(steps.len());
        let mut current = initial;
        packets.push(current.clone());
        for step in steps {
            current = step(&current);
            packets.push(current.clone());
        }
        packets
    }

    /// Time between packets of the same frame as they arrive off the wire.
    const INTRA_FRAME_ARRIVAL: Duration = Duration::from_micros(200);

    /// Builds RTP packet streams whose payloads are real H.264 NAL structures,
    /// packetized the way libwebrtc packetizes them: a keyframe is a STAP-A of
    /// SPS+PPS followed by the IDR split into FU-A fragments, and a delta frame
    /// is one or more non-IDR slices.
    ///
    /// Tests that care about switch decodability must use this rather than
    /// hand-setting `is_keyframe`, because the whole class of bugs it guards
    /// against lives in the gap between "carries an IDR" and "is decodable".
    pub struct H264StreamBuilder {
        ssrc: Ssrc,
        seq: u64,
        rtp_ts: u64,
        clock: Instant,
        /// RTP ticks per frame, and the wall-clock interval that matches it
        /// exactly, so tests measure the forwarder rather than rounding.
        ts_step: u64,
        frame_interval: Duration,
        parameter_sets: ParameterSetStyle,
        sent_parameter_sets: bool,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum ParameterSetStyle {
        /// SPS and PPS ride in their own packet ahead of the IDR (Chrome).
        SeparatePacket,
        /// SPS, PPS and the first IDR fragment share one STAP-A.
        AggregatedWithIdr,
        /// The encoder sends parameter sets only once, at stream start.
        OnceAtStreamStart,
    }

    impl H264StreamBuilder {
        pub fn new(ssrc: u32, seq: u64, rtp_ts: u64, clock: Instant) -> Self {
            Self {
                ssrc: Ssrc::from(ssrc),
                seq,
                rtp_ts,
                clock,
                ts_step: VIDEO_FREQUENCY.get() as u64 / 30,
                frame_interval: Duration::from_nanos(1_000_000_000 / 30),
                parameter_sets: ParameterSetStyle::SeparatePacket,
                sent_parameter_sets: false,
            }
        }

        pub fn with_parameter_sets(mut self, style: ParameterSetStyle) -> Self {
            self.parameter_sets = style;
            self
        }

        pub fn with_fps(mut self, fps: u32) -> Self {
            self.ts_step = VIDEO_FREQUENCY.get() as u64 / fps as u64;
            self.frame_interval = Duration::from_nanos(1_000_000_000 / fps as u64);
            self
        }

        /// RTP ticks between consecutive frames of this stream.
        pub fn ts_step(&self) -> u64 {
            self.ts_step
        }

        pub fn next_seq(&self) -> u64 {
            self.seq
        }

        /// Advance as if `n` packets were sent but lost before reaching us.
        pub fn drop_packets(&mut self, n: u64) {
            self.seq += n;
        }

        fn packet(
            &mut self,
            payload: Vec<u8>,
            marker: bool,
            offset: usize,
        ) -> PacketForwardingState {
            let at = self.clock
                + INTRA_FRAME_ARRIVAL * u32::try_from(offset).expect("offset fits a u32");
            let nal = crate::rtp::h264::classify(&payload);
            let pkt = PacketForwardingState {
                ssrc: self.ssrc,
                marker,
                seq_no: SequenceNumber::from(self.seq),
                rtp_ts: MediaTime::new(self.rtp_ts, VIDEO_FREQUENCY),
                arrival_ts: at,
                playout_time: at,
                is_keyframe: nal.idr(),
                is_frame_start: true,
                nal,
                payload,
                ..Default::default()
            };
            self.seq += 1;
            pkt
        }

        fn end_frame(&mut self) {
            self.rtp_ts += self.ts_step;
            self.clock += self.frame_interval;
        }

        /// A decodable keyframe: parameter sets followed by `fragments` FU-A
        /// fragments of an IDR, terminated by the marker bit.
        pub fn keyframe(&mut self, fragments: usize) -> Vec<PacketForwardingState> {
            self.keyframe_with_slices(1, fragments)
        }

        /// A keyframe coded as `slices` independent IDR slices. Each slice
        /// produces its own FU-A start fragment, so multiple packets in the one
        /// frame report as carrying an IDR.
        pub fn keyframe_with_slices(
            &mut self,
            slices: usize,
            fragments_per_slice: usize,
        ) -> Vec<PacketForwardingState> {
            debug_assert!(slices >= 1 && fragments_per_slice >= 1);
            let mut out = Vec::new();
            let send_parameter_sets = self.parameter_sets != ParameterSetStyle::OnceAtStreamStart
                || !self.sent_parameter_sets;
            self.sent_parameter_sets = true;

            let aggregate = self.parameter_sets == ParameterSetStyle::AggregatedWithIdr;
            if send_parameter_sets && !aggregate {
                let payload = h264::test_utils::stap_a(&[(7, 24), (8, 6)]);
                let p = self.packet(payload, false, out.len());
                out.push(p);
            }

            for slice in 0..slices {
                for frag in 0..fragments_per_slice {
                    let start = frag == 0;
                    let end = frag == fragments_per_slice - 1;
                    let payload = if start && slice == 0 && aggregate {
                        h264::test_utils::stap_a(&[(7, 24), (8, 6), (5, 900)])
                    } else {
                        h264::test_utils::idr_fu_a(start, end, 1100)
                    };
                    let last = slice == slices - 1 && end;
                    let p = self.packet(payload, last, out.len());
                    out.push(p);
                }
            }

            self.end_frame();
            out
        }

        /// An ordinary inter-coded frame of `packets` non-IDR slice packets.
        pub fn delta_frame(&mut self, packets: usize) -> Vec<PacketForwardingState> {
            debug_assert!(packets >= 1);
            let mut out = Vec::new();
            for i in 0..packets {
                let payload = h264::test_utils::non_idr(1100);
                let p = self.packet(payload, i == packets - 1, i);
                out.push(p);
            }
            self.end_frame();
            out
        }

        /// `n` delta frames, flattened.
        pub fn delta_frames(
            &mut self,
            n: usize,
            packets_each: usize,
        ) -> Vec<PacketForwardingState> {
            (0..n)
                .flat_map(|_| self.delta_frame(packets_each))
                .collect()
        }
    }
}

//! Shard-local media packets. Owned shard handoffs copy payloads and extension snapshots.

pub mod cache;
mod extensions;
pub use extensions::RtpExtensions;
#[cfg(debug_assertions)]
pub mod egress_guard;
pub mod frame_selector;
pub mod h264;
pub mod monitor;
pub mod normalize;
pub mod switcher;
pub mod sync;
pub mod time;
pub mod timeline;

#[cfg(test)]
pub mod conformance;

use crate::media::{Rid, SeqNo, Ssrc};
#[allow(
    clippy::disallowed_types,
    reason = "immutable payload snapshots stay shard-local; transit copies storage"
)]
use std::sync::Arc;
pub use time::{Frequency, MediaTime};
use tokio::time::Instant;

use crate::entity::{ParticipantId, TrackId};

/// The standard 90kHz clock rate for video RTP, used for all internal timestamp math.
/// TODO: get these clocks from SDP instead.
pub const VIDEO_FREQUENCY: Frequency = Frequency::NINETY_KHZ;
pub const AUDIO_FREQUENCY: Frequency = Frequency::FORTY_EIGHT_KHZ;

#[derive(Debug, Clone, Copy)]
pub enum Codec {
    H264,
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
#[allow(
    clippy::disallowed_types,
    reason = "immutable payload snapshots stay shard-local; transit copies storage"
)]
pub struct RtpPacket {
    pub ssrc: Ssrc,
    pub marker: bool,
    pub ext_vals: RtpExtensions,
    pub seq_no: SeqNo,
    pub rtp_ts: MediaTime,
    pub arrival_ts: Instant,

    /// Scheduled playout time for the packet, in the server's monotonic clock domain.
    /// Since all streams in this process share the same monotonic clock, this time can
    /// be compared directly between unrelated streams for scheduling or synchronization.
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
    pub payload: Arc<[u8]>,
}

impl Default for RtpPacket {
    #[allow(
        clippy::disallowed_types,
        reason = "immutable payload snapshots stay shard-local; transit copies storage"
    )]
    fn default() -> Self {
        Self {
            ssrc: 1234.into(),
            marker: false,
            ext_vals: RtpExtensions::default(),
            seq_no: SeqNo::from(1u64),
            rtp_ts: MediaTime::new(0, VIDEO_FREQUENCY),
            arrival_ts: Instant::now(),
            playout_time: Instant::now(),
            is_keyframe: false,
            is_frame_start: true,
            nal: h264::NalFlags::empty(),
            payload: Arc::new([0u8; 1200]), // 1.2KB payload for test realism
        }
    }
}

impl RtpPacket {
    #[allow(
        clippy::disallowed_types,
        reason = "copy the RTC payload into a shard-local immutable snapshot"
    )]
    pub fn from_rtc(
        rtp: pulsebeam_rtc::RtpPacket,
        codec: Codec,
        frequency: Frequency,
        sequence: u64,
        timestamp: u64,
        rid: Option<Rid>,
    ) -> Option<Self> {
        let nal = match codec {
            Codec::H264 => h264::classify(&rtp.payload),
            Codec::Opus => h264::NalFlags::empty(),
        };
        let mut ext_vals = RtpExtensions {
            rid,
            ..Default::default()
        };
        for extension in rtp.extensions {
            match extension.uri.as_ref() {
                pulsebeam_core::dd::URI
                | "http://www.webrtc.org/experiments/rtp-hdrext/video-dependency-descriptor" => {
                    if extension.value.len() > pulsebeam_core::dd::MAX_DD_LEN {
                        return None;
                    }
                    ext_vals.set_raw_descriptor(pulsebeam_core::dd::RawDependencyDescriptor(
                        extension.value.iter().copied().collect(),
                    ));
                }
                pulsebeam_core::vla::URI => {
                    if let Ok(vla) =
                        pulsebeam_core::vla::VideoLayersAllocation::parse(&extension.value)
                    {
                        ext_vals.set_video_layers_allocation(vla);
                    }
                }
                "urn:ietf:params:rtp-hdrext:ssrc-audio-level" => {
                    ext_vals.audio_level = extension
                        .value
                        .first()
                        .and_then(|level| i8::try_from(level & 0x7f).ok())
                        .map(i8::saturating_neg);
                    ext_vals.opaque.push(extension);
                }
                "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay" => {}
                _ => ext_vals.opaque.push(extension),
            }
        }
        Some(Self {
            ssrc: rtp.header.ssrc.into(),
            marker: rtp.header.marker,
            ext_vals,
            seq_no: sequence.into(),
            rtp_ts: MediaTime::new(timestamp, frequency),
            arrival_ts: rtp.arrival.into(),
            playout_time: rtp.arrival.into(),
            is_keyframe: matches!(codec, Codec::Opus) || nal.idr(),
            is_frame_start: true,
            nal,
            payload: Arc::from(rtp.payload.as_ref()),
        })
    }

    #[allow(
        clippy::disallowed_types,
        reason = "fresh payload storage for an owned shard handoff"
    )]
    pub fn to_transit(&self) -> Self {
        Self {
            payload: Arc::from(self.payload.as_ref()),
            ext_vals: self.ext_vals.to_transit(),
            ..self.clone()
        }
    }

    pub fn rehome_extensions(&mut self) {
        self.ext_vals.rehome();
    }

    pub fn with_playout_time(mut self, playout_time: Instant) -> Self {
        self.playout_time = playout_time;
        self
    }
}

#[cfg(test)]
pub mod test_utils {
    // A fixture that overflows should fail the test, not clamp into a pass.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
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

            new_packet.playout_time += playout_time_delta;
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

        fn packet(&mut self, payload: Vec<u8>, marker: bool, offset: usize) -> RtpPacket {
            let at = self.clock
                + INTRA_FRAME_ARRIVAL * u32::try_from(offset).expect("offset fits a u32");
            let nal = crate::rtp::h264::classify(&payload);
            let pkt = RtpPacket {
                ssrc: self.ssrc,
                marker,
                seq_no: SeqNo::from(self.seq),
                rtp_ts: MediaTime::new(self.rtp_ts, VIDEO_FREQUENCY),
                arrival_ts: at,
                playout_time: at,
                is_keyframe: nal.idr(),
                is_frame_start: true,
                nal,
                payload: Arc::from(payload.as_slice()),
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
        pub fn keyframe(&mut self, fragments: usize) -> Vec<RtpPacket> {
            self.keyframe_with_slices(1, fragments)
        }

        /// A keyframe coded as `slices` independent IDR slices. Each slice
        /// produces its own FU-A start fragment, so multiple packets in the one
        /// frame report as carrying an IDR.
        pub fn keyframe_with_slices(
            &mut self,
            slices: usize,
            fragments_per_slice: usize,
        ) -> Vec<RtpPacket> {
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
        pub fn delta_frame(&mut self, packets: usize) -> Vec<RtpPacket> {
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
        pub fn delta_frames(&mut self, n: usize, packets_each: usize) -> Vec<RtpPacket> {
            (0..n)
                .flat_map(|_| self.delta_frame(packets_each))
                .collect()
        }
    }
}

//! Frame ↔ RTP pipeline that sits ABOVE the agent transport.
//!
//! The agent forwards [`RtpPacket`]s and never touches a frame. This module is
//! the composable layer that turns encoder frames into RTP on the way out and
//! reassembles RTP back into frames on the way in — codec-agnostically, driven by
//! the RTP headers and the Dependency Descriptor, never by parsing the payload.
//! That is what lets end-to-end-encrypted (opaque) media flow through unchanged.
//!
//! Frame boundaries ride in the DD's per-packet `start_of_frame`/`end_of_frame`
//! flags (not inferred from timestamps), so reassembly is correct under the
//! reordering that retransmissions introduce. Jitter buffering and E2EE are
//! further stages that compose around [`FrameSender`]/[`FrameReceiver`].

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use pulsebeam_core::dd::temporal::TemporalDdSource;
use pulsebeam_core::dd::{DependencyDescriptorReader, RawDependencyDescriptor, read_mandatory};
use pulsebeam_core::framing::{FrameDepacketizer, FramePacketizer};
use str0m::media::{MediaTime, Mid, Rid};
use str0m::rtp::vla::{
    ResolutionAndFramerate, SimulcastStreamAllocation, SpatialLayerAllocation,
    TemporalLayerAllocation, VideoLayersAllocation,
};
use str0m::rtp::{AbsCaptureTime, ExtensionValues, SeqNo};
use tokio::time::Instant;

use crate::{MediaFrame, RtpPacket};

const START_OF_FRAME_BIT: u8 = 0b1000_0000;
const END_OF_FRAME_BIT: u8 = 0b0100_0000;

/// Packetizes one outgoing stream's frames into RTP.
///
/// Owns the stream's RTP sequence numbering and its Dependency Descriptor source,
/// so that even a non-scalable (or opaque/encrypted) encoder produces
/// DD-annotated packets and the SFU can route/shed on the descriptor rather than
/// on the payload.
pub struct FrameSender {
    mid: Mid,
    rid: Option<Rid>,
    encoding_count: usize,
    packetizer: FramePacketizer,
    next_seq: u64,
    dd: TemporalDdSource,
}

impl FrameSender {
    /// `temporal_layers` is the stream's L1T{n} depth (1 = non-scalable).
    pub fn new(mid: Mid, rid: Option<Rid>, encoding_count: usize, temporal_layers: u8) -> Self {
        Self {
            mid,
            rid,
            encoding_count,
            packetizer: FramePacketizer::default(),
            next_seq: 0,
            dd: TemporalDdSource::new(temporal_layers.max(1)),
        }
    }

    /// Split `frame` into RTP packets. A single Dependency Descriptor is generated
    /// for the frame, then stamped on every packet with that packet's own
    /// start/end-of-frame flags so the receiver can reassemble under reordering.
    pub fn packetize(&mut self, frame: &MediaFrame) -> Vec<RtpPacket> {
        let dd_bytes = self.dd.next(frame.is_keyframe);

        let vla = frame.target_bitrate_bps.and_then(|bps| {
            vla_for(
                self.encoding_count,
                bps,
                frame.resolution,
                frame.temporal_layers,
            )
        });

        let chunks: Vec<_> = self.packetizer.packetize(&frame.data).collect();
        let mut packets = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let mut ext_vals = ExtensionValues::default();
            if let Some(raw) = &dd_bytes {
                let mut bytes = raw.0.clone();
                if let Some(first) = bytes.first_mut() {
                    let mut flags = *first & !(START_OF_FRAME_BIT | END_OF_FRAME_BIT);
                    if chunk.start_of_frame {
                        flags |= START_OF_FRAME_BIT;
                    }
                    if chunk.end_of_frame {
                        flags |= END_OF_FRAME_BIT;
                    }
                    *first = flags;
                }
                ext_vals.user_values.set(RawDependencyDescriptor(bytes));
            }
            if let Some(vla) = vla.clone() {
                ext_vals.user_values.set(vla);
            }
            if let Some(capture_time) = frame.abs_capture_time {
                ext_vals.abs_capture_time = Some(AbsCaptureTime {
                    capture_time,
                    clock_offset: None,
                });
            }

            let seq = SeqNo::from(self.next_seq);
            self.next_seq = self.next_seq.wrapping_add(1);
            packets.push(RtpPacket {
                mid: self.mid,
                rid: self.rid,
                seq,
                ts: frame.ts,
                marker: chunk.end_of_frame,
                payload: Arc::from(chunk.data),
                ext_vals,
                arrival: frame.capture_time,
            });
        }
        packets
    }
}

struct PendingFrame {
    is_keyframe: bool,
    ts: MediaTime,
    capture_time: Instant,
    abs_capture_time: Option<SystemTime>,
}

/// Reassembles one incoming stream's RTP back into frames, using the DD's
/// per-packet start/end-of-frame flags and sequence contiguity. A jitter buffer
/// stage (future) would sit in front to smooth playout; the bounded reorder
/// window here already absorbs the reordering that retransmissions cause.
pub struct FrameReceiver {
    depacketizer: FrameDepacketizer,
    dd_reader: DependencyDescriptorReader,
    /// Metadata captured at each frame's start packet, keyed by its sequence.
    pending: BTreeMap<u64, PendingFrame>,
    /// Last emitted frame's final sequence, to judge inter-frame continuity.
    prev_last_seq: Option<u64>,
}

impl Default for FrameReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReceiver {
    pub fn new() -> Self {
        Self {
            depacketizer: FrameDepacketizer::default(),
            dd_reader: DependencyDescriptorReader::new(),
            pending: BTreeMap::new(),
            prev_last_seq: None,
        }
    }

    /// Feed one RTP packet; returns a reassembled [`MediaFrame`] when a frame
    /// becomes contiguously complete.
    pub fn push(&mut self, rtp: &RtpPacket) -> Option<MediaFrame> {
        let seq = *rtp.seq;

        // Forward-only: a retransmission that lands after we have already
        // delivered its frame's position must be dropped, never re-emitted late —
        // otherwise a very-late RTX reorders the output and regresses the media
        // clock. This is the role a jitter buffer's playout deadline plays.
        if let Some(prev) = self.prev_last_seq
            && seq <= prev
        {
            return None;
        }

        let raw = rtp.ext_vals.user_values.get::<RawDependencyDescriptor>();

        // Boundaries come from the DD when present. A packet without a DD (e.g.
        // single-packet audio) is treated as a self-contained frame.
        let (start_of_frame, end_of_frame) = match raw {
            Some(r) => read_mandatory(&r.0)
                .map(|m| (m.start_of_frame, m.end_of_frame))
                .unwrap_or((true, true)),
            None => (true, true),
        };

        if start_of_frame {
            let is_keyframe = raw
                .and_then(|r| self.dd_reader.read(&r.0).ok())
                .map(|dd| dd.attached_structure.is_some())
                .unwrap_or(false);
            self.pending.insert(
                seq,
                PendingFrame {
                    is_keyframe,
                    ts: rtp.ts,
                    capture_time: rtp.arrival,
                    abs_capture_time: rtp.ext_vals.abs_capture_time.map(|a| a.capture_time),
                },
            );
            // Bound the metadata map against frames whose start we saw but whose
            // completion never came (permanent loss).
            while self.pending.len() > 256 {
                let oldest = *self.pending.keys().next().unwrap();
                self.pending.remove(&oldest);
            }
        }

        let frame = self
            .depacketizer
            .push(seq, &rtp.payload, start_of_frame, end_of_frame)?;

        let meta = self.pending.remove(&frame.first_seq)?;
        let contiguous = self.prev_last_seq.is_none_or(|p| frame.first_seq == p + 1);
        self.prev_last_seq = Some(frame.last_seq);

        Some(MediaFrame {
            ts: meta.ts,
            data: Arc::from(frame.data.as_slice()),
            capture_time: meta.capture_time,
            abs_capture_time: meta.abs_capture_time,
            contiguous,
            is_keyframe: meta.is_keyframe,
            target_bitrate_bps: None,
            resolution: None,
            dependency_descriptor: None,
            temporal_layers: None,
        })
    }
}

/// A single-encoding stream's Video Layers Allocation, declaring the encoder's
/// target so the SFU allocates against it rather than measured bytes. Emitted
/// only for non-simulcast tracks (simulcast layers are inferred from the rids).
fn vla_for(
    encoding_count: usize,
    target_bps: u64,
    resolution: Option<(u16, u16, u8)>,
    temporal_layers: Option<u8>,
) -> Option<VideoLayersAllocation> {
    if encoding_count != 1 {
        return None;
    }
    Some(VideoLayersAllocation {
        current_simulcast_stream_index: 0,
        simulcast_streams: vec![SimulcastStreamAllocation {
            spatial_layers: vec![SpatialLayerAllocation {
                temporal_layers: temporal_cumulative_kbps(target_bps, temporal_layers),
                resolution_and_framerate: resolution.map(|(width, height, framerate)| {
                    ResolutionAndFramerate {
                        width,
                        height,
                        framerate,
                    }
                }),
            }],
        }],
    })
}

fn temporal_cumulative_kbps(
    target_bps: u64,
    temporal_layers: Option<u8>,
) -> Vec<TemporalLayerAllocation> {
    let full_kbps = target_bps / 1000;
    let layers = temporal_layers.unwrap_or(1).max(1);
    if layers <= 1 {
        return vec![TemporalLayerAllocation {
            cumulative_kbps: full_kbps,
        }];
    }
    (0..layers)
        .map(|k| {
            let frac = 0.5 + 0.5 * (k as f64) / ((layers - 1) as f64);
            TemporalLayerAllocation {
                cumulative_kbps: ((full_kbps as f64) * frac).round() as u64,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(data: Vec<u8>, is_keyframe: bool) -> MediaFrame {
        MediaFrame {
            ts: MediaTime::from_90khz(9000),
            data: Arc::from(data.as_slice()),
            capture_time: Instant::now(),
            abs_capture_time: None,
            contiguous: true,
            is_keyframe,
            target_bitrate_bps: None,
            resolution: None,
            dependency_descriptor: None,
            temporal_layers: None,
        }
    }

    #[test]
    fn a_multi_packet_frame_round_trips_through_send_and_receive() {
        let mid = Mid::from("v0");
        let mut sender = FrameSender::new(mid, None, 1, 1);
        let mut receiver = FrameReceiver::new();

        let payload: Vec<u8> = (0..3000u32).map(|i| (i * 5 + 1) as u8).collect();
        let packets = sender.packetize(&frame(payload.clone(), true));
        assert!(packets.len() > 1, "should split across packets");
        assert!(packets.last().unwrap().marker, "last packet ends the frame");

        let mut out = None;
        for pkt in &packets {
            out = receiver.push(pkt);
        }
        let got = out.expect("frame reassembled");
        assert_eq!(
            &*got.data,
            &payload[..],
            "opaque payload round-trips byte-exact"
        );
        assert!(got.is_keyframe, "keyframe recovered from the DD");
        assert!(got.contiguous);
    }

    #[test]
    fn a_multi_packet_frame_reassembles_despite_reordered_packets() {
        // The reordering an RTX/NACK recovery introduces must not tear the frame:
        // boundaries come from the DD, not the arrival order.
        let mid = Mid::from("v0");
        let mut sender = FrameSender::new(mid, None, 1, 1);
        let mut receiver = FrameReceiver::new();

        let payload: Vec<u8> = (0..3000u32).map(|i| (i * 5 + 1) as u8).collect();
        let mut packets = sender.packetize(&frame(payload.clone(), true));
        assert!(packets.len() >= 3);
        packets.reverse();

        let mut out = None;
        for pkt in &packets {
            if let Some(f) = receiver.push(pkt) {
                out = Some(f);
            }
        }
        assert_eq!(&*out.expect("reassembled").data, &payload[..]);
    }

    #[test]
    fn distinct_frames_are_flagged_by_keyframe_and_stay_contiguous() {
        let mid = Mid::from("v0");
        let mut sender = FrameSender::new(mid, None, 1, 3);
        let mut receiver = FrameReceiver::new();

        let mut kf = frame(vec![1u8; 400], true);
        kf.ts = MediaTime::from_90khz(1000);
        let mut delta = frame(vec![2u8; 400], false);
        delta.ts = MediaTime::from_90khz(4000);

        let mut frames = Vec::new();
        for f in [&kf, &delta] {
            for pkt in sender.packetize(f) {
                if let Some(out) = receiver.push(&pkt) {
                    frames.push(out);
                }
            }
        }
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_keyframe && !frames[1].is_keyframe);
        assert!(frames.iter().all(|f| f.contiguous));
    }

    #[test]
    fn vla_is_declared_only_for_single_encoding_tracks() {
        assert!(vla_for(2, 500_000, None, None).is_none());

        let allocation = vla_for(1, 500_000, Some((1280, 720, 30)), None).unwrap();
        assert_eq!(allocation.current_simulcast_stream_index, 0);
        assert_eq!(allocation.simulcast_streams.len(), 1);
        let temporal = &allocation.simulcast_streams[0].spatial_layers[0].temporal_layers;
        assert_eq!(temporal.len(), 1);
        assert_eq!(temporal[0].cumulative_kbps, 500);
    }

    #[test]
    fn vla_declares_a_nested_temporal_ladder_for_scalable_streams() {
        let allocation = vla_for(1, 600_000, Some((1280, 720, 30)), Some(3)).unwrap();
        let temporal = &allocation.simulcast_streams[0].spatial_layers[0].temporal_layers;
        assert_eq!(temporal.len(), 3);
        assert_eq!(temporal[0].cumulative_kbps, 300);
        assert_eq!(temporal[2].cumulative_kbps, 600);
        assert!(
            temporal
                .windows(2)
                .all(|w| w[1].cumulative_kbps > w[0].cumulative_kbps)
        );
    }
}

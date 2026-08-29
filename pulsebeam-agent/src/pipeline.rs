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
use std::time::{Duration, SystemTime};

use pulsebeam_core::dd::temporal::TemporalDdSource;
use pulsebeam_core::dd::{
    DdWriteError, DependencyDescriptorReader, RawDependencyDescriptor, read_mandatory,
};
use pulsebeam_core::{
    framing::{FrameDepacketizer, FramePacketizer},
    h264::Packetizer as H264Packetizer,
};
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
    h264_packetizer: Option<H264Packetizer>,
    next_seq: u64,
    dd: Option<TemporalDdSource>,
}

impl FrameSender {
    /// `temporal_layers` is the stream's L1T{n} depth (1 = non-scalable).
    pub fn new(mid: Mid, rid: Option<Rid>, encoding_count: usize, temporal_layers: u8) -> Self {
        Self {
            mid,
            rid,
            encoding_count,
            packetizer: FramePacketizer::default(),
            h264_packetizer: None,
            next_seq: 0,
            dd: Some(TemporalDdSource::new(temporal_layers.max(1))),
        }
    }

    pub fn h264(mid: Mid, rid: Option<Rid>, encoding_count: usize, temporal_layers: u8) -> Self {
        let mut sender = Self::new(mid, rid, encoding_count, temporal_layers);
        sender.h264_packetizer = Some(H264Packetizer::new(
            pulsebeam_core::framing::DEFAULT_MTU_PAYLOAD,
        ));
        sender
    }

    pub fn without_dependency_descriptor(
        mid: Mid,
        rid: Option<Rid>,
        encoding_count: usize,
    ) -> Self {
        let mut sender = Self::new(mid, rid, encoding_count, 1);
        sender.dd = None;
        sender
    }

    pub fn packetize(&mut self, frame: &MediaFrame) -> Vec<RtpPacket> {
        let vla = frame.target_bitrate_bps.and_then(|bps| {
            vla_for(
                self.encoding_count,
                bps,
                frame.resolution,
                frame.temporal_layers,
            )
        });

        let chunks: Vec<_> = if let Some(packetizer) = &self.h264_packetizer {
            packetizer
                .packetize(&frame.data)
                .into_iter()
                .map(|chunk| {
                    (
                        Arc::from(chunk.payload),
                        chunk.start_of_frame,
                        chunk.end_of_frame,
                    )
                })
                .collect()
        } else {
            self.packetizer
                .packetize(&frame.data)
                .map(|chunk| {
                    (
                        Arc::from(chunk.data),
                        chunk.start_of_frame,
                        chunk.end_of_frame,
                    )
                })
                .collect()
        };
        if chunks.is_empty() {
            return Vec::new();
        }
        let dd_bytes = match self.dd.as_mut() {
            Some(source) => match source.next_frame(frame.is_keyframe, chunks.len()) {
                Ok(raws) => {
                    debug_assert_eq!(raws.len(), chunks.len());
                    Some(raws)
                }
                Err(DdWriteError::NoStructure) => return Vec::new(),
                Err(error) => {
                    debug_assert!(false, "dependency descriptor encoding failed: {error}");
                    return Vec::new();
                }
            },
            None => None,
        };
        let mut packets = Vec::with_capacity(chunks.len());
        for (index, (payload, start_of_frame, end_of_frame)) in chunks.into_iter().enumerate() {
            // Audio level is not optional decoration: the SFU's speaker selector ranks by it and
            // drops any audio packet that arrives without one.
            let mut ext_vals = ExtensionValues {
                audio_level: frame.audio_level,
                voice_activity: frame.voice_activity,
                ..ExtensionValues::default()
            };
            if let Some(raw) = dd_bytes.as_ref().and_then(|raws| raws.get(index)).cloned() {
                debug_assert_eq!(
                    raw.0.first().map(|first| first & START_OF_FRAME_BIT != 0),
                    Some(start_of_frame)
                );
                debug_assert_eq!(
                    raw.0.first().map(|first| first & END_OF_FRAME_BIT != 0),
                    Some(end_of_frame)
                );
                ext_vals.user_values.set(raw);
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
                marker: end_of_frame,
                payload_type: None,
                ssrc: None,
                payload,
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

/// Default upper bound on how long the jitter buffer holds a gap open waiting
/// for reordered/retransmitted packets. Generous, favouring loss recovery over
/// latency — a latency-sensitive real-time consumer can lower it (accepting more
/// residual loss) via [`FrameReceiver::with_max_wait`].
pub const DEFAULT_JITTER_MAX_WAIT: Duration = Duration::from_secs(5);

/// How long the buffer absorbs reordering before committing to a first sequence number.
///
/// Separate from [`DEFAULT_JITTER_MAX_WAIT`] because the two answer different questions. A gap
/// budget asks how long to hold a hole open hoping a retransmission fills it, and being generous
/// there costs nothing until a packet is actually lost. The opening question is only how far a
/// packet can arrive out of order, which is tens of milliseconds on any real path.
///
/// Sharing one constant made every stream pay the gap budget before its first frame: measured at a
/// 5.1s median time-to-first-frame across the simulation suite, with variance far too tight to be
/// anything but a fixed wait. A viewer joins a call and the tile is blank for five seconds.
pub const DEFAULT_INITIAL_COMMIT_WAIT: Duration = Duration::from_millis(40);

/// A time-bounded reorder / jitter buffer.
///
/// Holds incoming RTP briefly so out-of-order and retransmitted (NACK/RTX)
/// packets can take their place before a gap is declared lost, then releases
/// packets strictly in sequence order. `max_wait` bounds how long any single gap
/// is held open: larger recovers more under bursty loss, at the cost of
/// buffering latency. Opening the stream is bounded separately by
/// `initial_wait` — see [`DEFAULT_INITIAL_COMMIT_WAIT`].
pub struct JitterBuffer {
    buf: BTreeMap<u64, RtpPacket>,
    next: Option<u64>,
    latest_arrival: Option<Instant>,
    max_wait: Duration,
    initial_wait: Duration,
    /// Whether anything has reached the screen yet. See the gap budget in [`Self::pop`].
    delivered_frame: bool,
}

impl JitterBuffer {
    pub fn new(max_wait: Duration) -> Self {
        Self {
            buf: BTreeMap::new(),
            next: None,
            latest_arrival: None,
            max_wait,
            // Never longer than the gap budget: a caller that asked for a tight budget wants low
            // latency, and handing it a slower start than it asked for would be perverse.
            initial_wait: DEFAULT_INITIAL_COMMIT_WAIT.min(max_wait),
            delivered_frame: false,
        }
    }

    pub fn push(&mut self, rtp: RtpPacket) {
        let arrival = rtp.arrival;
        self.latest_arrival = Some(self.latest_arrival.map_or(arrival, |l| l.max(arrival)));
        self.buf.insert(*rtp.seq, rtp);
    }

    /// Release the next in-order packet that is ready, or `None` while still
    /// waiting for it to arrive (up to `max_wait` from the head packet's arrival).
    pub fn pop(&mut self) -> Option<RtpPacket> {
        let now = self.latest_arrival?;
        let next = match self.next {
            Some(n) => n,
            None => {
                // Absorb initial reordering before committing to a first sequence. Bounded by
                // `initial_wait`, not the gap budget: this is "how late can the real first packet
                // be", not "how long to hope a lost packet is retransmitted".
                let (&min_seq, min_pkt) = self.buf.iter().next()?;
                if now.saturating_duration_since(min_pkt.arrival) < self.initial_wait {
                    return None;
                }
                min_seq
            }
        };
        if let Some(pkt) = self.buf.remove(&next) {
            self.next = Some(next.wrapping_add(1));
            return Some(pkt);
        }
        // Gap at `next`: wait for it to fill, then skip it (lost).
        //
        // The budget depends on whether anything is on screen yet. `max_wait` protects
        // *continuity* - it is worth stalling a running picture to recover a packet, because the
        // alternative is a visible tear. Before the first frame there is no continuity to protect,
        // and a viewer looking at a blank tile would far rather see the next decodable frame than
        // wait seconds for a packet that may never come. Measured: two packets went missing right
        // after the first one the buffer committed to, and the viewer sat blank for 5s of loss
        // budget before showing anything - on a link configured with no loss at all.
        let budget = if self.delivered_frame {
            self.max_wait
        } else {
            self.initial_wait
        };
        let (_, head_pkt) = self.buf.first_key_value()?;
        if now.saturating_duration_since(head_pkt.arrival) < budget {
            return None;
        }
        let (head_seq, pkt) = self.buf.pop_first()?;
        self.next = Some(head_seq.wrapping_add(1));
        Some(pkt)
    }

    /// Note that a frame has reached the application, so gaps are now worth waiting out.
    pub fn note_frame_delivered(&mut self) {
        self.delivered_frame = true;
    }

    /// Release everything still buffered, in sequence order (end of stream).
    pub fn drain(&mut self) -> impl Iterator<Item = RtpPacket> {
        std::mem::take(&mut self.buf).into_values()
    }
}

/// Reassembles one incoming stream's RTP into frames.
///
/// A [`JitterBuffer`] fronts the reassembler: RTP is reordered and gap-recovered
/// (NACK/RTX) with bounded latency first, then frames are cut from the ordered
/// stream using the DD's per-packet start/end-of-frame flags.
pub struct FrameReceiver {
    jitter: JitterBuffer,
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
        Self::with_max_wait(DEFAULT_JITTER_MAX_WAIT)
    }

    /// Build a receiver whose jitter buffer holds a gap open for at most
    /// `max_wait` before declaring it lost.
    ///
    /// This is the receiver's only loss-recovery lever: NACK generation and RTX
    /// retransmission happen in the transport (str0m) *below* this layer, so
    /// `max_wait` must simply outlast the recovery round-trip. Set it below the
    /// worst-case NACK+RTX budget and a retransmit will land *after* the gap has
    /// already been given up on — wasting the recovery and cutting the frame
    /// anyway. Two things make that budget larger than one RTT:
    /// - bursty loss needs several NACK rounds (a retransmit can itself be lost
    ///   and be re-NACKed), each ~1 RTT;
    /// - high-latency links (cellular) stretch every round.
    ///
    /// So trade latency for recovery deliberately: lower `max_wait` only as far
    /// as your links' worst-case NACK+RTX round-trip. The default
    /// ([`DEFAULT_JITTER_MAX_WAIT`], 5s) sits comfortably above that.
    pub fn with_max_wait(max_wait: Duration) -> Self {
        Self {
            jitter: JitterBuffer::new(max_wait),
            depacketizer: FrameDepacketizer::default(),
            dd_reader: DependencyDescriptorReader::new(),
            pending: BTreeMap::new(),
            prev_last_seq: None,
        }
    }

    /// Feed one RTP packet; returns any frames that became ready (0+). Frames may
    /// be released now or on a later push once the jitter buffer's delay elapses.
    pub fn push(&mut self, rtp: RtpPacket) -> Vec<MediaFrame> {
        self.jitter.push(rtp);
        let mut frames = Vec::new();
        while let Some(ordered) = self.jitter.pop() {
            if let Some(frame) = self.reassemble(ordered) {
                self.jitter.note_frame_delivered();
                frames.push(frame);
            }
        }
        frames
    }

    /// Release everything buffered (end of stream): drains the jitter buffer with
    /// no further wait and reassembles whatever completes.
    pub fn flush(&mut self) -> Vec<MediaFrame> {
        let drained: Vec<RtpPacket> = self.jitter.drain().collect();
        let mut frames = Vec::new();
        for ordered in drained {
            if let Some(frame) = self.reassemble(ordered) {
                frames.push(frame);
            }
        }
        frames
    }

    fn reassemble(&mut self, rtp: RtpPacket) -> Option<MediaFrame> {
        let seq = *rtp.seq;

        // Forward-only: the jitter buffer already orders output, but a stray
        // late packet must never re-emit an already-delivered frame position.
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
            while self.pending.len() > 256 {
                self.pending.pop_first();
            }
        }

        let frame = self
            .depacketizer
            .push(seq, &rtp.payload, start_of_frame, end_of_frame)?;

        let meta = self.pending.remove(&frame.first_seq)?;
        let contiguous = self
            .prev_last_seq
            .is_none_or(|p| frame.first_seq == p.saturating_add(1));
        self.prev_last_seq = Some(frame.last_seq);

        Some(MediaFrame {
            audio_level: None,
            voice_activity: None,
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
            let frac = 0.5 + 0.5 * (k as f64) / (layers.saturating_sub(1) as f64);
            TemporalLayerAllocation {
                cumulative_kbps: crate::media::saturating_u64_from_f64((full_kbps as f64) * frac),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    use super::*;

    fn frame(data: Vec<u8>, is_keyframe: bool) -> MediaFrame {
        MediaFrame {
            audio_level: None,
            voice_activity: None,
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

        let payload: Vec<u8> = (0..3000u32)
            .map(|i| u8::try_from((i * 5 + 1) % 256).expect("masked to a byte"))
            .collect();
        let packets = sender.packetize(&frame(payload.clone(), true));
        assert!(packets.len() > 1, "should split across packets");
        assert!(packets.last().unwrap().marker, "last packet ends the frame");

        let mut frames = Vec::new();
        for pkt in packets {
            frames.extend(receiver.push(pkt));
        }
        frames.extend(receiver.flush());
        let got = frames.into_iter().next().expect("frame reassembled");
        assert_eq!(
            &*got.data,
            &payload[..],
            "opaque payload round-trips byte-exact"
        );
        assert!(got.is_keyframe, "keyframe recovered from the DD");
        assert!(got.contiguous);
    }

    #[test]
    fn dependency_descriptor_sender_waits_for_its_first_keyframe() {
        let mid = Mid::from("v0");
        let mut sender = FrameSender::new(mid, None, 1, 1);

        assert!(
            sender.packetize(&frame(vec![1; 100], false)).is_empty(),
            "a delta frame cannot establish a DD template"
        );
        let keyframe = sender.packetize(&frame(vec![2; 100], true));
        assert!(
            !keyframe.is_empty(),
            "a keyframe establishes the DD template"
        );
        assert!(keyframe.iter().all(|packet| {
            packet
                .ext_vals
                .user_values
                .get::<RawDependencyDescriptor>()
                .is_some()
        }));
    }

    #[test]
    fn audio_sender_does_not_require_or_emit_a_dependency_descriptor() {
        let mid = Mid::from("a0");
        let mut sender = FrameSender::without_dependency_descriptor(mid, None, 1);

        let packets = sender.packetize(&frame(vec![1; 100], false));
        assert_eq!(packets.len(), 1);
        assert!(
            packets[0]
                .ext_vals
                .user_values
                .get::<RawDependencyDescriptor>()
                .is_none()
        );
    }

    #[test]
    fn a_fragmented_keyframe_carries_its_template_only_on_the_first_packet() {
        let mid = Mid::from("v0");
        let mut sender = FrameSender::new(mid, None, 1, 1);
        let payload: Vec<u8> = (0..3000u32)
            .map(|i| u8::try_from((i * 7 + 3) % 256).expect("masked to a byte"))
            .collect();
        let packets = sender.packetize(&frame(payload, true));
        assert!(packets.len() > 1, "fixture must fragment the keyframe");

        let mut reader = DependencyDescriptorReader::new();
        for (index, packet) in packets.iter().enumerate() {
            let raw = packet
                .ext_vals
                .user_values
                .get::<RawDependencyDescriptor>()
                .expect("every video packet carries a dependency descriptor");
            let dd = reader
                .read(&raw.0)
                .expect("descriptor parses in packet order");
            assert_eq!(dd.start_of_frame, index == 0);
            assert_eq!(dd.end_of_frame, index + 1 == packets.len());
            assert_eq!(dd.attached_structure.is_some(), index == 0);
        }
    }

    #[test]
    fn a_multi_packet_frame_reassembles_despite_reordered_packets() {
        // The reordering an RTX/NACK recovery introduces must not tear the frame:
        // boundaries come from the DD, not the arrival order.
        let mid = Mid::from("v0");
        let mut sender = FrameSender::new(mid, None, 1, 1);
        let mut receiver = FrameReceiver::new();

        let payload: Vec<u8> = (0..3000u32)
            .map(|i| u8::try_from((i * 5 + 1) % 256).expect("masked to a byte"))
            .collect();
        let mut packets = sender.packetize(&frame(payload.clone(), true));
        assert!(packets.len() >= 3);
        packets.reverse();

        let mut frames = Vec::new();
        for pkt in packets {
            frames.extend(receiver.push(pkt));
        }
        frames.extend(receiver.flush());
        assert_eq!(
            &*frames.into_iter().next().expect("reassembled").data,
            &payload[..]
        );
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
                frames.extend(receiver.push(pkt));
            }
        }
        frames.extend(receiver.flush());
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_keyframe && !frames[1].is_keyframe);
        assert!(frames.iter().all(|f| f.contiguous));
    }

    #[test]
    fn jitter_buffer_orders_reordered_packets_and_skips_a_permanent_gap() {
        use tokio::time::Instant;

        let base = Instant::now();
        let pkt = |seq: u64, at_ms: u64| RtpPacket {
            ssrc: None,
            mid: Mid::from("v0"),
            rid: None,
            seq: SeqNo::from(seq),
            ts: MediaTime::from_90khz(seq * 3000),
            marker: true,
            payload_type: None,
            payload: Arc::from([u8::try_from(seq % 256).expect("masked to a byte")].as_slice()),
            ext_vals: ExtensionValues::default(),
            arrival: base + Duration::from_millis(at_ms),
        };

        let mut jb = JitterBuffer::new(Duration::from_millis(200));
        // Deliver 0,2,3 out of order; 1 is lost.
        jb.push(pkt(2, 10));
        jb.push(pkt(0, 20));
        jb.push(pkt(3, 30));

        // 0 releases after the startup wait elapses (head aged >= max_wait).
        assert!(jb.pop().is_none(), "still within the reordering window");
        jb.push(pkt(3, 300)); // advance 'now' past the 200ms wait
        let mut seqs = Vec::new();
        while let Some(p) = jb.pop() {
            seqs.push(*p.seq);
        }
        // 1 was never delivered; after the wait it is skipped and 2,3 follow in order.
        assert_eq!(seqs, vec![0, 2, 3]);
    }

    /// Opening a stream costs the reordering window, not the loss-recovery budget.
    ///
    /// These are different questions sharing one constant until now: how late the real first
    /// packet can be, versus how long to hold a hole open hoping a retransmission fills it. With
    /// the gap budget at its 5s default, every stream paid five seconds before its first frame -
    /// measured as a 5.1s median time-to-first-frame across the simulation suite.
    #[test]
    fn a_stream_opens_without_paying_the_loss_recovery_budget() {
        use tokio::time::Instant;

        let base = Instant::now();
        let pkt = |seq: u64, at_ms: u64| RtpPacket {
            ssrc: None,
            mid: Mid::from("v0"),
            rid: None,
            seq: SeqNo::from(seq),
            ts: MediaTime::from_90khz(seq * 3000),
            marker: true,
            payload_type: None,
            payload: Arc::from([u8::try_from(seq % 256).expect("masked to a byte")].as_slice()),
            ext_vals: ExtensionValues::default(),
            arrival: base + Duration::from_millis(at_ms),
        };

        // A generous loss-recovery budget, as a real consumer has.
        let mut jb = JitterBuffer::new(Duration::from_secs(5));
        jb.push(pkt(0, 0));

        assert!(
            jb.pop().is_none(),
            "the opening packet is held briefly so a reordered predecessor can arrive"
        );

        // Well past the reordering window, nowhere near the 5s gap budget.
        jb.push(pkt(1, 150));
        assert_eq!(
            jb.pop().map(|p| *p.seq),
            Some(0),
            "the stream must open on the reordering window, not the loss-recovery budget: at 5s \
             a viewer stares at a blank tile for five seconds before the first frame"
        );
    }

    /// The shorter opening window must still absorb the reordering it exists for.
    ///
    /// Simulated paths push a reordered packet back by 30ms, so the default window leaves a small
    /// margin. If it were tightened below that, a stream that merely opened out of order would
    /// commit to the wrong first sequence and drop the true first packet as late.
    #[test]
    fn the_opening_window_still_absorbs_reordering() {
        use tokio::time::Instant;

        let base = Instant::now();
        let pkt = |seq: u64, at_ms: u64| RtpPacket {
            ssrc: None,
            mid: Mid::from("v0"),
            rid: None,
            seq: SeqNo::from(seq),
            ts: MediaTime::from_90khz(seq * 3000),
            marker: true,
            payload_type: None,
            payload: Arc::from([u8::try_from(seq % 256).expect("masked to a byte")].as_slice()),
            ext_vals: ExtensionValues::default(),
            arrival: base + Duration::from_millis(at_ms),
        };

        let mut jb = JitterBuffer::new(Duration::from_secs(5));
        // 1 arrives first; 0 is 30ms behind it, the delay the simulated shaper applies.
        jb.push(pkt(1, 0));
        jb.push(pkt(0, 30));
        jb.push(pkt(2, 150));

        let mut seqs = Vec::new();
        while let Some(p) = jb.pop() {
            seqs.push(*p.seq);
        }
        assert_eq!(
            seqs,
            vec![0, 1, 2],
            "a stream that opened out of order must still be delivered in order"
        );
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

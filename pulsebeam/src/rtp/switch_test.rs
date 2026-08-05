//! End-to-end stream-switching tests.
//!
//! The harness mirrors production wiring exactly: `ShardRoutingTable::route_video`
//! pushes every packet into the per-layer `StreamCache` and then hands the packet
//! plus that cache to each subscriber's `Slot::on_rtp`, which feeds the `Switcher`.
//! Tests here drive that same pair and assert on what the subscriber's decoder
//! would actually receive.

use ahash::HashMap;
use pulsebeam_runtime::rand::seeded_rng;
use str0m::media::Rid;
use tokio::time::Instant;

use crate::entity::{ParticipantId, TrackKind};
use crate::rtp::cache::StreamCache;
use crate::rtp::conformance::{assert_decodable, check_egress};
use crate::rtp::switcher::Switcher;
use crate::rtp::test_utils::{H264StreamBuilder, ParameterSetStyle};
use crate::rtp::{self, RtpPacket};
use crate::track::StreamId;

/// Identifies a simulcast layer in the harness.
pub type LayerId = u32;

/// Drives one subscriber slot across layer switches.
///
/// Mirrors production wiring exactly: every packet is pushed into the per-layer
/// [`StreamCache`], then the switcher is fed the cache update — the same pair
/// that `route_video` and `Slot::on_rtp` drive.
pub struct Forwarder {
    track: crate::entity::TrackId,
    caches: HashMap<LayerId, StreamCache>,
    switcher: Switcher,
    out: Vec<RtpPacket>,
    /// Packets emitted by each single ingest, which is what actually leaves the
    /// SFU back-to-back.
    bursts: Vec<usize>,
}

impl Forwarder {
    pub fn new(seed: u64) -> Self {
        let track =
            ParticipantId::new(&mut seeded_rng(seed)).derive_track_id(TrackKind::Video, "sw");
        Self {
            track,
            caches: HashMap::default(),
            switcher: Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(seed)),
            out: Vec::new(),
            bursts: Vec::new(),
        }
    }

    /// Distinct stream identity per simulcast layer of the one track.
    fn stream_id(&self, layer: LayerId) -> StreamId {
        (self.track, Some(Rid::from(format!("l{layer}").as_str())))
    }

    /// Mirrors `Slot::switch_to`: request the switcher move to a new layer.
    pub fn switch_to(&mut self, layer: LayerId) {
        let sid = self.stream_id(layer);
        self.switcher.switch_to(sid);
    }

    /// Mirrors `route_video` + `Slot::on_rtp` for one packet of `layer`.
    pub fn ingest(&mut self, layer: LayerId, pkt: &RtpPacket) {
        let before = self.out.len();
        let sid = self.stream_id(layer);
        let now = pkt.arrival_ts;

        self.caches.entry(layer).or_default().push(pkt);

        let Forwarder {
            switcher,
            caches,
            out,
            ..
        } = self;
        let cache = &caches[&layer];
        switcher.feed(sid, cache, now, &mut |o| out.push(o));

        self.bursts.push(self.out.len() - before);
    }

    pub fn ingest_all(&mut self, layer: LayerId, pkts: &[RtpPacket]) {
        for p in pkts {
            self.ingest(layer, p);
        }
    }

    pub fn emitted(&self) -> &[RtpPacket] {
        &self.out
    }

    pub fn switched(&self) -> bool {
        !self.switcher.awaiting_switch()
    }

    /// The most packets this forwarder ever pushed out in one go.
    pub fn largest_burst(&self) -> usize {
        self.bursts.iter().copied().max().unwrap_or(0)
    }

    /// Output sequence numbers that were never emitted. The subscriber counts
    /// these as lost, NACKs for them, and reports them as loss in its feedback —
    /// which walks the bandwidth estimate down.
    pub fn sequence_holes(&self) -> u64 {
        let Some(min) = self.out.iter().map(|p| *p.seq_no).min() else {
            return 0;
        };
        let max = self.out.iter().map(|p| *p.seq_no).max().unwrap();
        let seen: ahash::HashSet<u64> = self.out.iter().map(|p| *p.seq_no).collect();
        (max - min + 1) - seen.len() as u64
    }

    /// How far the output RTP clock has run ahead of real time, in ms. This is
    /// latency the subscriber pays: its jitter buffer holds frames whose
    /// timestamps say they fall due later than they really do.
    pub fn clock_lead_ms(&self) -> f64 {
        let (Some(first), Some(last)) = (self.out.first(), self.out.last()) else {
            return 0.0;
        };
        let ts_elapsed = last.rtp_ts.numer() as f64 - first.rtp_ts.numer() as f64;
        let wall = last
            .playout_time
            .saturating_duration_since(first.playout_time)
            .as_secs_f64();
        (ts_elapsed - wall * rtp::VIDEO_FREQUENCY.get() as f64) / 90.0
    }

    /// Output frame timestamps in emission order, deduplicated.
    pub fn frame_timestamps(&self) -> Vec<u64> {
        let mut out: Vec<u64> = Vec::new();
        for p in &self.out {
            let ts = p.rtp_ts.numer();
            if out.last() != Some(&ts) {
                out.push(ts);
            }
        }
        out
    }
}

/// Two simulcast layers publishing in lockstep, the way a real publisher does.
struct TwoLayerPublisher {
    high: H264StreamBuilder,
    low: H264StreamBuilder,
}

impl TwoLayerPublisher {
    fn new(start: Instant, style: ParameterSetStyle) -> Self {
        Self {
            high: H264StreamBuilder::new(0xAAAA, 1000, 90_000, start).with_parameter_sets(style),
            low: H264StreamBuilder::new(0xBBBB, 55_000, 4_000_000, start)
                .with_parameter_sets(style),
        }
    }
}

fn now() -> Instant {
    Instant::now()
}

// ---------------------------------------------------------------------------
// D1: parameter sets must survive the switch.
// ---------------------------------------------------------------------------

#[test]
fn switch_delivers_parameter_sets_with_the_keyframe() {
    let start = now();
    let mut pubr = TwoLayerPublisher::new(start, ParameterSetStyle::SeparatePacket);
    let mut fwd = Forwarder::new(1);

    // High layer is established and flowing.
    fwd.switch_to(0);
    fwd.ingest_all(0, &pubr.high.keyframe(4));
    fwd.ingest_all(0, &pubr.low.delta_frames(0, 1)); // no-op, keeps builders aligned
    for _ in 0..10 {
        let f = pubr.high.delta_frame(3);
        fwd.ingest_all(0, &f);
        let f = pubr.low.delta_frame(1);
        fwd.ingest_all(1, &f);
    }

    // Low layer produces a keyframe, then we switch to it.
    let kf = pubr.low.keyframe(2);
    fwd.switch_to(1);
    fwd.ingest_all(1, &kf);
    for _ in 0..5 {
        let f = pubr.low.delta_frame(1);
        fwd.ingest_all(1, &f);
    }

    assert!(fwd.switched(), "slot should have promoted the staged layer");
    assert_decodable(fwd.emitted(), "after simulcast switch high -> low");
}

#[test]
fn switch_delivers_parameter_sets_when_encoder_sends_them_only_once() {
    let start = now();
    let mut pubr = TwoLayerPublisher::new(start, ParameterSetStyle::OnceAtStreamStart);
    let mut fwd = Forwarder::new(2);

    fwd.switch_to(0);
    fwd.ingest_all(0, &pubr.high.keyframe(4));

    // The low layer sent its parameter sets long ago, at stream start.
    let low_kf = pubr.low.keyframe(2);
    fwd.ingest_all(1, &low_kf);
    for _ in 0..20 {
        let f = pubr.high.delta_frame(3);
        fwd.ingest_all(0, &f);
        let f = pubr.low.delta_frame(1);
        fwd.ingest_all(1, &f);
    }

    // A later IDR carries no parameter sets at all.
    let low_kf2 = pubr.low.keyframe(2);
    fwd.switch_to(1);
    fwd.ingest_all(1, &low_kf2);
    for _ in 0..5 {
        let f = pubr.low.delta_frame(1);
        fwd.ingest_all(1, &f);
    }

    assert!(fwd.switched());
    assert_decodable(
        fwd.emitted(),
        "switch to a layer whose IDR omits parameter sets",
    );
}

// ---------------------------------------------------------------------------
// D2: keyframe segments are frames, not arrival instants.
// ---------------------------------------------------------------------------

#[test]
fn multi_slice_keyframe_is_replayed_whole() {
    let start = now();
    let mut hi = H264StreamBuilder::new(1, 100, 9_000, start);
    let mut lo = H264StreamBuilder::new(2, 900, 700_000, start);
    let mut fwd = Forwarder::new(3);

    fwd.switch_to(0);
    fwd.ingest_all(0, &hi.keyframe(3));
    for _ in 0..5 {
        let f = hi.delta_frame(2);
        fwd.ingest_all(0, &f);
    }

    // Four independent IDR slices in one frame: four packets report an IDR.
    let kf = lo.keyframe_with_slices(4, 2);
    let idr_packets = kf.iter().filter(|p| p.nal.idr()).count();
    assert!(
        idr_packets >= 4,
        "fixture must produce a multi-slice keyframe"
    );

    fwd.switch_to(1);
    fwd.ingest_all(1, &kf);
    let f = lo.delta_frame(1);
    fwd.ingest_all(1, &f);

    assert!(fwd.switched());
    let emitted = fwd.emitted();
    assert_decodable(emitted, "multi-slice keyframe switch");

    // The whole keyframe must be forwarded, not just its last slice.
    let forwarded_idr = emitted.iter().filter(|p| p.nal.idr()).count();
    assert!(
        forwarded_idr >= idr_packets,
        "expected all {idr_packets} IDR slices forwarded, got {forwarded_idr}"
    );
}

#[test]
fn cache_segments_on_frames_not_arrival_time() {
    let start = now();
    let mut b = H264StreamBuilder::new(7, 10, 3_000, start);
    let mut cache = StreamCache::default();

    let kf = b.keyframe_with_slices(3, 2);
    for p in &kf {
        cache.push(p);
    }
    let last = kf.last().unwrap();
    let replay = cache
        .replay()
        .expect("a complete keyframe must be replayable");

    assert_eq!(
        replay.len(),
        kf.len(),
        "replay must contain the entire keyframe frame, not a suffix of it"
    );
}

// ---------------------------------------------------------------------------
// D3 / D4: timestamps stay monotonic and do not drift forward per switch.
// ---------------------------------------------------------------------------

#[test]
fn switch_never_reuses_a_timestamp_across_frames() {
    let start = now();
    let mut pubr = TwoLayerPublisher::new(start, ParameterSetStyle::SeparatePacket);
    let mut fwd = Forwarder::new(4);

    fwd.switch_to(0);
    fwd.ingest_all(0, &pubr.high.keyframe(3));
    for _ in 0..30 {
        let f = pubr.high.delta_frame(3);
        fwd.ingest_all(0, &f);
        let f = pubr.low.delta_frame(1);
        fwd.ingest_all(1, &f);
    }

    let kf = pubr.low.keyframe(2);
    fwd.switch_to(1);
    fwd.ingest_all(1, &kf);
    let f = pubr.low.delta_frame(1);
    fwd.ingest_all(1, &f);

    let emitted = fwd.emitted();
    let violations = check_egress(emitted);
    let ts_issues: Vec<_> = violations
        .iter()
        .filter(|v| v.reason.contains("timestamp") || v.reason.contains("resumed"))
        .collect();
    assert!(
        ts_issues.is_empty(),
        "timestamp violations across switch: {ts_issues:#?}"
    );

    // Distinct frames must have distinct timestamps.
    let mut seen: HashMap<u64, usize> = HashMap::default();
    let mut prev_ts = None;
    let mut frame_index = 0usize;
    for p in emitted {
        let ts = p.rtp_ts.numer();
        if prev_ts != Some(ts) {
            frame_index += 1;
            if let Some(first) = seen.insert(ts, frame_index) {
                panic!("frames {first} and {frame_index} share rtp_ts {ts}");
            }
            prev_ts = Some(ts);
        }
    }
}

#[test]
fn repeated_switching_does_not_drift_the_output_clock() {
    let start = now();
    let mut pubr = TwoLayerPublisher::new(start, ParameterSetStyle::SeparatePacket);
    let mut fwd = Forwarder::new(5);

    fwd.switch_to(0);
    fwd.ingest_all(0, &pubr.high.keyframe(3));

    // Flip layers 20 times, each time after a long GOP so the cache holds a lot.
    for round in 0..20 {
        for _ in 0..30 {
            let f = pubr.high.delta_frame(3);
            fwd.ingest_all(0, &f);
            let f = pubr.low.delta_frame(1);
            fwd.ingest_all(1, &f);
        }
        let target = if round % 2 == 0 { 1 } else { 0 };
        fwd.switch_to(target);
        let kf = if target == 1 {
            pubr.low.keyframe(2)
        } else {
            pubr.high.keyframe(3)
        };
        fwd.ingest_all(target, &kf);
        let other = 1 - target;
        let f = if other == 1 {
            pubr.low.delta_frame(1)
        } else {
            pubr.high.delta_frame(3)
        };
        fwd.ingest_all(other, &f);
    }

    let emitted = fwd.emitted();
    assert_decodable(emitted, "20 layer switches");

    // The output RTP clock must still track real elapsed time. Anything that
    // replays a whole GOP on every switch accumulates a forward jump per switch.
    let first = emitted.first().unwrap();
    let last = emitted.last().unwrap();
    let ts_elapsed = last.rtp_ts.numer() - first.rtp_ts.numer();
    let wall_elapsed = last
        .playout_time
        .saturating_duration_since(first.playout_time);
    let wall_ticks = (wall_elapsed.as_secs_f64() * rtp::VIDEO_FREQUENCY.get() as f64) as u64;

    let skew_ticks = ts_elapsed.abs_diff(wall_ticks);
    let skew_ms = skew_ticks as f64 / 90.0;
    assert!(
        skew_ms < 500.0,
        "output clock drifted {skew_ms:.0}ms from real time over 20 switches \
         (rtp advanced {ts_elapsed} ticks, wall clock {wall_ticks} ticks)"
    );
}

// ---------------------------------------------------------------------------
// D7 / reordering: no silent loss, no duplicate output sequence numbers.
// ---------------------------------------------------------------------------

#[test]
fn switch_emits_each_output_sequence_number_exactly_once() {
    let start = now();
    let mut pubr = TwoLayerPublisher::new(start, ParameterSetStyle::SeparatePacket);
    let mut fwd = Forwarder::new(6);

    fwd.switch_to(0);
    fwd.ingest_all(0, &pubr.high.keyframe(3));
    for _ in 0..10 {
        let f = pubr.high.delta_frame(3);
        fwd.ingest_all(0, &f);
        let f = pubr.low.delta_frame(2);
        fwd.ingest_all(1, &f);
    }

    let kf = pubr.low.keyframe(2);
    fwd.switch_to(1);
    fwd.ingest_all(1, &kf);
    for _ in 0..10 {
        let f = pubr.low.delta_frame(2);
        fwd.ingest_all(1, &f);
    }

    let mut seen = HashMap::default();
    for (i, p) in fwd.emitted().iter().enumerate() {
        if let Some(prev) = seen.insert(*p.seq_no, i) {
            panic!(
                "output seq {} emitted twice (index {prev} and {i})",
                *p.seq_no
            );
        }
    }
}

#[test]
fn reordered_ingress_around_a_switch_stays_decodable() {
    let start = now();
    let mut hi = H264StreamBuilder::new(11, 4000, 90_000, start);
    let mut lo = H264StreamBuilder::new(22, 9000, 500_000, start);
    let mut fwd = Forwarder::new(7);

    fwd.switch_to(0);
    fwd.ingest_all(0, &hi.keyframe(3));
    for _ in 0..8 {
        let f = hi.delta_frame(3);
        fwd.ingest_all(0, &f);
        let f = lo.delta_frame(2);
        fwd.ingest_all(1, &f);
    }

    // The low layer's keyframe arrives with its parameter-set packet delayed
    // behind the first IDR fragment, which the network is free to do.
    let mut kf = lo.keyframe(3);
    kf.swap(0, 1);

    fwd.switch_to(1);
    fwd.ingest_all(1, &kf);
    for _ in 0..5 {
        let f = lo.delta_frame(2);
        fwd.ingest_all(1, &f);
    }

    assert!(fwd.switched(), "reordering must not block the switch");
    assert_decodable(fwd.emitted(), "switch with reordered keyframe packets");
}

#[test]
fn upstream_loss_is_visible_to_the_subscriber_as_a_sequence_gap() {
    let start = now();
    let mut b = H264StreamBuilder::new(3, 500, 90_000, start);
    let mut fwd = Forwarder::new(8);

    fwd.switch_to(0);
    fwd.ingest_all(0, &b.keyframe(3));
    let f = b.delta_frame(3);
    fwd.ingest_all(0, &f);

    let before = fwd.emitted().len();
    b.drop_packets(5);
    let f = b.delta_frame(3);
    fwd.ingest_all(0, &f);

    let emitted = fwd.emitted();
    let gap_start = *emitted[before - 1].seq_no;
    let gap_end = *emitted[before].seq_no;
    assert_eq!(
        gap_end - gap_start,
        6,
        "5 lost upstream packets must leave a 5-wide hole in the output sequence \
         so the subscriber can detect the loss; got {gap_start} -> {gap_end}"
    );
}

/// Switches are requested at arbitrary points in a GOP, not conveniently on a
/// keyframe. Whatever the forwarder does to bridge that gap must not push the
/// output clock away from real time, or video drifts ahead of audio a little
/// more with every switch until lip sync is visibly wrong.
#[test]
fn switching_mid_gop_does_not_walk_the_output_clock_away_from_real_time() {
    let start = now();
    let mut pubr = TwoLayerPublisher::new(start, ParameterSetStyle::SeparatePacket);
    let mut fwd = Forwarder::new(21);

    fwd.switch_to(0);
    fwd.ingest_all(0, &pubr.high.keyframe(3));
    fwd.ingest_all(1, &pubr.low.keyframe(2));

    // Both layers run 1-second GOPs. Every switch is requested a few frames into
    // the current GOP, and the publisher only produces the next keyframe after
    // the resulting PLI.
    const SWITCHES: usize = 25;
    for round in 0..SWITCHES {
        let target = (round % 2) as LayerId;

        // A few frames into the GOP, the allocator decides to move.
        for _ in 0..4 {
            let f = pubr.high.delta_frame(3);
            fwd.ingest_all(0, &f);
            let f = pubr.low.delta_frame(1);
            fwd.ingest_all(1, &f);
        }
        fwd.switch_to(target);

        // The PLI takes a few frames to come back as a keyframe.
        for _ in 0..3 {
            let f = pubr.high.delta_frame(3);
            fwd.ingest_all(0, &f);
            let f = pubr.low.delta_frame(1);
            fwd.ingest_all(1, &f);
        }
        let kf = if target == 0 {
            pubr.high.keyframe(3)
        } else {
            pubr.low.keyframe(2)
        };
        fwd.ingest_all(target, &kf);
        let other = 1 - target;
        let f = if other == 0 {
            pubr.high.delta_frame(3)
        } else {
            pubr.low.delta_frame(1)
        };
        fwd.ingest_all(other, &f);

        assert!(
            fwd.switched(),
            "switch {round} never completed even after a fresh keyframe"
        );
    }

    let emitted = fwd.emitted();
    assert_decodable(emitted, "25 mid-GOP switches");

    let first = emitted.first().unwrap();
    let last = emitted.last().unwrap();
    let ts_elapsed = last.rtp_ts.numer() - first.rtp_ts.numer();
    let wall = last
        .playout_time
        .saturating_duration_since(first.playout_time);
    let wall_ticks = (wall.as_secs_f64() * rtp::VIDEO_FREQUENCY.get() as f64) as u64;
    let skew_ms = ts_elapsed.abs_diff(wall_ticks) as f64 / 90.0;

    assert!(
        skew_ms < 100.0,
        "output clock drifted {skew_ms:.0}ms from real time over {SWITCHES} mid-GOP \
         switches ({:.1}ms per switch) — video would run ahead of audio",
        skew_ms / SWITCHES as f64
    );
}

// ---------------------------------------------------------------------------
// Switching away from a stream whose current frame is still in flight.
// ---------------------------------------------------------------------------

/// If the active layer stops sending, the frame it was midway through will
/// never complete. Rather than stall the slot forever, the switch goes ahead
/// after a grace period — and burns a sequence number so the subscriber reads
/// the fragment as damaged instead of decoding it.
#[test]
fn a_dead_active_layer_does_not_stall_the_switch_forever() {
    let start = now();
    let mut hi = H264StreamBuilder::new(0xA1, 200, 90_000, start);
    let mut lo = H264StreamBuilder::new(0xB1, 7000, 300_000, start);
    let mut fwd = Forwarder::new(31);

    fwd.switch_to(0);
    fwd.ingest_all(0, &hi.keyframe(3));
    for _ in 0..4 {
        let f = hi.delta_frame(3);
        fwd.ingest_all(0, &f);
        let f = lo.delta_frame(2);
        fwd.ingest_all(1, &f);
    }

    // The active layer goes silent partway through a frame.
    let partial = hi.delta_frame(3);
    fwd.ingest_all(0, &partial[..2]);
    let truncated_ts = partial[0].rtp_ts.numer();

    let kf = lo.keyframe(2);
    fwd.switch_to(1);
    fwd.ingest_all(1, &kf);

    // The staged layer keeps running; the active one never comes back.
    for _ in 0..4 {
        let f = lo.delta_frame(2);
        fwd.ingest_all(1, &f);
    }

    assert!(
        fwd.switched(),
        "the slot must not stall waiting on a layer that stopped"
    );
    assert_decodable(fwd.emitted(), "switch forced past a dead layer");

    let truncated: Vec<_> = fwd
        .emitted()
        .iter()
        .filter(|p| p.rtp_ts.numer() == truncated_ts)
        .collect();
    assert_eq!(
        truncated.len(),
        2,
        "only what arrived before the layer died"
    );
    assert!(!truncated.iter().any(|p| p.marker));
}

/// The marker packet overtakes an earlier packet of the same frame, so the
/// frame looks finished while a hole remains inside it. Switching on that
/// appearance abandons the in-flight packet; the switch has to wait for the
/// frame to actually be whole, not merely marked.
#[test]
fn a_reordered_marker_does_not_look_like_a_frame_boundary() {
    let start = now();
    let mut hi = H264StreamBuilder::new(0xA2, 400, 90_000, start);
    let mut lo = H264StreamBuilder::new(0xB2, 9100, 700_000, start);
    let mut fwd = Forwarder::new(32);

    fwd.switch_to(0);
    fwd.ingest_all(0, &hi.keyframe(3));
    for _ in 0..4 {
        let f = hi.delta_frame(3);
        fwd.ingest_all(0, &f);
        let f = lo.delta_frame(2);
        fwd.ingest_all(1, &f);
    }

    // Packets 0 and 2 of the frame arrive; packet 1 is still in flight. The
    // marker has been forwarded, so the frame *looks* complete.
    let frame = hi.delta_frame(3);
    let frame_ts = frame[0].rtp_ts.numer();
    fwd.ingest(0, &frame[0]);
    fwd.ingest(0, &frame[2]);

    let kf = lo.keyframe(2);
    fwd.switch_to(1);
    fwd.ingest_all(1, &kf);

    // The missing middle packet arrives.
    fwd.ingest(0, &frame[1]);
    for _ in 0..4 {
        let f = lo.delta_frame(2);
        fwd.ingest_all(1, &f);
    }

    assert!(fwd.switched());
    assert_decodable(fwd.emitted(), "switch after a reordered marker");

    let forwarded: Vec<_> = fwd
        .emitted()
        .iter()
        .filter(|p| p.rtp_ts.numer() == frame_ts)
        .collect();
    assert_eq!(
        forwarded.len(),
        3,
        "the switch must wait for the in-flight packet rather than abandon it"
    );
    assert_eq!(
        fwd.sequence_holes(),
        0,
        "abandoning the in-flight packet would read as congestion loss"
    );
}

// ---------------------------------------------------------------------------
// Switching must not disturb pacing, and must not manufacture loss.
// ---------------------------------------------------------------------------

/// Sets up two layers publishing 30fps, switches repeatedly, and returns the
/// forwarder so a test can inspect what the switch did to the output stream.
fn run_switch_workload(seed: u64, switches: usize, gap_frames: usize) -> Forwarder {
    let start = now();
    let mut pubr = TwoLayerPublisher::new(start, ParameterSetStyle::SeparatePacket);
    let mut fwd = Forwarder::new(seed);

    fwd.switch_to(0);
    fwd.ingest_all(0, &pubr.high.keyframe(3));

    for round in 0..switches {
        for _ in 0..gap_frames {
            let f = pubr.high.delta_frame(3);
            fwd.ingest_all(0, &f);
            let f = pubr.low.delta_frame(3);
            fwd.ingest_all(1, &f);
        }

        let target = (round % 2) as LayerId;
        fwd.switch_to(target);
        let kf = if target == 0 {
            pubr.high.keyframe(3)
        } else {
            pubr.low.keyframe(3)
        };
        fwd.ingest_all(target, &kf);
        let other = 1 - target;
        let f = if other == 0 {
            pubr.high.delta_frame(3)
        } else {
            pubr.low.delta_frame(3)
        };
        fwd.ingest_all(other, &f);
    }
    fwd
}

/// A switch must not manufacture a sequence hole. The subscriber cannot tell an
/// SFU-created hole from real congestion loss: it NACKs for packets that were
/// never written to str0m's retransmission cache and can never be served, and it
/// reports the loss in its feedback, walking the bandwidth estimate down.
#[test]
fn switching_does_not_manufacture_packet_loss() {
    let fwd = run_switch_workload(41, 12, 4);
    assert!(fwd.switched());

    let holes = fwd.sequence_holes();
    assert_eq!(
        holes, 0,
        "switching invented {holes} lost packets over 12 switches; the subscriber \
         will NACK for packets that were never sent and report them as congestion loss"
    );
}

/// The output frame rate must be the source's. A switch that replays buffered
/// media compresses several frames into one instant and then resumes at 1x: the
/// subscriber sees a speed-up followed by a step change in buffer depth.
#[test]
fn switching_keeps_the_source_frame_cadence() {
    let fwd = run_switch_workload(42, 12, 4);
    let timestamps = fwd.frame_timestamps();
    assert!(timestamps.len() > 50);

    // The publishers run at a steady 30fps, so every output frame interval must
    // be one frame long.
    const FRAME: u64 = 90_000 / 30;
    for (i, w) in timestamps.windows(2).enumerate() {
        let delta = w[1] - w[0];
        assert_eq!(
            delta,
            FRAME,
            "frame interval {i} was {delta} ticks ({:.1}ms) instead of one frame; \
             the switch changed the playback rate",
            delta as f64 / 90.0
        );
    }
}

/// A switch must not dump buffered media in one burst. The subscriber's
/// congestion control reads that spike as queue build-up and lowers the
/// estimate, so the SFU degrades the very stream it just switched to.
#[test]
fn switching_does_not_burst_buffered_media_at_the_subscriber() {
    let fwd = run_switch_workload(43, 12, 4);

    // Forwarding is one-in-one-out except at a switch, where the keyframe
    // already received is released together. One frame is the most that can be.
    let largest = fwd.largest_burst();
    assert!(
        largest <= 5,
        "a switch released {largest} packets back-to-back; that spike reads as \
         congestion to the subscriber's bandwidth estimator"
    );
}

/// The decision to switch arrives while the active layer is midway through a
/// frame. Cutting there costs the subscriber twice: the frame is incomplete, and
/// the packets never sent read as congestion loss it will NACK for and report.
#[test]
fn a_switch_waits_for_the_active_frame_instead_of_cutting_it() {
    let start = now();
    let mut hi = H264StreamBuilder::new(0xC1, 900, 90_000, start);
    let mut lo = H264StreamBuilder::new(0xC2, 44_000, 600_000, start);
    let mut fwd = Forwarder::new(51);

    fwd.switch_to(0);
    fwd.ingest_all(0, &hi.keyframe(3));
    for _ in 0..4 {
        let f = hi.delta_frame(4);
        fwd.ingest_all(0, &f);
        let f = lo.delta_frame(3);
        fwd.ingest_all(1, &f);
    }

    // Two of four packets of the active frame have been forwarded.
    let partial = hi.delta_frame(4);
    fwd.ingest_all(0, &partial[..2]);

    // The staged layer's keyframe lands mid-frame.
    let kf = lo.keyframe(3);
    fwd.switch_to(1);
    fwd.ingest_all(1, &kf);

    // The rest of the active frame arrives right behind it.
    fwd.ingest_all(0, &partial[2..]);
    for _ in 0..4 {
        let f = lo.delta_frame(3);
        fwd.ingest_all(1, &f);
    }

    assert!(fwd.switched(), "the switch must still complete");
    assert_decodable(fwd.emitted(), "switch deferred to a frame boundary");

    let holes = fwd.sequence_holes();
    assert_eq!(
        holes, 0,
        "the switch cut into a frame and left {holes} sequence numbers unsent; \
         the subscriber will NACK for packets str0m never cached and count them \
         as congestion loss"
    );

    // And the frame that was in progress must have been delivered whole.
    let frame_ts = partial[0].rtp_ts.numer();
    let delivered = fwd
        .emitted()
        .iter()
        .filter(|p| p.rtp_ts.numer() == frame_ts)
        .count();
    assert_eq!(delivered, 4, "the in-progress frame must not be truncated");
}

/// A layer that has been flowing for a while has a keyframe well behind the
/// live edge. Switching to it must not release everything since that keyframe in
/// one go: that spike reads as queue build-up to the subscriber's congestion
/// control, which lowers the estimate right when the new layer needs headroom.
#[test]
fn switching_to_an_already_flowing_layer_does_not_release_a_backlog() {
    let start = now();
    let mut hi = H264StreamBuilder::new(0xD1, 100, 90_000, start);
    let mut lo = H264StreamBuilder::new(0xD2, 70_000, 200_000, start);
    let mut fwd = Forwarder::new(52);

    fwd.switch_to(0);
    fwd.ingest_all(0, &hi.keyframe(3));

    // The low layer sent a keyframe and has been running ever since.
    fwd.ingest_all(1, &lo.keyframe(3));
    for _ in 0..5 {
        let f = hi.delta_frame(3);
        fwd.ingest_all(0, &f);
        let f = lo.delta_frame(3);
        fwd.ingest_all(1, &f);
    }

    fwd.switch_to(1);
    for _ in 0..5 {
        let f = lo.delta_frame(3);
        fwd.ingest_all(1, &f);
        let f = hi.delta_frame(3);
        fwd.ingest_all(0, &f);
    }

    let largest = fwd.largest_burst();
    assert!(
        largest <= 6,
        "switching released {largest} packets back-to-back from the layer's backlog"
    );
}

// ---------------------------------------------------------------------------
// The abandoned stream still finishes what it started.
// ---------------------------------------------------------------------------

/// The switch does not wait around for a packet that is merely late. It goes
/// ahead, and the abandoned stream is still allowed to fill the gap it left, so
/// the frame the subscriber already has part of gets completed rather than
/// counting as loss.
#[test]
fn the_abandoned_stream_completes_its_frame_after_the_switch() {
    let start = now();
    let mut hi = H264StreamBuilder::new(0xE1, 500, 90_000, start);
    let mut lo = H264StreamBuilder::new(0xE2, 31_000, 800_000, start);
    let mut fwd = Forwarder::new(61);

    fwd.switch_to(0);
    fwd.ingest_all(0, &hi.keyframe(3));
    for _ in 0..4 {
        let f = hi.delta_frame(4);
        fwd.ingest_all(0, &f);
        let f = lo.delta_frame(3);
        fwd.ingest_all(1, &f);
    }

    // The active layer's frame arrives with a hole: the marker packet overtook
    // one of the middle packets, which is still on the wire.
    let frame = hi.delta_frame(4);
    let frame_ts = frame[0].rtp_ts.numer();
    fwd.ingest(0, &frame[0]);
    fwd.ingest(0, &frame[1]);
    fwd.ingest(0, &frame[3]);

    // Enough of the staged layer arrives that the grace period lapses and the
    // switch proceeds without the missing packet.
    fwd.switch_to(1);
    for _ in 0..3 {
        let f = lo.delta_frame(3);
        fwd.ingest_all(1, &f);
    }
    let kf = lo.keyframe(3);
    fwd.ingest_all(1, &kf);
    let f = lo.delta_frame(3);
    fwd.ingest_all(1, &f);
    assert!(fwd.switched(), "the switch must not wait on a late packet");

    let before = fwd.emitted().len();

    // The straggler finally arrives, after the new layer is already flowing.
    fwd.ingest(0, &frame[2]);

    assert_eq!(
        fwd.emitted().len(),
        before + 1,
        "the abandoned stream's packet must still be forwarded to finish the frame"
    );
    let filled = fwd.emitted().last().unwrap();
    assert_eq!(
        filled.rtp_ts.numer(),
        frame_ts,
        "it must carry the timestamp of the frame it belongs to, not the new one"
    );
    assert_eq!(
        fwd.sequence_holes(),
        0,
        "the gap the switch left must have been filled, not counted as loss"
    );
    assert_decodable(fwd.emitted(), "tail-completed frame across a switch");
}

/// A packet from the abandoned stream that does not fit a gap the switch left
/// must never be emitted: the new stream owns everything past the switch point.
#[test]
fn the_abandoned_stream_cannot_write_into_the_new_streams_sequence_space() {
    let start = now();
    let mut hi = H264StreamBuilder::new(0xF1, 800, 90_000, start);
    let mut lo = H264StreamBuilder::new(0xF2, 21_000, 500_000, start);
    let mut fwd = Forwarder::new(62);

    fwd.switch_to(0);
    fwd.ingest_all(0, &hi.keyframe(3));
    for _ in 0..4 {
        let f = hi.delta_frame(3);
        fwd.ingest_all(0, &f);
        let f = lo.delta_frame(3);
        fwd.ingest_all(1, &f);
    }

    fwd.switch_to(1);
    let kf = lo.keyframe(3);
    fwd.ingest_all(1, &kf);
    assert!(fwd.switched());
    for _ in 0..3 {
        let f = lo.delta_frame(3);
        fwd.ingest_all(1, &f);
    }

    let before = fwd.emitted().len();
    // The old layer keeps producing whole new frames. None of them belong to
    // anything the subscriber is waiting for.
    for _ in 0..3 {
        let f = hi.delta_frame(3);
        fwd.ingest_all(0, &f);
    }
    assert_eq!(
        fwd.emitted().len(),
        before,
        "the abandoned stream may only fill gaps, never extend past the switch"
    );
    assert_decodable(fwd.emitted(), "abandoned stream kept out of the new stream");
}

/// Gaps belong to the stream that left them. A straggler must only ever fill a
/// gap in its own stream, never be dropped into a gap another stream left with a
/// timestamp from the wrong frame. Gaps are tracked per stream in input-sequence
/// space and reset on every switch, so a late packet on layer A can only land in
/// a gap layer A itself stepped over — the result must stay decodable.
#[test]
fn a_gap_can_only_be_filled_by_the_stream_that_left_it() {
    let start = now();
    let mut a = H264StreamBuilder::new(0x11, 600, 90_000, start);
    let mut b = H264StreamBuilder::new(0x22, 25_000, 400_000, start);
    let mut fwd = Forwarder::new(63);

    fwd.switch_to(0);
    fwd.ingest_all(0, &a.keyframe(3));

    // Stream A leaves a gap behind when we switch away from it.
    let orphan = a.delta_frame(4);
    fwd.ingest(0, &orphan[0]);
    fwd.ingest(0, &orphan[1]);
    fwd.ingest(0, &orphan[3]);

    fwd.switch_to(1);
    for _ in 0..2 {
        let f = b.delta_frame(3);
        fwd.ingest_all(1, &f);
    }
    fwd.ingest_all(1, &b.keyframe(3));
    let f = b.delta_frame(3);
    fwd.ingest_all(1, &f);
    assert!(fwd.switched());

    // Switch back to A, so B becomes the stream being drained.
    fwd.switch_to(0);
    for _ in 0..2 {
        let f = a.delta_frame(3);
        fwd.ingest_all(0, &f);
    }
    fwd.ingest_all(0, &a.keyframe(3));
    let f = a.delta_frame(3);
    fwd.ingest_all(0, &f);
    assert!(fwd.switched());

    // A's long-lost packet finally arrives. Whether or not it lands — that
    // depends on whether the switch back to A replayed the frame it belongs to —
    // it must never be dropped into another stream's gap, so the output stays
    // decodable with no reused or backwards timestamps.
    fwd.ingest(0, &orphan[2]);
    let violations = check_egress(fwd.emitted());
    assert!(
        violations.is_empty(),
        "a straggler corrupted the stream: {violations:#?}"
    );
    assert_decodable(fwd.emitted(), "two switches with a stale straggler");
}

#[test]
fn a_delayed_packet_cannot_fill_a_hole_from_an_older_switch() {
    let start = now();
    let mut a = H264StreamBuilder::new(0x31, 100, 90_000, start);
    let mut b = H264StreamBuilder::new(0x32, 1_000, 25_000, start);
    let mut fwd = Forwarder::new(64);

    fwd.switch_to(0);
    fwd.ingest_all(0, &a.keyframe(3));
    let orphan = a.delta_frame(4);
    fwd.ingest(0, &orphan[0]);
    fwd.ingest(0, &orphan[1]);
    fwd.ingest(0, &orphan[3]);

    fwd.switch_to(1);
    let delayed = b.delta_frame(20);
    for packet in delayed.iter().take(19) {
        fwd.ingest(1, packet);
    }
    fwd.ingest_all(1, &b.keyframe(3));
    fwd.ingest_all(1, &b.delta_frame(3));
    assert!(fwd.switched());

    fwd.switch_to(0);
    fwd.ingest_all(0, &a.keyframe(3));
    fwd.ingest_all(0, &a.delta_frame(3));
    assert!(fwd.switched());

    fwd.ingest(1, &delayed[19]);
    let violations = check_egress(fwd.emitted());
    assert!(
        violations.is_empty(),
        "a delayed packet from a previous stream corrupted the egress: {violations:#?}"
    );
}

/// Screen share: the publisher sends a keyframe and then goes quiet because
/// nothing on screen changed. A viewer attaching later must be shown that
/// keyframe as soon as anything arrives, not held on black until a PLI round
/// trip produces a duplicate of the frame the SFU already has.
#[test]
fn a_viewer_joining_a_still_screen_share_gets_the_cached_keyframe() {
    let start = now();
    // Screen share runs slowly and mostly emits nothing at all.
    let mut screen = H264StreamBuilder::new(0x5C, 900, 90_000, start)
        .with_parameter_sets(ParameterSetStyle::SeparatePacket)
        .with_fps(5);
    let mut camera = H264StreamBuilder::new(0xCA, 300, 40_000, start);
    let mut fwd = Forwarder::new(71);

    // The viewer is watching a camera; the screen share published one keyframe
    // long ago and has been static ever since.
    fwd.switch_to(0);
    fwd.ingest_all(0, &camera.keyframe(3));
    fwd.ingest_all(1, &screen.keyframe(4));
    for _ in 0..40 {
        let f = camera.delta_frame(3);
        fwd.ingest_all(0, &f);
    }

    // The viewer switches to the screen share. The only thing that arrives is a
    // single packet from the long-idle screen.
    fwd.switch_to(1);
    let nudge = screen.delta_frame(1);
    fwd.ingest_all(1, &nudge);

    assert!(
        fwd.switched(),
        "a still screen must render from cache, not wait on a keyframe request"
    );
    let emitted = fwd.emitted();
    assert_decodable(emitted, "viewer joining a still screen share");

    // The last thing sent must be the screen's keyframe with its parameter sets,
    // not the camera frames that preceded the switch.
    let tail = &emitted[emitted.len().saturating_sub(6)..];
    assert!(
        tail.iter().any(|p| p.nal.idr()),
        "the viewer must receive a decodable entry point"
    );
    assert!(
        tail.iter().any(|p| p.nal.sps()) && tail.iter().any(|p| p.nal.pps()),
        "and the parameter sets describing it"
    );
}

// ---------------------------------------------------------------------------
// What a switch is allowed to cost the subscriber.
// ---------------------------------------------------------------------------

/// Runs `fps` video on two layers, lets both build a full GOP, then switches to
/// the stale one and answers the resulting keyframe request.
fn switch_onto_a_stale_layer(fps: u32, gop_frames: usize, seed: u64) -> Forwarder {
    let start = now();
    let mut a = H264StreamBuilder::new(1, 100, 90_000, start).with_fps(fps);
    let mut b = H264StreamBuilder::new(2, 50_000, 700_000, start).with_fps(fps);
    let mut fwd = Forwarder::new(seed);

    fwd.switch_to(0);
    fwd.ingest_all(0, &a.keyframe(30));
    fwd.ingest_all(1, &b.keyframe(30));
    for _ in 0..gop_frames {
        let f = a.delta_frame(8);
        fwd.ingest_all(0, &f);
        let f = b.delta_frame(8);
        fwd.ingest_all(1, &f);
    }

    fwd.switch_to(1);
    let f = b.delta_frame(8);
    fwd.ingest_all(1, &f);
    assert!(
        !fwd.switched(),
        "a whole GOP of backlog must not be replayed at the subscriber"
    );

    // The keyframe request is answered.
    fwd.ingest_all(1, &b.keyframe(30));
    for _ in 0..5 {
        let f = b.delta_frame(8);
        fwd.ingest_all(1, &f);
    }
    assert!(
        fwd.switched(),
        "the switch must complete once a keyframe arrives"
    );
    fwd
}

/// Whatever a switch hands over goes into str0m's pacer, so releasing a lot at
/// once turns into queueing delay for the subscriber. A switch should hand over
/// the entry point and then let the stream flow, not dump a backlog.
#[test]
fn a_switch_releases_only_a_handful_of_packets_at_once() {
    for (fps, gop) in [(30u32, 60usize), (15, 30), (5, 10)] {
        let fwd = switch_onto_a_stale_layer(fps, gop, 100 + fps as u64);
        let burst = fwd.largest_burst();
        assert!(
            burst <= 8,
            "at {fps}fps a switch released {burst} packets in one go; that queues \
             in the pacer and comes out as latency"
        );
    }
}

/// Replaying media the subscriber has not seen yet puts the output clock ahead
/// of real time, and it stays there — every switch would add to it.
#[test]
fn a_switch_does_not_put_the_subscriber_behind_live() {
    for (fps, gop) in [(30u32, 60usize), (15, 30), (5, 10)] {
        let fwd = switch_onto_a_stale_layer(fps, gop, 200 + fps as u64);
        let lead = fwd.clock_lead_ms();
        assert!(
            lead.abs() < 50.0,
            "at {fps}fps a switch left the output clock {lead:.0}ms ahead of real \
             time; the subscriber pays that as latency for the rest of the call"
        );
    }
}

/// The cache is shared and long-lived; it must not grow with the stream.
#[test]
fn the_cache_does_not_grow_without_bound() {
    let start = now();
    let mut b = H264StreamBuilder::new(9, 1000, 90_000, start);
    let mut cache = StreamCache::default();

    for p in b.keyframe(4) {
        cache.push(&p);
    }
    for _ in 0..2000 {
        for p in b.delta_frame(8) {
            cache.push(&p);
        }
    }

    // Whatever it still holds must not be replayable as a backlog.
    assert!(
        cache.replay().is_none(),
        "a stream that has run for a long time must not be replayable in one go"
    );
}

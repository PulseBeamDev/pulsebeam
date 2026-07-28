//! End-to-end stream-switching tests.
//!
//! The harness mirrors production wiring exactly: `ShardRoutingTable::route_video`
//! pushes every packet into the per-layer `StreamCache` and then hands the packet
//! plus that cache to each subscriber's `Slot::on_rtp`, which feeds the `Switcher`.
//! Tests here drive that same pair and assert on what the subscriber's decoder
//! would actually receive.

use ahash::HashMap;
use pulsebeam_runtime::rand::seeded_rng;
use tokio::time::Instant;

use crate::rtp::cache::StreamCache;
use crate::rtp::conformance::{assert_decodable, check_egress};
use crate::rtp::switcher::Switcher;
use crate::rtp::test_utils::{H264StreamBuilder, ParameterSetStyle};
use crate::rtp::{self, RtpPacket};

/// Identifies a simulcast layer in the harness.
pub type LayerId = u32;

/// Drives one subscriber slot across layer switches.
pub struct Forwarder {
    caches: HashMap<LayerId, StreamCache>,
    switcher: Switcher,
    active: Option<LayerId>,
    staging: Option<LayerId>,
    out: Vec<RtpPacket>,
}

impl Forwarder {
    pub fn new(seed: u64) -> Self {
        Self {
            caches: HashMap::default(),
            switcher: Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(seed)),
            active: None,
            staging: None,
            out: Vec::new(),
        }
    }

    /// Mirrors `Slot::switch_to`: stage a new layer, leaving the active one
    /// forwarding until the staged one is promotable.
    pub fn switch_to(&mut self, layer: LayerId) {
        if self.active == Some(layer) {
            self.staging = None;
            self.switcher.clear_staging();
            return;
        }
        self.staging = Some(layer);
        self.switcher.clear_staging();
    }

    /// Mirrors `route_video` + `Slot::on_rtp` for one packet of `layer`.
    pub fn ingest(&mut self, layer: LayerId, pkt: &RtpPacket) {
        let cache = self.caches.entry(layer).or_default();
        cache.push(pkt);

        if self.active == Some(layer) {
            self.switcher.push(pkt.clone());
            while let Some(out) = self.switcher.pop() {
                self.out.push(out);
            }
            return;
        }

        if self.staging == Some(layer) && !self.switcher.is_switching() {
            let cache = &self.caches[&layer];
            if let Some(pkts) = cache.replay(pkt.arrival_ts) {
                self.switcher.stage_direct(pkts);
                while let Some(out) = self.switcher.pop() {
                    self.out.push(out);
                }
                if self.switcher.ready_to_switch() {
                    self.active = self.staging.take();
                }
            }
        }
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
        self.staging.is_none()
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
        .replay(last.arrival_ts)
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

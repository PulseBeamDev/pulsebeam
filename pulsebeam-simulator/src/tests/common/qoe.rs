//! A receiver-side Quality-of-Experience harness for simulcast tests.
//!
//! Prior simulcast tests only checked "did enough bits arrive" (a throughput
//! floor). That misses two whole classes of regression: (1) a stream that
//! delivers plenty of bits but glitches every time the SFU switches which
//! simulcast layer it forwards (a P-frame spliced onto the wrong layer is
//! undecodable garbage even though the byte counter looks healthy), and (2)
//! an allocator that delivers acceptable aggregate throughput while silently
//! starving a viewer's highest-priority stream in favor of a lower-priority
//! one.
//!
//! This module reads the actual reassembled H.264 access units the receiver
//! gets (not RTCP/byte counters) and turns them into:
//!   - a hard, always-on decodability invariant (`StreamHealth::assert_decodable`)
//!   - a composite 0-100 QoE score per stream (`StreamHealth::qoe_score`)
//!   - a priority-ordering check across streams (`assert_priority_ordering`)

use pulsebeam_agent::str0m::media::Mid;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LayerClass {
    Full,
    Half,
    Quarter,
    /// A frame whose size doesn't resemble any of the three known CBR
    /// encodes closely enough to trust — e.g. a splice artifact.
    Unknown,
}

impl LayerClass {
    fn level(self) -> f64 {
        match self {
            LayerClass::Full => 3.0,
            LayerClass::Half => 2.0,
            LayerClass::Quarter => 1.0,
            LayerClass::Unknown => 0.0,
        }
    }
}

/// Each test asset carries exactly one keyframe per ~111s loop (see
/// `pulsebeam_agent::media::H264Looper`), and that keyframe is several times
/// larger than the layer's own delta frames (CBR padding equalizes delta
/// frames with each other, not with the keyframe). A single size centroid
/// per layer would therefore misclassify a keyframe from a low layer as
/// belonging to a higher one -- e.g. the half-layer keyframe is close in
/// size to the full layer's *delta* frames. Keeping separate keyframe and
/// delta-frame centroids avoids manufacturing a false "layer changed without
/// a keyframe" violation, or a false stability penalty, on every ordinary
/// loop wrap.
struct LayerReference {
    class: LayerClass,
    delta_bytes: f64,
    key_bytes: f64,
}

fn layer_references() -> &'static [LayerReference; 3] {
    static REFS: std::sync::OnceLock<[LayerReference; 3]> = std::sync::OnceLock::new();
    REFS.get_or_init(|| {
        let centroids = |data: &[u8]| {
            let sizes = pulsebeam_testdata::h264_frame_sizes(data);
            let key_bytes = *sizes.iter().max().expect("asset has at least one frame") as f64;
            let delta_sum: usize = sizes.iter().sum::<usize>() - key_bytes as usize;
            let delta_bytes = delta_sum as f64 / (sizes.len() - 1) as f64;
            (delta_bytes, key_bytes)
        };
        let (full_delta, full_key) = centroids(pulsebeam_testdata::RAW_H264_FULL_CBR);
        let (half_delta, half_key) = centroids(pulsebeam_testdata::RAW_H264_HALF_CBR);
        let (quarter_delta, quarter_key) = centroids(pulsebeam_testdata::RAW_H264_QUARTER_CBR);
        [
            LayerReference {
                class: LayerClass::Full,
                delta_bytes: full_delta,
                key_bytes: full_key,
            },
            LayerReference {
                class: LayerClass::Half,
                delta_bytes: half_delta,
                key_bytes: half_key,
            },
            LayerReference {
                class: LayerClass::Quarter,
                delta_bytes: quarter_delta,
                key_bytes: quarter_key,
            },
        ]
    })
}

/// Nearest-centroid classification against the three known CBR encodes. A
/// frame further than this (relative distance) from every centroid is
/// reported `Unknown` rather than forced into the closest bucket, since a
/// genuinely spliced/corrupt access unit often lands between two sizes.
const CLASSIFY_TOLERANCE: f64 = 0.45;

fn classify_layer(byte_len: usize, is_keyframe: bool) -> LayerClass {
    let refs = layer_references();
    let byte_len = byte_len as f64;
    let (best, best_dist) = refs
        .iter()
        .map(|r| {
            let reference = if is_keyframe { r.key_bytes } else { r.delta_bytes };
            (r.class, (byte_len - reference).abs() / reference)
        })
        .fold(
            (LayerClass::Unknown, f64::MAX),
            |acc, cur| if cur.1 < acc.1 { cur } else { acc },
        );
    if best_dist <= CLASSIFY_TOLERANCE {
        best
    } else {
        LayerClass::Unknown
    }
}

/// A gap between consecutive frames longer than this is a freeze, not
/// ordinary 30fps cadence jitter (nominal interval is ~33ms).
const FREEZE_THRESHOLD: Duration = Duration::from_millis(140);
const NOMINAL_FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Layer changes beyond this rate are flapping, not adaptation. Legitimate
/// bandwidth-driven re-tiering happens on the order of once per network
/// condition change, not continuously; existing bandwidth-step tests expect
/// on the order of one or two transitions total over tens of seconds.
const FLAPPING_TRANSITIONS_PER_MINUTE: f64 = 10.0;

/// Accumulates per-access-unit observations for a single downstream track
/// and turns them into a decodability verdict and a QoE score.
pub struct StreamHealth {
    label: String,
    frames_total: u64,
    keyframes_total: u64,
    violations: Vec<String>,
    prev_layer: Option<LayerClass>,
    prev_capture: Option<Instant>,
    first_capture: Option<Instant>,
    last_capture: Option<Instant>,
    prev_ts: Option<pulsebeam_agent::str0m::media::MediaTime>,
    layer_time: HashMap<LayerClass, Duration>,
    frozen_time: Duration,
    freeze_events: u64,
    transitions: u64,
}

impl StreamHealth {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            frames_total: 0,
            keyframes_total: 0,
            violations: Vec::new(),
            prev_layer: None,
            prev_capture: None,
            first_capture: None,
            last_capture: None,
            prev_ts: None,
            layer_time: HashMap::new(),
            frozen_time: Duration::ZERO,
            freeze_events: 0,
            transitions: 0,
        }
    }

    /// Feed one reassembled access unit (one `MediaFrame`) into the monitor.
    pub fn record(
        &mut self,
        ts: pulsebeam_agent::str0m::media::MediaTime,
        capture_time: Instant,
        data: &[u8],
    ) {
        let is_keyframe = pulsebeam_testdata::h264_frame_is_keyframe(data);
        let layer = classify_layer(data.len(), is_keyframe);

        if self.frames_total == 0 {
            self.first_capture = Some(capture_time);
            if !is_keyframe {
                self.violations.push(format!(
                    "{}: stream started with a non-keyframe access unit ({} bytes)",
                    self.label,
                    data.len()
                ));
            }
        } else {
            if let Some(prev_ts) = self.prev_ts {
                if ts < prev_ts {
                    self.violations.push(format!(
                        "{}: frame {} arrived with a timestamp earlier than the previous \
                         frame's ({:?} < {:?}) -- a decoder cannot play this back in order",
                        self.label,
                        self.frames_total + 1,
                        ts,
                        prev_ts
                    ));
                }
            }
            if let Some(prev_layer) = self.prev_layer {
                if prev_layer != layer {
                    self.transitions += 1;
                    if !is_keyframe {
                        self.violations.push(format!(
                            "{}: layer changed {prev_layer:?} -> {layer:?} at frame {} ({} \
                             bytes) without a keyframe -- this would desync a real decoder",
                            self.label,
                            self.frames_total + 1,
                            data.len()
                        ));
                    }
                }
            }
            if let Some(prev_capture) = self.prev_capture {
                let gap = capture_time.saturating_duration_since(prev_capture);
                let attributed_layer = self.prev_layer.unwrap_or(layer);
                *self.layer_time.entry(attributed_layer).or_default() += gap;
                if gap > FREEZE_THRESHOLD {
                    self.freeze_events += 1;
                    self.frozen_time += gap.saturating_sub(NOMINAL_FRAME_INTERVAL);
                }
            }
        }

        self.frames_total += 1;
        if is_keyframe {
            self.keyframes_total += 1;
        }
        self.prev_layer = Some(layer);
        self.prev_capture = Some(capture_time);
        self.last_capture = Some(capture_time);
        self.prev_ts = Some(ts);
    }

    pub fn total_duration(&self) -> Duration {
        match (self.first_capture, self.last_capture) {
            (Some(a), Some(b)) => b.saturating_duration_since(a),
            _ => Duration::ZERO,
        }
    }

    pub fn frames_total(&self) -> u64 {
        self.frames_total
    }

    /// A composite 0-100 quality score blending availability (time not
    /// frozen), quality (time-weighted layer level reached), and stability
    /// (freedom from repeated re-tiering). Decodability is intentionally
    /// *not* part of this score -- see `assert_decodable`, which is a
    /// separate, unconditional invariant rather than something a good score
    /// elsewhere can offset.
    pub fn qoe_score(&self) -> f64 {
        let total = self.total_duration().as_secs_f64();
        if total <= 0.0 {
            return 0.0;
        }

        let availability = 1.0 - (self.frozen_time.as_secs_f64() / total).clamp(0.0, 1.0);

        let tracked_seconds: f64 = self.layer_time.values().map(Duration::as_secs_f64).sum();
        let level_seconds: f64 = self
            .layer_time
            .iter()
            .map(|(class, dur)| class.level() * dur.as_secs_f64())
            .sum();
        let quality = if tracked_seconds > 0.0 {
            (level_seconds / tracked_seconds) / LayerClass::Full.level()
        } else {
            0.0
        };

        let minutes = (total / 60.0).max(1.0 / 60.0);
        let transitions_per_minute = self.transitions as f64 / minutes;
        let stability =
            1.0 - (transitions_per_minute / FLAPPING_TRANSITIONS_PER_MINUTE).clamp(0.0, 1.0);

        100.0 * (0.5 * availability + 0.3 * quality + 0.2 * stability)
    }

    /// The unconditional invariant: every access unit this receiver was
    /// handed must be safely decodable in sequence, whatever the QoE score
    /// says. A high score built on top of undetected corruption would be
    /// worse than useless.
    pub fn assert_decodable(&self) {
        assert!(
            self.violations.is_empty(),
            "{}: stream is not reliably decodable across switches ({} frames, {} keyframes):\n{}",
            self.label,
            self.frames_total,
            self.keyframes_total,
            self.violations.join("\n")
        );
    }

    pub fn assert_min_score(&self, floor: f64) {
        let score = self.qoe_score();
        assert!(
            score >= floor,
            "{}: QoE score {score:.1} fell below required floor {floor:.1} \
             (frames={}, keyframes={}, freezes={}, frozen={:?}, transitions={})",
            self.label,
            self.frames_total,
            self.keyframes_total,
            self.freeze_events,
            self.frozen_time,
            self.transitions
        );
    }
}

/// Verifies that, under contention, streams the viewer weighted higher
/// achieve at least as good a QoE score as streams weighted lower.
///
/// `entries` are `(label, weight, score)`. Weight is whatever priority knob
/// drove the subscription (e.g. requested `max_height`); a strictly higher
/// weight must not end up with a strictly worse score by more than `slack`,
/// which absorbs legitimate measurement noise (e.g. two streams landing on
/// the same layer with slightly different freeze counts) without masking a
/// genuine priority inversion (a low-weight stream sustainedly beating a
/// high-weight one).
pub fn assert_priority_ordering(entries: &[(&str, f64, f64)], slack: f64) {
    for a in entries {
        for b in entries {
            if a.1 > b.1 {
                assert!(
                    a.2 + slack >= b.2,
                    "priority inversion: {} (weight {}) scored {:.1}, worse than {} (weight {}) \
                     which scored {:.1} -- higher-weight subscriptions must not be starved in \
                     favor of lower-weight ones",
                    a.0,
                    a.1,
                    a.2,
                    b.0,
                    b.1,
                    b.2
                );
            }
        }
    }
}

pub type SharedStreamHealth = std::sync::Arc<std::sync::Mutex<StreamHealth>>;

pub fn new_shared(label: impl Into<String>) -> SharedStreamHealth {
    std::sync::Arc::new(std::sync::Mutex::new(StreamHealth::new(label)))
}

/// Convenience for tests: decodability + score across every tracked stream.
pub fn assert_all_decodable(health: &HashMap<Mid, SharedStreamHealth>) {
    for h in health.values() {
        h.lock().unwrap_or_else(|p| p.into_inner()).assert_decodable();
    }
}

pub fn scores(health: &HashMap<Mid, SharedStreamHealth>) -> HashMap<Mid, f64> {
    health
        .iter()
        .map(|(mid, h)| {
            (
                *mid,
                h.lock().unwrap_or_else(|p| p.into_inner()).qoe_score(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_agent::str0m::media::MediaTime;

    /// Replays an entire real asset loop (one keyframe, thousands of delta
    /// frames, at a fixed simulated 30fps) through `StreamHealth` exactly as
    /// the simulator's drain task would. This is the harness's own
    /// regression test: if classification or the keyframe-required-on-switch
    /// rule were wrong, this would false-positive on every simulation test
    /// that runs long enough to loop, since nothing here ever changes layer.
    fn replay_single_layer(data: &[u8], expected: LayerClass) -> StreamHealth {
        let frames = pulsebeam_testdata::h264_frames(data);
        let mut health = StreamHealth::new("test");
        let start = Instant::now();
        for (i, frame) in frames.iter().enumerate() {
            let ts = MediaTime::from_seconds(i as f64 / 30.0);
            let capture_time = start + Duration::from_secs_f64(i as f64 / 30.0);
            health.record(ts, capture_time, frame);
        }
        assert_eq!(health.frames_total() as usize, frames.len());
        assert_eq!(health.prev_layer, Some(expected));
        health
    }

    #[test]
    fn a_full_asset_loop_is_decodable_with_no_spurious_transitions() {
        for (data, expected) in [
            (pulsebeam_testdata::RAW_H264_FULL_CBR, LayerClass::Full),
            (pulsebeam_testdata::RAW_H264_HALF_CBR, LayerClass::Half),
            (pulsebeam_testdata::RAW_H264_QUARTER_CBR, LayerClass::Quarter),
        ] {
            let health = replay_single_layer(data, expected);
            health.assert_decodable();
            assert_eq!(
                health.transitions, 0,
                "a single unchanging layer must never register a transition, even across \
                 its own periodic keyframe"
            );
            assert_eq!(health.keyframes_total, 1);
        }
    }

    #[test]
    fn two_full_asset_loops_back_to_back_stay_decodable() {
        // Exercises the natural loop-wrap boundary itself (not just one
        // pass), since that's exactly where the keyframe-size outlier could
        // previously manufacture a false violation.
        let data = pulsebeam_testdata::RAW_H264_QUARTER_CBR;
        let frames = pulsebeam_testdata::h264_frames(data);
        let mut health = StreamHealth::new("test");
        let start = Instant::now();
        let mut i = 0usize;
        for _ in 0..2 {
            for frame in &frames {
                let ts = MediaTime::from_seconds(i as f64 / 30.0);
                let capture_time = start + Duration::from_secs_f64(i as f64 / 30.0);
                health.record(ts, capture_time, frame);
                i += 1;
            }
        }
        health.assert_decodable();
        assert_eq!(health.transitions, 0);
        assert_eq!(health.keyframes_total, 2);
    }

    #[test]
    fn a_switch_without_a_keyframe_is_flagged() {
        let full_frames = pulsebeam_testdata::h264_frames(pulsebeam_testdata::RAW_H264_FULL_CBR);
        let quarter_frames =
            pulsebeam_testdata::h264_frames(pulsebeam_testdata::RAW_H264_QUARTER_CBR);
        let mut health = StreamHealth::new("test");
        let start = Instant::now();
        // A few frames of full, then splice directly into a quarter delta
        // frame (not its keyframe) -- exactly the corruption this harness
        // exists to catch.
        for (i, frame) in full_frames.iter().take(5).enumerate() {
            let ts = MediaTime::from_seconds(i as f64 / 30.0);
            let capture_time = start + Duration::from_secs_f64(i as f64 / 30.0);
            health.record(ts, capture_time, frame);
        }
        let splice_frame = quarter_frames
            .iter()
            .find(|f| !pulsebeam_testdata::h264_frame_is_keyframe(f))
            .expect("asset has delta frames");
        health.record(MediaTime::from_seconds(5.0 / 30.0), start + Duration::from_secs_f64(5.0 / 30.0), splice_frame);

        assert!(
            !health.violations.is_empty(),
            "expected a decodability violation when splicing layers without a keyframe"
        );
    }

    #[test]
    fn priority_ordering_flags_an_inversion() {
        let ok = std::panic::catch_unwind(|| {
            assert_priority_ordering(&[("high", 2.0, 80.0), ("low", 1.0, 60.0)], 1.0);
        });
        assert!(ok.is_ok(), "correctly ordered scores must not panic");

        let inverted = std::panic::catch_unwind(|| {
            assert_priority_ordering(&[("high", 2.0, 40.0), ("low", 1.0, 90.0)], 1.0);
        });
        assert!(
            inverted.is_err(),
            "a lower-weight stream sustainedly beating a higher-weight one must be flagged"
        );
    }
}

#![allow(
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::string_slice,
    clippy::indexing_slicing
)] // test / simulation support
//! Test-only observation points for the simulator.
//!
//! The simulator runs the SFU in-process under turmoil, so a task-local sink is enough to get
//! internal state out to an assertion without threading a handle through every layer.
//!
//! The sink is thread-local rather than process-global because `cargo test` runs test functions
//! in parallel while turmoil drives each simulation on its own thread. A process-global would let
//! plans observe each other's participants - a plan making 303 allocation passes would see 665,
//! with another plan's estimates folded into its minimum - which is both wrong and flaky.
//!
//! This exists because the interesting congestion-control failures are not visible in received
//! byte counts. A bandwidth estimate can be poisoned - pulled far below what the link carries -
//! while throughput still looks acceptable, because the allocator simply picks a lower simulcast
//! layer and the viewer keeps receiving *something*. Asserting on bytes alone lets that regress
//! silently; asserting on the estimate catches it directly.
//!
//! Compiled only under the `sim` feature.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

/// Downstream bandwidth estimate observations, since the last [`reset`].
#[derive(Debug, Default, Clone)]
struct Samples {
    min_bwe_bps: Option<u64>,
    max_bwe_bps: Option<u64>,
    last_bwe_bps: Option<u64>,
    /// Number of allocation passes observed. Distinguishes "estimate stayed high" from
    /// "nothing was ever recorded", which would otherwise both satisfy a minimum.
    count: u64,
    /// Media payload bytes the SFU handed to str0m for forwarding, keyed by the *subscriber*
    /// receiving them.
    ///
    /// Per subscriber because the only use is comparing it against what one participant received.
    /// A single total is not merely imprecise with several subscribers, it is meaningless: a
    /// two-viewer plan measured 1024% "efficiency", being everyone's forwarded bytes over one
    /// viewer's received bytes.
    forwarded_media_bytes: HashMap<String, u64>,
    /// Last-seen forwarded quality per track origin (the publisher's participant id, as a
    /// string), across every subscriber and slot forwarding from that origin. 0 = paused, else
    /// [`pulsebeam_core::simulcast::LayerQuality`] as its numeric rank (Low=1 .. High=3).
    ///
    /// Keyed by origin rather than by (subscriber, slot) because the failure this exists to
    /// catch - a stream regressing even as bandwidth grows - is about the *publisher's* track,
    /// and a test only needs "what is this origin's viewer(s) currently getting" to check it.
    forwarded_quality: HashMap<String, u8>,
    /// Highest forwarded quality observed for each track origin since the last reset.
    max_forwarded_quality: HashMap<String, u8>,
    /// Media frames that actually crossed a shard boundary, counted where they
    /// are resolved.
    ///
    /// A cross-shard test cannot assert this from received bytes: a room whose
    /// participants happen to be co-located delivers exactly the same video
    /// while reaching none of the route, envelope or restamping code the test
    /// exists for. Placement is a hash, so that co-location is luck, and
    /// without this the test passes either way.
    cross_shard_media_frames: u64,
    /// How many times each origin's forwarded layer changed since the last reset.
    ///
    /// Switching layer is not free: the receiver needs a keyframe and the picture stutters. A
    /// stream that changes several times a second never settles into a decodable run at all, so
    /// the viewer sees nothing rather than something imperfect - which is how this presents in
    /// production, and why counting the changes catches it where checking the final layer does
    /// not. A settled stream changes a handful of times over a minute.
    quality_changes: HashMap<String, u64>,
    /// How many times each origin's forwarded layer *reversed direction* since the last reset.
    ///
    /// Climbing q -> h -> f as bandwidth is discovered is correct behaviour, so counting raw
    /// transitions cannot tell a healthy ramp from a stream oscillating between two layers. A
    /// reversal - an upgrade after a downgrade, or a downgrade after an upgrade - is the thing that
    /// is never desirable, and it is just as wrong during the cold-start ramp as it is in steady
    /// state, which is where a viewer notices it most.
    quality_reversals: HashMap<String, u64>,
    /// Direction of each origin's last forwarded-layer change: `true` for an upgrade.
    last_quality_direction: HashMap<String, bool>,
    /// Every downstream estimate as `(elapsed, estimate_bps, desired_bps)`, keyed by the
    /// *subscriber's* participant id.
    ///
    /// Min/max/last collapse a trace into three numbers, which cannot distinguish an estimate
    /// that settled at capacity from one that touched it once and fell away, nor an estimate
    /// that ramped in two seconds from one that took thirty. Both distinctions are the
    /// difference between working and broken congestion control, so the whole series is kept.
    ///
    /// Keyed per subscriber because capacity is configured per link: an assertion comparing an
    /// estimate against its link's capacity is meaningless if two subscribers' traces are mixed.
    ///
    /// Demand is kept alongside the estimate because most interesting claims are relative to it.
    /// An estimate far below link capacity is correct when the application is only asking for a
    /// fraction of it - a delay-based estimator cannot measure bandwidth it is not using - so
    /// "tracks capacity" is only a fair test under saturation. "Reached the lesser of what was
    /// wanted and what the link had" is the claim that holds in both regimes.
    bwe_series: HashMap<String, Vec<(Duration, u64, u64)>>,
    /// Reference point for series timestamps, set on the first sample after a reset.
    series_origin: Option<Instant>,
}

thread_local! {
    static SAMPLES: RefCell<Samples> = RefCell::new(Samples::default());
}

/// Record one downstream allocation pass. Called from the allocator's reporting path.
///
/// `subscriber` is the participant whose downstream link this estimate describes.
pub fn record_downstream_bwe(subscriber: &str, bwe_bps: u64, desired_bps: u64) {
    let now = Instant::now();
    SAMPLES.with_borrow_mut(|s| {
        let origin = *s.series_origin.get_or_insert(now);
        s.bwe_series
            .entry(subscriber.to_string())
            .or_default()
            .push((now.saturating_duration_since(origin), bwe_bps, desired_bps));
        s.min_bwe_bps = Some(match s.min_bwe_bps {
            Some(m) => m.min(bwe_bps),
            None => bwe_bps,
        });
        s.max_bwe_bps = Some(match s.max_bwe_bps {
            Some(m) => m.max(bwe_bps),
            None => bwe_bps,
        });
        s.last_bwe_bps = Some(bwe_bps);
        s.count += 1;
    });
}

/// One media frame arrived from another shard and resolved to a live route.
pub fn record_cross_shard_media() {
    SAMPLES.with_borrow_mut(|s| s.cross_shard_media_frames += 1);
}

pub fn cross_shard_media_frames() -> u64 {
    SAMPLES.with_borrow(|s| s.cross_shard_media_frames)
}

/// Clear observations. The harness calls this at the start of each timed step so assertions
/// describe the window just run, matching the byte-counter semantics.
pub fn reset() {
    SAMPLES.with_borrow_mut(|s| *s = Samples::default());
}

/// Record media payload handed to str0m for forwarding.
///
/// Everything else that reaches the subscriber - RTX, padding, probe bursts, RTCP - is generated
/// below this point, so comparing this against what the subscriber actually received measures how
/// much of the link carried video rather than overhead. That is the quantity Chrome reports as
/// `retransmittedBytesReceived` against `bytesReceived`, and a capture showing 54% of payload
/// being retransmission with zero packet loss is the failure this exists to catch.
pub fn record_forwarded_media(subscriber: &str, bytes: u64) {
    SAMPLES.with_borrow_mut(|s| {
        *s.forwarded_media_bytes
            .entry(subscriber.to_string())
            .or_default() += bytes;
    });
}

/// Media payload forwarded since [`reset`].
pub fn forwarded_media_bytes(subscriber: &str) -> u64 {
    SAMPLES.with_borrow(|s| {
        s.forwarded_media_bytes
            .get(subscriber)
            .copied()
            .unwrap_or(0)
    })
}

/// Record the layer currently forwarded to a subscriber from `origin`'s track. `None` means the
/// slot is paused. Called from every allocation pass, not only ones that change anything, so the
/// last value always reflects the current steady state.
pub fn record_forwarded_quality(origin: &str, quality: Option<u8>) {
    let rank = quality.unwrap_or(0);
    SAMPLES.with_borrow_mut(|s| {
        // Count transitions, not passes: this is called every pass, changed or not.
        if let Some(previous) = s.forwarded_quality.get(origin).copied()
            && previous != rank
        {
            *s.quality_changes.entry(origin.to_string()).or_default() += 1;

            let up = rank > previous;
            if let Some(was_up) = s.last_quality_direction.insert(origin.to_string(), up)
                && was_up != up
            {
                *s.quality_reversals.entry(origin.to_string()).or_default() += 1;
            }
        }
        s.forwarded_quality.insert(origin.to_string(), rank);
        s.max_forwarded_quality
            .entry(origin.to_string())
            .and_modify(|max| *max = (*max).max(rank))
            .or_insert(rank);
    });
}

/// Last-seen forwarded quality rank for `origin` since [`reset`]. `None` if nothing was ever
/// recorded for that origin (never subscribed, or reset before the first pass); `Some(0)` means
/// paused.
pub fn forwarded_quality(origin: &str) -> Option<u8> {
    SAMPLES.with_borrow(|s| s.forwarded_quality.get(origin).copied())
}

/// How many times `origin`'s forwarded layer changed since [`reset`].
pub fn quality_changes(origin: &str) -> u64 {
    SAMPLES.with_borrow(|s| s.quality_changes.get(origin).copied().unwrap_or(0))
}

/// How many times `origin`'s forwarded layer reversed direction since [`reset`].
///
/// A monotonic ramp reports zero however many layers it climbs through.
pub fn quality_reversals(origin: &str) -> u64 {
    SAMPLES.with_borrow(|s| s.quality_reversals.get(origin).copied().unwrap_or(0))
}

/// Highest forwarded quality rank observed for `origin` since [`reset`]. `None` means no
/// allocation pass recorded that origin during the window; `Some(0)` means it was only paused.
pub fn max_forwarded_quality(origin: &str) -> Option<u8> {
    SAMPLES.with_borrow(|s| s.max_forwarded_quality.get(origin).copied())
}

/// Downstream estimate summary since [`reset`]: `(min, max, last, sample_count)`.
///
/// Returns `None` when nothing was recorded, so a test can tell an untested path from a healthy
/// one rather than vacuously passing. The spread matters as much as the minimum: an estimate
/// pinned at one value across hundreds of passes is a different failure from one that dips.
pub fn downstream_bwe_summary() -> Option<(u64, u64, u64, u64)> {
    SAMPLES.with_borrow(|s| Some((s.min_bwe_bps?, s.max_bwe_bps?, s.last_bwe_bps?, s.count)))
}

/// Full estimate trace for one subscriber since [`reset`], as `(elapsed, estimate, desired)`.
///
/// Empty when that subscriber ran no allocation passes in the window, which a caller should
/// treat as "untested" rather than "passed".
pub fn bwe_series(subscriber: &str) -> Vec<(Duration, u64, u64)> {
    SAMPLES.with_borrow(|s| s.bwe_series.get(subscriber).cloned().unwrap_or_default())
}

/// Subscribers that recorded at least one estimate since [`reset`].
pub fn bwe_subscribers() -> Vec<String> {
    SAMPLES.with_borrow(|s| s.bwe_series.keys().cloned().collect())
}

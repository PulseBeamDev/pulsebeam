#![allow(
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::string_slice,
    clippy::indexing_slicing
)] // test / simulation support
//! Internal diagnostics and fault controls for deterministic simulation.
//!
//! Adaptation and forwarding samples are diagnostic, not correctness oracles. Application-facing
//! tests use public SDK observations and native decoded output. Fault-consumption evidence
//! qualifies injected input, independently of SUT correctness.
//!
//! Thread-local storage isolates parallel simulations driven on separate threads.
//! Compiled only under the `sim` feature.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;
use tokio::{sync::oneshot, time::Instant};

use crate::entity::ParticipantId;

/// Downstream bandwidth estimate observations, since the last [`reset`].
#[derive(Debug, Default, Clone)]
struct Samples {
    /// SFU media payload bytes per subscriber, excluding transport-generated overhead.
    forwarded_media_bytes: HashMap<ParticipantId, u64>,
    /// Last forwarding rank per publisher, aggregated across subscribers for diagnostics.
    /// Zero is paused; other ranks use [`pulsebeam_core::simulcast::LayerQuality`].
    forwarded_quality: HashMap<ParticipantId, u8>,
    /// How many times each origin's forwarded layer changed since the last reset.
    ///
    /// Switching layer is not free: the receiver needs a keyframe and the picture stutters. A
    /// stream that changes several times a second never settles into a decodable run at all, so
    /// the viewer sees nothing rather than something imperfect - which is how this presents in
    /// production, and why counting the changes catches it where checking the final layer does
    /// not. A settled stream changes a handful of times over a minute.
    quality_changes: HashMap<ParticipantId, u64>,
    /// How many times each origin's forwarded layer *reversed direction* since the last reset.
    ///
    /// Climbing q -> h -> f as bandwidth is discovered is correct behaviour, so counting raw
    /// transitions cannot tell a healthy ramp from a stream oscillating between two layers. A
    /// reversal identifies oscillation when budget and intent are stable. Budget-driven
    /// suspension and resumption can legitimately reverse an optional stream's direction.
    quality_reversals: HashMap<ParticipantId, u64>,
    routing_counters: HashMap<String, u64>,
    /// Direction of each origin's last forwarded-layer change: `true` for an upgrade.
    last_quality_direction: HashMap<ParticipantId, bool>,
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
    bwe_series: HashMap<ParticipantId, Vec<(Duration, u64, u64)>>,
    /// Reference point for series timestamps, set on the first sample after a reset.
    series_origin: Option<Instant>,
}

// Fault requests cross hosts; their deadline uses the process-wide shimmed clock.
struct ControllerStall {
    until: std::time::Instant,
    begun: oneshot::Sender<()>,
}

thread_local! {
    static SAMPLES: RefCell<Samples> = RefCell::new(Samples::default());
    static CONTROLLER_STALL: RefCell<Option<ControllerStall>> = const { RefCell::new(None) };
    static FAIL_NEXT_MATERIALIZATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn fail_next_materialization() {
    FAIL_NEXT_MATERIALIZATION.with(|fail| fail.set(true));
}

pub fn take_materialization_failure() -> bool {
    FAIL_NEXT_MATERIALIZATION.with(std::cell::Cell::take)
}

pub fn request_controller_stall(duration: Duration) -> (std::time::Instant, oneshot::Receiver<()>) {
    let until = std::time::Instant::now()
        .checked_add(duration)
        .unwrap_or_else(std::time::Instant::now);
    let (begun, receipt) = oneshot::channel();
    CONTROLLER_STALL.with_borrow_mut(|pending| {
        assert!(pending.replace(ControllerStall { until, begun }).is_none());
    });
    (until, receipt)
}

pub async fn wait_controller_stall() {
    let request = CONTROLLER_STALL.with_borrow_mut(Option::take);
    if let Some(ControllerStall { until, begun }) = request {
        let _ = begun.send(());
        tokio::time::sleep(until.saturating_duration_since(std::time::Instant::now())).await;
    }
}

/// Record one downstream allocation pass. Called from the allocator's reporting path.
///
/// `subscriber` is the participant whose downstream link this estimate describes.
pub fn record_downstream_bwe_for(subscriber: ParticipantId, bwe_bps: u64, desired_bps: u64) {
    let now = Instant::now();
    SAMPLES.with_borrow_mut(|s| {
        let origin = *s.series_origin.get_or_insert(now);
        s.bwe_series.entry(subscriber).or_default().push((
            now.saturating_duration_since(origin),
            bwe_bps,
            desired_bps,
        ));
    });
}

/// Record a named diagnostic or fault-consumption event.
pub fn record_routing_counter(name: &'static str) {
    SAMPLES.with_borrow_mut(|s| {
        *s.routing_counters.entry(name.to_owned()).or_default() += 1;
    });
}

pub fn record_routing_work(name: &'static str, amount: usize) {
    SAMPLES.with_borrow_mut(|s| {
        *s.routing_counters.entry(name.to_owned()).or_default() += amount as u64;
    });
}

pub fn record_routing_drop(lane: &'static str, stage: &'static str, origin: &'static str) {
    SAMPLES.with_borrow_mut(|s| {
        let key = format!("routing_drop:{lane}:{stage}:{origin}");
        *s.routing_counters.entry(key).or_default() += 1;
    });
}

pub fn routing_counter(name: &'static str) -> u64 {
    SAMPLES.with_borrow(|s| s.routing_counters.get(name).copied().unwrap_or(0))
}

/// Start a new diagnostic measurement window.
pub fn reset() {
    SAMPLES.with_borrow_mut(|s| *s = Samples::default());
}

/// Record diagnostic media payload bytes handed to str0m for forwarding.
pub fn record_forwarded_media_for(subscriber: ParticipantId, bytes: u64) {
    SAMPLES.with_borrow_mut(|s| {
        *s.forwarded_media_bytes.entry(subscriber).or_default() += bytes;
    });
}

/// Media payload forwarded since [`reset`].
pub fn forwarded_media_bytes(subscriber: &str) -> u64 {
    let Ok(subscriber) = subscriber.parse() else {
        return 0;
    };
    forwarded_media_bytes_for(subscriber)
}

pub fn forwarded_media_bytes_for(subscriber: ParticipantId) -> u64 {
    SAMPLES.with_borrow(|s| {
        s.forwarded_media_bytes
            .get(&subscriber)
            .copied()
            .unwrap_or(0)
    })
}

/// Record the layer currently forwarded to a subscriber from `origin`'s track. `None` means the
/// slot is paused. Called from every allocation pass, not only ones that change anything, so the
/// last value always reflects the current steady state.
pub fn record_forwarded_quality_for(origin: ParticipantId, quality: Option<u8>) {
    let rank = quality.unwrap_or(0);
    SAMPLES.with_borrow_mut(|s| {
        // Count transitions, not passes: this is called every pass, changed or not.
        if let Some(previous) = s.forwarded_quality.get(&origin).copied()
            && previous != rank
        {
            tracing::debug!(%origin, previous, rank, "forwarded quality transition");
            *s.quality_changes.entry(origin).or_default() += 1;

            let up = rank > previous;
            if let Some(was_up) = s.last_quality_direction.insert(origin, up)
                && was_up != up
            {
                *s.quality_reversals.entry(origin).or_default() += 1;
            }
        }
        s.forwarded_quality.insert(origin, rank);
    });
}

/// Last-seen forwarded quality rank for `origin` since [`reset`]. `None` if nothing was ever
/// recorded for that origin (never subscribed, or reset before the first pass); `Some(0)` means
/// paused.
pub fn forwarded_quality(origin: &str) -> Option<u8> {
    let origin = origin.parse().ok()?;
    forwarded_quality_for(origin)
}

pub fn forwarded_quality_for(origin: ParticipantId) -> Option<u8> {
    SAMPLES.with_borrow(|s| s.forwarded_quality.get(&origin).copied())
}

/// How many times `origin`'s forwarded layer changed since [`reset`].
pub fn quality_changes(origin: &str) -> u64 {
    let Ok(origin) = origin.parse() else {
        return 0;
    };
    quality_changes_for(origin)
}

pub fn quality_changes_for(origin: ParticipantId) -> u64 {
    SAMPLES.with_borrow(|s| s.quality_changes.get(&origin).copied().unwrap_or(0))
}

/// How many times `origin`'s forwarded layer reversed direction since [`reset`].
///
/// A monotonic ramp reports zero however many layers it climbs through.
pub fn quality_reversals(origin: &str) -> u64 {
    let Ok(origin) = origin.parse() else {
        return 0;
    };
    quality_reversals_for(origin)
}

pub fn quality_reversals_for(origin: ParticipantId) -> u64 {
    SAMPLES.with_borrow(|s| s.quality_reversals.get(&origin).copied().unwrap_or(0))
}

/// Diagnostic estimate trace since [`reset`], as `(elapsed, estimate, desired)`.
/// Empty when no allocation pass recorded this subscriber in the window.
pub fn bwe_series(subscriber: &str) -> Vec<(Duration, u64, u64)> {
    let Ok(subscriber) = subscriber.parse() else {
        return Vec::new();
    };
    bwe_series_for(subscriber)
}

pub fn bwe_series_for(subscriber: ParticipantId) -> Vec<(Duration, u64, u64)> {
    SAMPLES.with_borrow(|s| s.bwe_series.get(&subscriber).cloned().unwrap_or_default())
}

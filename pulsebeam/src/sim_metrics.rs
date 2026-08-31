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

use crate::entity::ParticipantId;

const MAX_FORWARDING_LATENCY_SAMPLES: usize = 1_048_576;
const MAX_EXPECTED_MEDIA: usize = 262_144;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ForwardingLatencySample {
    pub service: Duration,
    pub pacing: Duration,
    pub egress_lateness: Duration,
    pub total: Duration,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ExpectedVideoFrame {
    pub origin: ParticipantId,
    pub source_timestamp: u64,
    pub height: u32,
    pub packet_count: u32,
    pub complete: bool,
    window: u64,
    decoded: bool,
    completed_at: Option<std::time::Instant>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ExpectedAudioPacket {
    pub origin: ParticipantId,
    pub source_timestamp: u64,
}

/// Downstream bandwidth estimate observations, since the last [`reset`].
#[derive(Debug, Default, Clone)]
struct Samples {
    window: u64,
    min_bwe_bps: Option<u64>,
    max_bwe_bps: Option<u64>,
    last_bwe_bps: Option<u64>,
    /// Number of allocation passes observed. Distinguishes "estimate stayed high" from
    /// "nothing was ever recorded", which would otherwise both satisfy a minimum.
    count: u64,
    /// Media payload bytes the SFU handed to transport forwarding, keyed by the *subscriber*
    /// receiving them.
    ///
    /// Per subscriber because the only use is comparing it against what one participant received.
    /// A single total is not merely imprecise with several subscribers, it is meaningless: a
    /// two-viewer plan measured 1024% "efficiency", being everyone's forwarded bytes over one
    /// viewer's received bytes.
    forwarded_media_bytes: HashMap<ParticipantId, u64>,
    /// Last-seen forwarded quality per track origin (the publisher's participant id, as a
    /// string), across every subscriber and slot forwarding from that origin. 0 = paused, else
    /// [`pulsebeam_core::simulcast::LayerQuality`] as its numeric rank (Low=1 .. High=3).
    ///
    /// Keyed by origin rather than by (subscriber, slot) because the failure this exists to
    /// catch - a stream regressing even as bandwidth grows - is about the *publisher's* track,
    /// and a test only needs "what is this origin's viewer(s) currently getting" to check it.
    forwarded_quality: HashMap<ParticipantId, u8>,
    /// Highest forwarded quality observed for each track origin since the last reset.
    max_forwarded_quality: HashMap<ParticipantId, u8>,
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
    quality_changes: HashMap<ParticipantId, u64>,
    /// How many times each origin's forwarded layer *reversed direction* since the last reset.
    ///
    /// Climbing q -> h -> f as bandwidth is discovered is correct behaviour, so counting raw
    /// transitions cannot tell a healthy ramp from a stream oscillating between two layers. A
    /// reversal - an upgrade after a downgrade, or a downgrade after an upgrade - is the thing that
    /// is never desirable, and it is just as wrong during the cold-start ramp as it is in steady
    /// state, which is where a viewer notices it most.
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
    forwarding_latency: Vec<ForwardingLatencySample>,
    expected_video: HashMap<(ParticipantId, ParticipantId, u64), ExpectedVideoFrame>,
    expected_audio: HashMap<(ParticipantId, u32, u64), ExpectedAudioPacket>,
    quality_sources: HashMap<ParticipantId, u8>,
}

thread_local! {
    static SAMPLES: RefCell<Samples> = RefCell::new(Samples::default());
    static CONTROLLER_STALL_UNTIL: RefCell<Option<Instant>> = const { RefCell::new(None) };
    static FAIL_NEXT_MATERIALIZATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn fail_next_materialization() {
    FAIL_NEXT_MATERIALIZATION.with(|fail| fail.set(true));
}

pub fn register_quality_source(origin: ParticipantId, source: u8) {
    debug_assert!(source < 2, "quality corpus source is declared");
    SAMPLES.with_borrow_mut(|samples| {
        samples.quality_sources.insert(origin, source);
    });
}

pub fn quality_source(origin: ParticipantId) -> Option<u8> {
    SAMPLES.with_borrow(|samples| samples.quality_sources.get(&origin).copied())
}

pub fn record_expected_video(
    subscriber: ParticipantId,
    origin: ParticipantId,
    output_timestamp: u64,
    source_timestamp: u64,
    height: u32,
    marker: bool,
) {
    SAMPLES.with_borrow_mut(|samples| {
        let window = samples.window;
        if samples.expected_video.len() >= MAX_EXPECTED_MEDIA
            && !samples
                .expected_video
                .contains_key(&(subscriber, origin, output_timestamp))
        {
            debug_assert!(false, "expected video timeline exceeded its bound");
            return;
        }
        let frame = samples
            .expected_video
            .entry((subscriber, origin, output_timestamp))
            .or_insert(ExpectedVideoFrame {
                origin,
                source_timestamp,
                height,
                packet_count: 0,
                complete: false,
                window,
                decoded: false,
                completed_at: None,
            });
        debug_assert_eq!(frame.origin, origin);
        debug_assert_eq!(frame.source_timestamp, source_timestamp);
        debug_assert_eq!(frame.height, height);
        frame.packet_count = frame.packet_count.saturating_add(1);
        frame.complete |= marker;
        if marker {
            frame.completed_at = Some(std::time::Instant::now());
        }
    });
}

pub fn record_decoded_video(
    subscriber: ParticipantId,
    origin: ParticipantId,
    output_timestamp: u64,
) {
    SAMPLES.with_borrow_mut(|samples| {
        let Some(frame) = samples
            .expected_video
            .get_mut(&(subscriber, origin, output_timestamp))
        else {
            return;
        };
        debug_assert!(frame.complete, "only complete expected frames can decode");
        frame.decoded = true;
    });
}

pub fn expected_video_progress(subscriber: ParticipantId, settlement: Duration) -> (u64, u64) {
    SAMPLES.with_borrow(|samples| {
        let now = std::time::Instant::now();
        samples
            .expected_video
            .iter()
            .filter(|((expected_subscriber, _, _), frame)| {
                *expected_subscriber == subscriber
                    && frame.window == samples.window
                    && frame.complete
                    && frame.completed_at.is_some_and(|completed| {
                        now.saturating_duration_since(completed) >= settlement
                    })
            })
            .fold((0u64, 0u64), |(expected, decoded), (_, frame)| {
                (
                    expected.saturating_add(1),
                    decoded.saturating_add(u64::from(frame.decoded)),
                )
            })
    })
}

pub fn expected_video(
    subscriber: ParticipantId,
    origin: ParticipantId,
    output_timestamp: u64,
) -> Option<ExpectedVideoFrame> {
    SAMPLES.with_borrow(|samples| {
        samples
            .expected_video
            .get(&(subscriber, origin, output_timestamp))
            .copied()
    })
}

pub fn record_expected_audio(
    subscriber: ParticipantId,
    ssrc: u32,
    origin: ParticipantId,
    output_timestamp: u64,
    source_timestamp: u64,
) {
    SAMPLES.with_borrow_mut(|samples| {
        if samples.expected_audio.len() >= MAX_EXPECTED_MEDIA {
            debug_assert!(
                samples
                    .expected_audio
                    .contains_key(&(subscriber, ssrc, output_timestamp)),
                "expected audio timeline exceeded its bound"
            );
        }
        if samples.expected_audio.len() < MAX_EXPECTED_MEDIA
            || samples
                .expected_audio
                .contains_key(&(subscriber, ssrc, output_timestamp))
        {
            let previous = samples.expected_audio.insert(
                (subscriber, ssrc, output_timestamp),
                ExpectedAudioPacket {
                    origin,
                    source_timestamp,
                },
            );
            debug_assert!(
                previous.is_none_or(|previous| {
                    previous.origin == origin && previous.source_timestamp == source_timestamp
                }),
                "audio output timestamp collision: subscriber={subscriber:?} ssrc={ssrc} origin={origin:?} output={output_timestamp} previous={previous:?} source={source_timestamp}"
            );
        }
    });
}

pub fn expected_audio(
    subscriber: ParticipantId,
    ssrc: u32,
    output_timestamp: u64,
) -> Option<ExpectedAudioPacket> {
    SAMPLES.with_borrow(|samples| {
        samples
            .expected_audio
            .get(&(subscriber, ssrc, output_timestamp))
            .copied()
    })
}

pub fn take_materialization_failure() -> bool {
    FAIL_NEXT_MATERIALIZATION.with(std::cell::Cell::take)
}

pub fn request_controller_stall(duration: Duration) {
    let until = Instant::now()
        .checked_add(duration)
        .unwrap_or_else(Instant::now);
    CONTROLLER_STALL_UNTIL.with_borrow_mut(|deadline| *deadline = Some(until));
}

pub async fn wait_controller_stall() {
    let deadline = CONTROLLER_STALL_UNTIL.with_borrow_mut(Option::take);
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    }
}

/// Record one downstream allocation pass. Called from the allocator's reporting path.
///
/// `subscriber` is the participant whose downstream link this estimate describes.
pub fn record_downstream_bwe(subscriber: &str, bwe_bps: u64, desired_bps: u64) {
    let Ok(subscriber) = subscriber.parse() else {
        return;
    };
    record_downstream_bwe_for(subscriber, bwe_bps, desired_bps);
}

pub fn record_downstream_bwe_for(subscriber: ParticipantId, bwe_bps: u64, desired_bps: u64) {
    let now = Instant::now();
    SAMPLES.with_borrow_mut(|s| {
        let origin = *s.series_origin.get_or_insert(now);
        s.bwe_series.entry(subscriber).or_default().push((
            now.saturating_duration_since(origin),
            bwe_bps,
            desired_bps,
        ));
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

pub fn routing_work(name: &'static str) -> u64 {
    routing_counter(name)
}

pub fn routing_drop(lane: &'static str, stage: &'static str, origin: &'static str) -> u64 {
    let key = format!("routing_drop:{lane}:{stage}:{origin}");
    SAMPLES.with_borrow(|s| s.routing_counters.get(&key).copied().unwrap_or(0))
}

/// Clear observations. The harness calls this at the start of each timed step so assertions
/// describe the window just run, matching the byte-counter semantics.
pub fn reset() {
    SAMPLES.with_borrow_mut(|samples| {
        let expected_video = std::mem::take(&mut samples.expected_video);
        let expected_audio = std::mem::take(&mut samples.expected_audio);
        let quality_sources = std::mem::take(&mut samples.quality_sources);
        let window = samples.window.wrapping_add(1);
        *samples = Samples {
            window,
            expected_video,
            expected_audio,
            quality_sources,
            ..Samples::default()
        };
    });
}

pub fn record_forwarding_latency(
    service: Duration,
    pacing: Duration,
    egress_lateness: Duration,
    total: Duration,
) {
    debug_assert_eq!(
        total,
        service
            .saturating_add(pacing)
            .saturating_add(egress_lateness),
        "forwarding latency sample must exactly decompose its total"
    );
    SAMPLES.with_borrow_mut(|s| {
        debug_assert!(
            s.forwarding_latency.len() < MAX_FORWARDING_LATENCY_SAMPLES,
            "one simulation assertion window exceeded its forwarding latency sample bound"
        );
        if s.forwarding_latency.len() < MAX_FORWARDING_LATENCY_SAMPLES {
            s.forwarding_latency.push(ForwardingLatencySample {
                service,
                pacing,
                egress_lateness,
                total,
            });
        }
    });
}

pub fn forwarding_latency_samples() -> Vec<ForwardingLatencySample> {
    SAMPLES.with_borrow(|s| s.forwarding_latency.clone())
}

/// Record media payload handed to transport forwarding.
///
/// Everything else that reaches the subscriber - RTX, padding, probe bursts, RTCP - is generated
/// below this point, so comparing this against what the subscriber actually received measures how
/// much of the link carried video rather than overhead. That is the quantity Chrome reports as
/// `retransmittedBytesReceived` against `bytesReceived`, and a capture showing 54% of payload
/// being retransmission with zero packet loss is the failure this exists to catch.
pub fn record_forwarded_media(subscriber: &str, bytes: u64) {
    let Ok(subscriber) = subscriber.parse() else {
        return;
    };
    record_forwarded_media_for(subscriber, bytes);
}

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
pub fn record_forwarded_quality(origin: &str, quality: Option<u8>) {
    let Ok(origin) = origin.parse() else {
        return;
    };
    record_forwarded_quality_for(origin, quality);
}

pub fn record_forwarded_quality_for(origin: ParticipantId, quality: Option<u8>) {
    let rank = quality.unwrap_or(0);
    SAMPLES.with_borrow_mut(|s| {
        // Count transitions, not passes: this is called every pass, changed or not.
        if let Some(previous) = s.forwarded_quality.get(&origin).copied()
            && previous != rank
        {
            *s.quality_changes.entry(origin).or_default() += 1;

            let up = rank > previous;
            if let Some(was_up) = s.last_quality_direction.insert(origin, up)
                && was_up != up
            {
                *s.quality_reversals.entry(origin).or_default() += 1;
            }
        }
        s.forwarded_quality.insert(origin, rank);
        s.max_forwarded_quality
            .entry(origin)
            .and_modify(|max| *max = (*max).max(rank))
            .or_insert(rank);
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

/// Highest forwarded quality rank observed for `origin` since [`reset`]. `None` means no
/// allocation pass recorded that origin during the window; `Some(0)` means it was only paused.
pub fn max_forwarded_quality(origin: &str) -> Option<u8> {
    let origin = origin.parse().ok()?;
    max_forwarded_quality_for(origin)
}

pub fn max_forwarded_quality_for(origin: ParticipantId) -> Option<u8> {
    SAMPLES.with_borrow(|s| s.max_forwarded_quality.get(&origin).copied())
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
    let Ok(subscriber) = subscriber.parse() else {
        return Vec::new();
    };
    bwe_series_for(subscriber)
}

pub fn bwe_series_for(subscriber: ParticipantId) -> Vec<(Duration, u64, u64)> {
    SAMPLES.with_borrow(|s| s.bwe_series.get(&subscriber).cloned().unwrap_or_default())
}

/// Subscribers that recorded at least one estimate since [`reset`].
pub fn bwe_subscribers() -> Vec<String> {
    SAMPLES.with_borrow(|s| s.bwe_series.keys().map(ToString::to_string).collect())
}

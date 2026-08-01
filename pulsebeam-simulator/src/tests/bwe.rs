//! End-to-end bandwidth-estimation behaviour.
//!
//! These exercise the interaction between pulsebeam's allocator and str0m's BWE, which unit
//! tests on either side cannot cover: `desired` is computed by pulsebeam and consumed by str0m's
//! probe controller, and the failure modes only appear once both are in the loop.

use super::common::{LocalNodeSim, Participant, Room, Step, VideoQuality};
use std::time::Duration;

/// Upgrading after a long stretch at low quality must not break the stream.
///
/// pulsebeam derives `desired` from `stable_bitrate_bps`, an EWMA over the *measured* rate with
/// a 30s fall constant and no lower bound, and only counts layers that are currently healthy and
/// flowing. While a subscriber sits on a low layer the higher layers get paused
/// (`StreamPaused rid=f`) and drop out of the sum entirely, so `desired` collapses. Since str0m
/// caps every probe at `2 x desired` (`ProbeControl::queue_probe`), that starves the probe
/// controller precisely when the subscriber asks for more.
///
/// With `AllocationEngine::requested_capacity` this holds up: reading the probe targets emitted
/// in this sim (`RUST_LOG=str0m::bwe_::probe::control=trace`, target is `2 x desired`), `desired`
/// stays at ~1.8 Mbps across the upgrade instead of decaying to ~600 kbps.
#[test]
fn upgrade_after_long_low_quality_period_test() {
    LocalNodeSim::new()
        .with_tick(Duration::from_millis(1))
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish connection and discover tracks",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeAll {
                description: "Bob starts on the lowest layer",
                participant: "bob",
                heights: &[180],
            },
            // Long enough for the 30s-fall stable filter to decay substantially.
            Step::Run {
                description: "60s on the low layer - `desired` would decay here",
                duration: Duration::from_secs(60),
            },
            Step::SubscribeAll {
                description: "Bob asks for full quality",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Allow BWE to probe up and the allocator to upgrade",
                duration: Duration::from_secs(15),
            },
            // ~437 kbps. Enough to prove the upgrade past the 150 kbps "q" layer happened and
            // that the stream did not stall. See the ignored test below for why it is not "f".
            Step::CheckRxBytesInterval {
                description: "Bob is upgraded well past the lowest layer",
                participant: "bob",
                min_bytes: 700_000,
            },
            Step::CheckVideoQuality {
                description: "Frames stay renderable across the upgrade",
                participant: "bob",
                quality: VideoQuality::min_frames(200).allow_gaps(5),
            },
        ]);
}

/// A subscriber asking for 720p on an unconstrained link must eventually receive the top layer.
///
/// # Known failure - a real bug, not a flaky test
///
/// Alice publishes q=150 kbps / h=400 kbps / f=1.25 Mbps. Bob subscribes at 720p on a turmoil
/// link with no configured bandwidth limit. Bob receives ~819 KB per 15s window (~437 kbps),
/// i.e. the *"h"* layer, and never reaches "f" - even when given 180s to settle. It is stuck,
/// not slow.
///
/// Traced with `RUST_LOG=str0m::bwe_=trace`, the subscriber's downstream estimate pins at
/// ~460 kbps with `cause=DelayBasedLimited in_alr=false`, creeping up by exactly the 1000 bps
/// floor in `AimdRateControl::increase` per feedback batch. The equilibrium is self-sustaining:
///
///   - the allocator can only afford "h" (400 kbps) at a 460 kbps estimate, so it sends ~437 kbps
///   - send rate / estimate is then ~0.94, above the ALR threshold, so the sender is never
///     considered application-limited and **no probes fire**
///   - without probes the estimate can only grow by AIMD additive increase, which is additionally
///     capped at `1.5 x acked` - and acked is bounded by the 437 kbps we are sending
///
/// So the estimate cannot grow enough to afford "f", and nothing pushes the send rate low enough
/// to enter ALR and probe out of it. Escaping needs either the estimate to reach ~1.5x the media
/// rate (the loss controller appears to prevent this - the log shows repeated
/// `DelayBased -> Decreasing` transitions at ~0.12% loss) or an allocation probe, which itself
/// requires ALR.
///
/// Un-ignore once the deadlock is fixed. 2_000_000 bytes/15s is ~1.07 Mbps: comfortably above
/// the 400 kbps "h" layer and below a fully-utilised 1.25 Mbps "f".
#[test]
#[ignore = "known bug: downstream BWE deadlocks at ~460kbps and never reaches the top layer"]
fn subscriber_reaches_top_layer_on_fast_link_test() {
    LocalNodeSim::new()
        .with_tick(Duration::from_millis(1))
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish connection and discover tracks",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeAll {
                description: "Bob asks for 720p from the start",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Generous settle window - the link can carry the top layer",
                duration: Duration::from_secs(60),
            },
            Step::Run {
                description: "Measurement window",
                duration: Duration::from_secs(15),
            },
            Step::CheckRxBytesInterval {
                description: "Bob receives the top layer, not just the middle one",
                participant: "bob",
                min_bytes: 2_000_000,
            },
        ]);
}

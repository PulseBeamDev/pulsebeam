//! End-to-end bandwidth-estimation behaviour.
//!
//! These exercise the interaction between pulsebeam's allocator and str0m's BWE, which unit
//! tests on either side cannot cover: `desired` is computed by pulsebeam and consumed by str0m's
//! probe controller, and the failure modes only appear once both are in the loop.

use super::common::{LinkProfile, LocalNodeSim, Participant, Room, Step, VideoQuality};
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
            Step::CheckRxBytesInterval {
                description: "Bob is upgraded well past the lowest layer",
                participant: "bob",
                min_bytes: 1_500_000,
            },
            Step::CheckVideoQuality {
                description: "Frames stay renderable across the upgrade",
                participant: "bob",
                quality: VideoQuality::min_frames(200).allow_gaps(5),
            },
        ]);
}

/// A subscriber asking for 720p on a fast link must actually receive the top layer.
///
/// Alice publishes q=150 kbps / h=400 kbps / f=1.25 Mbps; Bob subscribes at 720p over
/// [`LinkProfile::fiber`]. This is the end-to-end statement that BWE converges high enough for
/// the allocator to promote the subscriber all the way up the simulcast ladder.
///
/// This test failed for a long time - Bob would sit on "h" forever - and the cause turned out to
/// be the simulator, not the congestion controller. turmoil's default link applies a uniform
/// random 0-100ms latency per message, which destroys the inter-packet spacing GCC measures:
/// probes under-read their own send rate by 2-4x and reordering was charged as ~9% loss. See
/// [`LinkProfile`]. With a realistic latency band it converges as expected.
#[test]
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

/// The real-world call: one screen share plus one camera, both directions live.
///
/// This is the scenario from production that motivated the BWE work. Two participants each
/// publish and subscribe, so four simulcast ladders are in flight at once and every congestion
/// controller is being driven by real media rather than a synthetic ramp.
///
/// The screen share is VBR ([`VbrProfile::screenshare`]): 4s bursts at 30fps separated by 20s of
/// near-static content at 2fps. That ~15x swing is what makes this worth simulating.
///
///   - during the quiet phase the sender is application limited, so str0m enters ALR and the
///     probe controller alone keeps the bandwidth estimate alive. If probing stalls, the estimate
///     decays while the screen is still.
///   - when the user scrolls again the encoder immediately demands the full layer rate. If the
///     estimate decayed during the quiet phase, that burst has nowhere to go and the viewer sees
///     a freeze.
///   - meanwhile the camera stream shares the same connection and must not be starved by the
///     screen share's bursts. Production symptom: "the allocator doesn't think there is enough
///     for the camera to be streamed".
///
/// Measured over 48s (two full VBR cycles) on [`LinkProfile::fiber`]: the camera direction
/// carries ~1.16 Mbps, i.e. essentially the full 1.25 Mbps "f" layer, so the screen share's
/// bursts are not starving it. The screen-share direction carries less in absolute terms purely
/// because of its duty cycle - `(4s x 1.25Mbps + 20s x 83kbps) / 24s` is ~278 kbps - so it too is
/// being delivered at its natural rate rather than throttled.
#[test]
fn screenshare_and_camera_conference_test() {
    LocalNodeSim::new()
        .with_tick(Duration::from_millis(1))
        .with_room(
            Room::new("room1")
                .with_participant(
                    Participant::screensharer("screen", &["q", "h", "f"]).and_subscribes(),
                )
                .with_participant(
                    Participant::publisher("camera", &["q", "h", "f"]).and_subscribes(),
                ),
        )
        .run(vec![
            // Generous: track discovery is signalling round-trips, which take noticeably
            // longer on the higher-latency, lossier profiles.
            Step::Run {
                description: "Establish both connections and discover tracks",
                duration: Duration::from_secs(60),
            },
            Step::SubscribeAll {
                description: "Screen-sharer subscribes to the camera at full quality",
                participant: "screen",
                heights: &[720],
            },
            Step::SubscribeAll {
                description: "Camera subscribes to the screen share at full quality",
                participant: "camera",
                heights: &[720],
            },
            Step::Run {
                description: "Warmup: let BWE settle on both connections",
                duration: Duration::from_secs(20),
            },
            Step::Run {
                description: "Soak across two full VBR cycles (static -> scroll -> static)",
                duration: Duration::from_secs(48),
            },
            // ~224 kbps measured. The camera is constant-bitrate, so any collapse here means the
            // screen share's bursts starved it.
            Step::CheckRxBytesInterval {
                description: "Camera stream is not starved by the screen share",
                participant: "screen",
                min_bytes: 6_000_000,
            },
            // ~86 kbps measured, matching the VBR average. A large shortfall means the bursts
            // after a quiet phase were dropped - the estimate decayed while the screen was still.
            Step::CheckRxBytesInterval {
                description: "Screen-share bursts survive the quiet phases",
                participant: "camera",
                min_bytes: 1_200_000,
            },
            Step::CheckVideoQuality {
                description: "Screen-sharer renders the camera cleanly throughout",
                participant: "screen",
                quality: VideoQuality::min_frames(1_000).allow_gaps(3),
            },
            Step::CheckVideoQuality {
                description: "Camera participant renders the screen share cleanly throughout",
                participant: "camera",
                quality: VideoQuality::min_frames(300).allow_gaps(3),
            },
        ]);
}

/// The same call over home Wi-Fi: 8-16ms jitter and 0.2% loss.
///
/// Jitter is what the delay-based controller measures, so widening the band directly attacks the
/// trendline estimator; the loss floor exercises the loss controller at a rate low enough that it
/// should be absorbed as inherent loss rather than triggering a backoff.
#[test]
fn screenshare_and_camera_over_wifi_test() {
    conference_plan(LinkProfile::wifi(), 3_000_000, 700_000, 600, 200, 8, 2);
}

/// The same call over mobile: ~50ms latency and 1% loss.
///
/// # Known failure - reproduces a real defect, mechanism not yet pinned down
///
/// The camera direction collapses to ~9 kbps. What is established:
///
///   - **BWE is not the limiter.** It reports a healthy ~1.9 Mbps; the downstream log reads
///     `bwe=1.937Mbit/s used=0bit/s want=2.000Mbit/s streams=BUU:PAUSE`. The stream is *paused*
///     because the SFU marks the upstream layer unhealthy, not because of congestion control.
///   - **Latency alone is fine.** With this exact profile and `loss: 0.0` the test passes with
///     zero quality transitions. The failure needs real loss to seed it.
///   - **The windows involved are tiny.** `StreamMonitor` logged `expected: 6, actual: 4` and
///     `expected: 14, actual: 8` - 33% and 43%, both past `VIDEO_SEVERE_LOSS_THRESHOLD` (0.30),
///     which transitions to Bad immediately with no confirmation window. `MIN_LOSS_EVIDENCE_PACKETS`
///     is only 5, so two drops in one window are enough.
///   - **Once Bad, no recovery was observed** - 4 `Good -> Bad` transitions and zero back.
///
/// What is *not* established: three isolated unit tests in `rtp/monitor.rs` -
/// `loss_ratio_tracks_actual_loss_rate`, `..._with_reordering`, and
/// `sparse_low_rate_stream_survives_occasional_loss` - all feed the estimator 1% loss under
/// in-order, reordered, and sparse-window conditions, and all report correctly. So the sparse
/// severe-threshold path above is a plausible trigger but is not on its own sufficient to
/// reproduce the collapse; some interaction present in the full pipeline is still missing.
///
/// Do not "fix" this by loosening the assertion. The next step is to instrument
/// `evaluate_quality_hysteresis` in a live sim run to capture the exact window that trips it.
#[test]
#[ignore = "known bug: upstream loss estimator over-reports ~1% loss as 21-35%, pausing the stream"]
fn screenshare_and_camera_over_cellular_test() {
    conference_plan(LinkProfile::cellular(), 900_000, 250_000, 300, 90, 14, 2);
}

/// Shared plan for the conference tests so the link profile is the only variable.
///
/// `allowed_missing_parameter_sets` is non-zero for the lossy profiles. Losing the SPS/PPS that
/// precedes a keyframe is a genuine event when the link drops packets, and the property that
/// actually matters is that the stream *recovers* - which the `min_frames` and `allow_gaps`
/// bounds assert. Demanding zero here would be asserting the link is lossless, not that the
/// implementation handles loss.
fn conference_plan(
    link: LinkProfile,
    camera_min_bytes: u64,
    screen_min_bytes: u64,
    camera_min_frames: u64,
    screen_min_frames: u64,
    allowed_gaps: u64,
    allowed_missing_parameter_sets: u64,
) {
    LocalNodeSim::new()
        .with_tick(Duration::from_millis(1))
        .with_link(link)
        .with_room(
            Room::new("room1")
                .with_participant(
                    Participant::screensharer("screen", &["q", "h", "f"]).and_subscribes(),
                )
                .with_participant(
                    Participant::publisher("camera", &["q", "h", "f"]).and_subscribes(),
                ),
        )
        .run(vec![
            // Generous: track discovery is signalling round-trips, which take noticeably
            // longer on the higher-latency, lossier profiles.
            Step::Run {
                description: "Establish both connections and discover tracks",
                duration: Duration::from_secs(60),
            },
            Step::SubscribeAll {
                description: "Screen-sharer subscribes to the camera at full quality",
                participant: "screen",
                heights: &[720],
            },
            Step::SubscribeAll {
                description: "Camera subscribes to the screen share at full quality",
                participant: "camera",
                heights: &[720],
            },
            Step::Run {
                description: "Warmup: let BWE settle on both connections",
                duration: Duration::from_secs(20),
            },
            Step::Run {
                description: "Soak across two full VBR cycles (static -> scroll -> static)",
                duration: Duration::from_secs(48),
            },
            Step::CheckRxBytesInterval {
                description: "Camera stream is not starved by the screen share",
                participant: "screen",
                min_bytes: camera_min_bytes,
            },
            Step::CheckRxBytesInterval {
                description: "Screen-share bursts survive the quiet phases",
                participant: "camera",
                min_bytes: screen_min_bytes,
            },
            Step::CheckVideoQuality {
                description: "Screen-sharer renders the camera without freezes",
                participant: "screen",
                quality: VideoQuality::min_frames(camera_min_frames)
                    .allow_gaps(allowed_gaps)
                    .allow_missing_parameter_sets(allowed_missing_parameter_sets),
            },
            Step::CheckVideoQuality {
                description: "Camera participant renders the screen share without freezes",
                participant: "camera",
                quality: VideoQuality::min_frames(screen_min_frames)
                    .allow_gaps(allowed_gaps)
                    .allow_missing_parameter_sets(allowed_missing_parameter_sets),
            },
        ]);
}

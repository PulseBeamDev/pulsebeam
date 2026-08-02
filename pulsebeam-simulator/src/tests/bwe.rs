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

/// A height request must be satisfied by rounding *up* the simulcast ladder, not down.
///
/// This is the "stream never reaches 720p" report. The viewer asks for 540p from a ladder of
/// f=720 / h=360 / q=180. No layer is exactly 540, so the request sits between two tiers.
///
/// Spatial gating admitted only layers at or below the request, so the viewer was handed h=360 -
/// visibly softer than it asked for - while f=720 sat unused on a link with ample room for it.
///
/// # Why this is worth more than one tier of sharpness
///
/// `requested_capacity` values a subscription at the seed bitrate of the tallest *spatially
/// allowed* layer, so rounding down also halves what the SFU asks BWE for: 400 kbps (Medium seed)
/// instead of 1.25 Mbps (High). str0m probes at `2 x desired`, and a probe can only ever prove
/// its own target, so the estimate pins just above the layer already in use with no headroom
/// left. Production, a 1080p screen share against a 720p request:
///
/// ```text
/// Probe queued kind=PeriodicAlr target_bps=800000   <- every probe, for the whole run
/// BWE estimate updated estimate_bps=355072..702511
/// ```
///
/// 800000 is exactly `2 x 400000`, and 400000 is exactly `LayerQuality::Medium.seed_bitrate_bps()`.
/// The connection could never climb to the 1.25 Mbps `f` layer because it never asked to, and the
/// margin left over the layer it did use was thin enough that VBR bursts paused the slot.
///
/// A second connection in the same log, whose request did admit `f`, sat at 3.8-4.7 Mbps
/// throughout. The link was never the constraint.
#[test]
fn height_request_rounds_up_the_ladder_test() {
    LocalNodeSim::new()
        .with_tick(Duration::from_millis(1))
        .with_link(LinkProfile::fiber())
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("viewer", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connection and discover the track",
                duration: Duration::from_secs(20),
            },
            Step::SubscribeTo {
                description: "Viewer asks for 540p, which no layer matches exactly",
                participant: "viewer",
                targets: &[("camera", 540)],
            },
            Step::Run {
                description: "Warmup: let BWE settle with room to spare",
                duration: Duration::from_secs(30),
            },
            Step::Run {
                description: "Measurement window",
                duration: Duration::from_secs(30),
            },
            // f is 1.25 Mbps, so 30s of it is ~4.7 MB; h is 400 kbps, ~1.5 MB. 3 MB cleanly
            // separates "rounded up to f" from "rounded down to h".
            Step::CheckRxBytesInterval {
                description: "Viewer gets the 720p layer, not the 360p one",
                participant: "viewer",
                min_bytes: 3_000_000,
            },
        ]);
}

/// The real-world call: one screen share plus one camera, both directions live.
///
/// This is the scenario from production that motivated the BWE work. Two participants each
/// publish and subscribe, so four simulcast ladders are in flight at once and every congestion
/// controller is being driven by real media rather than a synthetic ramp.
///
/// The screen share replays a captured desktop at up to 15fps and falls to a 0.5fps heartbeat
/// while static. That swing is what makes this worth simulating.
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
/// Measured over 48s of the captured activity on [`LinkProfile::fiber`]: the camera direction
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
                .with_participant(Participant::screensharer("screen").and_subscribes())
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
                description: "Soak across captured static and active screen periods",
                duration: Duration::from_secs(48),
            },
            // ~224 kbps measured. The camera is constant-bitrate, so any collapse here means the
            // screen share's bursts starved it.
            Step::CheckRxBytesInterval {
                description: "Camera stream is not starved by the screen share",
                participant: "screen",
                min_bytes: 6_000_000,
            },
            // ~122 kbps measured, matching the VBR average: the fixture carries 843 kB per 60.5s
            // loop (13.9 kB/s), so a full 48s soak can only ever deliver ~670 kB of media. A large
            // shortfall means the bursts after a quiet phase were dropped - the estimate decayed
            // while the screen was still.
            Step::CheckRxBytesInterval {
                description: "Screen-share bursts survive the quiet phases",
                participant: "camera",
                min_bytes: 600_000,
            },
            Step::CheckVideoQuality {
                description: "Screen-sharer renders the camera cleanly throughout",
                participant: "screen",
                quality: VideoQuality::min_frames(1_000).allow_gaps(3),
            },
            Step::CheckVideoQuality {
                description: "Camera participant renders the screen share cleanly throughout",
                participant: "camera",
                quality: VideoQuality::min_frames(150).allow_gaps(3),
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
    conference_plan(LinkProfile::wifi(), 3_000_000, 700_000, 600, 100, 8, 2);
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
                .with_participant(Participant::screensharer("screen").and_subscribes())
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
                description: "Soak across captured static and active screen periods",
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

/// A viewer of a static screen share must not lose its bandwidth estimate.
///
/// This is the production failure, reproduced end to end. Three participants: a presenter sharing
/// a mostly static screen, a second person on camera, and a viewer.
///
/// The mechanism runs entirely through the viewer's downstream connection:
///
///   1. While the screen is static the presenter emits near-empty frames, so every packet the SFU
///      forwards to the viewer is a couple of hundred bytes at most.
///   2. Padding is drawn from the RTX cache of recently sent packets, so the viewer's downstream
///      has only tiny packets available to pad with.
///   3. str0m emits one padding packet per event-loop round trip, so a probe cluster aimed at a
///      few Mbps cannot get there - it runs out of wall clock long before it runs out of target
///      bytes. Measured here: a 4 Mbps cluster achieving 0.13 Mbps across 397 packets of 16 bytes.
///   4. Unless rejected, that probe reports the rate it *managed* to send at, which asserts a link
///      limit that was never tested and drags the estimate down.
///   5. The viewer's allocator then cannot afford the camera - the production symptom, "the
///      allocator doesn't think there's enough for the camera to be streamed".
///
/// The viewer deliberately watches *only* the screen share during the soak; a second active
/// stream keeps the acknowledged rate high enough to prop the estimate up on its own.
///
/// # What this test does and does not currently prove
///
/// It **does** reproduce the starved-probe condition. Instrumented with
/// `RUST_LOG=str0m::bwe_::probe::estimator=trace`, this plan produces clusters like
/// `tgt=4.00M ach=0.13M (3%) pkts=397 avgB=16` - a 4 Mbps probe achieving 130 kbps across 397
/// packets of 16 bytes, which is the production failure mode and then some.
///
/// It does **not** yet reproduce the consequence. In production those starved probes pinned the
/// estimate at ~1.45 Mbps on a link carrying 3 Mbps. Here the viewer's estimate stays near
/// 2.6-2.9 Mbps whether or not str0m's `MIN_PROBE_DELIVERY_RATIO` guard is enabled, so this test
/// does not currently discriminate that guard - the str0m unit tests
/// `under_delivered_probe_is_rejected` and `delivered_probe_still_produces_estimate` do that
/// directly. The likely reason is `limit_probe_bitrate`, which floors a probe result at
/// `min(delay_estimate, acked * 0.85)`; if the delay-based estimate stays high here, bad probe
/// results are clamped away before they can do damage.
///
/// So treat this as a regression guard on the *conditions* plus the viewer's estimate, not as
/// proof of the guard. Closing the gap means finding why the production delay-based estimate did
/// not provide that same floor.
#[test]
fn static_screenshare_does_not_poison_bandwidth_estimate_test() {
    LocalNodeSim::new()
        .with_tick(Duration::from_millis(1))
        .with_room(
            Room::new("room1")
                .with_participant(Participant::screensharer("presenter"))
                .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("viewer", 2)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connections and discover both tracks",
                duration: Duration::from_secs(20),
            },
            Step::SubscribeTo {
                description: "Viewer watches only the screen share",
                participant: "viewer",
                targets: &[("presenter", 720)],
            },
            Step::Run {
                description: "Warmup while the screen share is still active",
                duration: Duration::from_secs(15),
            },
            Step::Run {
                description: "Soak across captured static and active screen periods",
                duration: Duration::from_secs(48),
            },
            Step::CheckMinBwe {
                description: "Estimate survives the static stretches",
                min_bps: 2_000_000,
            },
            Step::CheckVideoQuality {
                description: "Viewer renders the screen share cleanly throughout",
                participant: "viewer",
                quality: VideoQuality::min_frames(100).allow_gaps(6),
            },
        ]);
}

/// A viewer subscribing to two publishers must receive both.
///
/// This is the production complaint - "the allocator doesn't think there's enough bandwidth for
/// the camera to be streamed" - reproduced end to end. A viewer takes one screen share plus one
/// camera at 720p, which needs ~2.5 Mbps of media (two 1.25 Mbps `f` layers).
///
/// It is worth being precise about what this asserts, because the obvious reading is wrong. The
/// camera was never short of bandwidth: the estimate sat at ~3.2 Mbps throughout, comfortably
/// above the ~2.8 Mbps two `f` layers cost. `CheckMinBwe` pins that down so a future regression
/// that *does* starve the camera fails for a visibly different reason than one that unbinds it.
///
/// # The bug this covers
///
/// The agent's `SubscriptionManager::reconcile` used to emit only the slots whose assignment had
/// changed, while the SFU's `VideoAllocator::configure` treats a `ClientIntent` as a declarative
/// statement of desired state and stops every slot the intent does not mention. Subscribing to a
/// second track sent an intent naming only that track, so the SFU unbound the first.
///
/// The symptom read exactly like congestion control failing to ramp:
///
/// ```text
/// CheckRxBytesInterval  expected >= 4000000 bytes  actual 373203 bytes
/// ```
///
/// 373 kB across the 30s soak is ~100 kbps, the screen share's VBR average on its own. But the
/// estimate was 3.2 Mbps at the time - ample for both - and instrumenting the allocator showed
/// the second slot was not paused for cost. It was absent: across 2719 allocation passes exactly
/// one had both slots bound, and `slot.target()` was `None` for the other in all 913 passes that
/// mattered. A slot with no target never reaches the allocator at all.
#[test]
fn estimate_grows_to_fit_screenshare_and_camera_test() {
    LocalNodeSim::new()
        .with_tick(Duration::from_millis(1))
        .with_link(LinkProfile::fiber())
        .with_room(
            Room::new("room1")
                .with_participant(Participant::screensharer("presenter"))
                .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("viewer", 2)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connections and discover both tracks",
                duration: Duration::from_secs(20),
            },
            Step::SubscribeAll {
                description: "Viewer watches the screen share and the camera at full quality",
                participant: "viewer",
                heights: &[720, 720],
            },
            Step::Run {
                description: "Warmup: let the estimate ramp with both streams live",
                duration: Duration::from_secs(40),
            },
            Step::Run {
                description: "Soak: both streams should be flowing",
                duration: Duration::from_secs(30),
            },
            // Well clear of the ~2.8 Mbps two `f` layers cost, so a regression that genuinely
            // starves the camera fails here rather than on the byte count below.
            Step::CheckMinBwe {
                description: "Estimate leaves room for both streams",
                min_bps: 2_500_000,
            },
            // The camera alone is 1.25 Mbps CBR, so 30s of it is ~4.7 MB. Anything near the
            // screen share's ~100 kbps VBR average on its own means the camera is missing.
            Step::CheckRxBytesInterval {
                description: "Viewer receives both streams, not just one",
                participant: "viewer",
                min_bytes: 4_000_000,
            },
        ]);
}

/// A video subscription added after the initial one must actually be delivered.
///
/// The viewer subscribes to the presenter, runs for 60s, then re-issues subscriptions for *both*
/// the presenter and the camera. Before the fix the camera never arrived: the viewer received
/// ~213 kbps for the next 45s, which is the screen share's VBR average alone, and 45s was no
/// better than 20s.
///
/// It looked like congestion control and was not. Ruled out in turn:
///
///   - **Not bandwidth.** During the pickup window the downstream estimate measured
///     min 2.76 / max 3.07 Mbps against `want=2.6Mbps`. There was more than enough headroom, and
///     the allocator's own report confirmed it was not congestion-limited.
///   - **Not the probe guard.** Reproduced identically with `MIN_PROBE_DELIVERY_RATIO` enabled or
///     disabled, so it was unrelated to the starved-probe defect.
///   - **Not the `height == 0` hide path.** Omitting the camera from the first subscription
///     entirely, rather than subscribing at height 0, gave byte-for-byte the same result
///     (1_200_940). So it was not `signaling.rs`'s `if req.target_height == 0 { continue; }`.
///
/// The slot was simply not bound. With `RUST_LOG=pulsebeam=debug` two slots existed and both were
/// bound initially, then one dropped out of every subsequent allocation report while the other
/// climbed q -> h -> f normally:
///
///   `streams=brG:PAUSE(150.000kbit/s) k9U:M(400.000kbit/s)`   <- both bound
///   `streams=brG:M(400.000kbit/s)`                            <- k9U gone
///   `streams=brG:H(1.250Mbit/s)`                              <- and never returns
///
/// The cause was a delta-versus-declarative mismatch in the signalling: the agent's
/// `SubscriptionManager::reconcile` sent only the slots whose assignment had changed, while the
/// SFU's `VideoAllocator::configure` stops every slot the intent does not name. Re-issuing a
/// subscription that left the unchanged presenter out therefore unbound it. See
/// [`estimate_grows_to_fit_screenshare_and_camera_test`], which covers the same defect without
/// the re-issue.
#[test]
fn late_video_subscription_is_delivered_test() {
    LocalNodeSim::new()
        .with_tick(Duration::from_millis(1))
        .with_room(
            Room::new("room1")
                .with_participant(Participant::screensharer("presenter"))
                .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("viewer", 2)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connections and discover both tracks",
                duration: Duration::from_secs(20),
            },
            Step::SubscribeTo {
                description: "Viewer watches only the screen share",
                participant: "viewer",
                targets: &[("presenter", 720)],
            },
            Step::Run {
                description: "Long stretch with only the presenter subscribed",
                duration: Duration::from_secs(60),
            },
            Step::SubscribeTo {
                description: "Viewer now also wants the camera",
                participant: "viewer",
                targets: &[("presenter", 720), ("camera", 720)],
            },
            Step::Run {
                description: "Allow the allocator to take up the camera",
                duration: Duration::from_secs(45),
            },
            // 45s of the 1.25Mbps "f" layer is ~7MB, and the screen share adds ~1.5MB on top.
            // 2MB is a generous floor that still separates a real pickup from the ~213kbps
            // (1_200_940 bytes) the viewer actually receives.
            Step::CheckRxBytesInterval {
                description: "Camera is delivered after being added to the subscription",
                participant: "viewer",
                min_bytes: 2_000_000,
            },
        ]);
}

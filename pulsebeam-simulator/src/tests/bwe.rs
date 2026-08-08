//! End-to-end bandwidth-estimation behaviour.
//!
//! These exercise the interaction between pulsebeam's allocator and str0m's BWE, which unit
//! tests on either side cannot cover: `desired` is computed by pulsebeam and consumed by str0m's
//! probe controller, and the failure modes only appear once both are in the loop.

use super::common::{
    Capacity, LinkProfile, LinkReport, LocalNodeSim, Loss, Participant, Property, Reorder, Room,
    Step, VideoQuality,
};
use std::time::Duration;

/// Seeds every tight allocation plan is run under.
///
/// Determinism is what makes a failure reproducible; it is not what makes a plan representative.
/// One seed exercises one interleaving of packet arrival, jitter and loss, and a plan that holds
/// under exactly one interleaving has demonstrated very little. Re-running the whole plan under
/// several fixed seeds keeps every failure reproducible while asserting the property is a property
/// of the implementation rather than of a lucky schedule.
const QOS_SEEDS: [u64; 4] = [0xDEAD_BEEF, 0x1234_5678, 0x0BAD_F00D, 0xFEED_FACE];

/// Upgrading after a long stretch at low quality must not break the stream.
///
/// This is an end-to-end anchor for the transition from a long low-quality period to an explicit
/// upgrade request. It covers the allocator, BWE-facing demand, and forwarding path together.
#[test]
fn upgrade_after_long_low_quality_period_test() {
    LocalNodeSim::new()
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
            Step::CheckForwardedQuality {
                description: "Bob is upgraded well past the lowest layer",
                origin: "alice",
                min_quality: 3,
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
            Step::CheckForwardedQuality {
                description: "Bob receives the top layer, not just the middle one",
                origin: "alice",
                min_quality: 3,
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
/// The ladder has no exact 540p tier, so the spatial assertion specifically guards the choice to
/// round up to the smallest layer that satisfies the request.
#[test]
fn height_request_rounds_up_the_ladder_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("viewer", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connection and discover the track",
                duration: Duration::from_secs(5),
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
            // Stated as the layer rather than inferred from a byte count. Which rung was chosen
            // is the entire question here, and the forwarded quality answers it directly instead
            // of via a threshold that has to be re-derived whenever the ladder's rates change.
            Step::CheckForwardedQuality {
                description: "Viewer gets the 720p layer, not the 360p one",
                origin: "camera",
                min_quality: 3,
            },
        ]);
}

/// A high-priority camera request must reclaim bandwidth from a lower-priority screen share.
///
/// Both slots start on a low request, then the camera is explicitly raised to 720p while the
/// screen share remains a low-priority 180p subscription. The link can carry either the camera's
/// top layer or both streams' current layers, but not both the camera upgrade and the retained
/// screen layer. The priority contract is that the camera wins this contention; a lower-priority
/// stream may pause rather than pin the camera at its middle layer.
#[test]
fn high_priority_camera_reclaims_bandwidth_from_screenshare_test() {
    LocalNodeSim::new()
        .with_bandwidth(2_750_000)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::screensharer("screen"))
                .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                .with_participant(Participant::manual_subscriber("viewer", 2)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connections and discover both tracks",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeToQos {
                description: "Viewer starts both streams at low priority and height",
                participant: "viewer",
                targets: &[("camera", 180, 90, 10), ("screen", 180, 90, 10)],
            },
            Step::Run {
                description: "Let the screen share become the retained co-tenant",
                duration: Duration::from_secs(30),
            },
            Step::SubscribeToQos {
                description: "Viewer raises the camera to 720p with higher priority",
                participant: "viewer",
                targets: &[("camera", 720, 360, 200), ("screen", 180, 90, 10)],
            },
            Step::Run {
                description: "Allow the allocator and BWE to apply the priority change",
                duration: Duration::from_secs(60),
            },
            Step::CheckForwardedQualityReached {
                description: "The high-priority camera reaches its top layer during contention",
                origin: "camera",
                min_quality: 3,
            },
        ]);
}

/// The standard QoS contention room: a variable-bitrate screen share (single 2.5 Mbit/s layer),
/// a three-layer camera (q/h/f ≈ 150 k / 400 k / 1.25 Mbit/s at 180/360/720p), and a viewer with
/// two manual slots. This is the setup that makes the priority levers of the user-intent protocol
/// visible: the streams together cost more than a typical link, so `priority`, `min_height`, and
/// `target_height` actually decide who gets what.
fn screen_camera_viewer_room() -> Room {
    Room::new("room1")
        .with_participant(Participant::screensharer("screen"))
        .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
        .with_participant(Participant::manual_subscriber("viewer", 2))
}

/// The protocol contract, exercised across a reconfiguration: when the viewer hands focus to the
/// camera (high `priority`, 720p target), the focused stream must reach and *hold* its top layer,
/// and the backgrounded screen share must yield cleanly rather than flap.
///
/// SPEC / RED under today's allocator. The allocator is floors-first-regardless-of-priority, so
/// the low-priority screen's `min_height=90` floor — which can only be met by its single 2.5 Mbit/s
/// layer — is guaranteed ahead of the focused camera's target and starves it at the middle layer.
/// The ruled contract is *priority-gate floors*: a lower-priority floor yields to a higher-priority
/// target. This test encodes that; it goes green when `run_compute` gains priority-gating.
///
/// (Both streams reaching quality 3 is impossible here on purpose: camera f (1.25M) + screen
/// (2.5M) = 3.75M exceeds the ~3.0 Mbit/s estimate on a 3.5 Mbit/s link. The focused camera wins;
/// the background screen pauses.)
#[test]
fn priority_reconfiguration_quality_churn_test() {
    for seed in QOS_SEEDS {
        LocalNodeSim::new()
            .with_rng_seed(seed)
            .with_bandwidth(3_500_000)
            .with_room(screen_camera_viewer_room())
            .run(vec![
                Step::Run {
                    description: "Establish connections and discover the camera",
                    duration: Duration::from_secs(5),
                },
                Step::SubscribeToQos {
                    description: "Viewer starts with the camera at low quality",
                    participant: "viewer",
                    targets: &[("camera", 180, 90, 10)],
                },
                Step::Run {
                    description: "Let the initial camera subscription settle",
                    duration: Duration::from_secs(10),
                },
                Step::SubscribeToQos {
                    description: "Viewer adds the screen share at equal low priority",
                    participant: "viewer",
                    targets: &[("camera", 180, 90, 10), ("screen", 180, 90, 10)],
                },
                Step::Run {
                    description: "Let both subscriptions settle before reconfiguration",
                    duration: Duration::from_secs(15),
                },
                Step::SubscribeToQos {
                    description: "Viewer hands focus to the camera; screen share is backgrounded",
                    participant: "viewer",
                    targets: &[("camera", 720, 360, 200), ("screen", 180, 90, 10)],
                },
                Step::Run {
                    description: "Require the focused camera to reach top quality after reconfiguration",
                    duration: Duration::from_secs(30),
                },
                Step::CheckForwardedQuality {
                    description: "The focused high-priority camera reaches its requested top layer",
                    origin: "camera",
                    min_quality: 3,
                },
                Step::Run {
                    description: "Soak the focused allocation",
                    duration: Duration::from_secs(30),
                },
                Step::Report {
                    description: "priority reconfiguration diagnostic",
                    participant: "viewer",
                },
                Step::CheckForwardedQuality {
                    description: "The focused camera holds its top layer through the soak",
                    origin: "camera",
                    min_quality: 3,
                },
                Step::Expect {
                    description: "The focused camera settles on one layer without oscillating",
                    participant: "viewer",
                    property: Property::QualityReversalsBelow {
                        origin: "camera",
                        max: 0,
                    },
                },
                Step::Expect {
                    description: "The backgrounded screen yields cleanly rather than flapping",
                    participant: "viewer",
                    property: Property::QualityReversalsBelow {
                        origin: "screen",
                        max: 0,
                    },
                },
            ]);
    }
}

/// `priority` gates `min_height`: a lower-priority stream's floor must not preempt a
/// higher-priority stream's target when the two cannot both fit.
///
/// SPEC / RED under today's allocator (same root cause as the reconfiguration test, isolated to a
/// single static subscription). The background screen's oversized 2.5 Mbit/s floor is guaranteed
/// ahead of the focused camera's 720p target, capping the camera at the middle layer. Under the
/// ruled priority-gate contract the focused camera reaches its top layer and the screen yields.
#[test]
fn low_priority_floor_yields_to_high_priority_target_test() {
    for seed in QOS_SEEDS {
        LocalNodeSim::new()
            .with_rng_seed(seed)
            .with_bandwidth(3_500_000)
            .with_room(screen_camera_viewer_room())
            .run(vec![
                Step::Run {
                    description: "Establish connections and discover both tracks",
                    duration: Duration::from_secs(5),
                },
                Step::SubscribeToQos {
                    description: "Focused camera (720p, high priority); background screen with a floor",
                    participant: "viewer",
                    targets: &[("camera", 720, 360, 200), ("screen", 180, 90, 10)],
                },
                Step::Run {
                    description: "Let the allocator resolve the contention",
                    duration: Duration::from_secs(45),
                },
                Step::Report {
                    description: "priority-gated floor diagnostic",
                    participant: "viewer",
                },
                Step::CheckForwardedQuality {
                    description: "The high-priority camera target beats the low-priority screen floor",
                    origin: "camera",
                    min_quality: 3,
                },
            ]);
    }
}

/// The droppable counterpart to `low_priority_floor_yields_to_high_priority_target`: with the
/// screen explicitly droppable (`min_height=0`), Pass 1 skips it and the focused camera reaches its
/// top layer today. This isolates the bug to `min_height`: the same scenario differing only in the
/// floor is green here and red there.
#[test]
fn droppable_background_yields_to_focused_camera_test() {
    for seed in QOS_SEEDS {
        LocalNodeSim::new()
            .with_rng_seed(seed)
            .with_bandwidth(3_500_000)
            .with_room(screen_camera_viewer_room())
            .run(vec![
                Step::Run {
                    description: "Establish connections and discover both tracks",
                    duration: Duration::from_secs(5),
                },
                Step::SubscribeToQos {
                    description: "Focused camera (720p, high priority); droppable background screen",
                    participant: "viewer",
                    targets: &[("camera", 720, 360, 200), ("screen", 180, 0, 10)],
                },
                Step::Run {
                    description: "Let the allocator resolve the contention",
                    duration: Duration::from_secs(45),
                },
                Step::CheckForwardedQuality {
                    description: "The focused camera reaches its top layer over a droppable stream",
                    origin: "camera",
                    min_quality: 3,
                },
                Step::Expect {
                    description: "The focused camera holds its top layer without flapping",
                    participant: "viewer",
                    property: Property::QualityReversalsBelow {
                        origin: "camera",
                        max: 0,
                    },
                },
            ]);
    }
}

/// `target_height=0` means off: the server forwards nothing for a hidden stream and frees its
/// bandwidth for the streams that are actually on screen. The link fits only one camera at its top
/// layer, so the visible camera reaches `f` only if the hidden one is truly off. A second plain
/// (constant-rate) camera stands in for the hidden stream so the check turns on the target-0
/// semantics, not on a variable-bitrate source.
#[test]
fn hidden_stream_frees_its_bandwidth_test() {
    for seed in QOS_SEEDS {
        LocalNodeSim::new()
            .with_rng_seed(seed)
            .with_bandwidth(4_000_000)
            .with_room(
                Room::new("room1")
                    .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                    .with_participant(Participant::publisher("hidden", &["q", "h", "f"]))
                    .with_participant(Participant::manual_subscriber("viewer", 2)),
            )
            .run(vec![
                Step::Run {
                    description: "Establish connections and discover both cameras",
                    duration: Duration::from_secs(5),
                },
                Step::SubscribeToQos {
                    description: "Camera visible at 720p; the other stream hidden (target 0)",
                    participant: "viewer",
                    targets: &[("camera", 720, 90, 100), ("hidden", 0, 0, 10)],
                },
                Step::Run {
                    description: "Let the camera claim the bandwidth the hidden stream frees",
                    duration: Duration::from_secs(70),
                },
                Step::CheckForwardedQualityReached {
                    description: "The visible camera reaches its top layer once the other is truly off",
                    origin: "camera",
                    min_quality: 3,
                },
                Step::Expect {
                    description: "The lone visible camera holds its top layer without flapping",
                    participant: "viewer",
                    property: Property::QualityReversalsBelow {
                        origin: "camera",
                        max: 0,
                    },
                },
            ]);
    }
}

/// A settled allocation holds its layer: once the viewer's request stops changing and the link is
/// steady, the forwarded layer must not oscillate. This is the instability guard that the endpoint
/// and byte-count checks cannot see.
#[test]
fn steady_state_allocation_does_not_churn_test() {
    for seed in QOS_SEEDS {
        LocalNodeSim::new()
            .with_rng_seed(seed)
            .with_bandwidth(3_500_000)
            .with_room(
                Room::new("room1")
                    .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                    .with_participant(Participant::manual_subscriber("viewer", 1)),
            )
            .run(vec![
                Step::Run {
                    description: "Establish connection and discover the camera",
                    duration: Duration::from_secs(5),
                },
                Step::SubscribeToQos {
                    description: "Camera at 720p with room to spare",
                    participant: "viewer",
                    targets: &[("camera", 720, 360, 100)],
                },
                Step::Run {
                    description: "Let the allocation reach steady state",
                    duration: Duration::from_secs(20),
                },
                Step::CheckForwardedQuality {
                    description: "The camera settles on its top layer",
                    origin: "camera",
                    min_quality: 3,
                },
                Step::Run {
                    description: "Soak the steady allocation",
                    duration: Duration::from_secs(60),
                },
                Step::CheckForwardedQuality {
                    description: "The camera holds its top layer through the soak",
                    origin: "camera",
                    min_quality: 3,
                },
                Step::Expect {
                    description: "A settled stream does not oscillate on a steady link",
                    participant: "viewer",
                    property: Property::QualityReversalsBelow {
                        origin: "camera",
                        max: 0,
                    },
                },
            ]);
    }
}

#[test]
fn priority_swap_reaches_every_requested_top_layer_on_fast_link_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::screensharer("screen"))
                .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                .with_participant(Participant::manual_subscriber("viewer", 2)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connections and discover both tracks",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeToQos {
                description: "Viewer prioritizes the screen share",
                participant: "viewer",
                targets: &[("camera", 180, 90, 10), ("screen", 1080, 90, 200)],
            },
            Step::Run {
                description: "Let the screen share reach full quality",
                duration: Duration::from_secs(30),
            },
            Step::SubscribeToQos {
                description: "Viewer transfers priority to the camera",
                participant: "viewer",
                targets: &[("camera", 1080, 360, 200), ("screen", 180, 90, 10)],
            },
            Step::Run {
                description: "Allow BWE to satisfy the new allocation",
                duration: Duration::from_secs(60),
            },
            Step::Expect {
                description: "The estimate reaches the requested allocation",
                participant: "viewer",
                property: Property::EstimateMeetsNeed { percent: 95 },
            },
            Step::CheckForwardedQuality {
                description: "Camera finishes on its top layer",
                origin: "camera",
                min_quality: 3,
            },
            Step::CheckForwardedQuality {
                description: "Screen share remains on its top layer",
                origin: "screen",
                min_quality: 3,
            },
        ]);
}

/// A one-layer screen share must recover after a temporary downlink collapse while a camera
/// remains subscribed on the same viewer.
#[test]
fn screenshare_recovers_after_competing_camera_pause_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::screensharer("screen"))
                .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                .with_participant(Participant::manual_subscriber("viewer", 2)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connections and discover both tracks",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeToQos {
                description: "Viewer asks for the camera and one-layer screen share",
                participant: "viewer",
                targets: &[("camera", 180, 90, 10), ("screen", 180, 90, 10)],
            },
            Step::Run {
                description: "Warmup with enough capacity for both streams",
                duration: Duration::from_secs(30),
            },
            Step::SetBandwidth {
                description: "Temporary downlink collapse pauses the screen share",
                participant: "viewer",
                bits_per_sec: 1_200_000,
            },
            Step::Run {
                description: "Let the allocator respond to the constrained link",
                duration: Duration::from_secs(45),
            },
            Step::SetBandwidth {
                description: "Viewer link recovers",
                participant: "viewer",
                bits_per_sec: 3_000_000,
            },
            Step::Run {
                description: "Allow BWE and the allocator to recover the screen share",
                duration: Duration::from_secs(90),
            },
            Step::Report {
                description: "post-recovery state",
                participant: "viewer",
            },
            // The defect named directly. The link recovered to 3 Mbps, drops nothing and holds
            // only ~30ms of queue, so nothing about it justifies the estimate walking down. A
            // controller may take time to re-discover capacity; it may not talk itself out of it.
            Step::Expect {
                description: "The estimate recovers rather than staying down",
                participant: "viewer",
                property: Property::EstimateRecovers {
                    of_peak_percent: 70,
                },
            },
            Step::Expect {
                description: "The estimate finds enough for what the allocator asked for",
                participant: "viewer",
                property: Property::EstimateMeetsNeed { percent: 80 },
            },
            Step::Expect {
                description: "Recovery is not bought with standing queue",
                participant: "viewer",
                property: Property::QueueingDelayBelow(Duration::from_millis(150)),
            },
            // The user-visible consequence of the above, kept because it is what a viewer
            // actually experiences.
            Step::CheckForwardedQuality {
                description: "Screen share is not stranded after the link recovers",
                origin: "screen",
                min_quality: 3,
            },
            // Stability, which none of the claims above can see: a stream flipping between two
            // layers many times a second satisfies every one of them - right final layer, right
            // byte count, right estimate - while showing the viewer nothing.
            Step::Expect {
                description: "The screen share holds a layer instead of flapping",
                participant: "viewer",
                property: Property::QualityChangesPerMinuteBelow {
                    origin: "screen",
                    max: 30,
                },
            },
            Step::Expect {
                description: "The camera holds a layer instead of flapping",
                participant: "viewer",
                property: Property::QualityChangesPerMinuteBelow {
                    origin: "camera",
                    max: 30,
                },
            },
        ]);
}

#[test]
fn resizing_camera_keeps_single_layer_screenshare_rendering_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                .with_participant(Participant::screensharer("screen"))
                .with_participant(Participant::manual_subscriber("viewer", 2)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connections and discover both tracks",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Viewer asks for both streams at 720p",
                participant: "viewer",
                targets: &[("camera", 720), ("screen", 720)],
            },
            Step::Run {
                description: "Let both streams reach their requested layers",
                duration: Duration::from_secs(30),
            },
            Step::SubscribeTo {
                description: "Viewer resizes only the camera to 360p",
                participant: "viewer",
                targets: &[("camera", 360), ("screen", 720)],
            },
            Step::Run {
                description: "Render both streams after the resize",
                duration: Duration::from_secs(30),
            },
            Step::CheckVideoQuality {
                description: "Camera remains decodable after the layer change",
                participant: "viewer",
                quality: VideoQuality::min_frames(300).allow_gaps(4),
            },
            Step::CheckForwardedQuality {
                description: "Camera settles on its 360p layer",
                origin: "camera",
                min_quality: 2,
            },
            Step::CheckForwardedQuality {
                description: "Single-layer screen share keeps rendering",
                origin: "screen",
                min_quality: 3,
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
/// A mobile path with 1% datagram loss must not make the camera disappear while the screen share
/// is active. This is intentionally an end-to-end anchor: the BWE, loss monitor, allocator, and
/// packet forwarding path all have to tolerate the same lossy conditions together.
///
/// The simulator uses per-datagram loss here. turmoil's `fail_rate` is a link-partition model
/// that clears in-flight packets, and is therefore unsuitable for a packet-loss profile.
#[test]
fn screenshare_and_camera_over_cellular_test() {
    conference_plan(LinkProfile::cellular(), 900_000, 250_000, 300, 90, 30, 2);
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
        .with_room(
            Room::new("room1")
                .with_participant(Participant::screensharer("presenter"))
                .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("viewer", 2)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connections and discover both tracks",
                duration: Duration::from_secs(5),
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
            Step::Expect {
                description: "The estimate survives the static stretches",
                participant: "viewer",
                property: Property::EstimateMeetsNeed { percent: 80 },
            },
            Step::Expect {
                description: "The estimate holds rather than decaying away",
                participant: "viewer",
                property: Property::EstimateStable {
                    max_drop_percent: 25,
                },
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
        .with_room(
            Room::new("room1")
                .with_participant(Participant::screensharer("presenter"))
                .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("viewer", 2)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connections and discover both tracks",
                duration: Duration::from_secs(5),
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
            Step::Expect {
                description: "The estimate leaves room for both streams",
                participant: "viewer",
                property: Property::EstimateMeetsNeed { percent: 80 },
            },
            Step::Expect {
                description: "The estimate holds rather than decaying away",
                participant: "viewer",
                property: Property::EstimateStable {
                    max_drop_percent: 25,
                },
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
        .with_room(
            Room::new("room1")
                .with_participant(Participant::screensharer("presenter"))
                .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("viewer", 2)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connections and discover both tracks",
                duration: Duration::from_secs(5),
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

/// A capped subscription must not be served bandwidth it asked not to be given.
///
/// The viewer requests 360p, which the ladder satisfies exactly with the h layer at ~450 kbps.
/// Over a 60s window that is ~3.4 MB of media. Anything much beyond it is padding and probe
/// traffic aimed at capacity the subscription cannot use.
///
/// `BitrateController` used to return its quantized, smoothed internal target even after the raw
/// demand had fallen. That allowed a capped subscription to keep probing above what it could use:
///
/// ```text
/// raw_bps=472694  out_bps=800000  alloc_bps=449153
/// ```
///
/// str0m probes at `2 x desired`, so a 360p subscription was probing at 1.6 Mbps and the viewer
/// received 5074716 bytes for 3.4 MB of media - about 50% overhead.
#[test]
fn capped_subscription_is_not_over_served_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("bob", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connection and discover the track",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Bob asks for 360p, which the h layer satisfies exactly",
                participant: "bob",
                targets: &[("alice", 360)],
            },
            Step::Run {
                description: "Warmup",
                duration: Duration::from_secs(30),
            },
            Step::Run {
                description: "Measurement window",
                duration: Duration::from_secs(60),
            },
            // ~3.4 MB is the h layer itself. Measured against `headroom_factor`: 3.57 MB at
            // 1.0 (shipped), 4.25 MB at 1.2, 5.01 MB at 1.5, against 5.07 MB uncapped. 4.0 MB
            // leaves room for RTCP, retransmits and NAT-keepalive padding.
            Step::CheckMaxRxBytesInterval {
                description: "Bob is not served bandwidth beyond what 360p needs",
                participant: "bob",
                max_bytes: 4_000_000,
            },
            // Still actually receiving the layer, not starved into passing the cap.
            //
            // Stated as the layer rather than a byte floor. The floor was 3,000,000 - 60s at the
            // h layer's nominal 400 kbps - which left no margin for the encoder landing anywhere
            // below nominal, and it measured 2,853,400 (~380 kbps). The behaviour was correct and
            // the number was wrong, which is the failure mode of encoding a rate, a duration and
            // a codec into one constant.
            Step::CheckForwardedQuality {
                description: "Bob is still on the 360p layer, not dropped to the bottom",
                origin: "alice",
                min_quality: 2,
            },
        ]);
}

/// A screen share must come back to full quality after the screen has been still.
///
/// The scenario behind the "stuck at 360p" report. A viewer watches a screen share at 720p, the
/// screen goes quiet long enough that the SFU marks the layer dead and the pacer's RTX cache
/// drains, and then the user scrolls again.
///
/// # What this closes
///
/// Every other plan here runs against [`VbrProfile::screenshare`], whose captured schedule has a
/// frame every two seconds - just inside the SFU's 3s stream-dead timeout. Our "static" screen
/// share was therefore never actually static, and no plan reached the regime where a layer dies.
/// [`VbrProfile::screenshare_static`] adds real silence, and the allocator confirms the regime is
/// reached: the slot cycles `H(1.250Mbit/s)` -> `PAUSE(0bit/s)` -> `H(1.250Mbit/s)` as the layer
/// dies and recovers.
///
/// # What this does not prove
///
/// This anchor proves recovery through a genuinely dead layer. [`LinkProfile`] models latency and
/// loss only; bandwidth shaping is a separate simulator control. With no capacity to saturate
/// there is no queueing delay, so the delay-based estimate does not model a constrained-link
/// response here.
///
/// Production is the opposite regime:
///
/// ```text
/// Probe result estimate=20.224kbit/s
/// Probe result estimate=2.638kbit/s
/// Link capacity estimate updated to 26.984kbit/s from probe
/// ```
///
/// Reproducing that needs a rate-limited link on the SFU-to-client path. Until then, treat this as
/// a regression guard on the recovery path through a genuinely dead layer, not on congestion
/// control's response to a constrained link.
#[test]
fn screenshare_recovers_full_quality_after_going_still_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::static_screensharer("presenter"))
                .with_participant(Participant::multi_subscriber("viewer", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connection and discover the track",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Viewer watches the screen share at full quality",
                participant: "viewer",
                targets: &[("presenter", 720)],
            },
            Step::Run {
                description: "Warmup across a full replay, including the silence",
                duration: Duration::from_secs(90),
            },
            Step::Run {
                description: "Soak across two more quiet-then-active cycles",
                duration: Duration::from_secs(160),
            },
            // The fixture carries 843 kB per replay and the loop is now 80.5s, so 160s spans
            // about two replays: ~1.7 MB of media. A viewer stranded on a lower layer, or one
            // whose estimate collapsed during the silence, falls well short.
            Step::CheckRxBytesInterval {
                description: "Bursts after the silence are delivered, not dropped",
                participant: "viewer",
                min_bytes: 1_400_000,
            },
            Step::Expect {
                description: "The estimate survives the silence",
                participant: "viewer",
                property: Property::EstimateMeetsNeed { percent: 80 },
            },
        ]);
}

/// On a link that comfortably fits the top layer, the subscription must actually get there.
///
/// The "stuck at 360p" guard. 3 Mbps carries the 1.25 Mbps `f` layer several times over, so a
/// viewer asking for 720p has no excuse to settle for `h`.
///
/// # Why this needs a rate-limited link
///
/// Every other plan here runs unlimited, and that quietly removes the failure mode. turmoil models
/// latency and loss but not capacity, so an unlimited path carries any offered load: there is no
/// queueing delay, the delay-based estimate never falls, and `limit_probe_bitrate` floors probe
/// results at it. A starved probe simply cannot drag the estimate down, so no unlimited plan can
/// distinguish congestion control that ramps from congestion control that gives up.
///
/// [`LocalNodeSim::with_bandwidth`] adds a real bottleneck - rate, queueing delay, and tail drop
/// once the buffer fills - so the estimate tracks capacity for the first time. Sanity-checked at
/// 800 kbps, where the estimate settles at 663 kbps and the allocator walks the ladder down
/// `H -> M -> L` as it should.
#[test]
fn subscriber_reaches_top_layer_on_a_rate_limited_link_test() {
    LocalNodeSim::new()
        .with_bandwidth(3_000_000)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("bob", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connection and discover the track",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Bob asks for 720p; the link carries it twice over",
                participant: "bob",
                targets: &[("alice", 720)],
            },
            Step::Run {
                description: "Warmup: BWE has to find the capacity that is there",
                duration: Duration::from_secs(40),
            },
            Step::Run {
                description: "Measurement window",
                duration: Duration::from_secs(30),
            },
            // 30s of f is ~4.7 MB; of h, ~1.5 MB. 3 MB cleanly separates "reached the top layer"
            // from "stuck partway up the ladder".
            Step::CheckForwardedQuality {
                description: "Bob reaches the 720p layer rather than stalling below it",
                origin: "alice",
                min_quality: 3,
            },
            Step::CheckMinBwe {
                description: "The estimate finds the link's real capacity",
                min_bps: 1_500_000,
            },
            Step::Report {
                description: "steady state",
                participant: "bob",
            },
            // Measured here: 1.0% drawdown, 13.7ms of queue, no congestion loss, 100% media.
            // The thresholds sit well clear of those so ordinary variation does not flake, but
            // close enough that a real regression trips them.
            //
            // Deliberately *not* asserting utilisation or "tracks capacity": the 720p layer wants
            // ~1.25 Mbps of a 3 Mbps link, so the estimate correctly settles near demand rather
            // than near capacity. Asserting otherwise would encode a misunderstanding of what a
            // delay-based estimator can measure.
            Step::Expect {
                description: "The estimate covers what the viewer asked for",
                participant: "bob",
                property: Property::EstimateMeetsNeed { percent: 80 },
            },
            Step::Expect {
                description: "A steady link produces a steady estimate",
                participant: "bob",
                property: Property::EstimateStable {
                    max_drop_percent: 20,
                },
            },
            Step::Expect {
                description: "The link is not driven into its buffer",
                participant: "bob",
                property: Property::QueueingDelayBelow(Duration::from_millis(100)),
            },
            Step::Expect {
                description: "Throughput is video, not overhead",
                participant: "bob",
                property: Property::MediaEfficiencyAtLeast(90),
            },
        ]);
}

/// A subscription squeezed onto a low layer must climb back when the link recovers.
///
/// The link starts at 500 kbps, which only affords `q`, then widens to 3 Mbps. The viewer asked
/// for 720p throughout, so it has to find its way back to `f`.
///
/// This is a rate-limited downlink recovery anchor: the viewer must move from the bottom layer to
/// the requested top layer after its SFU egress recovers. It does not model publisher uplink
/// congestion, because the simulator's publisher socket bypasses the SFU egress shaper.
#[test]
fn subscription_climbs_back_after_the_link_recovers_test() {
    LocalNodeSim::new()
        .with_bandwidth(500_000)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("bob", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connection and discover the track",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Bob asks for 720p, but the link only affords the bottom layer",
                participant: "bob",
                targets: &[("alice", 720)],
            },
            Step::Run {
                description: "Squeeze: long enough to settle on the low layer",
                duration: Duration::from_secs(60),
            },
            Step::SetBandwidth {
                description: "The viewer link recovers to 3 Mbps",
                participant: "bob",
                bits_per_sec: 3_000_000,
            },
            Step::Run {
                description: "Give the viewer BWE room to re-discover the capacity",
                duration: Duration::from_secs(90),
            },
            Step::CheckForwardedQuality {
                description: "Bob has actually climbed back to the top layer",
                origin: "alice",
                min_quality: 3,
            },
            Step::Run {
                description: "Measurement window",
                duration: Duration::from_secs(30),
            },
            // 30s of f is ~4.7 MB; of h, ~1.5 MB; of q, ~0.5 MB. 3 MB means it climbed all the
            // way back rather than stalling on a rung.
            Step::CheckRxBytesInterval {
                description: "Bob is back on the 720p layer, not stranded below it",
                participant: "bob",
                min_bytes: 3_000_000,
            },
        ]);
}

/// A steady 720p subscription should mostly carry video, not retransmission and padding.
///
/// A production capture showed a viewer receiving 46 MB over ~190s to deliver only 15 MB of
/// actual video - 54% overhead - with `packetsLost` and `nackCount` at zero throughout, which
/// rules out real loss recovery. That pattern is small, numerous RTX packets standing in for
/// padding: str0m's pacer drawing from the RTX cache rather than sending blanks.
///
/// This asserts the ratio directly rather than only asserting total bytes delivered, which the
/// existing throughput tests do not distinguish from overhead - a connection could clear its byte
/// floor while spending most of the link on retransmission, and no test would notice.
#[test]
fn steady_subscription_is_mostly_media_test() {
    LocalNodeSim::new()
        .with_bandwidth(3_000_000)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("bob", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connection and discover the track",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Bob asks for 720p on a link that comfortably carries it",
                participant: "bob",
                targets: &[("alice", 720)],
            },
            Step::Run {
                description: "Warmup: let the ramp settle",
                duration: Duration::from_secs(30),
            },
            Step::Run {
                description: "Steady state",
                duration: Duration::from_secs(60),
            },
            // Measured here: 99.9%. The old floor was 45%, chosen to sit below the 46% seen in
            // the production capture - which made it unfailable, since nothing this side of a
            // total collapse goes near it. It was also reading a metric that summed every
            // subscriber's forwarded bytes over one viewer's received bytes.
            //
            // 90% is comfortably under what a healthy stream measures and far above the
            // production failure, so it catches a regression toward that failure rather than
            // merely recording it.
            //
            // Note this plan does *not* currently reproduce the production RTX flood: the SFU
            // forwards essentially pure media here. Whatever produced 54% overhead in the capture
            // is not yet modelled, so this guards the property rather than proving the fix.
            Step::Expect {
                description: "Most of what Bob received was video, not overhead",
                participant: "bob",
                property: Property::MediaEfficiencyAtLeast(90),
            },
        ]);
}

/// Capacity that slides instead of stepping, which is what a real link does.
///
/// Every other plan changes bandwidth instantaneously. A controller can handle square waves and
/// still misbehave on a gradual change: a slow decline gives the delay estimator a continuously
/// moving target, and the failure mode is riding the queue down rather than backing off, which
/// shows up as latency long before it shows up as throughput.
///
/// The capacity-relative properties deliberately refuse to run here — on a ramp "the capacity" is
/// not one number — so this asserts the two that remain meaningful: the controller must not park
/// in the bottleneck's buffer, and must not sustain congestion loss.
#[test]
fn estimate_follows_a_sliding_link_without_riding_the_queue_test() {
    LocalNodeSim::new()
        .with_bandwidth(3_000_000)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("bob", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connection and discover the track",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Bob asks for 720p",
                participant: "bob",
                targets: &[("alice", 720)],
            },
            Step::Run {
                description: "Warmup at full capacity",
                duration: Duration::from_secs(30),
            },
            Step::SetCapacity {
                description: "The link slides down to 700 kbps over 40s",
                participant: "bob",
                capacity: Capacity::Ramp {
                    from: 3_000_000,
                    to: 700_000,
                    over: Duration::from_secs(40),
                },
            },
            Step::Run {
                description: "Follow the decline",
                duration: Duration::from_secs(50),
            },
            // A second window, so the assertions describe the settled link rather than the
            // transition. Every counter resets on Run, and during a 4.3x decline a transient
            // queue spike is not a defect - the controller cannot shed rate before the capacity
            // it has not yet observed goes away. What it may not do is stay there.
            Step::Run {
                description: "Settled at the new, lower capacity",
                duration: Duration::from_secs(30),
            },
            Step::Report {
                description: "settled after the decline",
                participant: "bob",
            },
            // Measured settled: 68ms of queue, no congestion loss, and the estimate within 5.7%
            // of the new capacity. During the decline itself the queue hits the full 200ms buffer
            // with 7.2% tail-drop, which is what a window spanning the transition reports - and
            // asserting on that would be asserting that the controller predict a capacity drop
            // before observing it.
            Step::Expect {
                description: "Once settled, the controller is not sitting in the buffer",
                participant: "bob",
                property: Property::QueueingDelayBelow(Duration::from_millis(100)),
            },
            Step::Expect {
                description: "A settled link is not sustaining congestion loss",
                participant: "bob",
                property: Property::CongestionLossBelow(1),
            },
        ]);
}

/// A link that breathes, plus wireless-style burst loss.
///
/// Oscillation and Gilbert-Elliott loss together are the closest thing here to a real congested
/// Wi-Fi link: capacity moves continuously and loss arrives in correlated runs rather than spread
/// evenly. Uniform loss at the same average rate is a materially easier problem, so a controller
/// that only ever sees it is not tested against what it will actually meet.
#[test]
fn estimate_survives_an_oscillating_lossy_link_test() {
    LocalNodeSim::new()
        .with_bandwidth(2_500_000)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("bob", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connection and discover the track",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Bob asks for 720p",
                participant: "bob",
                targets: &[("alice", 720)],
            },
            Step::Run {
                description: "Warmup on a steady link",
                duration: Duration::from_secs(30),
            },
            Step::SetCapacity {
                description: "Capacity breathes between 800 kbps and 2.5 Mbps every 20s",
                participant: "bob",
                capacity: Capacity::Oscillate {
                    min: 800_000,
                    max: 2_500_000,
                    period: Duration::from_secs(20),
                },
            },
            Step::SetLoss {
                description: "Wi-Fi style burst loss",
                participant: "bob",
                loss: Loss::wifi(),
            },
            Step::Run {
                description: "Adapt to the new regime",
                duration: Duration::from_secs(40),
            },
            // Three whole 20s periods, so the window is not weighted toward one phase of the
            // cycle and the first adaptation is excluded. This link never settles, so unlike the
            // ramp there is no steady state to wait for - the claim has to hold while moving.
            Step::Run {
                description: "Three full periods of the oscillation",
                duration: Duration::from_secs(60),
            },
            Step::Report {
                description: "riding the oscillation",
                participant: "bob",
            },
            // Measured across three periods: 4.7% congestion loss, and peak queue reaching the
            // full 200ms buffer during the low phases of the cycle.
            //
            // No peak-queue assertion here, deliberately. Unlike the ramp there is no settled
            // state to measure against: capacity is always moving, so the queue is always either
            // filling or draining, and a peak taken over three periods says only that it touched
            // the buffer once. Whether a controller could avoid that on a link halving every 10s
            // is a real question and not one this plan answers, so pinning a number would be
            // asserting an unexamined opinion.
            //
            // The loss bound is the defensible claim: whatever the queue does transiently, the
            // controller must not sit in overuse. Recorded at 4.7%, bounded at 8% for headroom.
            // See the buffer-saturation follow-up.
            //
            // Also deliberately no media-efficiency assertion: this link drops packets, and that
            // property is not a meaningful ratio under loss. See its doc comment.
            Step::Expect {
                description: "Riding an oscillating link does not sustain congestion loss",
                participant: "bob",
                property: Property::CongestionLossBelow(8),
            },
        ]);
}

/// A still screen share on a healthy link must not talk the estimate down.
///
/// This is the production report, reduced to its smallest form: one publisher, one subscriber,
/// one link, no bandwidth changes and no competing stream. Everything the older recovery plan
/// used to reach this state - a co-tenant, a squeeze, a restore - turned out to be scaffolding
/// around the actual failure rather than part of it.
///
/// # The invariant
///
/// While the source is still, the sender is *application limited*: it is sending a fraction of
/// its layer rate because that is all a static screen share produces, not because the link is
/// full. The link carries 6 Mbps against a 2.5 Mbps layer and drops nothing.
///
/// The headroom is deliberate and had to be raised once the share was modelled at the rate the
/// client actually configures. At 2.5 Mbps on a 3 Mbps link the plan was running at 83%
/// utilisation, where backing off is legitimate and the claim below would be unfair. An estimate that falls under those conditions is
/// reporting the source's activity, not the network's capacity, and the two are unrelated.
///
/// # Why it fails today
///
/// str0m's delay-based rate control backs off to `observed_bitrate * BETA` on a delay increase
/// (`delay/rate_control.rs`, `decrease`). `observed_bitrate` is the acked rate, so under ALR the
/// backoff lands on the application-limited rate:
///
///     estimate=2151619  DelayBasedLimited                in_alr=true
///     estimate= 133499  DelayBasedLimitedDelayIncreased  in_alr=true
///
/// a 94% cut on an idle 3 Mbps link. Recovery is then additive at the 1 kbps floor in the same
/// file, so it takes thousands of feedback batches to climb back - which is what the viewer
/// experiences as a stream that never comes back.
#[test]
fn still_screenshare_does_not_talk_down_a_healthy_link_test() {
    LocalNodeSim::new()
        .with_bandwidth(6_000_000)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::screensharer("presenter"))
                .with_participant(Participant::multi_subscriber("viewer", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connection and discover the track",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Viewer asks for the screen share",
                participant: "viewer",
                targets: &[("presenter", 720)],
            },
            Step::Run {
                description: "Warmup: let the estimate find the link",
                duration: Duration::from_secs(40),
            },
            // The measurement window. The link does not change and the source stays still, so
            // nothing here should move the estimate at all.
            Step::Report {
                description: "end of warmup",
                participant: "viewer",
            },
            Step::Run {
                description: "Sit still on an unchanging link",
                duration: Duration::from_secs(60),
            },
            Step::Report {
                description: "after warmup",
                participant: "viewer",
            },
            // The user-visible consequence first, because it is what a viewer reports: the
            // stream is simply gone. A single-layer screen share has no lower rung to fall to,
            // so once the estimate drops under its cost the slot pauses outright.
            Step::CheckForwardedQuality {
                description: "The screen share is still being forwarded",
                origin: "presenter",
                min_quality: 3,
            },
            // The cause. Demand is ~1.4 Mbps and the link carries 3 Mbps, so the estimate has no
            // business sitting below what was asked for.
            Step::Expect {
                description: "The estimate covers what the viewer asked for",
                participant: "viewer",
                property: Property::EstimateMeetsNeed { percent: 80 },
            },
            // Recovery rather than a drawdown bound: this source is bursty by nature, so the
            // estimate moving is expected. Measured dipping ~40% and returning to within 6% of
            // its peak. The production failure was never the dip - it was that the estimate fell
            // and stayed down while the link was fine, which is what this catches.
            Step::Expect {
                description: "The estimate recovers rather than staying down",
                participant: "viewer",
                property: Property::EstimateRecovers {
                    of_peak_percent: 80,
                },
            },
            Step::Expect {
                description: "Nothing about the link justified a drop",
                participant: "viewer",
                property: Property::CongestionLossBelow(1),
            },
            // Measured at 3 changes/min on a settled stream. The production failure ran to
            // several a second, so this discriminates by two orders of magnitude rather than
            // sitting on a boundary.
            Step::Expect {
                description: "The share holds a layer rather than flapping",
                participant: "viewer",
                property: Property::QualityChangesPerMinuteBelow {
                    origin: "presenter",
                    max: 30,
                },
            },
        ]);
}

/// A stream must come back after the network drops out entirely.
///
/// The most ordinary fault a call meets - a Wi-Fi blip, a handover, a walk past a lift - and
/// until now the one condition no bandwidth plan exercised. Every other plan degrades the link;
/// none of them takes it away.
///
/// An outage is the sharpest form of the deadlock the allocator guards against. Nothing is
/// delivered, so no feedback arrives and the estimate cannot move; when the path returns there is
/// no traffic to re-measure it with, and a controller that waits for evidence before sending will
/// wait forever. The recovery is only observable because something breaks that circularity.
///
/// Deliberately measured after the repair, with the outage in its own window: what is claimed is
/// that the stream returns, not that anything survived the outage itself. Recovery is complete
/// rather than partial - measured at 3.0% off capacity with 1.5% drawdown, i.e. the estimate is
/// re-established rather than merely climbing again.
///
/// # The length is load-bearing
///
/// Six seconds is a blip the peer connection survives. Fifteen does not: ICE gives up, the
/// session is torn down, and the viewer records *no allocation passes at all* afterwards - not a
/// degraded stream but no stream, permanently, because nothing here reconnects it. That is a
/// different failure with a different fix (the client re-establishes the session; see
/// `connectivity::reconnection_recovery_test`), and folding it in here would leave this asserting
/// whichever of the two it happened to hit. This plan is about the congestion controller
/// recovering across an interruption the transport lived through.
#[test]
fn a_stream_returns_after_a_total_outage_test() {
    LocalNodeSim::new()
        .with_bandwidth(3_000_000)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("bob", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish connection and discover the track",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Bob asks for 720p",
                participant: "bob",
                targets: &[("alice", 720)],
            },
            Step::Run {
                description: "Settle on a healthy link",
                duration: Duration::from_secs(40),
            },
            Step::Partition {
                description: "The viewer's network drops out entirely",
                from: "bob",
                to: "server",
            },
            Step::Run {
                description: "Ride out the outage",
                duration: Duration::from_secs(6),
            },
            Step::Repair {
                description: "The network comes back",
                from: "bob",
                to: "server",
            },
            Step::Run {
                description: "Reconnect and re-establish flow",
                duration: Duration::from_secs(45),
            },
            // Fresh window, so the claims describe the recovered call rather than the outage.
            Step::Run {
                description: "Measurement window after recovery",
                duration: Duration::from_secs(30),
            },
            Step::Report {
                description: "after the outage",
                participant: "bob",
            },
            Step::CheckForwardedQuality {
                description: "The stream is being forwarded again",
                origin: "alice",
                min_quality: 3,
            },
            Step::Expect {
                description: "The estimate recovers what the viewer needs",
                participant: "bob",
                property: Property::EstimateMeetsNeed { percent: 70 },
            },
        ]);
}

/// Congestion control is a closed loop, and the return half of it is a network path too.
///
/// Every plan that came before this one configured impairment only on the SFU-to-participant
/// direction, so transport feedback arrived perfectly however bad the forward path was. An
/// estimator validated that way has been tested against half of a real network: it has never seen
/// a TWCC report vanish, arrive out of order, or arrive twice. This asserts the estimate still
/// converges and the stream still holds a layer when the feedback path is as lossy as the media
/// path - which on a mobile uplink it usually is, and worse.
#[test]
fn estimate_converges_when_feedback_is_lossy_test() {
    for seed in QOS_SEEDS {
        let mut link = LinkProfile::cellular();
        link.bandwidth_bps = Some(3_000_000);
        LocalNodeSim::new()
            .with_rng_seed(seed)
            .with_link(link)
            .with_room(
                Room::new("room1")
                    .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                    .with_participant(Participant::manual_subscriber("viewer", 1)),
            )
            .run(vec![
                Step::Run {
                    description: "Establish connection and discover the camera",
                    duration: Duration::from_secs(5),
                },
                Step::SubscribeToQos {
                    description: "Viewer asks for 720p",
                    participant: "viewer",
                    targets: &[("camera", 720, 180, 100)],
                },
                Step::Run {
                    description: "Converge with feedback that is itself being lost and reordered",
                    duration: Duration::from_secs(60),
                },
                Step::Expect {
                    description: "The estimate still finds most of the link through lossy feedback",
                    participant: "viewer",
                    property: Property::EstimateMeetsNeed { percent: 60 },
                },
                Step::Expect {
                    description: "Degraded feedback does not drive the sender into congestion",
                    participant: "viewer",
                    property: Property::CongestionLossBelow(5),
                },
                // Looser than the clean-link plans on purpose. Burst loss in both directions
                // makes shedding a layer and climbing back the *correct* response, and each such
                // cycle is a reversal, so demanding near-zero would assert the link is quiet
                // rather than that the allocator is stable. What must not happen is churn without
                // end; the convergence and congestion claims above pin the rest.
                Step::Expect {
                    description: "The forwarded layer settles rather than churning without end",
                    participant: "viewer",
                    property: Property::QualityReversalsBelow {
                        origin: "camera",
                        max: 5,
                    },
                },
            ]);
    }
}

/// Reordering is a normal internet condition, not a fault, and it is the one this suite has never
/// modelled: packets arriving late enough to overtake their successors.
///
/// The failure it provokes is specific - a receiver that treats a gap as loss will request a
/// keyframe for a packet that was merely late, and a stream that does that repeatedly never holds
/// a decodable run. So the assertions here are about *not over-reacting*: the stream keeps
/// delivering frames and keeps its layer, rather than churning keyframes.
#[test]
fn a_reordering_path_does_not_churn_keyframes_test() {
    for seed in QOS_SEEDS {
        let mut link = LinkProfile::fiber();
        link.bandwidth_bps = Some(3_000_000);
        // Markedly worse than the wifi default, so the assertion is about tolerating reordering rather
        // than about whether it happened to occur.
        link.reorder = Reorder {
            probability: 0.03,
            delay: Duration::from_millis(40),
        };
        link.duplicate = 0.01;
        LocalNodeSim::new()
            .with_rng_seed(seed)
            .with_link(link)
            .with_room(
                Room::new("room1")
                    .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                    .with_participant(Participant::manual_subscriber("viewer", 1)),
            )
            .run(vec![
                Step::Run {
                    description: "Establish connection and discover the camera",
                    duration: Duration::from_secs(5),
                },
                Step::SubscribeToQos {
                    description: "Viewer asks for 720p",
                    participant: "viewer",
                    targets: &[("camera", 720, 180, 100)],
                },
                Step::Run {
                    description: "Run on a path that reorders and duplicates",
                    duration: Duration::from_secs(60),
                },
                Step::CheckForwardedQualityReached {
                    description: "The camera still reaches its top layer despite reordering",
                    origin: "camera",
                    min_quality: 3,
                },
                Step::Expect {
                    description: "Late packets are not mistaken for congestion",
                    participant: "viewer",
                    property: Property::CongestionLossBelow(5),
                },
                Step::Expect {
                    description: "A late packet does not cost the stream its layer",
                    participant: "viewer",
                    property: Property::QualityReversalsBelow {
                        origin: "camera",
                        max: 1,
                    },
                },
            ]);
    }
}

/// Determinism is the guarantee every other plan in this suite rests on.
///
/// A threshold means nothing if the same plan produces different numbers each run, and a
/// regression cannot be attributed if a failure might just be this run. The clock and OS
/// randomness are both overridden process-wide (`sim_clock`, `sim_rand`) precisely so that holds,
/// but nothing asserted it, so a change that reintroduced a real-clock or real-entropy read would
/// have shown up only as thresholds slowly becoming unreliable.
///
/// This runs one plan twice under the same seed and demands the measurements agree exactly - not
/// approximately, since any tolerance here would hide the very drift it exists to catch.
#[test]
fn a_plan_measures_identically_when_replayed_test() {
    let first = deterministic_probe_plan(0x5EED_0001);
    let second = deterministic_probe_plan(0x5EED_0001);

    assert_eq!(
        deterministic_core(&first),
        deterministic_core(&second),
        "the same plan under the same seed produced different measurements, so the simulation is \
         reading a clock or an entropy source it does not control; every threshold in this suite \
         is unreliable until that is fixed"
    );
}

/// The part of a report that must be reproducible, rendered for comparison.
///
/// `max_backlog` is deliberately excluded. It is the shaper's peak queue occupancy, and the shaper
/// releases queued packets on the next send attempt rather than from a timer, so its depth is
/// sampled wherever the event loop happened to look. Measured across two replays of this plan,
/// every other figure agreed exactly while backlog read 22.032ms and 19.36ms. That makes it a
/// quantisation artifact rather than evidence of an uncontrolled clock, so it is named here as a
/// known exclusion instead of being hidden behind a tolerance that would also mask a real drift.
///
/// Nothing else may be added to this list without the same kind of evidence.
fn deterministic_core(report: &LinkReport) -> String {
    format!(
        "window={:?} received={} forwarded={} samples={} estimate={}/{}/{} drawdown={:.6} \
         demand={}/{}/{} delivered={} congestion_drops={} link_loss={} changes={:?} \
         reversals={:?} quality={:?}",
        report.window,
        report.received_bytes,
        report.forwarded_media_bytes,
        report.samples,
        report.estimate_min_bps,
        report.estimate_last_bps,
        report.estimate_max_bps,
        report.worst_drawdown_percent,
        report.demand_min_bps,
        report.demand_last_bps,
        report.demand_max_bps,
        report.delivered_packets,
        report.congestion_drops,
        report.link_loss_drops,
        report.quality_changes,
        report.quality_reversals,
        report.forwarded_quality,
    )
}

/// The counterpart claim: the seed is actually an input.
///
/// If replays agreed because the plan is insensitive to scheduling rather than because it is
/// controlled, the test above would pass while asserting nothing. Two different seeds must produce
/// two different networks.
#[test]
fn a_different_seed_is_a_different_network_test() {
    let first = deterministic_probe_plan(0x5EED_0001);
    let second = deterministic_probe_plan(0x5EED_0002);

    assert_ne!(
        deterministic_core(&first),
        deterministic_core(&second),
        "two seeds produced byte-identical measurements, so the seed is not reaching the \
         simulation and running plans under several of them proves nothing"
    );
}

/// A plan with enough going on - loss, reordering, contention, an allocation decision - that any
/// uncontrolled input would move at least one of the numbers it reports.
fn deterministic_probe_plan(seed: u64) -> LinkReport {
    LocalNodeSim::new()
        .with_rng_seed(seed)
        .with_link(LinkProfile::wifi())
        .with_bandwidth(2_000_000)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                .with_participant(Participant::publisher("cotenant", &["q", "h", "f"]))
                .with_participant(Participant::manual_subscriber("viewer", 2)),
        )
        .run_collecting(vec![
            Step::Run {
                description: "Establish connections and discover both cameras",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeToQos {
                description: "Both streams wanted, the camera with priority",
                participant: "viewer",
                targets: &[("camera", 720, 180, 100), ("cotenant", 360, 0, 10)],
            },
            Step::Run {
                description: "Contend for a link that cannot carry both at full quality",
                duration: Duration::from_secs(40),
            },
        ])
        .get("viewer")
        .cloned()
        .expect("the viewer should have been measured")
}

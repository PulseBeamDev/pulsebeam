use super::common::{LinkProfile, LocalNodeSim, Participant, Property, Room, Step, VideoQuality};
use std::time::Duration;

fn cross_shard_video_room() -> super::common::Room {
    super::common::Room::new("cross-shard-video")
        .with_participant(super::common::Participant::single_publisher("publisher"))
        .with_participant(super::common::Participant::subscriber("subscriber"))
}

#[test]
fn video_does_not_loop_back_to_publisher_test() {
    LocalNodeSim::new()
        .with_room(
            super::common::Room::new("video-no-loopback")
                .with_participant(Participant::publisher("publisher", &["q"]).and_subscribes())
                .with_participant(Participant::subscriber("viewer")),
        )
        .run(vec![
            Step::Run {
                description: "Establish the publisher and viewer",
                duration: Duration::from_secs(5),
            },
            Step::Run {
                description: "Forward the track to the viewer",
                duration: Duration::from_secs(5),
            },
            Step::CheckVideoQuality {
                description: "Viewer receives the publisher video",
                participant: "viewer",
                quality: VideoQuality::min_frames(50).allow_gaps(5),
            },
            Step::CheckVideoNotReceivedFrom {
                description: "Publisher receives no looped-back video",
                participant: "publisher",
                publisher: "publisher",
            },
        ]);
}

#[test]
fn quality_fixture_is_proven_from_each_viewers_decoded_output_test() {
    LocalNodeSim::new()
        .with_link(LinkProfile::fiber())
        .with_room(
            Room::new("decoded-quality-fixture")
                .with_participant(Participant::quality_publisher_source(
                    "publisher-a",
                    pulsebeam_testdata::QualityVideoSource::Zero,
                ))
                .with_participant(Participant::quality_publisher_source(
                    "publisher-b",
                    pulsebeam_testdata::QualityVideoSource::One,
                ))
                .with_participant(Participant::manual_subscriber("viewer-a", 1))
                .with_participant(Participant::manual_subscriber("viewer-b", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish the quality publisher and discover its fixture track",
                duration: Duration::from_secs(1),
            },
            Step::SubscribeTo {
                description: "Activate the first viewer route",
                participant: "viewer-a",
                targets: &[("publisher-a", 180)],
            },
            Step::SubscribeTo {
                description: "Activate the second viewer route",
                participant: "viewer-b",
                targets: &[("publisher-b", 720)],
            },
            Step::Run {
                description: "Deliver the deterministic fixture to both viewers",
                duration: Duration::from_secs(4),
            },
            Step::CheckVideoQuality {
                description: "First viewer decodes fixture pixels at source resolution",
                participant: "viewer-a",
                quality: VideoQuality::min_frames(30).fixture_fidelity((320, 180), 12, 240),
            },
            Step::CheckVideoQuality {
                description: "Second viewer independently decodes fixture pixels at source resolution",
                participant: "viewer-b",
                quality: VideoQuality::min_frames(30).fixture_fidelity((1280, 720), 12, 240),
            },
            Step::CheckForwardingLatency {
                description: "Packet lifecycle stays bounded and decomposes across every forwarding stage",
                min_samples: 30,
                max_service: Duration::from_millis(40),
                max_egress_lateness: Duration::from_millis(1),
                max_total: Duration::from_millis(150),
            },
            Step::CheckFirstDecodedFrame {
                description: "First viewer renders promptly after activation",
                participant: "viewer-a",
                max_latency: Duration::from_millis(175),
            },
            Step::CheckFirstDecodedFrame {
                description: "Second viewer renders promptly after activation",
                participant: "viewer-b",
                max_latency: Duration::from_millis(175),
            },
        ]);
}

#[test]
fn two_minute_quality_fixture_soak_never_blanks_or_freezes_test() {
    LocalNodeSim::new()
        .with_link(LinkProfile::fiber())
        .with_room(
            Room::new("two-minute-decoded-soak")
                .with_participant(Participant::quality_publisher("publisher"))
                .with_participant(Participant::manual_subscriber("viewer", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish the quality source and viewer",
                duration: Duration::from_secs(1),
            },
            Step::SubscribeTo {
                description: "Select the inexpensive layer for the long decoder gate",
                participant: "viewer",
                targets: &[("publisher", 180)],
            },
            Step::Run {
                description: "Complete the bounded activation transition",
                duration: Duration::from_secs(1),
            },
            Step::Run {
                description: "Decode continuously across twenty-four maintenance intervals and forty fixture epochs",
                duration: Duration::from_secs(120),
            },
            Step::CheckVideoQualityInterval {
                description: "Every steady-state picture remains the requested fixture with no visible freeze",
                participant: "viewer",
                quality: VideoQuality::min_frames(3_400)
                    .fixture_fidelity((320, 180), 12, 240)
                    .exact_decoded_resolution((320, 180))
                    .max_frame_gap(Duration::from_millis(100)),
            },
            Step::CheckVideoReceivedFromInterval {
                description: "Decoded progress belongs to the selected publisher throughout the soak",
                participant: "viewer",
                publisher: "publisher",
                min_frames: 3_400,
            },
            Step::CheckFirstDecodedFrame {
                description: "The activation envelope produces a renderable frame promptly",
                participant: "viewer",
                max_latency: Duration::from_millis(175),
            },
            Step::CheckKeyframeRequestsInterval {
                description: "The steady-state decoder window does not hide a PLI loop",
                participant: "publisher",
                max: 0,
            },
        ]);
}

#[test]
fn independent_viewer_capacities_keep_their_requested_simulcast_quality_test() {
    LocalNodeSim::new()
        .with_link(LinkProfile::fiber())
        .with_room(
            Room::new("independent-viewer-simulcast-quality")
                .with_participant(Participant::publisher("publisher", &["q", "h", "f"]))
                .with_participant(Participant::manual_subscriber("high-viewer", 1))
                .with_participant(Participant::manual_subscriber("low-viewer", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish the publisher and both independent viewers",
                duration: Duration::from_secs(5),
            },
            Step::SetBandwidth {
                description: "Constrain only the low-resolution viewer",
                participant: "low-viewer",
                bits_per_sec: 600_000,
            },
            Step::SubscribeTo {
                description: "High-capacity viewer requests the full encoding",
                participant: "high-viewer",
                targets: &[("publisher", 720)],
            },
            Step::SubscribeTo {
                description: "Constrained viewer requests the quarter encoding",
                participant: "low-viewer",
                targets: &[("publisher", 180)],
            },
            Step::Run {
                description: "Let each viewer settle on its independently allocated encoding",
                duration: Duration::from_secs(10),
            },
            Step::Run {
                description: "Measure both viewers after the simulcast ramp is complete",
                duration: Duration::from_secs(4),
            },
            Step::CheckVideoQualityInterval {
                description: "High-capacity viewer continuously decodes full-resolution video",
                participant: "high-viewer",
                quality: VideoQuality::min_frames(100)
                    .min_decoded_resolution((1280, 720))
                    .max_frame_gap(Duration::from_millis(100)),
            },
            Step::CheckVideoQualityInterval {
                description: "Constrained viewer continuously decodes its requested low-resolution video",
                participant: "low-viewer",
                quality: VideoQuality::min_frames(100)
                    .min_decoded_resolution((320, 180))
                    .max_frame_gap(Duration::from_millis(100)),
            },
        ]);
}

#[test]
fn three_viewers_decode_exact_independent_simulcast_layers_test() {
    LocalNodeSim::new()
        .with_link(LinkProfile::fiber())
        .with_room(
            Room::new("exact-three-layer-viewers")
                .with_participant(Participant::quality_publisher("publisher"))
                .with_participant(Participant::manual_subscriber("low-viewer", 1))
                .with_participant(Participant::manual_subscriber("mid-viewer", 1))
                .with_participant(Participant::manual_subscriber("high-viewer", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish the publisher and three viewers",
                duration: Duration::from_secs(2),
            },
            Step::SubscribeTo {
                description: "Pin the low viewer to 180p",
                participant: "low-viewer",
                targets: &[("publisher", 180)],
            },
            Step::SubscribeTo {
                description: "Pin the middle viewer to 360p",
                participant: "mid-viewer",
                targets: &[("publisher", 360)],
            },
            Step::SubscribeTo {
                description: "Pin the high viewer to 720p",
                participant: "high-viewer",
                targets: &[("publisher", 720)],
            },
            Step::Run {
                description: "Let every independent path reach its requested layer",
                duration: Duration::from_secs(8),
            },
            Step::Run {
                description: "Measure steady decoded progress on every layer",
                duration: Duration::from_secs(4),
            },
            Step::Expect {
                description: "High viewer has capacity evidence for its requested layer",
                participant: "high-viewer",
                property: Property::EstimateMeetsNeed { percent: 90 },
            },
            Step::CheckVideoQualityInterval {
                description: "Low viewer decodes only the engineered 180p layer",
                participant: "low-viewer",
                quality: VideoQuality::min_frames(90)
                    .exact_decoded_resolution((320, 180))
                    .max_frame_gap(Duration::from_millis(100)),
            },
            Step::CheckVideoQualityInterval {
                description: "Middle viewer decodes only the engineered 360p layer",
                participant: "mid-viewer",
                quality: VideoQuality::min_frames(90)
                    .exact_decoded_resolution((640, 360))
                    .max_frame_gap(Duration::from_millis(100)),
            },
            Step::CheckVideoQualityInterval {
                description: "High viewer decodes only the engineered 720p layer",
                participant: "high-viewer",
                quality: VideoQuality::min_frames(90)
                    .exact_decoded_resolution((1280, 720))
                    .max_frame_gap(Duration::from_millis(100)),
            },
        ]);
}

#[test]
fn one_viewer_switches_every_simulcast_layer_decodably_test() {
    LocalNodeSim::new()
        .with_link(LinkProfile::fiber())
        .with_room(
            Room::new("decoded-layer-switches")
                .with_participant(Participant::quality_publisher("publisher"))
                .with_participant(Participant::manual_subscriber("viewer", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish the publisher and viewer",
                duration: Duration::from_secs(2),
            },
            Step::SubscribeTo {
                description: "Start at 180p",
                participant: "viewer",
                targets: &[("publisher", 180)],
            },
            Step::Run {
                description: "Cross the initial low-layer keyframe boundary",
                duration: Duration::from_secs(1),
            },
            Step::Run {
                description: "Measure steady decoded progress on the low layer",
                duration: Duration::from_secs(4),
            },
            Step::CheckVideoQualityInterval {
                description: "Low layer is exact and continuously decodable",
                participant: "viewer",
                quality: VideoQuality::min_frames(60)
                    .all_forwarded_frames()
                    .exact_decoded_resolution((320, 180))
                    .max_frame_gap(Duration::from_millis(102)),
            },
            Step::SubscribeTo {
                description: "Switch to 360p",
                participant: "viewer",
                targets: &[("publisher", 360)],
            },
            Step::Run {
                description: "Cross a complete middle-layer keyframe boundary",
                duration: Duration::from_secs(1),
            },
            Step::Run {
                description: "Measure steady decoded progress on the middle layer",
                duration: Duration::from_secs(4),
            },
            Step::CheckVideoQualityInterval {
                description: "Middle layer is exact and continuously decodable",
                participant: "viewer",
                quality: VideoQuality::min_frames(60)
                    .all_forwarded_frames()
                    .exact_decoded_resolution((640, 360))
                    .max_frame_gap(Duration::from_millis(102)),
            },
            Step::SubscribeTo {
                description: "Switch to 720p",
                participant: "viewer",
                targets: &[("publisher", 720)],
            },
            Step::Run {
                description: "Cross a complete high-layer keyframe boundary",
                duration: Duration::from_secs(1),
            },
            Step::Run {
                description: "Measure steady decoded progress on the high layer",
                duration: Duration::from_secs(4),
            },
            Step::CheckVideoQualityInterval {
                description: "High layer is exact and continuously decodable",
                participant: "viewer",
                quality: VideoQuality::min_frames(60)
                    .all_forwarded_frames()
                    .exact_decoded_resolution((1280, 720))
                    .max_frame_gap(Duration::from_millis(102)),
            },
            Step::SubscribeTo {
                description: "Switch back to 180p",
                participant: "viewer",
                targets: &[("publisher", 180)],
            },
            Step::Run {
                description: "Cross a fresh low-layer keyframe boundary",
                duration: Duration::from_secs(1),
            },
            Step::Run {
                description: "Measure steady decoded progress after returning low",
                duration: Duration::from_secs(4),
            },
            Step::CheckVideoQualityInterval {
                description: "The return to low is exact and continuously decodable",
                participant: "viewer",
                quality: VideoQuality::min_frames(60)
                    .all_forwarded_frames()
                    .exact_decoded_resolution((320, 180))
                    .max_frame_gap(Duration::from_millis(102)),
            },
        ]);
}

#[test]
fn source_switch_is_decodable_and_does_not_disturb_stable_viewers_test() {
    LocalNodeSim::new()
        .with_link(LinkProfile::fiber())
        .with_room(
            Room::new("decoded-source-switches")
                .with_participant(Participant::quality_publisher_source(
                    "publisher-a",
                    pulsebeam_testdata::QualityVideoSource::Zero,
                ))
                .with_participant(Participant::quality_publisher_source(
                    "publisher-b",
                    pulsebeam_testdata::QualityVideoSource::One,
                ))
                .with_participant(Participant::manual_subscriber("switching-viewer", 1))
                .with_participant(Participant::manual_subscriber("stable-viewer", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish every source layer before testing route isolation",
                duration: Duration::from_secs(10),
            },
            Step::SubscribeTo {
                description: "Start the switching viewer on source A",
                participant: "switching-viewer",
                targets: &[("publisher-a", 180)],
            },
            Step::SubscribeTo {
                description: "Keep the stable viewer on source A",
                participant: "stable-viewer",
                targets: &[("publisher-a", 720)],
            },
            Step::Run {
                description: "Decode source A on both independent routes",
                duration: Duration::from_secs(4),
            },
            Step::CheckVideoReceivedFromInterval {
                description: "The switching viewer receives source A",
                participant: "switching-viewer",
                publisher: "publisher-a",
                min_frames: 60,
            },
            Step::CheckVideoQualityInterval {
                description: "The switching viewer decodes source A at 180p",
                participant: "switching-viewer",
                quality: VideoQuality::min_frames(60).exact_decoded_resolution((320, 180)),
            },
            Step::SubscribeToQos {
                description: "Switch the route to source B",
                participant: "switching-viewer",
                targets: &[("publisher-b", 360, 0, 100)],
            },
            Step::Run {
                description: "Decode source B while the other route stays on A",
                duration: Duration::from_secs(4),
            },
            Step::CheckVideoReceivedFromInterval {
                description: "The switching viewer receives source B",
                participant: "switching-viewer",
                publisher: "publisher-b",
                min_frames: 60,
            },
            Step::CheckVideoQualityInterval {
                description: "The switching viewer decodes source B at 360p",
                participant: "switching-viewer",
                quality: VideoQuality::min_frames(60).exact_decoded_resolution((640, 360)),
            },
            Step::CheckVideoReceivedFromInterval {
                description: "The stable viewer keeps receiving source A",
                participant: "stable-viewer",
                publisher: "publisher-a",
                min_frames: 60,
            },
            Step::CheckVideoQualityInterval {
                description: "The stable viewer remains on source A at 720p",
                participant: "stable-viewer",
                quality: VideoQuality::min_frames(60).exact_decoded_resolution((1280, 720)),
            },
            Step::SubscribeToQos {
                description: "Switch the route back to source A",
                participant: "switching-viewer",
                targets: &[("publisher-a", 720, 0, 200)],
            },
            Step::Run {
                description: "Decode a fresh source A keyframe on the returning route",
                duration: Duration::from_secs(4),
            },
            Step::CheckVideoReceivedFromInterval {
                description: "The switching viewer receives source A again",
                participant: "switching-viewer",
                publisher: "publisher-a",
                min_frames: 60,
            },
            Step::CheckVideoQualityInterval {
                description: "The returning source A route decodes at 720p",
                participant: "switching-viewer",
                quality: VideoQuality::min_frames(60).exact_decoded_resolution((1280, 720)),
            },
            Step::CheckVideoReceivedFromInterval {
                description: "The stable source A route continues independently",
                participant: "stable-viewer",
                publisher: "publisher-a",
                min_frames: 60,
            },
            Step::CheckVideoQualityInterval {
                description: "The stable viewer remains exact through both switches",
                participant: "stable-viewer",
                quality: VideoQuality::min_frames(60).exact_decoded_resolution((1280, 720)),
            },
        ]);
}

#[test]
fn subscription_activation_delivers_a_decoded_fixture_frame_without_slow_poll_test() {
    LocalNodeSim::new()
        .with_link(LinkProfile::fiber())
        .with_room(
            Room::new("immediate-decoder-activation")
                .with_participant(Participant::quality_publisher("publisher"))
                .with_participant(Participant::manual_subscriber("viewer", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish the publisher and discover its fixture track",
                duration: Duration::from_secs(1),
            },
            Step::SubscribeTo {
                description: "Activate the viewer route",
                participant: "viewer",
                targets: &[("publisher", 720)],
            },
            Step::Run {
                description: "Deliver the controlled source keyframe",
                duration: Duration::from_secs(1),
            },
            Step::CheckFirstDecodedFrame {
                description: "Initial activation reaches a real decoder promptly",
                participant: "viewer",
                max_latency: Duration::from_millis(175),
            },
            Step::SubscribeTo {
                description: "Deactivate the viewer route",
                participant: "viewer",
                targets: &[("publisher", 0)],
            },
            Step::Run {
                description: "Settle after deactivation",
                duration: Duration::from_millis(200),
            },
            Step::SubscribeTo {
                description: "Reactivate the viewer route",
                participant: "viewer",
                targets: &[("publisher", 720)],
            },
            Step::Run {
                description: "Deliver the next controlled source keyframe",
                duration: Duration::from_secs(1),
            },
            Step::CheckFirstDecodedFrame {
                description: "Reactivation reaches a real decoder promptly",
                participant: "viewer",
                max_latency: Duration::from_millis(175),
            },
            Step::CheckVideoQuality {
                description: "The reactivated viewer decodes source-resolution fixture pixels",
                participant: "viewer",
                quality: VideoQuality::min_frames(15).fixture_fidelity((320, 180), 12, 240),
            },
        ]);
}

fn bench_participant(name: &'static str) -> Participant {
    let mut participant = Participant::single_publisher(name).and_subscribes();
    participant.slots = 7;
    participant
}

#[test]
fn remote_bench_room_joiners_render_every_other_participant() {
    LocalNodeSim::new()
        .with_room(
            Room::new("bench-room")
                .with_participant(bench_participant("alice"))
                .with_participant(Participant {
                    starts_disconnected: true,
                    ..bench_participant("bob")
                })
                .with_participant(Participant {
                    starts_disconnected: true,
                    ..bench_participant("carol")
                })
                .with_participant(Participant {
                    starts_disconnected: true,
                    ..bench_participant("dave")
                }),
        )
        .run(vec![
            Step::Run {
                description: "Establish the first remote bench participant",
                duration: Duration::from_secs(2),
            },
            Step::Join {
                description: "Bob joins within the bench join spread",
                participant: "bob",
            },
            Step::Run {
                description: "Let Bob publish and subscribe",
                duration: Duration::from_secs(1),
            },
            Step::Join {
                description: "Carol joins within the bench join spread",
                participant: "carol",
            },
            Step::Run {
                description: "Let Carol publish and subscribe",
                duration: Duration::from_secs(1),
            },
            Step::Join {
                description: "Dave joins within the bench join spread",
                participant: "dave",
            },
            Step::Run {
                description: "Converge the remote four-way bench room",
                duration: Duration::from_secs(12),
            },
            Step::CheckVideoReceivedFrom {
                description: "Alice renders Bob",
                participant: "alice",
                publisher: "bob",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Alice renders Carol",
                participant: "alice",
                publisher: "carol",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Alice renders Dave",
                participant: "alice",
                publisher: "dave",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Bob renders Alice",
                participant: "bob",
                publisher: "alice",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Bob renders Carol",
                participant: "bob",
                publisher: "carol",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Bob renders Dave",
                participant: "bob",
                publisher: "dave",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Carol renders Alice",
                participant: "carol",
                publisher: "alice",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Carol renders Bob",
                participant: "carol",
                publisher: "bob",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Carol renders Dave",
                participant: "carol",
                publisher: "dave",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Dave renders Alice",
                participant: "dave",
                publisher: "alice",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Dave renders Bob",
                participant: "dave",
                publisher: "bob",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Dave renders Carol",
                participant: "dave",
                publisher: "carol",
                min_frames: 30,
            },
        ]);
}

#[test]
/// Replays a failing run with `PULSEBEAM_SIM_SEED=<seed>` from the test output.
fn cross_shard_stats_reach_the_subscriber_allocator() {
    super::common::LocalNodeSim::new()
        .with_room(cross_shard_video_room())
        .run(vec![
            super::common::Step::Run {
                description: "converge publisher telemetry across the shard boundary",
                duration: Duration::from_secs(12),
            },
            super::common::Step::CheckForwardedQualityReached {
                description: "subscriber quality responds to publisher stats",
                origin: "publisher",
                min_quality: 1,
            },
        ]);
}

/// Replays a failing run with `PULSEBEAM_SIM_SEED=<seed>` from the test output.
#[test]
fn cross_shard_keyframe_reaches_the_publisher() {
    super::common::LocalNodeSim::new()
        .with_room(
            super::common::Room::new("cross-shard-keyframe")
                .with_participant(super::common::Participant::publisher(
                    "publisher",
                    &["q", "h", "f"],
                ))
                .with_participant(super::common::Participant::subscriber("subscriber")),
        )
        .run(vec![
            super::common::Step::Run {
                description: "establish a cross-shard simulcast stream",
                duration: Duration::from_secs(8),
            },
            super::common::Step::SetBandwidth {
                description: "force a layer change that needs a fresh keyframe",
                participant: "subscriber",
                bits_per_sec: 250_000,
            },
            super::common::Step::Run {
                description: "allow the reverse feedback route to carry the request",
                duration: Duration::from_secs(8),
            },
            super::common::Step::CheckCrossShardMedia {
                description: "the request belongs to a genuinely cross-shard stream",
                min_frames: 100,
            },
            super::common::Step::CheckKeyframeRequestsAtLeast {
                description: "the publisher receives the cross-shard keyframe request",
                participant: "publisher",
                min: 1,
            },
        ]);
}

/// A publisher that attaches a synthetic L1T3 Dependency Descriptor to every
/// frame flows end-to-end and the subscriber decodes it — exercising the agent's
/// DD emission and the SFU's DD-aware forwarder (parse + Full-target forward)
/// against the egress invariants. Shedding is unit/allocation-tested; here the
/// point is that carrying DD never breaks the stream.
#[test]
fn dependency_descriptor_stream_is_forwarded_decodably_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q"]).with_temporal_dd(3))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Alice publishes an L1T3 DD stream; Bob subscribes and decodes",
                duration: Duration::from_secs(10),
            },
            Step::CheckVideoQuality {
                description: "Bob decodes the DD-annotated stream end to end",
                participant: "bob",
                quality: VideoQuality::min_frames(150).allow_gaps(3),
            },
        ]);
}

/// A single scalable (DD) encoding keeps delivering frames through a downlink
/// squeeze. With a minimum-height floor the slot cannot be dropped, so a broken DD
/// forwarding path would starve the subscriber — this asserts it does not. The
/// precise base-layer-degrade decision and the shed stream's decodability are
/// pinned by unit tests (`base_layer_degrade_*`, `dd_shedding_to_a_lower_target_*`);
/// this is the end-to-end smoke that carrying DD never breaks the stream under BWE
/// pressure.
#[test]
fn dependency_descriptor_stream_survives_congestion_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q"]).with_temporal_dd(3))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish flow and discover the DD track",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeToQos {
                description: "Bob keeps a floor so the slot must forward something, not drop it",
                participant: "bob",
                targets: &[("alice", 720, 180, 1)],
            },
            Step::Run {
                description: "Soak at full quality",
                duration: Duration::from_secs(5),
            },
            Step::SetBandwidth {
                description: "Squeeze the downlink so full quality no longer fits",
                participant: "bob",
                bits_per_sec: 350_000,
            },
            Step::Run {
                description: "Let the allocator degrade the scalable stream to its base layer",
                duration: Duration::from_secs(12),
            },
            Step::CheckVideoQuality {
                description: "Bob keeps receiving renderable frames through the squeeze",
                participant: "bob",
                quality: VideoQuality::min_frames(60).allow_gaps(30),
            },
        ]);
}

/// SFrame/E2EE: the publisher's payload is opaque, so the SFU's H.264 probe finds
/// no IDR, SPS, or PPS — the Dependency Descriptor is the only forwarding signal.
/// A subscribe forces a switch-up, which must replay the DD keyframe segment
/// despite the opaque payload; before the DD-native keyframe/cache path that
/// replay returned nothing and the subscriber would have been starved. This is the
/// end-to-end proof that the SFU forwards on DD alone.
#[test]
fn opaque_dependency_descriptor_stream_forwards_on_dd_alone_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q"]).with_opaque_dd(3))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Alice publishes an opaque (E2EE) L1T3 DD stream; Bob subscribes",
                duration: Duration::from_secs(10),
            },
            Step::CheckRxBytes {
                description: "Bob receives opaque forwarded media",
                participant: "bob",
                min_bytes: 10_000,
            },
            Step::CheckKeyframeRequests {
                // If the encrypted stream did not forward decodably, the SFU/decoder
                // would hammer the publisher with PLIs (the fps→1 "storm" seen in
                // the browser). A healthy stream needs only a handful.
                description: "no PLI storm on the opaque stream",
                participant: "alice",
                max: 10,
            },
        ]);
}

/// The encrypted-frame path under real loss — the conditions that surfaced the
/// browser's fps→1 collapse and constant-PLI storm. The SFU forwards on the DD
/// alone (opaque payload), the subscriber reassembles from raw RTP, and both the
/// frame rate must hold and keyframe requests must stay bounded. This is the sim
/// guard for the whole class of "DD-only + encrypted" reassembly bugs.
#[test]
fn opaque_dependency_descriptor_holds_framerate_under_loss_test() {
    LocalNodeSim::new()
        .with_link(LinkProfile::cellular())
        .with_room(
            Room::new("room1")
                .with_participant(
                    Participant::publisher("alice", &["q", "h", "f"]).with_opaque_dd(3),
                )
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish the opaque stream over lossy cellular",
                duration: Duration::from_secs(20),
            },
            Step::SubscribeAll {
                description: "Bob subscribes at full quality",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Soak: frames must keep flowing, decoder must not stall",
                duration: Duration::from_secs(30),
            },
            Step::CheckRxBytesInterval {
                description: "opaque media keeps flowing through the cellular soak",
                participant: "bob",
                min_bytes: 10_000,
            },
            Step::CheckKeyframeRequests {
                description: "no PLI storm even under loss",
                participant: "alice",
                max: 40,
            },
        ]);
}

/// Mixed room: a DD publisher and a marker-only subscriber that never negotiates
/// the DD extension. The SFU makes its forwarding decisions from the ingress DD and
/// the subscriber simply receives standard (possibly shed) media — DD support on the
/// receive leg is not required. Asserts the stream flows end-to-end.
#[test]
fn dd_publisher_streams_to_a_marker_only_subscriber_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q"]).with_temporal_dd(3))
                .with_participant(Participant::subscriber("bob").marker_only()),
        )
        .run(vec![
            Step::Run {
                description: "Alice publishes a DD stream; a marker-only Bob subscribes",
                duration: Duration::from_secs(10),
            },
            Step::CheckVideoQuality {
                description: "Bob decodes without negotiating DD on his receive leg",
                participant: "bob",
                quality: VideoQuality::min_frames(150).allow_gaps(3),
            },
        ]);
}

/// Mixed room: a legacy marker-only publisher (no DD) and a DD-capable subscriber.
/// With no ingress DD the SFU forwards via the marker/deep-inspection path; the
/// subscriber's DD support is simply unused. Asserts the legacy path is unaffected
/// by DD being negotiated on the receive side.
#[test]
fn marker_only_publisher_streams_to_a_dd_subscriber_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]).marker_only())
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "A marker-only Alice publishes; a DD-capable Bob subscribes",
                duration: Duration::from_secs(10),
            },
            Step::CheckVideoQuality {
                description: "Bob decodes the legacy stream end to end",
                participant: "bob",
                quality: VideoQuality::min_frames(150).allow_gaps(3),
            },
        ]);
}

#[test]
fn fast_initial_ramp_up_on_good_network_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "2s ramp window",
                duration: Duration::from_secs(2),
            },
            Step::CheckTxBytesInterval {
                description: "Alice reached ≥400kbps within 2s",
                participant: "alice",
                min_bytes: 100_000,
            },
            Step::Run {
                description: "18s soak at floor bitrate",
                duration: Duration::from_secs(18),
            },
            Step::CheckTxBytesInterval {
                description: "Alice never collapsed below 250kbps floor over 18s",
                participant: "alice",
                min_bytes: 562_500,
            },
            Step::CheckVideoQuality {
                description: "Bob received renderable frames throughout ramp",
                participant: "bob",
                quality: VideoQuality::min_frames(200).allow_gaps(3),
            },
        ]);
}

#[test]
fn repeated_simulcast_switching_stays_decodable_test() {
    LocalNodeSim::new()
        .with_link(LinkProfile::fiber())
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish initial flow and let highest layer settle",
                duration: Duration::from_secs(15),
            },
            Step::SubscribeAll {
                description: "Subscribe at 720p",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Accumulate frames at 720p",
                duration: Duration::from_secs(10),
            },
            Step::SubscribeAll {
                description: "Switch 1: 180p",
                participant: "bob",
                heights: &[180],
            },
            Step::Run {
                description: "Settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 2: 720p",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 3: 180p",
                participant: "bob",
                heights: &[180],
            },
            Step::Run {
                description: "Settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 4: 720p",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Cross the final high-layer keyframe boundary",
                duration: Duration::from_secs(1),
            },
            Step::Run {
                description: "Prove the final high-resolution switch stays continuously decodable",
                duration: Duration::from_secs(5),
            },
            Step::CheckVideoQualityInterval {
                description: "Frames remain continuously decodable at the requested high resolution",
                participant: "bob",
                quality: VideoQuality::min_frames(100).min_decoded_resolution((1280, 720)),
            },
        ]);
}

// The EgressGuard backward-timestamp violation is fixed (cache monotonicity
// check + push() frontier filter).  The remaining failure is a separate
// pre-existing bug: PLI for the layer switch targets a mid/rid that no longer
// exists on the publisher side, so no fresh keyframe is delivered and bob gets
// 0 bytes throughout the soak.
#[test]
fn simulcast_stream_stability_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Warmup: establish initial flow",
                duration: Duration::from_secs(5),
            },
            Step::Run {
                description: "60s soak: stream must stay alive",
                duration: Duration::from_secs(60),
            },
            Step::CheckRxBytesInterval {
                description: "Bob received steady throughput across the full soak",
                participant: "bob",
                min_bytes: 75_000,
            },
            Step::CheckVideoQuality {
                description: "Bob received renderable frames throughout soak",
                participant: "bob",
                quality: VideoQuality::min_frames(500).allow_gaps(5),
            },
        ]);
}

/// Congestion control across a shard boundary.
///
/// The rest of this suite runs single-shard, where the allocator reads a
/// publisher's measurements from a struct on its own core. Split across shards
/// those same measurements become `ShardFrame::Stats` messages and the
/// keyframe requests they provoke become reverse-lane frames, so the feedback
/// loop that degrades a stream is assembled from parts that cross a core
/// boundary and arrive late or not at all.
///
/// The property is unchanged by any of that: squeeze the downlink and the
/// subscriber keeps getting renderable frames instead of a stall.
#[test]
fn cross_shard_stream_survives_congestion_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q"]).with_temporal_dd(3))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish flow across the shard boundary",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeToQos {
                description: "Bob keeps a floor so the slot must forward something",
                participant: "bob",
                targets: &[("alice", 720, 180, 1)],
            },
            Step::Run {
                description: "Soak at full quality",
                duration: Duration::from_secs(5),
            },
            Step::SetBandwidth {
                description: "Squeeze the downlink so full quality no longer fits",
                participant: "bob",
                bits_per_sec: 350_000,
            },
            Step::Run {
                description: "Let the allocator degrade on measurements sent between shards",
                duration: Duration::from_secs(12),
            },
            Step::CheckCrossShardMedia {
                description: "the stream really did cross a shard boundary",
                min_frames: 100,
            },
            Step::CheckVideoQuality {
                description: "Bob keeps receiving renderable frames through the squeeze",
                participant: "bob",
                quality: VideoQuality::min_frames(60).allow_gaps(30),
            },
        ]);
}

/// Simulcast layer switching across a shard boundary.
///
/// Switching picks a different encoding of the same track, which cross-shard
/// means the destination's fanout re-keys while packets for the previous layer
/// are still in flight on the old route. The parameter-set replay that makes a
/// switch decodable has to survive the restamp on arrival.
#[test]
fn cross_shard_simulcast_switching_stays_decodable_test() {
    LocalNodeSim::new()
        .with_link(LinkProfile::fiber())
        .with_room(
            Room::new("room1")
                .with_participant(Participant::quality_publisher("alice"))
                .with_participant(Participant::manual_subscriber("bob", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Establish flow and discover all three encodings",
                duration: Duration::from_secs(2),
            },
            Step::SubscribeTo {
                description: "Select the lowest encoding across shards",
                participant: "bob",
                targets: &[("alice", 180)],
            },
            Step::Run {
                description: "Cross the low-layer activation boundary",
                duration: Duration::from_secs(1),
            },
            Step::Run {
                description: "Measure steady decoded progress on the low layer",
                duration: Duration::from_secs(4),
            },
            Step::CheckVideoQualityInterval {
                description: "The cross-shard low layer is exact and continuously decodable",
                participant: "bob",
                quality: VideoQuality::min_frames(90)
                    .exact_decoded_resolution((320, 180))
                    .max_frame_gap(Duration::from_millis(100)),
            },
            Step::SubscribeTo {
                description: "Select the highest encoding across shards",
                participant: "bob",
                targets: &[("alice", 720)],
            },
            Step::Run {
                description: "Cross a complete high-layer keyframe boundary",
                duration: Duration::from_secs(1),
            },
            Step::Run {
                description: "Measure steady decoded progress on the high layer",
                duration: Duration::from_secs(4),
            },
            Step::CheckCrossShardMedia {
                description: "the switching stream crossed a shard boundary",
                min_frames: 100,
            },
            Step::CheckVideoQualityInterval {
                description: "Bob returns to continuous decodable output after every capacity switch",
                participant: "bob",
                quality: VideoQuality::min_frames(100)
                    .exact_decoded_resolution((1280, 720))
                    .max_frame_gap(Duration::from_millis(100)),
            },
        ]);
}

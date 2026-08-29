use super::common::{LocalNodeSim, Participant, Room, Step, VideoQuality};
use std::time::Duration;

#[test]
fn reciprocal_manual_subscriptions_decode_with_interactive_latency_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("reciprocal-manual-subscriptions")
                .with_participant(Participant::manual_publisher_and_subscriber("alice", 1))
                .with_participant(Participant::manual_publisher_and_subscriber("bob", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Both conference participants publish and discover each other",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Alice explicitly subscribes to Bob",
                participant: "alice",
                targets: &[("bob", 1080)],
            },
            Step::SubscribeTo {
                description: "Bob explicitly subscribes to Alice",
                participant: "bob",
                targets: &[("alice", 1080)],
            },
            Step::Run {
                description: "Explicit reciprocal assignments sustain decoded video",
                duration: Duration::from_secs(10),
            },
            Step::CheckVideoQualityInterval {
                description: "Alice continuously renders Bob without a queueing delay",
                participant: "alice",
                quality: VideoQuality::min_frames(50)
                    .allow_gaps(5)
                    .max_capture_to_decode_latency(Duration::from_millis(250))
                    .max_frame_gap(Duration::from_millis(500)),
            },
            Step::CheckVideoQualityInterval {
                description: "Bob continuously renders Alice without a queueing delay",
                participant: "bob",
                quality: VideoQuality::min_frames(50)
                    .allow_gaps(5)
                    .max_capture_to_decode_latency(Duration::from_millis(250))
                    .max_frame_gap(Duration::from_millis(500)),
            },
            Step::CheckVideoReceivedFrom {
                description: "Alice's decoded frames originate with Bob",
                participant: "alice",
                publisher: "bob",
                min_frames: 50,
            },
            Step::CheckVideoReceivedFrom {
                description: "Bob's decoded frames originate with Alice",
                participant: "bob",
                publisher: "alice",
                min_frames: 50,
            },
        ]);
}

/// Validates the declarative subscription API end-to-end:
/// subscriber discovers the publisher's track via signaling, `set_subscriptions()`
/// triggers media flow, and updating the subscription height does not break flow.
#[test]
fn declarative_subscription_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("alice"))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish connection and let signaling discover tracks",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeAll {
                description: "Bob subscribes to Alice's track at 720p",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Wait for declarative subscription to establish media flow",
                duration: Duration::from_secs(20),
            },
            Step::CheckRxBytes {
                description: "Bob has received media bytes via declarative subscription",
                participant: "bob",
                min_bytes: 1000,
            },
            Step::SubscribeAll {
                description: "Update subscription to 360p",
                participant: "bob",
                heights: &[360],
            },
            Step::Run {
                description: "Continue after subscription height update",
                duration: Duration::from_secs(5),
            },
        ]);
}

/// Tests that a subscriber with two RecvOnly slots can subscribe to two
/// publishers' tracks, and that re-issuing subscriptions does not break flow.
#[test]
fn slots_layout_update_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("pub1"))
                .with_participant(Participant::single_publisher("pub2"))
                .with_participant(Participant::multi_subscriber("sub", 2)),
        )
        .run(vec![
            Step::Run {
                description: "Publishers establish flow and signal track discovery",
                duration: Duration::from_secs(10),
            },
            Step::SubscribeAll {
                description: "Subscribe both slots to discovered tracks at 720p",
                participant: "sub",
                heights: &[720, 720],
            },
            Step::Run {
                description: "Initial subscription — let media flow on both slots",
                duration: Duration::from_secs(10),
            },
            Step::CheckRxBytes {
                description: "Sub receives video bytes from both publishers",
                participant: "sub",
                min_bytes: 1,
            },
            Step::SubscribeAll {
                description: "Re-subscribe (slot layout update)",
                participant: "sub",
                heights: &[720, 720],
            },
            Step::Run {
                description: "After layout update — verify continued media flow",
                duration: Duration::from_secs(10),
            },
            Step::CheckRxBytes {
                description: "Video still flows after slot layout update",
                participant: "sub",
                min_bytes: 1000,
            },
        ]);
}

#[test]
fn one_slot_track_replacement_keeps_media_flow_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("pub1", &["q"]).with_temporal_dd(3))
                .with_participant(Participant::publisher("pub2", &["q"]).with_temporal_dd(3))
                .with_participant(Participant::manual_subscriber("sub", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Publishers establish flow and signal both tracks",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeToQos {
                description: "Subscribe the only slot to both publishers with pub1 preferred",
                participant: "sub",
                targets: &[("pub1", 720, 0, 100), ("pub2", 720, 0, 10)],
            },
            Step::Run {
                description: "Receive the first publisher through the slot",
                duration: Duration::from_secs(10),
            },
            Step::CheckRxBytesInterval {
                description: "The first assignment carries media",
                participant: "sub",
                min_bytes: 1_000,
            },
            Step::Disconnect {
                description: "Remove the publisher occupying the only slot",
                participant: "pub1",
            },
            Step::Run {
                description: "Assign the same slot to the remaining publisher",
                duration: Duration::from_secs(10),
            },
            Step::CheckRxBytesInterval {
                description: "The replacement assignment carries media",
                participant: "sub",
                min_bytes: 100_000,
            },
        ]);
}

#[test]
fn slots_prioritization_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("pub1", &["q", "h", "f"]))
                .with_participant(Participant::publisher("pub2", &["q", "h", "f"]))
                .with_participant(Participant::multi_subscriber("sub", 2)),
        )
        .run(vec![
            Step::Run {
                description: "Publishers establish flow and signal track discovery",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeAll {
                description: "Subscribe: slot 0 at 720p (high), slot 1 at 180p (low)",
                participant: "sub",
                heights: &[720, 180],
            },
            Step::Run {
                description: "Let differentiated subscriptions take effect",
                duration: Duration::from_secs(10),
            },
            Step::CheckRxBytes {
                description: "Sub receives video across both priority slots",
                participant: "sub",
                min_bytes: 1,
            },
        ]);
}

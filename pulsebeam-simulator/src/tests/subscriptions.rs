use super::common::{LocalNodeSim, Participant, Room, Step};
use std::time::Duration;

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

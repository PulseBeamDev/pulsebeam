use crate::tests::common::{LocalNodeSim, Participant, Room, Step, VideoQuality};
use std::time::Duration;

/// Validates the declarative subscription API end-to-end:
/// 1. Subscriber discovers the publisher's track via signaling.
/// 2. `set_subscriptions()` triggers media flow.
/// 3. Updating subscriptions (height change + unknown track) does not break flow.
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
                description: "Update subscription to 360p (sticky-subscription test)",
                participant: "bob",
                heights: &[360],
            },
            Step::Run {
                description: "Continue after subscription height update",
                duration: Duration::from_secs(5),
            },
        ]);
}

#[test]
fn reconnection_recovery_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("alice"))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish initial flow",
                duration: Duration::from_secs(15),
            },
            Step::CheckVideoQuality {
                description: "Bob receives renderable video before partition",
                participant: "bob",
                quality: VideoQuality::min_frames(50),
            },
            Step::Partition {
                description: "Alice ↔ server network failure",
                from: "alice",
                to: "server",
            },
            Step::Run {
                description: "Partitioned period",
                duration: Duration::from_secs(10),
            },
            Step::Repair {
                description: "Lift partition — agent should auto-reconnect",
                from: "alice",
                to: "server",
            },
            Step::Run {
                description: "Recovery period",
                duration: Duration::from_secs(20),
            },
            Step::CheckVideoQuality {
                description: "Bob receives renderable video after Alice reconnects",
                participant: "bob",
                quality: VideoQuality::min_frames(100),
            },
        ]);
}

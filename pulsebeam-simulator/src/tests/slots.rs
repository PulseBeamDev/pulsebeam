use super::common::{LocalNodeSim, Participant, Room, Step};
use std::time::Duration;

/// Tests that a subscriber with two RecvOnly slots can subscribe to two
/// publishers' tracks and that re-issuing subscriptions (slot swap) does not
/// break media flow.
#[test]
fn slots_layout_update_test() {
    LocalNodeSim::new()
        .with_tick(Duration::from_micros(100))
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
            // Re-issue subscriptions (equivalent to swapping slot assignments)
            // to verify the layout update mechanism does not break flow.
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

/// Tests that a subscriber can request different quality layers on two slots:
/// one high-priority (720p) and one low-priority (180p).
#[test]
fn slots_prioritization_test() {
    LocalNodeSim::new()
        .with_tick(Duration::from_micros(100))
        .with_rng_seed(0)
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

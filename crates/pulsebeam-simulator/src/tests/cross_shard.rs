//! Cross-shard lifecycle and data-plane invariants.
//!
//! These plans deliberately force publishers and subscribers onto different
//! shard owners. A passing local-delivery test is not evidence that a route,
//! envelope, destination runtime, or subscriber key survived the shard hop.

use super::common::{LocalNodeSim, Participant, Room, Step};
use std::time::Duration;

#[test]
fn video_candidates_do_not_cross_shards_until_allocated_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-candidates")
                .with_participant(Participant::single_publisher("publisher"))
                .with_participant(Participant::manual_subscriber("subscriber", 1)),
        )
        .run(vec![
            Step::Run {
                description: "Discover the remote video candidate without selecting it",
                duration: Duration::from_secs(5),
            },
            Step::CheckNoCrossShardMedia {
                description: "candidate discovery does not compile forwarding",
            },
            Step::SubscribeTo {
                description: "Select the discovered candidate",
                participant: "subscriber",
                targets: &[("publisher", 720)],
            },
            Step::Run {
                description: "Activate and forward the selected track",
                duration: Duration::from_secs(20),
            },
            Step::CheckCrossShardMedia {
                description: "allocated video crosses the shard boundary",
                min_frames: 100,
            },
            Step::CheckRxBytes {
                description: "the active binding delivers media",
                participant: "subscriber",
                min_bytes: 1_000,
            },
        ]);
}

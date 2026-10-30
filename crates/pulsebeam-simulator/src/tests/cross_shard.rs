//! Cross-shard lifecycle and data-plane invariants.
//!
//! These plans deliberately force publishers and subscribers onto different
//! shard owners. A passing local-delivery test is not evidence that a route,
//! envelope, destination runtime, or subscriber key survived the shard hop.

use super::common::{LocalNodeSim, Participant, Room, Step};
use std::time::Duration;

#[test]
fn unselected_video_candidates_do_not_reach_the_receiver_test() {
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
            Step::CheckVideoNotReceivedFrom {
                description: "An unselected source delivers no video to the receiver",
                participant: "subscriber",
                publisher: "publisher",
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
            Step::CheckVideoDecodedFromInterval {
                description: "The selected 180p fixture independently decodes across owner shards",
                participant: "subscriber",
                publisher: "publisher",
                min_frames: 1,
                min_resolution: (1, 180),
            },
            Step::CheckRxBytes {
                description: "The active receiver also clears the existing traffic floor",
                participant: "subscriber",
                min_bytes: 1_000,
            },
        ]);
}

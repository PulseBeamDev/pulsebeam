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

/// Publisher-scoped data subscriptions preserve publisher identity across shards.
#[test]
fn data_channel_scoped_subscribe_across_shards_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-scoped")
                .with_participant(Participant::data_participant("pub_a"))
                .with_participant(Participant::data_participant("pub_b"))
                .with_participant(Participant::data_participant("scoped_a"))
                .with_participant(Participant::data_participant("scoped_b"))
                .with_participant(Participant::data_participant("aggregate")),
        )
        .run(vec![
            Step::DeclarePublishTopic {
                description: "Publisher A declares topic",
                participant: "pub_a",
                topic: "scoped_topic",
            },
            Step::DeclarePublishTopic {
                description: "Publisher B declares topic",
                participant: "pub_b",
                topic: "scoped_topic",
            },
            // Run long enough for both publishers to connect and expose their
            // participant_ids (needed for scoped subscription resolution).
            Step::Run {
                description: "Let publishers connect and register participant IDs",
                duration: Duration::from_secs(2),
            },
            Step::DeclareSubscribeTopic {
                description: "scoped_a subscribes scoped to pub_a",
                participant: "scoped_a",
                topic: "scoped_topic",
                scoped_to: Some("pub_a"),
            },
            Step::DeclareSubscribeTopic {
                description: "scoped_b subscribes scoped to pub_b",
                participant: "scoped_b",
                topic: "scoped_topic",
                scoped_to: Some("pub_b"),
            },
            Step::DeclareSubscribeTopic {
                description: "aggregate subscribes unscoped (receives from all publishers)",
                participant: "aggregate",
                topic: "scoped_topic",
                scoped_to: None,
            },
            Step::Run {
                description: "Let subscriber data channels initialize",
                duration: Duration::from_millis(500),
            },
            Step::PublishData {
                description: "Publisher A sends its payload",
                participant: "pub_a",
                topic: "scoped_topic",
                data: b"payload-from-a",
            },
            Step::PublishData {
                description: "Publisher B sends its payload",
                participant: "pub_b",
                topic: "scoped_topic",
                data: b"payload-from-b",
            },
            Step::Run {
                description: "Let payloads propagate through the SFU",
                duration: Duration::from_millis(500),
            },
            // scoped_a: must receive A's payload, must NOT receive B's payload.
            Step::CheckDataReceived {
                description: "scoped_a received pub_a payload",
                participant: "scoped_a",
                topic: "scoped_topic",
                expected: b"payload-from-a",
            },
            Step::CheckDataNotReceived {
                description: "scoped_a did not receive pub_b payload",
                participant: "scoped_a",
                topic: "scoped_topic",
                excluded: b"payload-from-b",
            },
            Step::CheckDataCount {
                description: "scoped_a received exactly one matching payload",
                participant: "scoped_a",
                topic: "scoped_topic",
                expected: 1,
            },
            // scoped_b: must receive B's payload, must NOT receive A's payload.
            Step::CheckDataReceived {
                description: "scoped_b received pub_b payload",
                participant: "scoped_b",
                topic: "scoped_topic",
                expected: b"payload-from-b",
            },
            Step::CheckDataNotReceived {
                description: "scoped_b did not receive pub_a payload",
                participant: "scoped_b",
                topic: "scoped_topic",
                excluded: b"payload-from-a",
            },
            Step::CheckDataCount {
                description: "scoped_b received exactly one matching payload",
                participant: "scoped_b",
                topic: "scoped_topic",
                expected: 1,
            },
            // aggregate: must receive both payloads.
            Step::CheckDataReceived {
                description: "aggregate received pub_a payload",
                participant: "aggregate",
                topic: "scoped_topic",
                expected: b"payload-from-a",
            },
            Step::CheckDataReceived {
                description: "aggregate received pub_b payload",
                participant: "aggregate",
                topic: "scoped_topic",
                expected: b"payload-from-b",
            },
            Step::CheckDataCount {
                description: "aggregate received exactly both payloads",
                participant: "aggregate",
                topic: "scoped_topic",
                expected: 2,
            },
        ]);
}

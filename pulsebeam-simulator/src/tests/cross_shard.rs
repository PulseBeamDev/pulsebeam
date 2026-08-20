//! Pre-existing cross-shard defects, pinned.
//!
//! Every other plan in this suite runs on one shard, so nothing on the
//! forwarding path ever crosses a shard boundary: routes are granted only when
//! the destination differs from the publisher's shard, and with one shard it
//! never does. Across 103 passing plans, not one audio route was ever cut.
//!
//! Raising `LocalNodeSim`'s default shard count to 16 is a one-line change. It
//! fails the two plans below, both of which fail identically on the commit
//! before any of this branch's work — they are latent defects the single-shard
//! default was hiding, not regressions.
//!
//! They are `#[ignore]`d rather than deleted so the reproductions survive. Both
//! live in code the publication catalog replaces, and the shard-local key
//! confusion behind them is what that catalog removes by construction, so they
//! are expected to be fixed there rather than patched here. Run with
//! `make test-sim TEST=cross_shard -- --ignored`.

use super::common::{LocalNodeSim, Participant, Room, Step};
use std::time::Duration;

/// A paused assignment never resumes when its publisher sits on another shard.
///
/// Bandwidth is restored to 5 Mbit/s and the estimate stays near 456 kbit/s, so
/// the allocator never un-pauses. Not a routing fault: instrumenting
/// `record_routing_drop` over the whole run shows no media dropped at any
/// stage, and extending the recovery window to 600s does not help, so the
/// estimate is suppressed rather than converging slowly.
#[test]
#[ignore = "pre-existing: paused stream does not resume across a shard boundary"]
fn paused_stream_resumes_across_a_shard_boundary_test() {
    LocalNodeSim::new()
        .with_shards(16)
        .with_bandwidth(500_000)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::screensharer("screen"))
                .with_participant(Participant::publisher("camera", &["q", "h", "f"]))
                .with_participant(Participant::manual_subscriber("viewer", 2)),
        )
        .run(vec![
            Step::Run {
                description: "Establish both publishers and the receiver",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeToQos {
                description: "Subscribe to both streams with the camera preferred",
                participant: "viewer",
                targets: &[("camera", 720, 0, 100), ("screen", 720, 0, 10)],
            },
            Step::Run {
                description: "Force one assignment into the paused state",
                duration: Duration::from_secs(45),
            },
            Step::SetBandwidth {
                description: "Restore enough downstream capacity for both streams",
                participant: "viewer",
                bits_per_sec: 5_000_000,
            },
            Step::Run {
                description: "Let the allocator resume and the switcher receive keyframes",
                duration: Duration::from_secs(120),
            },
            Step::CheckForwardedQuality {
                description: "The screen assignment is no longer paused",
                origin: "screen",
                min_quality: 1,
            },
        ]);
}

/// Publisher-scoped data subscription mis-routes once publishers are spread
/// across shards.
///
/// Fails at two shards and passes at sixteen, so which placement breaks it
/// depends on the hash — the defect is in the scoped path, not in a particular
/// shard count. Publisher scoping is the thing per-publisher stream identity
/// buys, so this is the sharpest data-lane claim the suite makes.
#[test]
#[ignore = "pre-existing: scoped data subscription mis-routes across shards"]
fn data_channel_scoped_subscribe_across_shards_test() {
    LocalNodeSim::new()
        .with_shards(2)
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
        ]);
}

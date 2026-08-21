use super::common::{LocalNodeSim, Participant, Room, Step};
use std::time::Duration;

/// Validates end-to-end data-channel pub/sub forwarding through the SFU.
#[test]
fn data_channel_pubsub_forwarding_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-data")
                .with_participant(Participant::data_participant("pub"))
                .with_participant(Participant::data_participant("sub"))
                .with_participant(Participant::data_participant("sub2")),
        )
        .run(vec![
            Step::DeclarePublishTopic {
                description: "Publisher declares topic",
                participant: "pub",
                topic: "sim_topic",
            },
            Step::DeclareSubscribeTopic {
                description: "Subscriber subscribes (unscoped)",
                participant: "sub",
                topic: "sim_topic",
                scoped_to: None,
            },
            Step::Run {
                description: "Let data-channel setup complete",
                duration: Duration::from_millis(500),
            },
            Step::PublishData {
                description: "Publisher sends payload",
                participant: "pub",
                topic: "sim_topic",
                data: b"hello-data-channel",
            },
            Step::Run {
                description: "Let payload travel through SFU to subscriber",
                duration: Duration::from_millis(200),
            },
            Step::CheckDataReceived {
                description: "Subscriber received the payload",
                participant: "sub",
                topic: "sim_topic",
                expected: b"hello-data-channel",
            },
            // The bystander. `sub2` is in the room and never subscribed, so delivery to it would
            // be the SFU sending a topic to someone who did not ask for it. Nothing asserted this
            // before, and a plan that creates a participant it never checks is how the
            // cross-shard fanout defect stayed green.
            Step::CheckDataNotReceived {
                description: "A participant that never subscribed receives nothing",
                participant: "sub2",
                topic: "sim_topic",
                excluded: b"hello-data-channel",
            },
        ]);
}

#[test]
fn data_channel_does_not_loop_back_to_publisher_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-data-no-loopback")
                .with_participant(Participant::data_participant("publisher"))
                .with_participant(Participant::data_participant("subscriber")),
        )
        .run(vec![
            Step::DeclarePublishTopic {
                description: "Publisher declares the topic",
                participant: "publisher",
                topic: "no_loopback",
            },
            Step::DeclareSubscribeTopic {
                description: "Publisher opens a receive subscription to the same topic",
                participant: "publisher",
                topic: "no_loopback",
                scoped_to: None,
            },
            Step::DeclareSubscribeTopic {
                description: "A second participant subscribes to the topic",
                participant: "subscriber",
                topic: "no_loopback",
                scoped_to: None,
            },
            Step::Run {
                description: "Install both subscriptions",
                duration: Duration::from_secs(2),
            },
            Step::PublishData {
                description: "Publisher sends one payload",
                participant: "publisher",
                topic: "no_loopback",
                data: b"must-not-return",
            },
            Step::Run {
                description: "Deliver the payload",
                duration: Duration::from_millis(500),
            },
            Step::CheckDataReceived {
                description: "The other participant receives the payload",
                participant: "subscriber",
                topic: "no_loopback",
                expected: b"must-not-return",
            },
            Step::CheckDataCount {
                description: "The publisher receives no looped-back payload",
                participant: "publisher",
                topic: "no_loopback",
                expected: 0,
            },
            Step::CheckDataCount {
                description: "The subscriber receives exactly one payload",
                participant: "subscriber",
                topic: "no_loopback",
                expected: 1,
            },
        ]);
}

#[test]
fn ordered_data_channel_does_not_loop_back_to_publisher_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-ordered-data-no-loopback")
                .with_participant(Participant::data_participant("publisher"))
                .with_participant(Participant::data_participant("subscriber")),
        )
        .run(vec![
            Step::DeclareOrderedPublisher {
                description: "Publisher declares the ordered topic",
                participant: "publisher",
                topic: "ordered_no_loopback",
            },
            Step::DeclareOrderedSubscriber {
                description: "Publisher opens an ordered receive subscription to itself",
                participant: "publisher",
                topic: "ordered_no_loopback",
            },
            Step::DeclareOrderedSubscriber {
                description: "A second participant subscribes to the ordered topic",
                participant: "subscriber",
                topic: "ordered_no_loopback",
            },
            Step::Run {
                description: "Install both ordered subscriptions",
                duration: Duration::from_secs(2),
            },
            Step::PublishOrdered {
                description: "Publisher sends one ordered payload",
                participant: "publisher",
                topic: "ordered_no_loopback",
                data: b"ordered-must-not-return",
            },
            Step::Run {
                description: "Deliver the ordered payload",
                duration: Duration::from_millis(500),
            },
            Step::CheckDataCount {
                description: "The publisher receives no ordered loopback",
                participant: "publisher",
                topic: "ordered_no_loopback",
                expected: 0,
            },
            Step::CheckDataSequence {
                description: "The subscriber receives the ordered payload",
                participant: "subscriber",
                topic: "ordered_no_loopback",
                expected: &[b"ordered-must-not-return"],
            },
        ]);
}

/// Validates scoped subscriptions: a subscriber scoped to publisher A receives
/// only A's payloads, never B's, and an unscoped aggregate subscriber sees both.
#[test]
fn data_channel_scoped_subscribe_routing_test() {
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

#[test]
fn ordered_topic_delivers_every_message_in_order() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-ordered")
                .with_participant(Participant::data_participant("pub"))
                .with_participant(Participant::data_participant("sub"))
                .with_participant(Participant::data_participant("sub2")),
        )
        .run(vec![
            Step::DeclareOrderedPublisher {
                description: "Declare ordered publisher",
                participant: "pub",
                topic: "boxes",
            },
            Step::DeclareOrderedSubscriber {
                description: "Declare ordered subscriber",
                participant: "sub",
                topic: "boxes",
            },
            Step::Run {
                description: "Open ordered channels",
                duration: Duration::from_millis(500),
            },
            Step::PublishOrdered {
                description: "Create box",
                participant: "pub",
                topic: "boxes",
                data: b"create:box-1",
            },
            Step::PublishOrdered {
                description: "Create another box",
                participant: "pub",
                topic: "boxes",
                data: b"create:box-2",
            },
            Step::PublishOrdered {
                description: "Delete first box",
                participant: "pub",
                topic: "boxes",
                data: b"delete:box-1",
            },
            Step::Run {
                description: "Deliver ordered lifecycle",
                duration: Duration::from_millis(500),
            },
            Step::CheckDataSequence {
                description: "Lifecycle messages remain complete and ordered",
                participant: "sub",
                topic: "boxes",
                expected: &[b"create:box-1", b"create:box-2", b"delete:box-1"],
            },
        ]);
}

#[test]
fn latest_topic_eventually_delivers_newest_state() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-latest")
                .with_participant(Participant::data_participant("pub"))
                .with_participant(Participant::data_participant("sub"))
                .with_participant(Participant::data_participant("sub2")),
        )
        .run(vec![
            Step::DeclarePublishTopic {
                description: "Declare latest publisher",
                participant: "pub",
                topic: "pose",
            },
            Step::DeclareSubscribeTopic {
                description: "Declare latest subscriber",
                participant: "sub",
                topic: "pose",
                scoped_to: None,
            },
            Step::Run {
                description: "Open latest channels",
                duration: Duration::from_millis(500),
            },
            Step::PublishData {
                description: "Publish old pose",
                participant: "pub",
                topic: "pose",
                data: b"pose:1",
            },
            Step::PublishData {
                description: "Publish intermediate pose",
                participant: "pub",
                topic: "pose",
                data: b"pose:2",
            },
            Step::PublishData {
                description: "Publish newest pose",
                participant: "pub",
                topic: "pose",
                data: b"pose:3",
            },
            Step::Run {
                description: "Deliver newest state",
                duration: Duration::from_millis(300),
            },
            Step::CheckDataReceived {
                description: "Newest state arrives",
                participant: "sub",
                topic: "pose",
                expected: b"pose:3",
            },
            Step::CheckDataNotReceived {
                description: "A participant that never subscribed receives nothing",
                participant: "sub2",
                topic: "pose",
                excluded: b"pose:3",
            },
        ]);
}

#[test]
fn ordered_topic_continues_after_publisher_reconnect() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-ordered-reconnect")
                .with_participant(Participant::data_participant("pub"))
                .with_participant(Participant::data_participant("sub"))
                .with_participant(Participant::data_participant("sub2")),
        )
        .run(vec![
            Step::DeclareOrderedPublisher {
                description: "Declare first ordered stream",
                participant: "pub",
                topic: "boxes",
            },
            Step::DeclareOrderedSubscriber {
                description: "Declare persistent subscriber",
                participant: "sub",
                topic: "boxes",
            },
            Step::Run {
                description: "Open first stream",
                duration: Duration::from_millis(500),
            },
            Step::PublishOrdered {
                description: "Send before reconnect",
                participant: "pub",
                topic: "boxes",
                data: b"create:before",
            },
            Step::Run {
                description: "Deliver before reconnect",
                duration: Duration::from_millis(300),
            },
            Step::Disconnect {
                description: "Disconnect publisher",
                participant: "pub",
            },
            Step::Run {
                description: "Observe publisher departure",
                duration: Duration::from_millis(300),
            },
            Step::Reconnect {
                description: "Reconnect publisher",
                participant: "pub",
            },
            Step::Run {
                description: "Establish replacement connection",
                duration: Duration::from_secs(2),
            },
            Step::DeclareOrderedPublisher {
                description: "Declare replacement ordered stream",
                participant: "pub",
                topic: "boxes",
            },
            Step::Run {
                description: "Open replacement stream",
                duration: Duration::from_millis(500),
            },
            Step::PublishOrdered {
                description: "Send after reconnect",
                participant: "pub",
                topic: "boxes",
                data: b"create:after",
            },
            Step::Run {
                description: "Deliver after reconnect",
                duration: Duration::from_millis(500),
            },
            Step::CheckDataSequence {
                description: "Both stream generations deliver exactly once",
                participant: "sub",
                topic: "boxes",
                expected: &[b"create:before", b"create:after"],
            },
        ]);
}

/// Data crossing a shard boundary.
///
/// The realtime data lane has its own route family, its own import lifecycle
/// and its own wildcard resolution, none of which a co-located room touches.
/// `with_shards(2)` puts publisher and subscriber on different shards, so the
/// payload is addressed by a route the destination allocated, resolved against
/// its `RouteAction::Data { lane }`, and handed to the subscriber's channel.
#[test]
fn cross_shard_data_channel_forwarding_test() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(
            Room::new("room-data-xshard")
                .with_participant(Participant::data_participant("pub"))
                .with_participant(Participant::data_participant("sub"))
                .with_participant(Participant::data_participant("sub2")),
        )
        .run(vec![
            Step::DeclarePublishTopic {
                description: "Publisher declares topic",
                participant: "pub",
                topic: "xshard_topic",
            },
            Step::DeclareSubscribeTopic {
                description: "Subscriber on another shard subscribes",
                participant: "sub",
                topic: "xshard_topic",
                scoped_to: None,
            },
            Step::DeclareSubscribeTopic {
                description: "And a second, so at least one lands off-shard",
                participant: "sub2",
                topic: "xshard_topic",
                scoped_to: None,
            },
            Step::Run {
                description: "Let the destination install its data route",
                duration: Duration::from_secs(2),
            },
            Step::PublishData {
                description: "Publisher sends payload",
                participant: "pub",
                topic: "xshard_topic",
                data: b"hello-across-shards",
            },
            Step::Run {
                description: "Let the payload cross the shard boundary",
                duration: Duration::from_secs(1),
            },
            Step::CheckDataReceived {
                description: "Subscriber received it over a cross-shard route",
                participant: "sub",
                topic: "xshard_topic",
                expected: b"hello-across-shards",
            },
            // Both, not either. Which shard each subscriber lands on is decided
            // by the 4-tuple hash, so asserting on one of them passes whenever
            // that one happens to win - and a fanout serving only the first
            // subscriber on a shard looked healthy for exactly that reason.
            Step::CheckDataReceived {
                description: "So did the second subscriber, wherever it landed",
                participant: "sub2",
                topic: "xshard_topic",
                expected: b"hello-across-shards",
            },
            Step::CheckCrossShardMedia {
                description: "the payload genuinely crossed a shard boundary",
                min_frames: 1,
            },
        ]);
}

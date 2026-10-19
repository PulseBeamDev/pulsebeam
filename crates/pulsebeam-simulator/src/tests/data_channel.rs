use super::common::{LinkProfile, LocalNodeSim, Participant, Room, Step};
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
            Step::CheckDataCount {
                description: "Subscriber received exactly one payload",
                participant: "sub",
                topic: "sim_topic",
                expected: 1,
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
            Step::CheckDataCount {
                description: "A participant that never subscribed receives zero payloads",
                participant: "sub2",
                topic: "sim_topic",
                expected: 0,
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
            Step::CheckDataCount {
                description: "scoped_a received exactly one payload",
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
                description: "scoped_b received exactly one payload",
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
fn latest_data_topic_survives_same_identity_connection_recovery() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-data-reconnect")
                .with_participant(Participant::data_participant("pub"))
                .with_participant(Participant::data_participant("sub")),
        )
        .run(vec![
            Step::DeclarePublishTopic {
                description: "Declare the reconnecting publisher topic",
                participant: "pub",
                topic: "state",
            },
            Step::DeclareSubscribeTopic {
                description: "Subscribe before the connection drop",
                participant: "sub",
                topic: "state",
                scoped_to: None,
            },
            Step::Run {
                description: "Open the data channels",
                duration: Duration::from_secs(2),
            },
            Step::PublishData {
                description: "Send the first state update",
                participant: "pub",
                topic: "state",
                data: b"before",
            },
            Step::Run {
                description: "Deliver the first state update",
                duration: Duration::from_secs(1),
            },
            Step::CheckDataCount {
                description: "The subscriber received the first update",
                participant: "sub",
                topic: "state",
                expected: 1,
            },
            Step::Partition {
                description: "Partition the publisher connection",
                from: "pub",
                to: "server",
            },
            Step::Run {
                description: "Wait for the publisher connection to expire",
                duration: Duration::from_secs(12),
            },
            Step::Repair {
                description: "Repair the publisher path",
                from: "pub",
                to: "server",
            },
            Step::Run {
                description: "Reconnect the same publisher identity",
                duration: Duration::from_secs(12),
            },
            Step::PublishData {
                description: "Send the second state update on the remapped channel",
                participant: "pub",
                topic: "state",
                data: b"after",
            },
            Step::Run {
                description: "Deliver the second state update",
                duration: Duration::from_secs(2),
            },
            Step::CheckDataCount {
                description: "Both state updates arrived exactly once",
                participant: "sub",
                topic: "state",
                expected: 2,
            },
        ]);
}

#[test]
fn ordered_data_topic_survives_same_identity_connection_recovery() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-ordered-data-reconnect")
                .with_participant(Participant::data_participant("pub"))
                .with_participant(Participant::data_participant("sub")),
        )
        .run(vec![
            Step::DeclareOrderedPublisher {
                description: "Declare the ordered reconnecting publisher",
                participant: "pub",
                topic: "state",
            },
            Step::DeclareOrderedSubscriber {
                description: "Declare the ordered subscriber",
                participant: "sub",
                topic: "state",
            },
            Step::Run {
                description: "Open the ordered channels",
                duration: Duration::from_secs(2),
            },
            Step::PublishOrdered {
                description: "Send the first ordered update",
                participant: "pub",
                topic: "state",
                data: b"before",
            },
            Step::Run {
                description: "Deliver the first ordered update",
                duration: Duration::from_secs(1),
            },
            Step::CheckDataSequence {
                description: "The first ordered update arrived",
                participant: "sub",
                topic: "state",
                expected: &[b"before"],
            },
            Step::Partition {
                description: "Partition the ordered publisher connection",
                from: "pub",
                to: "server",
            },
            Step::Run {
                description: "Wait for the ordered publisher connection to expire",
                duration: Duration::from_secs(12),
            },
            Step::Repair {
                description: "Repair the ordered publisher path",
                from: "pub",
                to: "server",
            },
            Step::Run {
                description: "Reconnect without redeclaring the ordered topic",
                duration: Duration::from_secs(12),
            },
            Step::PublishOrdered {
                description: "Send the second ordered update on the remapped channels",
                participant: "pub",
                topic: "state",
                data: b"after",
            },
            Step::Run {
                description: "Deliver the second ordered update",
                duration: Duration::from_secs(2),
            },
            Step::CheckDataSequence {
                description: "Both ordered stream generations arrived in order",
                participant: "sub",
                topic: "state",
                expected: &[b"before", b"after"],
            },
        ]);
}

#[test]
fn ordered_topic_reconfiguration_preserves_cross_shard_delivery() {
    ordered_topic_reconfiguration(true);
}

#[test]
fn ordered_topic_subscribers_can_precede_publisher() {
    ordered_topic_reconfiguration(false);
}

fn ordered_topic_reconfiguration(publisher_first: bool) {
    const PREFIX: &[&[u8]] = &[b"pub/v1/first", b"pub/v1/second"];
    const COMPLETE: &[&[u8]] = &[
        b"pub/v1/first",
        b"pub/v1/second",
        b"pub/v2/first",
        b"pub/v2/second",
    ];
    let mut declarations = vec![
        Step::DeclareOrderedPublisher {
            description: "Declare the publisher after initial connection",
            participant: "pub",
            topic: "state",
        },
        Step::DeclareOrderedSubscriber {
            description: "First receiver declares the ordered topic",
            participant: "sub_a",
            topic: "state",
        },
        Step::DeclareOrderedSubscriber {
            description: "Second receiver declares the ordered topic",
            participant: "sub_b",
            topic: "state",
        },
    ];
    if !publisher_first {
        declarations.rotate_left(1);
    }
    let mut plan = vec![Step::Run {
        description: "Connect all four agents without topic declarations",
        duration: Duration::from_secs(2),
    }];
    for (index, declaration) in declarations.into_iter().enumerate() {
        plan.push(declaration);
        if index == if publisher_first { 0 } else { 1 } {
            plan.push(Step::Run {
                description: "Settle the first declarations before submitting the other side",
                duration: Duration::from_secs(1),
            });
        }
    }
    plan.push(Step::Run {
        description: "Open the ordered paths after transport reconfiguration",
        duration: Duration::from_secs(2),
    });
    for data in PREFIX {
        plan.push(Step::PublishOrdered {
            description: "Send the original publisher's ordered prefix",
            participant: "pub",
            topic: "state",
            data,
        });
    }
    for participant in ["sub_a", "sub_b"] {
        plan.push(Step::WaitForDataSequence {
            participant,
            topic: "state",
            expected: PREFIX,
        });
    }
    plan.push(Step::CheckCrossShardMedia {
        description: "Data-only agents delivered the ordered prefix across shards",
        min_frames: 1,
    });
    plan.extend([
        Step::Disconnect {
            description: "Retire the original publisher connection",
            participant: "pub",
        },
        Step::Run {
            description: "Drain the original publisher's routes",
            duration: Duration::from_secs(1),
        },
        Step::Reconnect {
            description: "Rejoin the publisher without preloaded topics",
            participant: "pub",
        },
        Step::Run {
            description: "Establish the fresh publisher connection",
            duration: Duration::from_secs(2),
        },
        Step::DeclareOrderedPublisher {
            description: "Declare the ordered publisher on the new connection",
            participant: "pub",
            topic: "state",
        },
        Step::Run {
            description: "Reinstall the intended receiver routes",
            duration: Duration::from_secs(2),
        },
    ]);
    for data in &COMPLETE[2..] {
        plan.push(Step::PublishOrdered {
            description: "Send fresh ordered data after publisher rejoin",
            participant: "pub",
            topic: "state",
            data,
        });
    }
    for participant in ["sub_a", "sub_b"] {
        plan.push(Step::WaitForDataSequence {
            participant,
            topic: "state",
            expected: COMPLETE,
        });
    }
    plan.push(Step::Run {
        description: "Drain late traffic before checking exact delivery and isolation",
        duration: Duration::from_secs(1),
    });
    for participant in ["sub_a", "sub_b"] {
        plan.push(Step::CheckDataSequence {
            description: "Both publisher generations arrived exactly once and in order",
            participant,
            topic: "state",
            expected: COMPLETE,
        });
    }
    for participant in ["pub", "bystander"] {
        plan.push(Step::CheckDataCount {
            description: "Unsubscribed participants receive no ordered payloads",
            participant,
            topic: "state",
            expected: 0,
        });
    }
    LocalNodeSim::new()
        .without_initial_topics()
        .with_shards(2)
        .with_link(LinkProfile::fiber())
        .with_room(
            Room::new("ordered-reconfiguration")
                .with_participant(Participant::data_participant("pub"))
                .with_participant(Participant::data_participant("sub_a"))
                .with_participant(Participant::data_participant("sub_b"))
                .with_participant(Participant::data_participant("bystander")),
        )
        .run(plan);
}

#[test]
fn reliable_data_converges_across_shards_with_loss_and_feedback_impairment() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-reliable-xshard")
                .with_participant(Participant::data_participant("pub"))
                .with_participant(Participant::data_participant("sub_a"))
                .with_participant(Participant::data_participant("sub_b"))
                .with_participant(Participant::data_participant("late").starts_disconnected()),
        )
        .run(vec![
            Step::DeclareOrderedPublisher {
                description: "Declare reliable publisher",
                participant: "pub",
                topic: "state",
            },
            Step::DeclareOrderedSubscriber {
                description: "Declare subscriber A",
                participant: "sub_a",
                topic: "state",
            },
            Step::DeclareOrderedSubscriber {
                description: "Declare subscriber B",
                participant: "sub_b",
                topic: "state",
            },
            Step::DeclareOrderedSubscriber {
                description: "Reserve late subscriber transport topology",
                participant: "late",
                topic: "state",
            },
            Step::Run {
                description: "Open reliable paths across worker shards",
                duration: Duration::from_secs(3),
            },
            Step::PublishOrdered {
                description: "Send first payload",
                participant: "pub",
                topic: "state",
                data: b"first",
            },
            Step::WaitForDataSequence {
                participant: "sub_a",
                topic: "state",
                expected: &[b"first"],
            },
            Step::WaitForDataSequence {
                participant: "sub_b",
                topic: "state",
                expected: &[b"first"],
            },
            Step::StopServer,
            Step::PublishOrdered {
                description: "Admit a distinct tail while the SFU cannot deliver it",
                participant: "pub",
                topic: "state",
                data: b"lost-tail",
            },
            Step::WaitForOrderedPublisher {
                participant: "pub",
                topic: "state",
                next_sequence: 2,
            },
            Step::CheckDataSequence {
                description: "The admitted tail is outstanding at subscriber A",
                participant: "sub_a",
                topic: "state",
                expected: &[b"first"],
            },
            Step::CheckDataSequence {
                description: "The admitted tail is outstanding at subscriber B",
                participant: "sub_b",
                topic: "state",
                expected: &[b"first"],
            },
            Step::RestartServer,
            Step::WaitForDataSequence {
                participant: "sub_a",
                topic: "state",
                expected: &[b"first", b"lost-tail"],
            },
            Step::WaitForDataSequence {
                participant: "sub_b",
                topic: "state",
                expected: &[b"first", b"lost-tail"],
            },
            Step::PublishOrdered {
                description: "Only after tail recovery send a later payload",
                participant: "pub",
                topic: "state",
                data: b"later",
            },
            Step::WaitForDataSequence {
                participant: "sub_a",
                topic: "state",
                expected: &[b"first", b"lost-tail", b"later"],
            },
            Step::WaitForDataSequence {
                participant: "sub_b",
                topic: "state",
                expected: &[b"first", b"lost-tail", b"later"],
            },
            Step::Join {
                description: "Join after history exists",
                participant: "late",
            },
            Step::WaitForOrderedSubscriber {
                participant: "late",
                publisher: "pub",
                topic: "state",
                next_sequence: 3,
            },
            Step::PublishOrdered {
                description: "Live marker is an ordered-processing barrier after the history floor",
                participant: "pub",
                topic: "state",
                data: b"live",
            },
            Step::WaitForDataSequence {
                participant: "late",
                topic: "state",
                expected: &[b"live"],
            },
            Step::WaitForDataSequence {
                participant: "sub_a",
                topic: "state",
                expected: &[b"first", b"lost-tail", b"later", b"live"],
            },
            Step::WaitForDataSequence {
                participant: "sub_b",
                topic: "state",
                expected: &[b"first", b"lost-tail", b"later", b"live"],
            },
            Step::Run {
                description: "Rejoined control, shard and native client state converge",
                duration: Duration::from_secs(2),
            },
            Step::CheckDataSequence {
                description: "Late receiver has only the live marker",
                participant: "late",
                topic: "state",
                expected: &[b"live"],
            },
        ]);
}

/// Data crossing a shard boundary.
///
/// The realtime data lane has its own route family, its own import lifecycle
/// and its own wildcard resolution, none of which a co-located room touches.
/// The hardened simulator default uses multiple shards, so the payload is addressed by a route
/// the destination allocated, resolved against its `RouteAction::Unreliable { lane }`, and handed
/// to the subscriber's channel.
#[test]
fn cross_shard_data_channel_forwarding_test() {
    LocalNodeSim::new()
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

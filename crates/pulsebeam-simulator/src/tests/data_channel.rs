use super::common::{LinkProfile, LocalNodeSim, Loss, Participant, Reorder, Room, Step};
use pulsebeam_agent_native::agent_core::TopicMode;
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
                mode: TopicMode::Latest,
                description: "Publisher declares topic",
                participant: "pub",
                topic: "sim_topic",
            },
            Step::DeclareSubscribeTopic {
                mode: TopicMode::Latest,
                description: "Subscriber selects the latest mode",
                participant: "sub",
                topic: "sim_topic",
            },
            Step::Run {
                description: "Let data-channel setup complete",
                duration: Duration::from_millis(500),
            },
            Step::PublishData {
                mode: TopicMode::Latest,
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
                mode: TopicMode::Latest,
                description: "Publisher declares the topic",
                participant: "publisher",
                topic: "no_loopback",
            },
            Step::DeclareSubscribeTopic {
                mode: TopicMode::Latest,
                description: "Publisher opens a receive subscription to the same topic",
                participant: "publisher",
                topic: "no_loopback",
            },
            Step::DeclareSubscribeTopic {
                mode: TopicMode::Latest,
                description: "A second participant subscribes to the topic",
                participant: "subscriber",
                topic: "no_loopback",
            },
            Step::Run {
                description: "Install both subscriptions",
                duration: Duration::from_secs(2),
            },
            Step::PublishData {
                mode: TopicMode::Latest,
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
            Step::DeclarePublishTopic {
                mode: TopicMode::Ordered,
                description: "Publisher declares the ordered topic",
                participant: "publisher",
                topic: "ordered_no_loopback",
            },
            Step::DeclareSubscribeTopic {
                mode: TopicMode::Ordered,
                description: "Publisher opens an ordered receive subscription to itself",
                participant: "publisher",
                topic: "ordered_no_loopback",
            },
            Step::DeclareSubscribeTopic {
                mode: TopicMode::Ordered,
                description: "A second participant subscribes to the ordered topic",
                participant: "subscriber",
                topic: "ordered_no_loopback",
            },
            Step::Run {
                description: "Install both ordered subscriptions",
                duration: Duration::from_secs(2),
            },
            Step::PublishData {
                mode: TopicMode::Ordered,
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

/// A full-length topic name is independent across modes and publisher tracks.
#[test]
fn all_topic_modes_share_a_name_without_cross_delivery_or_publisher_collapse() {
    const TOPIC: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let mut room = Room::new("data-modes");
    for name in [
        "pub-a",
        "pub-b",
        "latest-sub",
        "lossy-sub",
        "ordered-sub",
        "all-sub",
    ] {
        room = room.with_participant(Participant::data_participant(name));
    }
    let modes = [
        (
            TopicMode::Latest,
            "latest-sub",
            b"latest-a".as_slice(),
            b"latest-b".as_slice(),
        ),
        (
            TopicMode::Lossy,
            "lossy-sub",
            b"lossy-a".as_slice(),
            b"lossy-b".as_slice(),
        ),
        (
            TopicMode::Ordered,
            "ordered-sub",
            b"ordered-a".as_slice(),
            b"ordered-b".as_slice(),
        ),
    ];
    let mut plan = Vec::new();
    for (mode, subscriber, _, _) in modes {
        for publisher in ["pub-a", "pub-b"] {
            plan.push(Step::DeclarePublishTopic {
                description: "same label, explicit independent mode",
                participant: publisher,
                topic: TOPIC,
                mode,
            });
        }
        for participant in [subscriber, "all-sub"] {
            plan.push(Step::DeclareSubscribeTopic {
                description: "mode-specific subscription",
                participant,
                topic: TOPIC,
                mode,
            });
        }
    }
    plan.push(Step::Run {
        description: "bind authoritative publications and ordered live frontiers",
        duration: Duration::from_secs(2),
    });
    for (mode, _, first, second) in modes {
        for (participant, data) in [("pub-a", first), ("pub-b", second)] {
            plan.push(Step::PublishData {
                description: "each publisher retains its own track and sequence space",
                participant,
                topic: TOPIC,
                mode,
                data,
            });
        }
    }
    for (participant, data) in [
        ("pub-a", b"lossy-a-next".as_slice()),
        ("pub-b", b"lossy-b-next".as_slice()),
    ] {
        plan.push(Step::PublishData {
            description: "lossy preserves each distinct admitted value",
            participant,
            topic: TOPIC,
            mode: TopicMode::Lossy,
            data,
        });
    }
    plan.push(Step::Run {
        description: "deliver all modes over production routes",
        duration: Duration::from_secs(1),
    });
    for (mode, subscriber, first, second) in modes {
        for participant in [subscriber, "all-sub"] {
            plan.push(Step::CheckTopicIdentity {
                description: "events carry external ownership and the exact authoritative track",
                participant,
                topic: TOPIC,
                mode,
                publishers: &["pub-a", "pub-b"],
                gap: None,
            });
            for expected in [first, second] {
                plan.push(Step::CheckDataReceived {
                    description: "both publishers reach the matching mode",
                    participant,
                    topic: TOPIC,
                    expected,
                });
            }
        }
        plan.push(Step::CheckDataCount {
            description: "same-name modes never cross-deliver",
            participant: subscriber,
            topic: TOPIC,
            expected: if mode == TopicMode::Lossy { 4 } else { 2 },
        });
    }
    plan.push(Step::CheckDataCount {
        description: "all independent mode/publisher streams survive",
        participant: "all-sub",
        topic: TOPIC,
        expected: 8,
    });
    plan.push(Step::CheckCrossShardMedia {
        description: "the real data routes cross shard ownership",
        min_frames: 1,
    });
    LocalNodeSim::new()
        .with_link(LinkProfile::fiber())
        .with_room(room)
        .run(plan);
}

#[test]
fn latest_is_monotonic_per_publisher_when_old_packets_arrive_after_new_state() {
    let mut plan = Vec::new();
    for participant in ["pub-a", "pub-b"] {
        plan.push(Step::DeclarePublishTopic {
            description: "independent latest publisher",
            participant,
            topic: "pose",
            mode: TopicMode::Latest,
        });
    }
    plan.push(Step::DeclareSubscribeTopic {
        description: "aggregate latest subscription",
        participant: "sub",
        topic: "pose",
        mode: TopicMode::Latest,
    });
    plan.push(Step::Run {
        description: "bind latest channels",
        duration: Duration::from_secs(1),
    });
    plan.push(Step::SetReorder {
        description: "hold old packets behind subsequent state",
        participant: "sub",
        reorder: Reorder {
            probability: 1.0,
            delay: Duration::from_millis(300),
        },
    });
    for (participant, data) in [
        ("pub-a", b"old-a".as_slice()),
        ("pub-b", b"old-b".as_slice()),
    ] {
        plan.push(Step::PublishData {
            description: "transmit old state before changing the path",
            participant,
            topic: "pose",
            mode: TopicMode::Latest,
            data,
        });
        plan.push(Step::WaitForTopicPublisher {
            participant,
            topic: "pose",
            mode: TopicMode::Latest,
            next_sequence: 1,
        });
    }
    plan.push(Step::Run {
        description: "consume both old transport attempts into the reorder queue",
        duration: Duration::from_millis(40),
    });
    plan.push(Step::SetReorder {
        description: "new state overtakes the held packets",
        participant: "sub",
        reorder: Reorder::NONE,
    });
    for (participant, data) in [
        ("pub-a", b"new-a".as_slice()),
        ("pub-b", b"new-b".as_slice()),
    ] {
        plan.push(Step::PublishData {
            description: "both tracks retain independent fresh state",
            participant,
            topic: "pose",
            mode: TopicMode::Latest,
            data,
        });
    }
    plan.push(Step::Run {
        description: "release newer and then delayed old state",
        duration: Duration::from_millis(500),
    });
    for expected in [b"new-a".as_slice(), b"new-b".as_slice()] {
        plan.push(Step::CheckDataReceived {
            description: "latest cannot collapse one publisher into another",
            participant: "sub",
            topic: "pose",
            expected,
        });
    }
    for excluded in [b"old-a".as_slice(), b"old-b".as_slice()] {
        plan.push(Step::CheckDataNotReceived {
            description: "delayed old state cannot follow a newer sequence",
            participant: "sub",
            topic: "pose",
            excluded,
        });
    }
    plan.push(Step::CheckDataCount {
        description: "only the fresh per-track values are delivered",
        participant: "sub",
        topic: "pose",
        expected: 2,
    });
    plan.push(Step::CheckTopicIdentity {
        description: "freshness and ownership are per authoritative data track",
        participant: "sub",
        topic: "pose",
        mode: TopicMode::Latest,
        publishers: &["pub-a", "pub-b"],
        gap: None,
    });
    plan.push(Step::CheckDataCount {
        description: "a non-subscriber receives no state",
        participant: "bystander",
        topic: "pose",
        expected: 0,
    });
    LocalNodeSim::new()
        .with_link(LinkProfile::fiber())
        .with_room(
            Room::new("room-latest")
                .with_participant(Participant::data_participant("pub-a"))
                .with_participant(Participant::data_participant("pub-b"))
                .with_participant(Participant::data_participant("sub"))
                .with_participant(Participant::data_participant("bystander")),
        )
        .run(plan);
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
                mode: TopicMode::Latest,
                description: "Declare the reconnecting publisher topic",
                participant: "pub",
                topic: "state",
            },
            Step::DeclareSubscribeTopic {
                mode: TopicMode::Latest,
                description: "Subscribe before the connection drop",
                participant: "sub",
                topic: "state",
            },
            Step::Run {
                description: "Open the data channels",
                duration: Duration::from_secs(2),
            },
            Step::PublishData {
                mode: TopicMode::Latest,
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
                mode: TopicMode::Latest,
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
            Step::DeclarePublishTopic {
                mode: TopicMode::Ordered,
                description: "Declare the ordered reconnecting publisher",
                participant: "pub",
                topic: "state",
            },
            Step::DeclareSubscribeTopic {
                mode: TopicMode::Ordered,
                description: "Declare the ordered subscriber",
                participant: "sub",
                topic: "state",
            },
            Step::Run {
                description: "Open the ordered channels",
                duration: Duration::from_secs(2),
            },
            Step::PublishData {
                mode: TopicMode::Ordered,
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
            Step::PublishData {
                mode: TopicMode::Ordered,
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
        Step::DeclarePublishTopic {
            mode: TopicMode::Ordered,
            description: "Declare the publisher after initial connection",
            participant: "pub",
            topic: "state",
        },
        Step::DeclareSubscribeTopic {
            mode: TopicMode::Ordered,
            description: "First receiver declares the ordered topic",
            participant: "sub_a",
            topic: "state",
        },
        Step::DeclareSubscribeTopic {
            mode: TopicMode::Ordered,
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
        plan.push(Step::PublishData {
            mode: TopicMode::Ordered,
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
        Step::DeclarePublishTopic {
            mode: TopicMode::Ordered,
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
        plan.push(Step::PublishData {
            mode: TopicMode::Ordered,
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
#[ignore = "Reconnect catalog pruning loses the ordered cursor; see agents/pulsebeam-agent-core/ORDERED_RECOVERY_CHECKPOINT.md"]
fn ordered_recovery_reports_retention_gaps_without_replaying_late_history() {
    const RETAINED: usize = pulsebeam_agent_native::agent_core::TOPIC_HISTORY_CAPACITY;
    const SENT: usize = RETAINED + 44;
    static RECOVERED: [&[u8]; RETAINED + 5] = {
        let mut values = [b"window".as_slice(); RETAINED + 5];
        values[0] = b"first";
        values[1] = b"lost-tail";
        values[2] = b"later";
        values[3] = b"live";
        values[RETAINED + 4] = b"continued";
        values
    };
    static LATE: [&[u8]; SENT + 2] = {
        let mut values = [b"window".as_slice(); SENT + 2];
        values[0] = b"live";
        values[SENT + 1] = b"continued";
        values
    };
    let mut plan = vec![
        Step::DeclarePublishTopic {
            mode: TopicMode::Ordered,
            description: "Declare ordered publisher",
            participant: "pub",
            topic: "state",
        },
        Step::DeclareSubscribeTopic {
            mode: TopicMode::Ordered,
            description: "Declare subscriber A",
            participant: "sub_a",
            topic: "state",
        },
        Step::DeclareSubscribeTopic {
            mode: TopicMode::Ordered,
            description: "Declare subscriber B",
            participant: "sub_b",
            topic: "state",
        },
        Step::DeclareSubscribeTopic {
            mode: TopicMode::Ordered,
            description: "Reserve late subscriber transport topology",
            participant: "late",
            topic: "state",
        },
        Step::Run {
            description: "Establish ordered live frontiers across worker shards",
            duration: Duration::from_secs(3),
        },
        Step::PublishData {
            mode: TopicMode::Ordered,
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
        Step::SetLoss {
            description: "Drop subscriber A's data without replacing its track",
            participant: "sub_a",
            loss: Loss::Independent(1.0),
        },
        Step::SetLoss {
            description: "Drop subscriber B's data without replacing its track",
            participant: "sub_b",
            loss: Loss::Independent(1.0),
        },
        Step::PublishData {
            mode: TopicMode::Ordered,
            description: "Admit a distinct tail while both downlinks drop it",
            participant: "pub",
            topic: "state",
            data: b"lost-tail",
        },
        Step::WaitForTopicPublisher {
            mode: TopicMode::Ordered,
            participant: "pub",
            topic: "state",
            next_sequence: 2,
        },
        Step::Run {
            description: "Consume the tail's actual transport attempt under complete loss",
            duration: Duration::from_millis(100),
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
        Step::SetLoss {
            description: "Restore subscriber A without replacing its publication",
            participant: "sub_a",
            loss: Loss::wifi(),
        },
        Step::SetLoss {
            description: "Restore subscriber B without replacing its publication",
            participant: "sub_b",
            loss: Loss::wifi(),
        },
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
        Step::PublishData {
            mode: TopicMode::Ordered,
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
        Step::PublishData {
            mode: TopicMode::Ordered,
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
    ];
    for participant in ["sub_a", "sub_b", "late"] {
        plan.push(Step::CheckTopicIdentity {
            description: "retained tail recovery does not report a historical gap",
            participant,
            topic: "state",
            mode: TopicMode::Ordered,
            publishers: &["pub"],
            gap: None,
        });
    }
    for participant in ["sub_a", "sub_b"] {
        plan.push(Step::SetLoss {
            description: "keep the same live track while falling behind retention",
            participant,
            loss: Loss::Independent(1.0),
        });
    }
    for sequence in 0..SENT {
        plan.push(Step::PublishData {
            description: "advance finite production history",
            participant: "pub",
            topic: "state",
            mode: TopicMode::Ordered,
            data: b"window",
        });
        plan.push(Step::WaitForTopicPublisher {
            participant: "pub",
            topic: "state",
            mode: TopicMode::Ordered,
            next_sequence: u64::try_from(sequence).unwrap().saturating_add(5),
        });
    }
    plan.push(Step::Run {
        description: "consume the final impaired data attempt",
        duration: Duration::from_millis(100),
    });
    for participant in ["sub_a", "sub_b"] {
        plan.push(Step::CheckDataSequence {
            description: "an admitted missing range was actually lost",
            participant,
            topic: "state",
            expected: &[b"first", b"lost-tail", b"later", b"live"],
        });
        plan.push(Step::SetLoss {
            description: "resume the same track after retained history is exhausted",
            participant,
            loss: Loss::wifi(),
        });
    }
    for participant in ["sub_a", "sub_b"] {
        plan.push(Step::WaitForDataSequence {
            participant,
            topic: "state",
            expected: &RECOVERED[..RETAINED + 4],
        });
    }
    plan.push(Step::WaitForDataSequence {
        participant: "late",
        topic: "state",
        expected: &LATE[..SENT + 1],
    });
    plan.push(Step::PublishData {
        description: "continue only after replayed-window recovery",
        participant: "pub",
        topic: "state",
        mode: TopicMode::Ordered,
        data: b"continued",
    });
    for participant in ["sub_a", "sub_b"] {
        plan.push(Step::WaitForDataSequence {
            participant,
            topic: "state",
            expected: &RECOVERED,
        });
        plan.push(Step::CheckTopicIdentity {
            description: "one exact inline gap precedes increasing resumed delivery",
            participant,
            topic: "state",
            mode: TopicMode::Ordered,
            publishers: &["pub"],
            gap: Some((4, u64::try_from(SENT + 4 - RETAINED).unwrap())),
        });
    }
    plan.push(Step::WaitForDataSequence {
        participant: "late",
        topic: "state",
        expected: &LATE,
    });
    plan.push(Step::CheckTopicIdentity {
        description: "another subscriber's recovery floor never skips a healthy track",
        participant: "late",
        topic: "state",
        mode: TopicMode::Ordered,
        publishers: &["pub"],
        gap: None,
    });
    plan.push(Step::Run {
        description: "recovered control and shard state converge",
        duration: Duration::from_millis(500),
    });
    LocalNodeSim::new()
        .with_udp_only()
        .with_room(
            Room::new("room-reliable-xshard")
                .with_participant(Participant::data_participant("pub"))
                .with_participant(Participant::data_participant("sub_a"))
                .with_participant(Participant::data_participant("sub_b"))
                .with_participant(Participant::data_participant("late").starts_disconnected()),
        )
        .run(plan);
}

/// Data crossing a shard boundary.
///
/// The latest data lane has its own route family, its own import lifecycle
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
                mode: TopicMode::Latest,
                description: "Publisher declares topic",
                participant: "pub",
                topic: "xshard_topic",
            },
            Step::DeclareSubscribeTopic {
                mode: TopicMode::Latest,
                description: "Subscriber on another shard subscribes",
                participant: "sub",
                topic: "xshard_topic",
            },
            Step::DeclareSubscribeTopic {
                mode: TopicMode::Latest,
                description: "And a second, so at least one lands off-shard",
                participant: "sub2",
                topic: "xshard_topic",
            },
            Step::Run {
                description: "Let the destination install its data route",
                duration: Duration::from_secs(2),
            },
            Step::PublishData {
                mode: TopicMode::Latest,
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

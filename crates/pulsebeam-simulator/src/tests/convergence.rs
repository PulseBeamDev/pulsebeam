use super::common::{LocalNodeSim, Participant, Room, Step};
use pulsebeam_agent_native::agent_core::TopicMode;
use std::time::Duration;

#[test]
fn crashed_publications_retire_from_every_owner_and_observer() {
    let mut plan = vec![Step::Run {
        description: "admit all observers and negotiate every lane",
        duration: Duration::from_secs(5),
    }];
    for participant in ["viewer1", "viewer2"] {
        plan.push(Step::DeclareSubscribeTopic {
            mode: TopicMode::Latest,
            description: "receive latest data",
            participant,
            topic: "latest",
        });
        plan.push(Step::DeclareSubscribeTopic {
            mode: TopicMode::Ordered,
            description: "receive ordered data",
            participant,
            topic: "ordered",
        });
    }
    plan.extend([
        Step::DeclarePublishTopic {
            mode: TopicMode::Latest,
            description: "publish latest data",
            participant: "publisher",
            topic: "latest",
        },
        Step::DeclarePublishTopic {
            mode: TopicMode::Ordered,
            description: "publish ordered data",
            participant: "publisher",
            topic: "ordered",
        },
        Step::Run {
            description: "settle channel registration",
            duration: Duration::from_secs(3),
        },
        Step::PublishData {
            mode: TopicMode::Latest,
            description: "send latest payload",
            participant: "publisher",
            topic: "latest",
            data: b"latest-origin",
        },
        Step::PublishData {
            mode: TopicMode::Ordered,
            description: "send reliable payload",
            participant: "publisher",
            topic: "ordered",
            data: b"ordered-origin",
        },
        Step::Run {
            description: "deliver all lanes across destination shards",
            duration: Duration::from_secs(5),
        },
    ]);
    for participant in ["viewer1", "viewer2"] {
        plan.extend([
            Step::CheckVideoReceivedFrom {
                description: "every intended viewer received the publisher",
                participant,
                publisher: "publisher",
                min_frames: 1,
            },
            Step::CheckHeardFrom {
                description: "every intended listener decoded the publisher",
                participant,
                expected: &["publisher"],
            },
            Step::CheckDataReceived {
                description: "every intended latest recipient received the payload",
                participant,
                topic: "latest",
                expected: b"latest-origin",
            },
            Step::CheckDataReceived {
                description: "every intended ordered recipient received the payload",
                participant,
                topic: "ordered",
                expected: b"ordered-origin",
            },
            Step::CheckDataCount {
                description: "ordered delivery is exactly once",
                participant,
                topic: "ordered",
                expected: 1,
            },
        ]);
    }
    for participant in ["bystander", "other-room"] {
        plan.extend([
            Step::CheckVideoNotReceivedFrom {
                description: "intentional bystanders received no video",
                participant,
                publisher: "publisher",
            },
            Step::CheckHeardFrom {
                description: "intentional bystanders heard no audio",
                participant,
                expected: &[],
            },
            Step::CheckDataNotReceived {
                description: "intentional bystanders received no latest payload",
                participant,
                topic: "latest",
                excluded: b"latest-origin",
            },
            Step::CheckDataNotReceived {
                description: "intentional bystanders received no ordered payload",
                participant,
                topic: "ordered",
                excluded: b"ordered-origin",
            },
        ]);
    }
    plan.extend([
        Step::AbruptExit {
            description: "stop the publisher without DELETE",
            participant: "publisher",
        },
        Step::Run {
            description: "exceed the ICE-lite detection and retirement bound",
            duration: Duration::from_secs(25),
        },
    ]);
    LocalNodeSim::new()
        .with_room(
            Room::new("retirement")
                .with_participant(Participant::single_publisher("publisher").speaking_at(-20))
                .with_participant(Participant::subscriber("viewer1").hearing(1))
                .with_participant(Participant::subscriber("viewer2").hearing(1))
                .with_participant(Participant::manual_subscriber("bystander", 1)),
        )
        .with_room(
            Room::new("isolation")
                .with_participant(Participant::subscriber("other-room").hearing(1)),
        )
        .run(plan);
}

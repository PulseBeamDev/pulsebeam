use super::common::{LocalNodeSim, Participant, Room, Step};
use pulsebeam_agent_native::agent_core::TopicMode;
use pulsebeam_testdata::QualityVideoSource;
use std::time::Duration;

#[test]
fn crashed_publications_retire_from_every_public_catalog() {
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
            mode: TopicMode::Lossy,
            description: "receive lossy data",
            participant,
            topic: "lossy",
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
            mode: TopicMode::Lossy,
            description: "publish lossy data",
            participant: "publisher",
            topic: "lossy",
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
            mode: TopicMode::Lossy,
            description: "send reliable payload",
            participant: "publisher",
            topic: "lossy",
            data: b"lossy-origin",
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
            Step::CheckAudioCatalogFrom {
                description: "every intended listener is assigned the publisher",
                participant,
                expected: &["publisher"],
            },
            Step::CheckNativeAudioDecoded {
                description: "Actual native PCM and SDK decode progress, without per-block source attribution",
                participant,
                min_samples: 4_800,
                min_receivers: 1,
                max_silence: None,
            },
            Step::CheckDataReceived {
                description: "every intended latest recipient received the payload",
                participant,
                topic: "latest",
                expected: b"latest-origin",
            },
            Step::CheckDataReceived {
                description: "every intended lossy recipient received the payload",
                participant,
                topic: "lossy",
                expected: b"lossy-origin",
            },
            Step::CheckDataCount {
                description: "lossy delivery is exactly once",
                participant,
                topic: "lossy",
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
            Step::CheckAudioCatalogFrom {
                description: "intentional bystanders have no audio assignment",
                participant,
                expected: &[],
            },
            Step::CheckNativeAudioSilent {
                description: "No native ingress or nontrivial PCM for an excluded listener",
                participant,
            },
            Step::CheckDataNotReceived {
                description: "intentional bystanders received no latest payload",
                participant,
                topic: "latest",
                excluded: b"latest-origin",
            },
            Step::CheckDataNotReceived {
                description: "intentional bystanders received no lossy payload",
                participant,
                topic: "lossy",
                excluded: b"lossy-origin",
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
                .with_participant(
                    Participant::quality_publisher("publisher", QualityVideoSource::Zero)
                        .speaking_at(-20),
                )
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

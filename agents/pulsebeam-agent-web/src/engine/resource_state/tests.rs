use super::*;
use agent_core::{
    Agent, AgentConfig, ResourceCommand, ResourceResult, TopicMode, TrackSelector, TrackSpec,
};

fn config() -> AgentConfig {
    AgentConfig {
        endpoint: "https://example.com".into(),
        token: "token".into(),
        retry: Default::default(),
        log_level: Default::default(),
    }
}

fn selector(label: &str) -> TrackSelector {
    TrackSelector {
        room: "room".into(),
        participant_external_id: "alice".into(),
        label: label.into(),
    }
}

fn owner(agent: &mut Agent, command: ResourceCommand) -> u64 {
    let ResourceResult::Owned { owner } = agent.resource(command).unwrap() else {
        panic!("expected owned resource");
    };
    owner
}

#[test]
fn named_resources_project_their_own_prejoin_configuration() {
    let mut agent = Agent::new(config()).unwrap();
    let video = owner(
        &mut agent,
        ResourceCommand::ClaimVideo {
            selector: selector("camera"),
        },
    );
    let other_video = owner(
        &mut agent,
        ResourceCommand::ClaimVideo {
            selector: selector("camera"),
        },
    );
    let audio = owner(
        &mut agent,
        ResourceCommand::ClaimAudioSubscription {
            selector: selector("microphone"),
        },
    );
    let data = TrackSpec::Data {
        label: "chat".into(),
        delivery: TopicMode::Ordered,
    };
    let publication = owner(
        &mut agent,
        ResourceCommand::DeclareDataPublication { spec: data.clone() },
    );
    let subscription = owner(
        &mut agent,
        ResourceCommand::ReceiveData {
            spec: data,
            source: selector("chat"),
        },
    );

    agent
        .resource(ResourceCommand::ObserveVideo {
            owner: video,
            observation: agent_core::VideoObservation {
                visible: true,
                height: 720,
                ..Default::default()
            },
        })
        .unwrap();
    agent
        .resource(ResourceCommand::ConfigureAudio {
            owner: audio,
            playout_delay: agent_core::PlayoutDelay::Fixed {
                min_ms: 100,
                max_ms: 200,
            },
        })
        .unwrap();

    let Some(ResourceState::Video(state)) = ResourceKind::Video.state(&agent, video) else {
        panic!("expected video projection")
    };
    assert_eq!(
        (
            state.selector.room.as_str(),
            state.selector.participant_id.as_str(),
            state.selector.label.as_str()
        ),
        ("room", "alice", "camera")
    );
    assert!(state.local_enabled);
    assert_eq!(state.configuration.height, 720);
    assert!(!state.closed);
    let Some(ResourceState::Video(other)) = ResourceKind::Video.state(&agent, other_video) else {
        panic!("expected other video projection")
    };
    assert!(!other.local_enabled);
    let Some(ResourceState::Audio(state)) = ResourceKind::Audio.state(&agent, audio) else {
        panic!("expected audio projection")
    };
    assert_eq!(
        (
            state.selector.room.as_str(),
            state.selector.participant_id.as_str(),
            state.selector.label.as_str()
        ),
        ("room", "alice", "microphone")
    );
    assert_eq!(
        state.playout_delay,
        agent_core::ffi::PlayoutDelay::Fixed {
            min_ms: 100,
            max_ms: 200
        }
    );
    assert!(!state.closed);
    let Some(ResourceState::DataPublication(state)) =
        ResourceKind::DataPublication.state(&agent, publication)
    else {
        panic!("expected data publication projection")
    };
    assert_eq!(state.label, "chat");
    assert_eq!(state.delivery, agent_core::ffi::TopicMode::Ordered);
    assert!(!state.closed);
    let Some(ResourceState::DataSubscription(state)) =
        ResourceKind::DataSubscription.state(&agent, subscription)
    else {
        panic!("expected data subscription projection")
    };
    assert_eq!(
        (
            state.selector.room.as_str(),
            state.selector.participant_id.as_str(),
            state.selector.label.as_str()
        ),
        ("room", "alice", "chat")
    );
    assert_eq!(state.delivery, agent_core::ffi::TopicMode::Ordered);
    assert!(!state.closed);
    assert!(ResourceKind::Video.state(&agent, audio).is_none());
    assert!(ResourceKind::Audio.state(&agent, video).is_none());
    assert!(
        ResourceKind::DataPublication
            .state(&agent, subscription)
            .is_none()
    );
    assert!(
        ResourceKind::DataSubscription
            .state(&agent, publication)
            .is_none()
    );
}

struct TestHost;

impl super::super::Host for TestHost {
    fn publish_turn(&self, _: &super::super::Turn) {}
    fn execute_effect(&self, _: agent_core::Effect) {}
    fn host_failed(&self, _: &agent_core::Failure) {}
    fn shutdown(&self) {}
}

#[test]
fn driver_release_projects_final_state_and_retires_only_that_owner() {
    use super::super::{Actor, channel::mpsc};

    let mut actor = Actor::new(config(), TestHost).unwrap();
    let (sender, _receiver) = mpsc::channel(4);
    let handle = actor.handle(sender);
    let claim = |actor: &mut Actor<TestHost>, label: &str| {
        let (result, _) = actor.driver.resource(ResourceCommand::ClaimVideo {
            selector: selector(label),
        });
        let ResourceResult::Owned { owner } = result.unwrap() else {
            panic!("expected video owner");
        };
        owner
    };
    let first = claim(&mut actor, "camera");
    let second = claim(&mut actor, "screen");
    assert!(matches!(
        handle.observe_resource(first, ResourceKind::Video),
        Some(ResourceState::Video(_))
    ));
    assert!(matches!(
        handle.observe_resource(second, ResourceKind::Video),
        Some(ResourceState::Video(_))
    ));

    let (result, turn) = actor.driver.resource(ResourceCommand::ObserveVideo {
        owner: first,
        observation: agent_core::VideoObservation {
            visible: true,
            height: 720,
            ..Default::default()
        },
    });
    result.unwrap();
    let (_, ResourceState::Video(before), closing) = turn
        .resources
        .iter()
        .find(|(owner, _, _)| *owner == first)
        .expect("updated owner projected")
    else {
        panic!("expected video state");
    };
    assert!(!closing);
    assert!(before.local_enabled);
    assert_eq!(before.configuration.height, 720);

    let (result, turn) = actor
        .driver
        .resource(ResourceCommand::Release { owner: first });
    result.unwrap();
    let states: Vec<_> = turn
        .resources
        .iter()
        .filter(|(owner, _, _)| *owner == first)
        .collect();
    assert_eq!(states.len(), 1, "release must emit one final state");
    let (_, ResourceState::Video(final_state), closing) = states[0] else {
        panic!("expected final video state")
    };
    assert!(*closing);
    assert!(final_state.closed);
    assert_eq!(final_state.selector.label, "camera");
    assert!(final_state.local_enabled);
    assert_eq!(final_state.configuration.height, 720);
    assert!(handle.observed_resource_state(first).is_none());
    assert!(
        turn.resources
            .iter()
            .any(|(owner, state, closing)| *owner == second
                && matches!(state, ResourceState::Video(_))
                && !closing)
    );

    let (result, turn) = actor.driver.resource(ResourceCommand::ObserveVideo {
        owner: second,
        observation: Default::default(),
    });
    result.unwrap();
    assert!(turn.resources.iter().all(|(owner, _, _)| *owner != first));
}

#[test]
fn queued_owned_data_send_cannot_outlive_its_owner_or_join() {
    use super::super::{Actor, DataSendError, Input, channel::mpsc};
    use agent_core::ffi::{ErrorCode, ErrorInfo};

    for released in [false, true] {
        let mut actor = Actor::new(config(), TestHost).unwrap();
        let (sender, receiver) = mpsc::channel(4);
        let handle = actor.handle(sender);
        let ResourceResult::Owned { owner } = handle
            .resource_now(ResourceCommand::DeclareDataPublication {
                spec: TrackSpec::Data {
                    label: "chat".into(),
                    delivery: TopicMode::Ordered,
                },
            })
            .unwrap()
        else {
            panic!("expected data publication");
        };
        let mut pending = handle.send_owned_data(owner, vec![0, 255]).unwrap();
        if released {
            handle
                .resource_now(ResourceCommand::Release { owner })
                .unwrap();
        } else {
            actor.turn(Input::Command(agent_core::AgentCommand::Join(
                "other-room".into(),
            )));
        }
        while let Some(message) = receiver.try_recv() {
            actor.process(message);
        }
        let Some(Err(DataSendError::Admission(error))) = pending.try_recv().unwrap() else {
            panic!("queued payload must be fenced before admission");
        };
        assert_eq!(
            ErrorInfo::from(&error).code,
            if released {
                ErrorCode::Closed
            } else {
                ErrorCode::NotReady
            }
        );
    }
}

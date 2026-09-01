use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::time::Duration;

use pulsebeam_proto::{
    prelude::Message,
    reliable::{RelControl, RelDelivery, RelMsg, RelNack, rel_control},
    signaling::{self, client_message, server_message},
};

use crate::*;

const PARTICIPANT_URI: &str = "https://sfu.test/api/v1/rooms/room/participants/p1?manual_sub=true";
const LOCAL_PUBLISHER: &str = "pa_00000000000000000000000000";
const REMOTE_PUBLISHER: &str = "pa_11111111111111111111111111";

fn config() -> AgentConfig {
    AgentConfig {
        endpoint: "https://sfu.test/".to_string(),
        room_id: "room".to_string(),
        topology: MediaTopology {
            local_video: vec!["camera".to_string()],
            local_audio: vec!["microphone".to_string()],
            remote_video: 1,
            remote_audio: 1,
        },
        manual_subscriptions: true,
        retry: RetryPolicy::default(),
    }
}

fn desired(revision: u64) -> DesiredState {
    DesiredState {
        revision,
        connected: true,
        publications: vec![PublicationIntent {
            slot: "camera".to_string(),
            active: true,
        }],
        video: vec![VideoSubscription {
            slot: 0,
            track_id: "video-track".to_string(),
            height: 720,
            min_height: 180,
            min_fps: 15,
            priority: 100,
        }],
        audio: AudioSubscription {
            pinned: vec!["audio-track".to_string()],
            automatic: true,
        },
        playout_delay: PlayoutDelay::Adaptive,
        topics: TopicRegistrations::default(),
    }
}

fn channel(value: u64) -> ChannelId {
    ChannelId::new(value).unwrap()
}

fn resources(channel: ChannelId) -> OfferResources {
    OfferResources {
        slots: vec![
            SlotBinding {
                slot: MediaSlot::LocalVideo("camera".to_string()),
                mid: "lv0".to_string(),
            },
            SlotBinding {
                slot: MediaSlot::LocalAudio("microphone".to_string()),
                mid: "la0".to_string(),
            },
            SlotBinding {
                slot: MediaSlot::RemoteVideo(0),
                mid: "rv0".to_string(),
            },
            SlotBinding {
                slot: MediaSlot::RemoteAudio(0),
                mid: "ra0".to_string(),
            },
        ],
        signaling_channel: channel,
        data_channels: vec![],
    }
}

fn publisher(topic: &str, mode: TopicMode) -> TopicPublisher {
    TopicPublisher {
        topic: topic.to_string(),
        mode,
    }
}

fn subscriber(topic: &str, mode: TopicMode, publisher_id: Option<&str>) -> TopicSubscriber {
    TopicSubscriber {
        topic: topic.to_string(),
        mode,
        publisher_id: publisher_id.map(str::to_string),
    }
}

fn connect_with_topics(
    registrations: TopicRegistrations,
) -> (Agent, Generation, ChannelId, BTreeMap<String, ChannelId>) {
    let mut agent = Agent::new(config()).unwrap();
    let mut state = desired(1);
    state.topics = registrations;
    agent.command(AgentCommand::ReplaceDesired(state)).unwrap();
    let (generation, specs) = match next_effect(&mut agent) {
        Effect::Rtc(RtcEffect::CreateOffer {
            generation,
            data_channels,
            ..
        }) => (generation, data_channels),
        effect => panic!("expected topic offer, got {effect:?}"),
    };
    let signal = channel(30);
    let mut channels = BTreeMap::new();
    let bindings = specs
        .iter()
        .skip(1)
        .enumerate()
        .map(|(index, spec)| {
            let id = channel(31 + u64::try_from(index).unwrap());
            channels.insert(spec.label.clone(), id);
            DataChannelBinding {
                label: spec.label.clone(),
                channel: id,
            }
        })
        .collect();
    let mut offer_resources = resources(signal);
    offer_resources.data_channels = bindings;
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: "topic-offer".to_string(),
            resources: offer_resources,
        }))
        .unwrap();
    let operation = match next_effect(&mut agent) {
        Effect::Http(HttpEffect::Request { operation, .. }) => operation,
        effect => panic!("expected participant request, got {effect:?}"),
    };
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: create_response(LOCAL_PUBLISHER, "etag-1", PARTICIPANT_URI),
        }))
        .unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::ApplyAnswer { .. })
    ));
    agent
        .handle(HostEvent::Rtc(RtcEvent::AnswerApplied { generation }))
        .unwrap();
    agent
        .handle(HostEvent::Rtc(RtcEvent::Connected { generation }))
        .unwrap();
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            channel: signal,
        }))
        .unwrap();
    for channel in channels.values().copied() {
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Opened {
                generation,
                channel,
            }))
            .unwrap();
    }
    let signaling = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            operation, channel, ..
        }) => {
            assert_eq!(channel, signal);
            operation
        }
        effect => panic!("expected signaling intent, got {effect:?}"),
    };
    acknowledge_send(&mut agent, generation, signal, signaling);
    let _ = drain_notifications(&mut agent);
    (agent, generation, signal, channels)
}

fn ordered_delivery(publisher_id: &str, stream_id: u64, seq: u64, payload: &[u8]) -> Vec<u8> {
    RelDelivery {
        publisher_id: publisher_id.to_string(),
        frame: RelMsg {
            stream_id,
            seq,
            payload: payload.to_vec(),
            resync_required: false,
        }
        .encode_to_vec(),
    }
    .encode_to_vec()
}

fn create_response(participant: &str, etag: &str, uri: &str) -> HttpResponse {
    HttpResponse {
        status: 201,
        headers: vec![
            HttpHeader {
                name: "location".to_string(),
                value: uri.to_string(),
            },
            HttpHeader {
                name: "ETAG".to_string(),
                value: format!("\"{etag}\""),
            },
            HttpHeader {
                name: "pb-participant-id".to_string(),
                value: participant.to_string(),
            },
        ],
        body: b"answer".to_vec(),
    }
}

fn update_response(etag: &str) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![HttpHeader {
            name: "etag".to_string(),
            value: etag.to_string(),
        }],
        body: b"replacement-answer".to_vec(),
    }
}

fn next_effect(agent: &mut Agent) -> Effect {
    agent.next_effect().expect("expected effect")
}

fn begin_connect(
    agent: &mut Agent,
    state: DesiredState,
    cid: ChannelId,
) -> (Generation, OperationId) {
    agent.command(AgentCommand::ReplaceDesired(state)).unwrap();
    let generation = match next_effect(agent) {
        Effect::Rtc(RtcEffect::CreateOffer {
            generation,
            topology,
            data_channels,
        }) => {
            assert_eq!(topology, config().topology);
            assert_eq!(data_channels.len(), 1);
            assert_eq!(data_channels[0].label, "v1/sys/signaling");
            generation
        }
        effect => panic!("expected CreateOffer, got {effect:?}"),
    };
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: "offer".to_string(),
            resources: resources(cid),
        }))
        .unwrap();
    let operation = match next_effect(agent) {
        Effect::Http(HttpEffect::Request {
            operation,
            generation: Some(actual_generation),
            request,
        }) => {
            assert_eq!(actual_generation, generation);
            assert_eq!(request.method, HttpMethod::Post);
            assert!(request.uri.ends_with("/participants?manual_sub=true"));
            operation
        }
        effect => panic!("expected create request, got {effect:?}"),
    };
    (generation, operation)
}

fn finish_connect(
    agent: &mut Agent,
    generation: Generation,
    operation: OperationId,
    cid: ChannelId,
) -> OperationId {
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: create_response("p1", "etag-1", PARTICIPANT_URI),
        }))
        .unwrap();
    assert_eq!(
        next_effect(agent),
        Effect::Rtc(RtcEffect::ApplyAnswer {
            generation,
            answer: "answer".to_string(),
        })
    );
    agent
        .handle(HostEvent::Rtc(RtcEvent::Connected { generation }))
        .unwrap();
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            channel: cid,
        }))
        .unwrap();
    assert_eq!(agent.snapshot().connection, ConnectionState::ApplyingAnswer);
    agent
        .handle(HostEvent::Rtc(RtcEvent::AnswerApplied { generation }))
        .unwrap();
    match next_effect(agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            operation,
            generation: actual_generation,
            channel,
            binary,
            payload,
        }) => {
            assert_eq!(actual_generation, generation);
            assert_eq!(channel, cid);
            assert!(binary);
            assert!(!payload.is_empty());
            operation
        }
        effect => panic!("expected signaling send, got {effect:?}"),
    }
}

fn connected_agent() -> (Agent, Generation, ChannelId, OperationId) {
    let mut agent = Agent::new(config()).unwrap();
    let cid = channel(9);
    let (generation, operation) = begin_connect(&mut agent, desired(1), cid);
    let send = finish_connect(&mut agent, generation, operation, cid);
    (agent, generation, cid, send)
}

fn acknowledge_send(
    agent: &mut Agent,
    generation: Generation,
    cid: ChannelId,
    operation: OperationId,
) {
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Sent {
            operation,
            generation,
            channel: cid,
        }))
        .unwrap();
}

fn decode_intent(payload: &[u8]) -> signaling::ClientIntent {
    let message = signaling::ClientMessage::decode(payload).unwrap();
    match message.payload.unwrap() {
        client_message::Payload::Intent(intent) => intent,
    }
}

fn server_state(state: signaling::ServerState) -> Vec<u8> {
    signaling::ServerMessage {
        payload: Some(server_message::Payload::State(state)),
    }
    .encode_to_vec()
}

fn drain_notifications(agent: &mut Agent) -> Vec<Notification> {
    let mut notifications = Vec::new();
    while let Some(notification) = agent.next_notification() {
        notifications.push(notification);
    }
    notifications
}

#[test]
fn construction_and_desired_state_validate_complete_external_input() {
    let mut invalid_endpoint = config();
    invalid_endpoint.endpoint = "https://".to_string();
    assert!(matches!(
        Agent::new(invalid_endpoint),
        Err(AgentError::InvalidConfiguration(ValidationError::Endpoint))
    ));

    let mut invalid = config();
    invalid.topology.local_video.push("second".to_string());
    invalid.topology.local_video.push("third".to_string());
    assert!(matches!(
        Agent::new(invalid),
        Err(AgentError::InvalidConfiguration(
            ValidationError::SlotLimit { .. }
        ))
    ));

    let mut duplicate = config();
    duplicate.topology.local_audio = vec!["camera".to_string()];
    assert!(matches!(
        Agent::new(duplicate),
        Err(AgentError::InvalidConfiguration(
            ValidationError::Duplicate { .. }
        ))
    ));

    let mut agent = Agent::new(config()).unwrap();
    let mut invalid_desired = desired(1);
    invalid_desired.video[0].min_height = 721;
    assert_eq!(
        agent.command(AgentCommand::ReplaceDesired(invalid_desired)),
        Err(AgentError::InvalidConfiguration(
            ValidationError::VideoHeight
        ))
    );
    assert_eq!(agent.snapshot().desired_revision, 0);
    assert!(agent.next_effect().is_none());

    let mut invalid_delay = desired(2);
    invalid_delay.playout_delay = PlayoutDelay::Fixed {
        min_ms: 200,
        max_ms: 100,
    };
    assert_eq!(
        agent.command(AgentCommand::ReplaceDesired(invalid_delay)),
        Err(AgentError::InvalidConfiguration(
            ValidationError::PlayoutDelay
        ))
    );
}

#[test]
fn desired_revisions_are_idempotent_and_offer_events_are_correlated() {
    let mut agent = Agent::new(config()).unwrap();
    let state = desired(1);
    let (generation, _) = begin_connect(&mut agent, state.clone(), channel(1));

    agent
        .command(AgentCommand::ReplaceDesired(state.clone()))
        .unwrap();
    assert!(agent.next_effect().is_none());

    let mut conflicting = state.clone();
    conflicting.audio.automatic = false;
    assert_eq!(
        agent.command(AgentCommand::ReplaceDesired(conflicting)),
        Err(AgentError::ConflictingDesiredRevision(1))
    );
    let mut stale = state;
    stale.revision = 0;
    assert!(matches!(
        agent.command(AgentCommand::ReplaceDesired(stale)),
        Err(AgentError::StaleDesiredRevision { .. })
    ));

    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: "duplicate".to_string(),
            resources: resources(channel(1)),
        }))
        .unwrap();
    assert!(agent.next_effect().is_none());
}

#[test]
fn malformed_offer_resources_are_rejected_without_consuming_the_attempt() {
    let mut agent = Agent::new(config()).unwrap();
    agent
        .command(AgentCommand::ReplaceDesired(desired(1)))
        .unwrap();
    let generation = match next_effect(&mut agent) {
        Effect::Rtc(RtcEffect::CreateOffer { generation, .. }) => generation,
        effect => panic!("expected offer, got {effect:?}"),
    };
    assert_eq!(
        agent.handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: "offer".to_string(),
            resources: OfferResources {
                slots: vec![],
                signaling_channel: channel(3),
                data_channels: vec![],
            },
        })),
        Err(AgentError::InvalidOffer(
            "slot mapping does not match configured topology"
        ))
    );
    assert_eq!(agent.snapshot().connection, ConnectionState::CreatingOffer);
    assert!(agent.next_effect().is_none());

    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: "corrected-offer".to_string(),
            resources: resources(channel(3)),
        }))
        .unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Http(HttpEffect::Request { .. })
    ));
}

#[test]
fn connection_waits_for_every_host_boundary_and_closes_both_resources() {
    let (mut agent, generation, cid, send) = connected_agent();
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert_eq!(agent.snapshot().participant_id.as_deref(), Some("p1"));
    acknowledge_send(&mut agent, generation, cid, send);

    let disconnected = DesiredState {
        revision: 2,
        ..DesiredState::default()
    };
    agent
        .command(AgentCommand::ReplaceDesired(disconnected))
        .unwrap();
    assert_eq!(agent.snapshot().connection, ConnectionState::Closing);
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close { generation })
    );
    let delete = match next_effect(&mut agent) {
        Effect::Http(HttpEffect::Request {
            operation,
            generation: None,
            request,
        }) => {
            assert_eq!(request.method, HttpMethod::Delete);
            assert_eq!(request.uri, PARTICIPANT_URI);
            operation
        }
        effect => panic!("expected delete, got {effect:?}"),
    };
    agent
        .handle(HostEvent::Rtc(RtcEvent::Closed { generation }))
        .unwrap();
    assert_eq!(agent.snapshot().connection, ConnectionState::Closing);
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation: delete,
            response: HttpResponse {
                status: 204,
                headers: vec![],
                body: vec![],
            },
        }))
        .unwrap();
    assert_eq!(agent.snapshot().connection, ConnectionState::Disconnected);
    assert_eq!(agent.snapshot().generation, None);
}

#[test]
fn transient_failure_schedules_a_bounded_retry_that_disconnect_cancels() {
    let mut agent = Agent::new(config()).unwrap();
    let (_generation, operation) = begin_connect(&mut agent, desired(1), channel(2));
    agent
        .handle(HostEvent::Http(HttpEvent::Failed {
            operation,
            message: "network down".to_string(),
        }))
        .unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close { .. })
    ));
    let timer = match next_effect(&mut agent) {
        Effect::Timer(TimerEffect::Schedule { timer, after }) => {
            assert_eq!(after, Duration::from_millis(500));
            timer
        }
        effect => panic!("expected retry timer, got {effect:?}"),
    };
    assert_eq!(
        agent.snapshot().connection,
        ConnectionState::RetryWaiting {
            attempt: 1,
            after: Duration::from_millis(500),
        }
    );

    let disconnected = DesiredState {
        revision: 2,
        ..DesiredState::default()
    };
    agent
        .command(AgentCommand::ReplaceDesired(disconnected))
        .unwrap();
    assert_eq!(
        next_effect(&mut agent),
        Effect::Timer(TimerEffect::Cancel { timer })
    );
    assert_eq!(agent.snapshot().connection, ConnectionState::Disconnected);
    agent
        .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
        .unwrap();
    assert!(agent.next_effect().is_none());
}

#[test]
fn replacement_uses_etag_and_swaps_only_after_the_candidate_is_ready() {
    let (mut agent, old_generation, old_cid, send) = connected_agent();
    acknowledge_send(&mut agent, old_generation, old_cid, send);
    agent
        .handle(HostEvent::Rtc(RtcEvent::Disconnected {
            generation: old_generation,
        }))
        .unwrap();
    let replacement_generation = match next_effect(&mut agent) {
        Effect::Rtc(RtcEffect::CreateOffer { generation, .. }) => generation,
        effect => panic!("expected replacement offer, got {effect:?}"),
    };
    assert_eq!(agent.snapshot().generation, Some(old_generation));
    let replacement_cid = channel(10);
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation: replacement_generation,
            offer: "replacement-offer".to_string(),
            resources: resources(replacement_cid),
        }))
        .unwrap();
    let update = match next_effect(&mut agent) {
        Effect::Http(HttpEffect::Request {
            operation, request, ..
        }) => {
            assert_eq!(request.method, HttpMethod::Patch);
            assert_eq!(request.uri, PARTICIPANT_URI);
            assert_eq!(
                request
                    .headers
                    .iter()
                    .find(|header| header.name == "If-Match")
                    .map(|header| header.value.as_str()),
                Some("etag-1")
            );
            operation
        }
        effect => panic!("expected PATCH, got {effect:?}"),
    };
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation: update,
            response: update_response("etag-2"),
        }))
        .unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::ApplyAnswer { .. })
    ));
    agent
        .handle(HostEvent::Rtc(RtcEvent::AnswerApplied {
            generation: replacement_generation,
        }))
        .unwrap();
    agent
        .handle(HostEvent::Rtc(RtcEvent::Connected {
            generation: replacement_generation,
        }))
        .unwrap();
    assert_eq!(agent.snapshot().generation, Some(old_generation));
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Opened {
            generation: replacement_generation,
            channel: replacement_cid,
        }))
        .unwrap();
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close {
            generation: old_generation,
        })
    );
    assert!(matches!(
        next_effect(&mut agent),
        Effect::DataChannel(DataChannelEffect::Send {
            generation,
            channel,
            ..
        }) if generation == replacement_generation && channel == replacement_cid
    ));
    assert_eq!(agent.snapshot().generation, Some(replacement_generation));
    assert_eq!(agent.snapshot().participant_id.as_deref(), Some("p1"));
}

#[test]
fn expired_replacement_falls_back_to_a_fresh_participant() {
    let (mut agent, old_generation, old_cid, send) = connected_agent();
    acknowledge_send(&mut agent, old_generation, old_cid, send);
    agent
        .handle(HostEvent::Rtc(RtcEvent::Disconnected {
            generation: old_generation,
        }))
        .unwrap();
    let replacement_generation = match next_effect(&mut agent) {
        Effect::Rtc(RtcEffect::CreateOffer { generation, .. }) => generation,
        effect => panic!("expected replacement offer, got {effect:?}"),
    };
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation: replacement_generation,
            offer: "replacement-offer".to_string(),
            resources: resources(channel(11)),
        }))
        .unwrap();
    let update = match next_effect(&mut agent) {
        Effect::Http(HttpEffect::Request { operation, .. }) => operation,
        effect => panic!("expected update, got {effect:?}"),
    };
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation: update,
            response: HttpResponse {
                status: 410,
                headers: vec![],
                body: vec![],
            },
        }))
        .unwrap();
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close {
            generation: replacement_generation,
        })
    );
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::CreateOffer { .. })
    ));
    assert_eq!(agent.snapshot().generation, Some(old_generation));
    assert_eq!(agent.snapshot().connection, ConnectionState::Reconnecting);
    assert!(drain_notifications(&mut agent).iter().any(|notification| {
        matches!(
            notification,
            Notification::Failure(Failure {
                class: FailureClass::ResourceExpired,
                ..
            })
        )
    }));
}

#[test]
fn authorization_is_terminal_for_one_revision_and_a_new_revision_retries() {
    let mut agent = Agent::new(config()).unwrap();
    let (failed_generation, operation) = begin_connect(&mut agent, desired(1), channel(12));
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: HttpResponse {
                status: 401,
                headers: vec![],
                body: vec![],
            },
        }))
        .unwrap();
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close {
            generation: failed_generation,
        })
    );
    assert_eq!(
        agent.snapshot().connection,
        ConnectionState::TerminalFailure
    );
    assert_eq!(
        agent
            .snapshot()
            .terminal_failure
            .as_ref()
            .map(|failure| failure.class),
        Some(FailureClass::Authorization)
    );
    assert!(agent.next_effect().is_none());

    let mut retry = desired(2);
    retry.video[0].height = 360;
    agent.command(AgentCommand::ReplaceDesired(retry)).unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::CreateOffer { generation, .. })
            if generation != failed_generation
    ));
    assert_eq!(agent.snapshot().connection, ConnectionState::CreatingOffer);
    assert_eq!(agent.snapshot().terminal_failure, None);
}

#[test]
fn signaling_snapshot_diff_and_empty_binding_groups_are_exact() {
    let (mut agent, generation, cid, send) = connected_agent();
    acknowledge_send(&mut agent, generation, cid, send);
    let _ = drain_notifications(&mut agent);
    let initial = signaling::ServerState {
        snapshot: true,
        participants_added: vec![signaling::Participant {
            participant_id: "publisher".to_string(),
        }],
        participants_removed: vec![],
        publications_added: vec![
            signaling::Publication {
                track_id: "video-track".to_string(),
                participant_id: "publisher".to_string(),
                kind: signaling::TrackKind::Video.into(),
            },
            signaling::Publication {
                track_id: "audio-track".to_string(),
                participant_id: "publisher".to_string(),
                kind: signaling::TrackKind::Audio.into(),
            },
        ],
        publications_removed: vec![],
        video: Some(signaling::VideoBindings {
            items: vec![signaling::VideoBinding {
                track_id: "video-track".to_string(),
                mid: "rv0".to_string(),
                paused: false,
            }],
        }),
        audio: Some(signaling::AudioBindings {
            items: vec![signaling::AudioBinding {
                track_id: "audio-track".to_string(),
                mid: "ra0".to_string(),
                level_dbov: -24,
            }],
        }),
    };
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: cid,
            payload: server_state(initial),
        }))
        .unwrap();
    assert_eq!(agent.snapshot().participants.len(), 1);
    assert_eq!(agent.snapshot().publications.len(), 2);
    assert_eq!(agent.snapshot().video["rv0"].track_id, "video-track");
    assert_eq!(agent.snapshot().audio[0].level_dbov, -24);
    let notifications = drain_notifications(&mut agent);
    assert_eq!(
        notifications
            .iter()
            .filter(|notification| matches!(notification, Notification::PublicationAdded(_)))
            .count(),
        2
    );

    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: cid,
            payload: server_state(signaling::ServerState {
                snapshot: false,
                participants_added: vec![],
                participants_removed: vec![],
                publications_added: vec![],
                publications_removed: vec![],
                video: Some(signaling::VideoBindings { items: vec![] }),
                audio: Some(signaling::AudioBindings { items: vec![] }),
            }),
        }))
        .unwrap();
    assert!(agent.snapshot().video.is_empty());
    assert!(agent.snapshot().audio.is_empty());
    assert_eq!(
        drain_notifications(&mut agent),
        vec![
            Notification::VideoBindingChanged {
                mid: "rv0".to_string(),
                binding: None,
            },
            Notification::AudioBindingsChanged(vec![]),
        ]
    );

    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: cid,
            payload: server_state(signaling::ServerState {
                snapshot: false,
                participants_added: vec![],
                participants_removed: vec!["publisher".to_string()],
                publications_added: vec![],
                publications_removed: vec!["video-track".to_string(), "audio-track".to_string()],
                video: None,
                audio: None,
            }),
        }))
        .unwrap();
    assert!(agent.snapshot().participants.is_empty());
    assert!(agent.snapshot().publications.is_empty());
}

#[test]
fn malformed_signaling_is_transactional() {
    let (mut agent, generation, cid, send) = connected_agent();
    acknowledge_send(&mut agent, generation, cid, send);
    let before = agent.snapshot().clone();
    let malformed = signaling::ServerState {
        snapshot: false,
        participants_added: vec![
            signaling::Participant {
                participant_id: "duplicate".to_string(),
            },
            signaling::Participant {
                participant_id: "duplicate".to_string(),
            },
        ],
        participants_removed: vec![],
        publications_added: vec![],
        publications_removed: vec![],
        video: None,
        audio: None,
    };
    assert!(matches!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: cid,
            payload: server_state(malformed),
        })),
        Err(AgentError::InvalidSignaling(
            SignalingError::Duplicate { .. }
        ))
    ));
    assert_eq!(agent.snapshot(), &before);
}

#[test]
fn complete_intent_retracts_omitted_state_and_playout_delay_is_one_way() {
    let (mut agent, generation, cid, initial_send) = connected_agent();
    assert!(agent.next_effect().is_none());
    acknowledge_send(&mut agent, generation, cid, initial_send);

    let mut retracted = desired(2);
    retracted.publications.clear();
    retracted.video.clear();
    retracted.audio = AudioSubscription::default();
    retracted.playout_delay = PlayoutDelay::Fixed {
        min_ms: 20,
        max_ms: 100,
    };
    agent
        .command(AgentCommand::ReplaceDesired(retracted))
        .unwrap();
    let (operation, intent) = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            operation, payload, ..
        }) => (operation, decode_intent(&payload)),
        effect => panic!("expected updated intent, got {effect:?}"),
    };
    assert!(intent.video.is_empty());
    assert!(intent.publish.iter().all(|publication| !publication.active));
    assert_eq!(
        intent.ext.and_then(|ext| ext.playout_delay),
        Some(signaling::PlayoutDelay {
            min_ms: 20,
            max_ms: 100,
        })
    );
    acknowledge_send(&mut agent, generation, cid, operation);

    let mut adaptive = desired(3);
    adaptive.playout_delay = PlayoutDelay::Adaptive;
    assert_eq!(
        agent.command(AgentCommand::ReplaceDesired(adaptive)),
        Err(AgentError::AdaptiveAfterFixed)
    );
    assert_eq!(agent.snapshot().desired_revision, 2);

    let disconnected = DesiredState {
        revision: 3,
        ..DesiredState::default()
    };
    agent
        .command(AgentCommand::ReplaceDesired(disconnected))
        .unwrap();
    assert_eq!(agent.snapshot().desired_revision, 3);
}

#[test]
fn failed_signaling_send_retries_only_the_latest_complete_intent() {
    let (mut agent, generation, cid, send) = connected_agent();
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::SendFailed {
            operation: send,
            generation,
            channel: cid,
            message: "full".to_string(),
        }))
        .unwrap();
    let timer = match next_effect(&mut agent) {
        Effect::Timer(TimerEffect::Schedule { timer, after }) => {
            assert_eq!(after, Duration::from_millis(100));
            timer
        }
        effect => panic!("expected signaling retry, got {effect:?}"),
    };
    let mut latest = desired(2);
    latest.video[0].height = 360;
    agent.command(AgentCommand::ReplaceDesired(latest)).unwrap();
    assert!(agent.next_effect().is_none());
    agent
        .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
        .unwrap();
    let intent = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send { payload, .. }) => decode_intent(&payload),
        effect => panic!("expected retried intent, got {effect:?}"),
    };
    assert_eq!(intent.video[0].height, 360);
}

#[test]
fn topic_registrations_validate_declare_and_retract_complete_channels() {
    let registrations = TopicRegistrations {
        publishers: vec![
            publisher("state", TopicMode::Latest),
            publisher("chat", TopicMode::Ordered),
        ],
        subscribers: vec![
            subscriber("state", TopicMode::Latest, Some(REMOTE_PUBLISHER)),
            subscriber("chat", TopicMode::Ordered, None),
        ],
    };
    let (mut agent, _, _, _) = connect_with_topics(registrations);
    assert_eq!(agent.snapshot().topics.publishers.len(), 2);
    assert_eq!(agent.snapshot().topics.subscribers.len(), 2);
    assert!(
        agent
            .snapshot()
            .topics
            .publishers
            .iter()
            .all(|status| status.channel.is_some())
    );

    let mut retracted = desired(2);
    retracted.topics = TopicRegistrations {
        publishers: vec![publisher("chat", TopicMode::Ordered)],
        subscribers: vec![subscriber("chat", TopicMode::Ordered, None)],
    };
    agent
        .command(AgentCommand::ReplaceDesired(retracted))
        .unwrap();
    let specs = match next_effect(&mut agent) {
        Effect::Rtc(RtcEffect::CreateOffer { data_channels, .. }) => data_channels,
        effect => panic!("expected replacement offer, got {effect:?}"),
    };
    assert_eq!(
        specs
            .iter()
            .map(|spec| spec.label.as_str())
            .collect::<Vec<_>>(),
        vec!["v1/sys/signaling", "v1/rel/pub/chat", "v1/rel/sub/chat"]
    );
    assert_eq!(agent.snapshot().topics.publishers.len(), 1);
    assert_eq!(agent.snapshot().topics.subscribers.len(), 1);

    let mut invalid = desired(3);
    invalid.topics.publishers = vec![publisher("bad/topic", TopicMode::Latest)];
    assert!(matches!(
        agent.command(AgentCommand::ReplaceDesired(invalid)),
        Err(AgentError::InvalidConfiguration(ValidationError::Topic(_)))
    ));

    let mut scoped_ordered = desired(3);
    scoped_ordered.topics.subscribers = vec![subscriber(
        "chat",
        TopicMode::Ordered,
        Some(REMOTE_PUBLISHER),
    )];
    assert!(matches!(
        agent.command(AgentCommand::ReplaceDesired(scoped_ordered)),
        Err(AgentError::InvalidConfiguration(
            ValidationError::TopicScope(_)
        ))
    ));
}

#[test]
fn latest_topic_send_admission_delivery_and_failures_are_explicit() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("state", TopicMode::Latest)],
        subscribers: vec![subscriber("state", TopicMode::Latest, None)],
    };
    let (mut agent, generation, _, channels) = connect_with_topics(registrations);
    let publish_channel = channels["v1/rt/pub/state"];
    let subscribe_channel = channels["v1/rt/sub/state"];
    let publish = publisher("state", TopicMode::Latest);

    agent
        .command(AgentCommand::SendTopic(TopicSend {
            publisher: publish.clone(),
            payload: b"first".to_vec(),
        }))
        .unwrap();
    let first = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            operation,
            channel,
            payload,
            ..
        }) => {
            assert_eq!(channel, publish_channel);
            assert_eq!(payload, b"first");
            operation
        }
        effect => panic!("expected latest send, got {effect:?}"),
    };
    assert_eq!(agent.snapshot().topics.accepted_sends, 0);
    assert!(agent.snapshot().topics.publishers[0].send_pending);

    agent
        .command(AgentCommand::SendTopic(TopicSend {
            publisher: publish.clone(),
            payload: b"second".to_vec(),
        }))
        .unwrap();
    assert!(agent.next_effect().is_none());
    agent
        .command(AgentCommand::SendTopic(TopicSend {
            publisher: publish.clone(),
            payload: b"third".to_vec(),
        }))
        .unwrap();
    assert!(agent.next_effect().is_none());
    assert_eq!(agent.snapshot().topics.dropped_sends, 1);
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Sent {
            operation: first,
            generation,
            channel: publish_channel,
        }))
        .unwrap();
    let second = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            operation, payload, ..
        }) => {
            assert_eq!(payload, b"third");
            operation
        }
        effect => panic!("expected queued latest send, got {effect:?}"),
    };
    assert_eq!(agent.snapshot().topics.accepted_sends, 1);
    assert!(drain_notifications(&mut agent).iter().any(|notification| {
        matches!(
            notification,
            Notification::Topic(TopicNotification::SendAdmitted {
                publisher,
                operation,
                stream_id: None,
                sequence: None,
            }) if publisher == &publish && *operation == first
        )
    }));

    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::SendFailed {
            operation: second,
            generation,
            channel: publish_channel,
            message: "host queue full".to_string(),
        }))
        .unwrap();
    assert_eq!(agent.snapshot().topics.dropped_sends, 2);
    assert_eq!(agent.snapshot().topics.channel_failures, 1);
    let failed = drain_notifications(&mut agent);
    assert!(failed.iter().any(|notification| matches!(
        notification,
        Notification::Topic(TopicNotification::SendDropped {
            reason: TopicDropReason::HostRejected,
            ..
        })
    )));
    assert!(failed.iter().any(|notification| matches!(
        notification,
        Notification::Topic(TopicNotification::ChannelFailed { .. })
    )));

    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: subscribe_channel,
            payload: b"latest".to_vec(),
        }))
        .unwrap();
    assert_eq!(agent.snapshot().topics.delivered_messages, 1);
    assert_eq!(
        drain_notifications(&mut agent),
        vec![Notification::Topic(TopicNotification::Message(
            TopicMessage::Latest {
                topic: "state".to_string(),
                publisher_id: None,
                payload: b"latest".to_vec(),
            }
        ))]
    );

    assert!(matches!(
        agent.command(AgentCommand::SendTopic(TopicSend {
            publisher: publish,
            payload: vec![0; MAX_TOPIC_PAYLOAD_BYTES + 1],
        })),
        Err(AgentError::InvalidTopic(TopicError::PayloadTooLarge { .. }))
    ));
    assert_eq!(agent.snapshot().topics.dropped_sends, 3);
}

#[test]
fn ordered_topics_reorder_deduplicate_nack_replay_and_resynchronize() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("chat", TopicMode::Ordered)],
        subscribers: vec![subscriber("chat", TopicMode::Ordered, None)],
    };
    let (mut agent, generation, _, channels) = connect_with_topics(registrations);
    let publish_channel = channels["v1/rel/pub/chat"];
    let subscribe_channel = channels["v1/rel/sub/chat"];
    let publish = publisher("chat", TopicMode::Ordered);

    for expected_sequence in 0..3 {
        agent
            .command(AgentCommand::SendTopic(TopicSend {
                publisher: publish.clone(),
                payload: vec![u8::try_from(expected_sequence).unwrap()],
            }))
            .unwrap();
        let (operation, delivery) = match next_effect(&mut agent) {
            Effect::DataChannel(DataChannelEffect::Send {
                operation, payload, ..
            }) => (operation, RelDelivery::decode(payload.as_slice()).unwrap()),
            effect => panic!("expected ordered send, got {effect:?}"),
        };
        assert_eq!(delivery.publisher_id, LOCAL_PUBLISHER);
        let message = RelMsg::decode(delivery.frame.as_slice()).unwrap();
        assert_eq!(message.stream_id, 1);
        assert_eq!(message.seq, expected_sequence);
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Sent {
                operation,
                generation,
                channel: publish_channel,
            }))
            .unwrap();
    }
    let _ = drain_notifications(&mut agent);
    assert_eq!(agent.snapshot().topics.publishers[0].replay_messages, 3);

    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: subscribe_channel,
            payload: ordered_delivery(REMOTE_PUBLISHER, 7, 0, b"zero"),
        }))
        .unwrap();
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: subscribe_channel,
            payload: ordered_delivery(REMOTE_PUBLISHER, 7, 2, b"two"),
        }))
        .unwrap();
    let nack_operation = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            operation,
            channel,
            payload,
            ..
        }) => {
            assert_eq!(channel, subscribe_channel);
            let control = RelControl::decode(payload.as_slice()).unwrap();
            assert_eq!(
                control.msg,
                Some(rel_control::Msg::Nack(RelNack {
                    stream_id: 7,
                    from_seq: 1,
                    publisher_id: REMOTE_PUBLISHER.to_string(),
                }))
            );
            operation
        }
        effect => panic!("expected ordered NACK, got {effect:?}"),
    };
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Sent {
            operation: nack_operation,
            generation,
            channel: subscribe_channel,
        }))
        .unwrap();
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: subscribe_channel,
            payload: ordered_delivery(REMOTE_PUBLISHER, 7, 1, b"one"),
        }))
        .unwrap();
    let deliveries = drain_notifications(&mut agent)
        .into_iter()
        .filter_map(|notification| match notification {
            Notification::Topic(TopicNotification::Message(TopicMessage::Ordered {
                sequence,
                ..
            })) => Some(sequence),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deliveries, vec![0, 1, 2]);
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: subscribe_channel,
            payload: ordered_delivery(REMOTE_PUBLISHER, 7, 2, b"duplicate"),
        }))
        .unwrap();
    assert!(drain_notifications(&mut agent).is_empty());

    let nack = RelControl {
        msg: Some(rel_control::Msg::Nack(RelNack {
            stream_id: 1,
            from_seq: 1,
            publisher_id: LOCAL_PUBLISHER.to_string(),
        })),
    };
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: publish_channel,
            payload: nack.encode_to_vec(),
        }))
        .unwrap();
    for expected_sequence in 1..3 {
        let replay = match next_effect(&mut agent) {
            Effect::DataChannel(DataChannelEffect::Send { payload, .. }) => {
                RelDelivery::decode(payload.as_slice()).unwrap()
            }
            effect => panic!("expected ordered replay, got {effect:?}"),
        };
        assert_eq!(
            RelMsg::decode(replay.frame.as_slice()).unwrap().seq,
            expected_sequence
        );
    }
    assert!(agent.next_effect().is_none());

    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: subscribe_channel,
            payload: ordered_delivery(REMOTE_PUBLISHER, 7, 259, b"far"),
        }))
        .unwrap();
    let resynchronized = drain_notifications(&mut agent);
    assert!(matches!(
        resynchronized.as_slice(),
        [
            Notification::Topic(TopicNotification::Resynchronized {
                stream_id: 7,
                next_sequence: 260,
                ..
            }),
            Notification::Topic(TopicNotification::Message(TopicMessage::Ordered {
                sequence: 259,
                ..
            }))
        ]
    ));
}

#[test]
fn ordered_replay_exhaustion_emits_reset_before_the_retained_window() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("events", TopicMode::Ordered)],
        subscribers: vec![],
    };
    let (mut agent, generation, _, channels) = connect_with_topics(registrations);
    let channel = channels["v1/rel/pub/events"];
    let publish = publisher("events", TopicMode::Ordered);
    for sequence in 0..=TOPIC_HISTORY_CAPACITY {
        agent
            .command(AgentCommand::SendTopic(TopicSend {
                publisher: publish.clone(),
                payload: vec![u8::try_from(sequence).unwrap_or(u8::MAX)],
            }))
            .unwrap();
        let operation = match next_effect(&mut agent) {
            Effect::DataChannel(DataChannelEffect::Send { operation, .. }) => operation,
            effect => panic!("expected ordered send, got {effect:?}"),
        };
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Sent {
                operation,
                generation,
                channel,
            }))
            .unwrap();
        let _ = drain_notifications(&mut agent);
    }
    assert_eq!(agent.snapshot().topics.publishers[0].accepted_history, 256);

    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            payload: RelControl {
                msg: Some(rel_control::Msg::Nack(RelNack {
                    stream_id: 1,
                    from_seq: 0,
                    publisher_id: LOCAL_PUBLISHER.to_string(),
                })),
            }
            .encode_to_vec(),
        }))
        .unwrap();
    let reset = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send { payload, .. }) => {
            let delivery = RelDelivery::decode(payload.as_slice()).unwrap();
            RelMsg::decode(delivery.frame.as_slice()).unwrap()
        }
        effect => panic!("expected replay reset, got {effect:?}"),
    };
    assert!(reset.resync_required);
    assert_eq!(reset.seq, 1);
    assert!(reset.payload.is_empty());
    let mut replayed = 0usize;
    while let Some(effect) = agent.next_effect() {
        let Effect::DataChannel(DataChannelEffect::Send { payload, .. }) = effect else {
            panic!("expected replay effect");
        };
        let delivery = RelDelivery::decode(payload.as_slice()).unwrap();
        let message = RelMsg::decode(delivery.frame.as_slice()).unwrap();
        assert!(!message.resync_required);
        replayed += 1;
    }
    assert_eq!(replayed, TOPIC_HISTORY_CAPACITY);
}

#[test]
fn topic_send_queue_overflow_and_channel_close_drop_every_admitted_message() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("state", TopicMode::Ordered)],
        subscribers: vec![],
    };
    let (mut agent, generation, _, channels) = connect_with_topics(registrations);
    let channel = channels["v1/rel/pub/state"];
    let publish = publisher("state", TopicMode::Ordered);
    agent
        .command(AgentCommand::SendTopic(TopicSend {
            publisher: publish.clone(),
            payload: b"pending".to_vec(),
        }))
        .unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::DataChannel(DataChannelEffect::Send { .. })
    ));
    for _ in 0..TOPIC_SEND_QUEUE_CAPACITY {
        agent
            .command(AgentCommand::SendTopic(TopicSend {
                publisher: publish.clone(),
                payload: b"queued".to_vec(),
            }))
            .unwrap();
    }
    assert_eq!(
        agent.snapshot().topics.publishers[0].queued_messages,
        TOPIC_SEND_QUEUE_CAPACITY
    );
    assert!(matches!(
        agent.command(AgentCommand::SendTopic(TopicSend {
            publisher: publish,
            payload: b"overflow".to_vec(),
        })),
        Err(AgentError::InvalidTopic(TopicError::SendQueueFull(_)))
    ));
    assert_eq!(agent.snapshot().topics.dropped_sends, 1);

    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Closed {
            generation,
            channel,
        }))
        .unwrap();
    assert_eq!(
        agent.snapshot().topics.dropped_sends,
        u64::try_from(TOPIC_SEND_QUEUE_CAPACITY).unwrap() + 2
    );
    assert_eq!(agent.snapshot().topics.channel_failures, 1);
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::CreateOffer { .. })
    ));
}

#[test]
fn topic_boundaries_reject_malformed_cross_lane_unknown_and_oversized_input() {
    let registrations = TopicRegistrations {
        publishers: vec![
            publisher("state", TopicMode::Latest),
            publisher("chat", TopicMode::Ordered),
        ],
        subscribers: vec![subscriber("state", TopicMode::Latest, None)],
    };
    let (mut agent, generation, _, channels) = connect_with_topics(registrations);
    let latest_publish = channels["v1/rt/pub/state"];
    let latest_subscribe = channels["v1/rt/sub/state"];
    let ordered_publish = channels["v1/rel/pub/chat"];
    let before = agent.snapshot().clone();

    assert_eq!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: ordered_publish,
            payload: b"not-protobuf".to_vec(),
        })),
        Err(AgentError::InvalidTopic(TopicError::MalformedMessage))
    );
    assert_eq!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: latest_publish,
            payload: b"subscriber-data-on-publisher-lane".to_vec(),
        })),
        Err(AgentError::InvalidTopic(TopicError::InvalidControl))
    );
    assert_eq!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: channel(99),
            payload: vec![],
        })),
        Err(AgentError::InvalidTopic(TopicError::UnknownChannel))
    );
    assert_eq!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            channel: channel(99),
        })),
        Err(AgentError::InvalidTopic(TopicError::UnknownChannel))
    );
    assert!(matches!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: latest_subscribe,
            payload: vec![0; MAX_TOPIC_PAYLOAD_BYTES + 1],
        })),
        Err(AgentError::InvalidTopic(TopicError::PayloadTooLarge { .. }))
    ));
    assert_eq!(agent.snapshot(), &before);
    assert!(drain_notifications(&mut agent).is_empty());
}

#[test]
fn reconnect_rotates_ordered_streams_without_replaying_accepted_history() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("chat", TopicMode::Ordered)],
        subscribers: vec![subscriber("chat", TopicMode::Ordered, None)],
    };
    let (mut agent, old_generation, _, old_channels) = connect_with_topics(registrations);
    let old_publish_channel = old_channels["v1/rel/pub/chat"];
    let old_subscribe_channel = old_channels["v1/rel/sub/chat"];
    let publish = publisher("chat", TopicMode::Ordered);
    agent
        .command(AgentCommand::SendTopic(TopicSend {
            publisher: publish.clone(),
            payload: b"accepted-before-reconnect".to_vec(),
        }))
        .unwrap();
    let accepted = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            operation, payload, ..
        }) => {
            let delivery = RelDelivery::decode(payload.as_slice()).unwrap();
            let message = RelMsg::decode(delivery.frame.as_slice()).unwrap();
            assert_eq!((message.stream_id, message.seq), (1, 0));
            operation
        }
        effect => panic!("expected ordered send, got {effect:?}"),
    };
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Sent {
            operation: accepted,
            generation: old_generation,
            channel: old_publish_channel,
        }))
        .unwrap();
    let _ = drain_notifications(&mut agent);

    agent
        .handle(HostEvent::Rtc(RtcEvent::Disconnected {
            generation: old_generation,
        }))
        .unwrap();
    let (generation, specs) = match next_effect(&mut agent) {
        Effect::Rtc(RtcEffect::CreateOffer {
            generation,
            data_channels,
            ..
        }) => (generation, data_channels),
        effect => panic!("expected replacement offer, got {effect:?}"),
    };
    let signal = channel(50);
    let mut new_channels = BTreeMap::new();
    let bindings = specs
        .iter()
        .skip(1)
        .enumerate()
        .map(|(index, spec)| {
            let id = channel(51 + u64::try_from(index).unwrap());
            new_channels.insert(spec.label.clone(), id);
            DataChannelBinding {
                label: spec.label.clone(),
                channel: id,
            }
        })
        .collect();
    let mut replacement_resources = resources(signal);
    replacement_resources.data_channels = bindings;
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: "replacement-topic-offer".to_string(),
            resources: replacement_resources,
        }))
        .unwrap();
    let update = match next_effect(&mut agent) {
        Effect::Http(HttpEffect::Request { operation, .. }) => operation,
        effect => panic!("expected replacement request, got {effect:?}"),
    };
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation: update,
            response: update_response("etag-2"),
        }))
        .unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::ApplyAnswer { .. })
    ));
    agent
        .handle(HostEvent::Rtc(RtcEvent::AnswerApplied { generation }))
        .unwrap();
    agent
        .handle(HostEvent::Rtc(RtcEvent::Connected { generation }))
        .unwrap();
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            channel: signal,
        }))
        .unwrap();
    for channel in new_channels.values().copied() {
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Opened {
                generation,
                channel,
            }))
            .unwrap();
    }
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close {
            generation: old_generation,
        })
    );
    let signaling = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            operation, channel, ..
        }) => {
            assert_eq!(channel, signal);
            operation
        }
        effect => panic!("expected replacement signaling intent, got {effect:?}"),
    };
    acknowledge_send(&mut agent, generation, signal, signaling);
    assert!(agent.next_effect().is_none());
    assert_eq!(agent.snapshot().topics.publishers[0].stream_id, Some(2));
    assert_eq!(agent.snapshot().topics.publishers[0].next_sequence, Some(0));
    assert_eq!(agent.snapshot().topics.publishers[0].accepted_history, 1);
    assert_eq!(agent.snapshot().topics.publishers[0].replay_messages, 0);
    let _ = drain_notifications(&mut agent);

    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation: old_generation,
            channel: old_subscribe_channel,
            payload: ordered_delivery(REMOTE_PUBLISHER, 1, 0, b"stale-channel"),
        }))
        .unwrap();
    assert!(drain_notifications(&mut agent).is_empty());

    agent
        .command(AgentCommand::SendTopic(TopicSend {
            publisher: publish,
            payload: b"after-reconnect".to_vec(),
        }))
        .unwrap();
    let new_publish_channel = new_channels["v1/rel/pub/chat"];
    let message = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            channel, payload, ..
        }) => {
            assert_eq!(channel, new_publish_channel);
            let delivery = RelDelivery::decode(payload.as_slice()).unwrap();
            RelMsg::decode(delivery.frame.as_slice()).unwrap()
        }
        effect => panic!("expected post-reconnect ordered send, got {effect:?}"),
    };
    assert_eq!((message.stream_id, message.seq), (2, 0));

    assert_eq!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: new_publish_channel,
            payload: RelControl {
                msg: Some(rel_control::Msg::Nack(RelNack {
                    stream_id: 1,
                    from_seq: 0,
                    publisher_id: LOCAL_PUBLISHER.to_string(),
                })),
            }
            .encode_to_vec(),
        })),
        Err(AgentError::InvalidTopic(TopicError::StaleStream))
    );
}

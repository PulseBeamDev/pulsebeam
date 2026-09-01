use alloc::{format, string::ToString, vec, vec::Vec};
use core::time::Duration;

use pulsebeam_proto::{
    prelude::Message,
    signaling::{self, client_message, server_message},
};

use crate::*;

const PARTICIPANT_URI: &str = "https://sfu.test/api/v1/rooms/room/participants/p1?manual_sub=true";

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
    }
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

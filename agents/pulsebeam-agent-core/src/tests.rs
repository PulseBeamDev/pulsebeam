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
    signaling_v1::{self as v1, client_message, server_message},
    topic::{
        TopicFloor, TopicFrame, TopicFrontier, TopicMessage as WireTopicMessage, TopicNack,
        TopicProbe, topic_frame::Body as TopicBody,
    },
};

use crate::*;

mod data_send;

const PARTICIPANT_URI: &str = "https://other-origin.test/an%20opaque/path?value=%2Fexact";
const LOCAL_PUBLISHER: &str = "pa_00000000000000000000000000";
const REMOTE_PUBLISHER: &str = "pa_11111111111111111111111111";
const VIDEO_INTENT_LOG_PREFIX: &str = "desired video subscriptions changed";

#[allow(clippy::disallowed_types)] // The process-global test logger serializes capture across tests.
mod test_log_capture {
    use std::sync::{Mutex, MutexGuard, Once, OnceLock};

    use super::*;

    struct TestLogger;

    static TEST_LOGGER: TestLogger = TestLogger;
    static TEST_LOGGER_INIT: Once = Once::new();
    static TEST_LOG_RECORDS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    static TEST_LOG_TEST_LOCK: Mutex<()> = Mutex::new(());

    impl log::Log for TestLogger {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.target().starts_with("pulsebeam_agent_core")
        }

        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                TEST_LOG_RECORDS
                    .get_or_init(|| Mutex::new(Vec::new()))
                    .lock()
                    .unwrap()
                    .push(record.args().to_string());
            }
        }

        fn flush(&self) {}
    }

    pub(super) fn serial() -> MutexGuard<'static, ()> {
        TEST_LOG_TEST_LOCK.lock().unwrap()
    }

    pub(super) fn take_video_intent_logs() -> Vec<String> {
        TEST_LOGGER_INIT.call_once(|| {
            log::set_logger(&TEST_LOGGER).unwrap();
            log::set_max_level(log::LevelFilter::Trace);
        });
        TEST_LOG_RECORDS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap()
            .drain(..)
            .filter(|record| record.starts_with(VIDEO_INTENT_LOG_PREFIX))
            .collect()
    }
}

use test_log_capture::{serial as serialize_log_test, take_video_intent_logs};

fn config() -> AgentConfig {
    AgentConfig {
        endpoint: "https://sfu.test/".to_string(),
        token: "private-token".to_string(),
        topology: MediaTopology {
            local_video: 1,
            local_audio: 1,
            remote_video: 1,
            remote_audio: 1,
        },
        retry: RetryPolicy::default(),
        log_level: LogLevel::default(),
    }
}

#[test]
fn log_level_applies_per_agent_verbosity() {
    assert!(LogLevel::Error.allows(log::Level::Error));
    assert!(!LogLevel::Error.allows(log::Level::Warn));
    assert!(LogLevel::Debug.allows(log::Level::Debug));
    assert!(!LogLevel::Debug.allows(log::Level::Trace));
    assert!(!LogLevel::Off.allows(log::Level::Error));
}

#[test]
fn configuration_debug_redacts_the_bearer_token() {
    let debug = format!("{:?}", config());

    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("private-token"));
}

#[test]
fn desired_video_intent_logs_only_accepted_semantic_changes() {
    let _log_test_lock = serialize_log_test();
    let mut debug_config = config();
    debug_config.log_level = LogLevel::Debug;
    let mut agent = Agent::new(debug_config).unwrap();
    let initial = desired(1);

    let _ = take_video_intent_logs();
    agent
        .command(AgentCommand::ReplaceDesired(initial.clone()))
        .unwrap();
    assert_eq!(
        take_video_intent_logs(),
        vec![format!(
            "desired video subscriptions changed revision=1 previous={:?} current={:?}",
            Vec::<VideoSubscription>::new(),
            initial.video,
        )]
    );

    let mut unrelated_change = initial;
    unrelated_change.revision = 2;
    unrelated_change.audio.automatic = false;
    agent
        .command(AgentCommand::ReplaceDesired(unrelated_change.clone()))
        .unwrap();
    assert!(take_video_intent_logs().is_empty());

    let mut changed_video = unrelated_change.clone();
    changed_video.revision = 3;
    changed_video.video[0].track_id = "replacement-video-track".to_string();
    changed_video.video[0].height = 1080;
    agent
        .command(AgentCommand::ReplaceDesired(changed_video.clone()))
        .unwrap();
    assert_eq!(
        take_video_intent_logs(),
        vec![format!(
            "desired video subscriptions changed revision=3 previous={:?} current={:?}",
            unrelated_change.video, changed_video.video,
        )]
    );

    let mut conflicting = changed_video.clone();
    conflicting.video[0].min_fps = 30;
    assert_eq!(
        agent.command(AgentCommand::ReplaceDesired(conflicting)),
        Err(AgentError::ConflictingDesiredRevision(3))
    );
    let mut stale = changed_video;
    stale.revision = 2;
    assert!(matches!(
        agent.command(AgentCommand::ReplaceDesired(stale)),
        Err(AgentError::StaleDesiredRevision { .. })
    ));
    assert!(take_video_intent_logs().is_empty());
}

#[test]
fn desired_video_intent_logs_do_not_follow_signaling_resends_or_reconnects() {
    let _log_test_lock = serialize_log_test();
    let mut debug_config = config();
    debug_config.log_level = LogLevel::Debug;
    let mut agent = Agent::new(debug_config).unwrap();
    let cid = channel(12);
    let (generation, operation) = begin_connect(&mut agent, desired(1), cid);
    let send = finish_connect(&mut agent, generation, operation, cid);
    let _ = take_video_intent_logs();

    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::SendFailed {
            operation: send,
            generation,
            channel: cid,
            message: "full".to_string(),
        }))
        .unwrap();
    let timer = match next_effect(&mut agent) {
        Effect::Timer(TimerEffect::Schedule { timer, .. }) => timer,
        effect => panic!("expected signaling retry, got {effect:?}"),
    };
    agent
        .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
        .unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::DataChannel(DataChannelEffect::Send { .. })
    ));
    assert!(take_video_intent_logs().is_empty());

    agent
        .handle(HostEvent::Rtc(RtcEvent::Disconnected { generation }))
        .unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::CreateOffer { .. })
    ));
    assert!(take_video_intent_logs().is_empty());
}

fn desired(revision: u64) -> DesiredState {
    DesiredState {
        revision,
        connected: true,
        publications: vec![PublicationIntent {
            slot: "v0".to_string(),
            label: "camera".to_string(),
            active: true,
        }],
        video: vec![VideoSubscription {
            slot: 0,
            track_id: "video-track".to_string(),
            selector: None,
            height: 720,
            min_height: 180,
            min_fps: 15,
            priority: 100,
            playout_delay: PlayoutDelay::Adaptive,
        }],
        audio: AudioSubscription {
            pinned: vec!["audio-track".to_string()],
            selected: Vec::new(),
            automatic: true,
            playout_delays: BTreeMap::new(),
            selector_delays: BTreeMap::new(),
        },
        topics: TopicRegistrations::default(),
    }
}

fn channel(value: u64) -> ChannelId {
    ChannelId::new(value).unwrap()
}

fn offer() -> String {
    [
        "v=0",
        "m=application 9 UDP/DTLS/SCTP webrtc-datachannel",
        "a=mid:data-channel",
        "m=video 9 UDP/TLS/RTP/SAVPF 102",
        "a=mid:lv0",
        "a=sendonly",
        "m=audio 9 UDP/TLS/RTP/SAVPF 111",
        "a=mid:ra0",
        "a=recvonly",
        "m=audio 9 UDP/TLS/RTP/SAVPF 111",
        "a=mid:la0",
        "a=sendonly",
        "m=video 9 UDP/TLS/RTP/SAVPF 102",
        "a=mid:rv0",
        "a=recvonly",
    ]
    .join("\r\n")
}

fn resources(channel: ChannelId) -> OfferResources {
    OfferResources {
        slots: negotiated_slot_bindings(
            &offer(),
            [
                (MediaSlot::LocalVideo("v0".to_string()), "lv0".to_string()),
                (MediaSlot::LocalAudio("a0".to_string()), "la0".to_string()),
                (MediaSlot::RemoteVideo(0), "rv0".to_string()),
                (MediaSlot::RemoteAudio(0), "ra0".to_string()),
            ],
        )
        .unwrap(),
        signaling_channel: channel,
        data_channels: vec![],
    }
}

#[test]
fn negotiated_coordinates_follow_sdp_sections_including_data() {
    assert_eq!(
        resources(channel(1)).slots,
        vec![
            SlotBinding {
                slot: MediaSlot::LocalVideo("v0".into()),
                mid: "lv0".into(),
                media_index: 1,
                kind: MediaKind::Video,
                direction: MediaDirection::SendOnly,
            },
            SlotBinding {
                slot: MediaSlot::LocalAudio("a0".into()),
                mid: "la0".into(),
                media_index: 3,
                kind: MediaKind::Audio,
                direction: MediaDirection::SendOnly,
            },
            SlotBinding {
                slot: MediaSlot::RemoteVideo(0),
                mid: "rv0".into(),
                media_index: 4,
                kind: MediaKind::Video,
                direction: MediaDirection::ReceiveOnly,
            },
            SlotBinding {
                slot: MediaSlot::RemoteAudio(0),
                mid: "ra0".into(),
                media_index: 2,
                kind: MediaKind::Audio,
                direction: MediaDirection::ReceiveOnly,
            },
        ]
    );
}

fn publisher(topic: &str, mode: TopicMode) -> TopicPublisher {
    TopicPublisher {
        topic: topic.to_string(),
        mode,
    }
}

fn subscriber(topic: &str, mode: TopicMode) -> TopicSubscriber {
    TopicSubscriber {
        topic: topic.to_string(),
        mode,
    }
}

fn connect_with_topics(
    registrations: TopicRegistrations,
) -> (Agent, Generation, ChannelId, BTreeMap<String, ChannelId>) {
    connect_with_topic_seed(registrations, true, &mut Vec::new())
}

fn connect_with_topic_seed(
    registrations: TopicRegistrations,
    complete_seed: bool,
    timers: &mut Vec<(TimerId, Duration)>,
) -> (Agent, Generation, ChannelId, BTreeMap<String, ChannelId>) {
    let mut agent = Agent::new(config()).unwrap();
    let mut state = desired(1);
    let has_ordered_subscriber = registrations
        .subscribers
        .iter()
        .any(|subscriber| subscriber.mode == TopicMode::Ordered);
    state.topics = registrations.clone();
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
            offer: offer(),
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
    let mut seeds = Vec::new();
    while let Some(effect) = agent.next_effect() {
        match effect {
            Effect::DataChannel(DataChannelEffect::Send {
                operation,
                channel,
                payload,
                ..
            }) => {
                assert_eq!(channel, signal);
                if let Some(v1::client_message::Payload::SubscriptionSeed(seed)) =
                    pulsebeam_proto::codec::decode_client(&payload)
                        .unwrap()
                        .payload
                {
                    seeds.push(seed.seed_id);
                }
                acknowledge_send(&mut agent, generation, signal, operation);
            }
            Effect::Timer(TimerEffect::Schedule { timer, after }) => timers.push((timer, after)),
            effect => panic!("unexpected activation effect: {effect:?}"),
        }
    }
    assert_eq!(!seeds.is_empty(), has_ordered_subscriber);
    inject_topic_catalog(&mut agent, generation, signal, &registrations, 1, "first");
    for seed_id in seeds.into_iter().filter(|_| complete_seed) {
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Message {
                generation,
                channel: signal,
                payload: pulsebeam_proto::codec::encode_server(&v1::ServerMessage {
                    payload: Some(v1::server_message::Payload::SubscriptionSeedComplete(
                        v1::SubscriptionSeedComplete {
                            seed_id,
                            catalog_revision: 1,
                        },
                    )),
                })
                .unwrap(),
            }))
            .unwrap();
    }
    while let Some(effect) = agent.next_effect() {
        if matches!(effect, Effect::Timer(TimerEffect::Cancel { .. })) {
            continue;
        }
        let Effect::DataChannel(DataChannelEffect::Send {
            operation,
            channel,
            payload,
            ..
        }) = effect
        else {
            panic!("expected initial Catalog probe");
        };
        assert!(matches!(
            TopicFrame::decode_bounded(&payload).unwrap().body,
            Some(TopicBody::Probe(_))
        ));
        acknowledge_send(&mut agent, generation, channel, operation);
    }
    let _ = drain_notifications(&mut agent);
    (agent, generation, signal, channels)
}

fn data_id(local: bool, topic: &str, mode: TopicMode, incarnation: &str) -> String {
    format!("server-{local}-{topic}-{mode:?}-{incarnation}")
}

fn inject_topic_catalog(
    agent: &mut Agent,
    generation: Generation,
    signal: ChannelId,
    registrations: &TopicRegistrations,
    revision: u64,
    incarnation: &str,
) {
    let mut tracks = Vec::new();
    for (local, topic, mode) in registrations
        .publishers
        .iter()
        .map(|p| (true, &p.topic, p.mode))
        .chain(
            registrations
                .subscribers
                .iter()
                .map(|s| (false, &s.topic, s.mode)),
        )
    {
        tracks.push(v1::RemoteTrack {
            track_id: data_id(local, topic, mode, incarnation),
            participant_id: if local {
                LOCAL_PUBLISHER
            } else {
                REMOTE_PUBLISHER
            }
            .into(),
            kind: v1::TrackKind::Data as i32,
            label: topic.clone(),
            mode: match mode {
                TopicMode::Latest => v1::TopicMode::Latest,
                TopicMode::Lossy => v1::TopicMode::Lossy,
                TopicMode::Ordered => v1::TopicMode::Ordered,
            } as i32,
        });
    }
    let catalog = v1::ServerMessage {
        payload: Some(server_message::Payload::Catalog(v1::Catalog {
            revision,
            state: Some(v1::catalog::State::Snapshot(v1::CatalogSnapshot {
                participants: vec![
                    v1::Participant {
                        participant_id: LOCAL_PUBLISHER.into(),
                        participant_external_id: "participant".into(),
                    },
                    v1::Participant {
                        participant_id: REMOTE_PUBLISHER.into(),
                        participant_external_id: "remote-app-user".into(),
                    },
                ],
                tracks,
            })),
        })),
    };
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: signal,
            payload: pulsebeam_proto::codec::encode_server(&catalog).unwrap(),
        }))
        .unwrap();
}

fn topic_input(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
    body: TopicBody,
) -> Result<(), AgentError> {
    agent.handle(HostEvent::DataChannel(DataChannelEvent::Message {
        generation,
        channel,
        payload: TopicFrame { body: Some(body) }.encode_to_vec(),
    }))
}

fn wire_message(id: &str, sequence: u64, payload: &[u8]) -> TopicBody {
    TopicBody::Message(WireTopicMessage {
        data_track_id: id.into(),
        sequence,
        payload: payload.to_vec(),
    })
}

fn next_topic_send(agent: &mut Agent) -> (OperationId, ChannelId, TopicBody) {
    match next_effect(agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            operation,
            channel,
            payload,
            ..
        }) => (
            operation,
            channel,
            TopicFrame::decode_bounded(&payload).unwrap().body.unwrap(),
        ),
        effect => panic!("expected topic send, got {effect:?}"),
    }
}

fn create_response(participant: &str, _etag: &str, uri: &str) -> HttpResponse {
    HttpResponse {
        status: 201,
        headers: vec![HttpHeader {
            name: "location".to_string(),
            value: uri.to_string(),
        }],
        body: serde_json::to_vec(&serde_json::json!({
            "room_external_id": "room",
            "room_id": "opaque room",
            "participant_external_id": "participant",
            "participant_id": participant,
            "connection_id": "not-canonical",
            "answer": "answer",
        }))
        .unwrap(),
    }
}

fn update_response(participant: &str) -> HttpResponse {
    let mut response = create_response(participant, "unused", PARTICIPANT_URI);
    let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    let mut value = value.as_object().unwrap().clone();
    value.insert(
        "answer".to_string(),
        serde_json::Value::String("replacement-answer".to_string()),
    );
    response.body = serde_json::to_vec(&value).unwrap();
    response
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
            offer: offer(),
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
            assert_eq!(request.uri, "https://sfu.test/api/v1/native");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
                serde_json::json!({"offer": offer(), "manual": true}),
            );
            assert!(request.headers.iter().any(|header| {
                header.name == "Authorization" && header.value == "Bearer private-token"
            }));
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

fn decode_intent(payload: &[u8]) -> v1::Intent {
    let message = pulsebeam_proto::codec::decode_client(payload).unwrap();
    match message.payload.unwrap() {
        client_message::Payload::Intent(intent) => intent,
        client_message::Payload::RenewAuthorization(_)
        | client_message::Payload::SubscriptionSeed(_) => panic!("expected media intent"),
    }
}

fn server_payload(payload: server_message::Payload) -> Vec<u8> {
    pulsebeam_proto::codec::encode_server(&v1::ServerMessage {
        payload: Some(payload),
    })
    .unwrap()
}

fn drain_notifications(agent: &mut Agent) -> Vec<Notification> {
    let mut notifications = Vec::new();
    while let Some(notification) = agent.next_notification() {
        notifications.push(notification);
    }
    notifications
}

#[test]
fn disconnected_credential_update_is_used_for_next_http_admission() {
    let mut agent = Agent::new(config()).unwrap();
    assert_eq!(
        agent.command(AgentCommand::RenewAuthorization("bad\nheader".to_string())),
        Err(AgentError::InvalidAuthorizationToken)
    );
    agent
        .command(AgentCommand::RenewAuthorization(
            "replacement-token".to_string(),
        ))
        .unwrap();
    agent
        .command(AgentCommand::ReplaceDesired(desired(1)))
        .unwrap();
    let generation = match next_effect(&mut agent) {
        Effect::Rtc(RtcEffect::CreateOffer { generation, .. }) => generation,
        effect => panic!("expected offer, got {effect:?}"),
    };
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: offer(),
            resources: resources(channel(9)),
        }))
        .unwrap();
    let request = match next_effect(&mut agent) {
        Effect::Http(HttpEffect::Request { request, .. }) => request,
        effect => panic!("expected HTTP admission, got {effect:?}"),
    };
    assert!(request.headers.iter().any(|header| {
        header.name == "Authorization" && header.value == "Bearer replacement-token"
    }));
    assert!(!format!("{request:?}").contains("replacement-token"));
}

#[test]
fn unsupported_intent_is_nonfatal_without_advancing_accepted_state() {
    for (fatal, intent_revision, valid) in [
        (false, Some(2), true),
        (true, Some(2), false),
        (false, None, false),
    ] {
        let (mut agent, generation, channel, send) = connected_agent();
        acknowledge_send(&mut agent, generation, channel, send);
        drain_notifications(&mut agent);
        let before = agent.snapshot().clone();
        let result = agent.handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            payload: server_payload(server_message::Payload::Error(v1::Error {
                code: v1::ErrorCode::UnsupportedIntent.into(),
                message: "unsupported receive policy".into(),
                fatal,
                intent_revision,
            })),
        }));
        assert_eq!(result.is_ok(), valid);
        assert_eq!(
            agent.snapshot().accepted_intent_revision,
            before.accepted_intent_revision
        );
        assert_eq!(agent.snapshot().audio_mapping, before.audio_mapping);
        if valid {
            assert_eq!(agent.snapshot().connection, before.connection);
            assert_eq!(agent.snapshot().generation, before.generation);
            assert!(
                drain_notifications(&mut agent).contains(&Notification::ServerError(
                    "unsupported receive policy".into()
                ))
            );
        } else {
            assert_eq!(
                agent.snapshot().connection,
                ConnectionState::TerminalFailure
            );
        }
    }
}

#[test]
fn rejected_renewal_preserves_the_committed_credential_and_expiry() {
    let (mut agent, generation, channel, send) = connected_agent();
    acknowledge_send(&mut agent, generation, channel, send);
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            payload: server_payload(server_message::Payload::Authorization(v1::Authorization {
                expires_at_unix_seconds: 100,
            })),
        }))
        .unwrap();
    assert_eq!(agent.snapshot().authorization_expires_at, Some(100));
    let command = AgentCommand::RenewAuthorization("new-secret".to_string());
    assert!(!format!("{command:?}").contains("new-secret"));
    agent.command(command).unwrap();
    let operation = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            operation, payload, ..
        }) => {
            let decoded = pulsebeam_proto::codec::decode_client(&payload).unwrap();
            assert!(matches!(
                decoded.payload,
                Some(client_message::Payload::RenewAuthorization(v1::RenewAuthorization { token }))
                if token == "new-secret"
            ));
            operation
        }
        effect => panic!("expected renewal, got {effect:?}"),
    };
    assert_eq!(
        agent.command(AgentCommand::RenewAuthorization("second".to_string())),
        Err(AgentError::RenewalBusy)
    );
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Sent {
            operation,
            generation,
            channel,
        }))
        .unwrap();
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            payload: server_payload(server_message::Payload::Error(v1::Error {
                code: v1::ErrorCode::AuthorizationRejected.into(),
                message: "rejected".to_string(),
                fatal: false,
                intent_revision: None,
            })),
        }))
        .unwrap();
    assert_eq!(agent.snapshot().authorization_expires_at, Some(100));
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 2,
            ..DesiredState::default()
        }))
        .unwrap();
    let delete = (0..4)
        .filter_map(|_| agent.next_effect())
        .find_map(|effect| match effect {
            Effect::Http(HttpEffect::Request { request, .. })
                if request.method == HttpMethod::Delete =>
            {
                Some(request)
            }
            _ => None,
        })
        .expect("delete request");
    assert!(delete.headers.iter().any(|header| {
        header.name == "Authorization" && header.value == "Bearer private-token"
    }));
    assert!(!format!("{delete:?}").contains("private-token"));
}

#[test]
fn accepted_renewal_commits_the_new_credential_only_on_authorization() {
    let (mut agent, generation, channel, send) = connected_agent();
    acknowledge_send(&mut agent, generation, channel, send);
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            payload: server_payload(server_message::Payload::Authorization(v1::Authorization {
                expires_at_unix_seconds: 100,
            })),
        }))
        .unwrap();
    agent
        .command(AgentCommand::RenewAuthorization("new-secret".to_string()))
        .unwrap();
    let _send = next_effect(&mut agent);
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            payload: server_payload(server_message::Payload::Authorization(v1::Authorization {
                expires_at_unix_seconds: 200,
            })),
        }))
        .unwrap();
    assert_eq!(agent.snapshot().authorization_expires_at, Some(200));
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 2,
            ..DesiredState::default()
        }))
        .unwrap();
    let delete = (0..4)
        .filter_map(|_| agent.next_effect())
        .find_map(|effect| match effect {
            Effect::Http(HttpEffect::Request { request, .. })
                if request.method == HttpMethod::Delete =>
            {
                Some(request)
            }
            _ => None,
        })
        .expect("delete request");
    assert!(
        delete.headers.iter().any(|header| {
            header.name == "Authorization" && header.value == "Bearer new-secret"
        })
    );
    assert!(!format!("{delete:?}").contains("new-secret"));
}

#[test]
fn default_topology_reserves_distinct_counted_sender_slots() {
    let topology = MediaTopology::default();
    assert_eq!(
        (
            topology.local_video,
            topology.local_audio,
            topology.remote_video,
            topology.remote_audio,
        ),
        (2, 2, 16, 8)
    );
    let slots = topology.slots();
    assert_eq!(
        &slots[..4],
        &[
            MediaSlot::LocalVideo("v0".into()),
            MediaSlot::LocalVideo("v1".into()),
            MediaSlot::LocalAudio("a0".into()),
            MediaSlot::LocalAudio("a1".into()),
        ]
    );
    assert_eq!(slots.len(), 28);
    assert_eq!(topology.local_slot_kind("v0"), Some(MediaKind::Video));
    assert_eq!(topology.local_slot_kind("a0"), Some(MediaKind::Audio));
    assert_eq!(topology.local_slot_kind("v00"), None);
    assert_eq!(topology.local_slot_kind("v2"), None);
}

#[test]
fn publishing_uses_only_accepted_local_publication_intent() {
    let mut agent = Agent::new(config()).unwrap();
    let video = MediaSlot::LocalVideo("v0".into());
    assert!(!agent.is_publishing(&video));
    let mut state = desired(1);
    agent
        .command(AgentCommand::ReplaceDesired(state.clone()))
        .unwrap();
    assert!(agent.is_publishing(&video));
    assert!(!agent.is_publishing(&MediaSlot::LocalAudio("v0".into())));
    assert!(!agent.is_publishing(&MediaSlot::RemoteVideo(0)));
    state.revision = 2;
    state.publications[0].label = "replacement".into();
    assert!(
        agent
            .command(AgentCommand::ReplaceDesired(state.clone()))
            .is_err()
    );
    assert!(agent.is_publishing(&video));
    state.publications[0].label = "camera".into();
    state.publications[0].active = false;
    agent
        .command(AgentCommand::ReplaceDesired(state.clone()))
        .unwrap();
    assert!(!agent.is_publishing(&video));
    state.revision = 3;
    state.publications.clear();
    agent.command(AgentCommand::ReplaceDesired(state)).unwrap();
    assert!(!agent.is_publishing(&video));
}

#[test]
fn publication_label_is_bound_to_its_slot_even_after_desire_is_cleared() {
    let mut agent = Agent::new(config()).unwrap();
    let original = desired(1);
    agent
        .command(AgentCommand::ReplaceDesired(original.clone()))
        .unwrap();
    let mut cleared = original.clone();
    cleared.revision = 2;
    cleared.publications.clear();
    agent
        .command(AgentCommand::ReplaceDesired(cleared))
        .unwrap();

    let mut relabelled = original.clone();
    relabelled.revision = 3;
    relabelled.publications[0].label = "different camera".into();
    assert!(matches!(
        agent.command(AgentCommand::ReplaceDesired(relabelled)),
        Err(AgentError::InvalidConfiguration(
            ValidationError::PublicationBinding(_)
        ))
    ));
    assert_eq!(agent.snapshot().desired_revision, 2);

    let mut audio = original;
    audio.revision = 3;
    audio.publications = vec![PublicationIntent {
        slot: "a0".into(),
        label: "camera".into(),
        active: true,
    }];
    agent.command(AgentCommand::ReplaceDesired(audio)).unwrap();
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
    invalid.topology.local_video = u8::try_from(MAX_LOCAL_VIDEO_SLOTS + 1).unwrap();
    assert!(matches!(
        Agent::new(invalid),
        Err(AgentError::InvalidConfiguration(
            ValidationError::SlotLimit { .. }
        ))
    ));

    let mut at_capacity = config();
    at_capacity.topology.local_video = u8::try_from(MAX_LOCAL_VIDEO_SLOTS).unwrap();
    at_capacity.topology.remote_audio = MAX_REMOTE_AUDIO_SLOTS;
    at_capacity.topology.remote_video = MAX_REMOTE_VIDEO_SLOTS;
    at_capacity.topology.local_audio = u8::try_from(MAX_LOCAL_AUDIO_SLOTS).unwrap();
    assert!(Agent::new(at_capacity).is_ok());

    let mut invalid_slot = desired(1);
    invalid_slot.publications[0].slot = "v00".into();
    assert!(matches!(
        invalid_slot.validate(&config().topology),
        Err(ValidationError::UnknownPublicationSlot(_))
    ));

    let mut protocol_header = config();
    protocol_header.token.clear();
    assert!(matches!(
        Agent::new(protocol_header),
        Err(AgentError::InvalidConfiguration(ValidationError::Token))
    ));

    let mut control_header = config();
    control_header.token = "invalid\u{1}".to_string();
    assert!(matches!(
        Agent::new(control_header),
        Err(AgentError::InvalidConfiguration(ValidationError::Token))
    ));

    let mut injected_header = config();
    injected_header.token = "redacted".to_string();
    let mut agent = Agent::new(injected_header).unwrap();
    agent
        .command(AgentCommand::ReplaceDesired(desired(1)))
        .unwrap();
    let generation = match next_effect(&mut agent) {
        Effect::Rtc(RtcEffect::CreateOffer { generation, .. }) => generation,
        effect => panic!("expected offer, got {effect:?}"),
    };
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: offer(),
            resources: resources(channel(99)),
        }))
        .unwrap();
    let request = match next_effect(&mut agent) {
        Effect::Http(HttpEffect::Request { request, .. }) => request,
        effect => panic!("expected request, got {effect:?}"),
    };
    assert!(
        request
            .headers
            .iter()
            .any(|header| { header.name == "Authorization" && header.value == "Bearer redacted" })
    );

    let mut agent = Agent::new(config()).unwrap();
    let mut valid = desired(1);
    valid.video[0].min_height = 721;
    valid.video[0].playout_delay = PlayoutDelay::Fixed {
        min_ms: 3000,
        max_ms: 500,
    };
    agent.command(AgentCommand::ReplaceDesired(valid)).unwrap();
    assert_eq!(agent.snapshot().desired_revision, 1);

    let mut invalid = desired(2);
    invalid.audio.playout_delays.insert(
        "not-pinned".into(),
        PlayoutDelay::Fixed {
            min_ms: 0,
            max_ms: 0,
        },
    );
    assert_eq!(
        agent.command(AgentCommand::ReplaceDesired(invalid)),
        Err(AgentError::InvalidConfiguration(
            ValidationError::UnknownAudioTrack("not-pinned".into())
        ))
    );
}

#[test]
fn rtc_host_failure_closes_the_generation_and_uses_core_retry_policy() {
    let mut agent = Agent::new(config()).unwrap();
    agent
        .command(AgentCommand::ReplaceDesired(desired(1)))
        .unwrap();
    let generation = match next_effect(&mut agent) {
        Effect::Rtc(RtcEffect::CreateOffer { generation, .. }) => generation,
        effect => panic!("expected offer, got {effect:?}"),
    };
    agent
        .handle(HostEvent::Rtc(RtcEvent::Failed {
            generation,
            message: "createOffer rejected".to_string(),
        }))
        .unwrap();
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close { generation })
    );
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Timer(TimerEffect::Schedule { .. })
    ));
    assert!(matches!(
        agent.snapshot().connection,
        ConnectionState::RetryWaiting { attempt: 1, .. }
    ));
    assert!(drain_notifications(&mut agent).iter().any(|notification| {
        matches!(
            notification,
            Notification::Failure(Failure {
                class: FailureClass::Transient,
                message,
            }) if message == "createOffer rejected"
        )
    }));
}

#[test]
fn transient_retries_continue_past_the_old_budget_with_a_ten_second_ceiling() {
    let mut config = config();
    config.retry.maximum_attempts = 1;
    config.retry.maximum_delay = Duration::from_secs(30);
    let mut agent = Agent::new(config).unwrap();
    agent
        .command(AgentCommand::ReplaceDesired(desired(1)))
        .unwrap();
    for _ in 0..24 {
        let generation = match next_effect(&mut agent) {
            Effect::Rtc(RtcEffect::CreateOffer { generation, .. }) => generation,
            effect => panic!("expected offer, got {effect:?}"),
        };
        agent
            .handle(HostEvent::Rtc(RtcEvent::Failed {
                generation,
                message: "temporary".to_string(),
            }))
            .unwrap();
        assert_eq!(
            next_effect(&mut agent),
            Effect::Rtc(RtcEffect::Close { generation })
        );
        let (timer, delay) = match next_effect(&mut agent) {
            Effect::Timer(TimerEffect::Schedule { timer, after }) => (timer, after),
            effect => panic!("expected retry timer, got {effect:?}"),
        };
        assert!(delay <= Duration::from_secs(10));
        assert!(delay > Duration::ZERO);
        assert!(agent.snapshot().terminal_failure.is_none());
        agent
            .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
            .unwrap();
    }
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
            offer: offer(),
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
            offer: offer(),
            resources: resources(channel(3)),
        }))
        .unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Http(HttpEffect::Request { .. })
    ));
}

#[test]
fn malformed_negotiated_coordinates_are_rejected_transactionally() {
    let mut duplicate = resources(channel(3));
    duplicate.slots[1].media_index = duplicate.slots[0].media_index;
    let mut out_of_range = resources(channel(3));
    out_of_range.slots[0].media_index = 99;
    let mut wrong_kind = resources(channel(3));
    wrong_kind.slots[0].kind = MediaKind::Audio;
    let mut wrong_direction = resources(channel(3));
    wrong_direction.slots[0].direction = MediaDirection::ReceiveOnly;

    for malformed in [duplicate, out_of_range, wrong_kind, wrong_direction] {
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
                offer: offer(),
                resources: malformed,
            })),
            Err(AgentError::InvalidOffer(
                "negotiated media coordinates do not match the SDP offer"
            ))
        );
        assert_eq!(agent.snapshot().connection, ConnectionState::CreatingOffer);
        assert!(agent.next_effect().is_none());
    }
}

#[test]
fn connection_waits_for_every_host_boundary_and_closes_both_resources() {
    let (mut agent, generation, cid, send) = connected_agent();
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert_eq!(agent.snapshot().participant_id.as_deref(), Some("p1"));
    assert_eq!(
        agent.snapshot().participant_external_id.as_deref(),
        Some("participant")
    );
    assert_eq!(agent.snapshot().room_external_id.as_deref(), Some("room"));
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
            assert_eq!(
                request.headers,
                vec![HttpHeader {
                    name: "Authorization".to_string(),
                    value: "Bearer private-token".to_string(),
                }]
            );
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
    assert_eq!(agent.snapshot().participant_external_id, None);
    assert_eq!(agent.snapshot().room_external_id, None);
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
            assert!((Duration::from_millis(250)..=Duration::from_millis(500)).contains(&after));
            assert_eq!(
                agent.snapshot().connection,
                ConnectionState::RetryWaiting { attempt: 1, after }
            );
            timer
        }
        effect => panic!("expected retry timer, got {effect:?}"),
    };

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
fn transport_loss_fences_incumbent_signaling_during_replacement() {
    for failed in [false, true] {
        let (mut agent, generation, channel, send) = connected_agent();
        acknowledge_send(&mut agent, generation, channel, send);
        let event = if failed {
            RtcEvent::Failed {
                generation,
                message: "transport failed".into(),
            }
        } else {
            RtcEvent::Disconnected { generation }
        };
        agent.handle(HostEvent::Rtc(event)).unwrap();
        assert!(matches!(
            next_effect(&mut agent),
            Effect::Rtc(RtcEffect::CreateOffer { .. })
        ));
        drain_notifications(&mut agent);
        let before = agent.snapshot().clone();
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Message {
                generation,
                channel,
                payload: vec![0xff],
            }))
            .unwrap();
        assert_eq!(agent.snapshot(), &before);
        assert!(agent.next_notification().is_none());
        assert!(agent.next_effect().is_none());
    }
}

#[test]
fn reconnect_posts_a_new_resource_and_swaps_only_after_the_candidate_is_ready() {
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
    assert_eq!(
        agent.snapshot().participant_external_id.as_deref(),
        Some("participant")
    );
    assert_eq!(agent.snapshot().room_external_id.as_deref(), Some("room"));
    let replacement_cid = channel(10);
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation: replacement_generation,
            offer: offer(),
            resources: resources(replacement_cid),
        }))
        .unwrap();
    let update = match next_effect(&mut agent) {
        Effect::Http(HttpEffect::Request {
            operation, request, ..
        }) => {
            assert_eq!(request.method, HttpMethod::Post);
            assert_eq!(request.uri, "https://sfu.test/api/v1/native");
            operation
        }
        effect => panic!("expected POST, got {effect:?}"),
    };
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation: update,
            response: update_response("p1"),
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
    assert_eq!(
        agent.snapshot().participant_external_id.as_deref(),
        Some("participant")
    );
    assert_eq!(agent.snapshot().room_external_id.as_deref(), Some("room"));
}

#[test]
fn rejected_reconnect_is_terminal_without_a_legacy_fallback() {
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
            offer: offer(),
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
    assert_eq!(agent.snapshot().generation, Some(old_generation));
    assert_eq!(
        agent.snapshot().connection,
        ConnectionState::TerminalFailure
    );
    assert!(drain_notifications(&mut agent).iter().any(|notification| {
        matches!(
            notification,
            Notification::Failure(Failure {
                class: FailureClass::InvalidConfiguration,
                ..
            })
        )
    }));
    let failure = agent.snapshot().terminal_failure.clone();
    let mut changed = desired(2);
    changed.video[0].height = 720;
    changed
        .topics
        .publishers
        .push(publisher("chat", TopicMode::Ordered));
    agent
        .command(AgentCommand::ReplaceDesired(changed))
        .unwrap();
    agent
        .command(AgentCommand::RenewAuthorization("renewed".to_owned()))
        .unwrap();
    for event in [
        RtcEvent::Disconnected {
            generation: old_generation,
        },
        RtcEvent::Failed {
            generation: old_generation,
            message: "late RTC failure".to_owned(),
        },
        RtcEvent::Connected {
            generation: replacement_generation,
        },
    ] {
        agent.handle(HostEvent::Rtc(event)).unwrap();
    }
    assert_eq!(agent.snapshot().terminal_failure, failure);
    assert_eq!(
        agent.snapshot().connection,
        ConnectionState::TerminalFailure
    );
    assert!(
        agent.next_effect().is_none(),
        "incidental work and late RTC events cannot create or schedule a retry"
    );
    agent.command(AgentCommand::Reconnect).unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::CreateOffer { .. })
    ));
}

#[test]
fn explicit_connect_retries_terminal_intent_but_not_an_in_flight_attempt() {
    let mut agent = Agent::new(config()).unwrap();
    let (failed_generation, operation) = begin_connect(&mut agent, desired(1), channel(12));
    agent.command(AgentCommand::Connect).unwrap();
    assert!(agent.next_effect().is_none());
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
            generation: failed_generation
        })
    );
    assert_eq!(
        agent.snapshot().connection,
        ConnectionState::TerminalFailure
    );
    agent.command(AgentCommand::Connect).unwrap();
    assert!(
        matches!(next_effect(&mut agent), Effect::Rtc(RtcEffect::CreateOffer { generation, .. }) if generation != failed_generation)
    );
    assert_eq!(agent.snapshot().terminal_failure, None);
    agent.command(AgentCommand::Connect).unwrap();
    assert!(agent.next_effect().is_none());
}

#[test]
fn explicit_connect_establishes_connected_intent_from_idle() {
    let mut agent = Agent::new(config()).unwrap();
    agent.command(AgentCommand::Connect).unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::CreateOffer { .. })
    ));
    agent.command(AgentCommand::Connect).unwrap();
    assert!(agent.next_effect().is_none());
}

#[test]
fn authorization_is_latched_across_incidental_updates_until_explicit_reconnect() {
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
    agent
        .command(AgentCommand::RenewAuthorization("new-token".to_owned()))
        .unwrap();
    agent.set_log_level(LogLevel::Debug);
    agent
        .handle(HostEvent::Rtc(RtcEvent::Connected {
            generation: failed_generation,
        }))
        .unwrap();
    agent
        .handle(HostEvent::Rtc(RtcEvent::Disconnected {
            generation: failed_generation,
        }))
        .unwrap();
    assert!(agent.next_effect().is_none());
    assert_eq!(
        agent.snapshot().connection,
        ConnectionState::TerminalFailure
    );
    assert_eq!(
        agent.snapshot().terminal_failure.as_ref().unwrap().class,
        FailureClass::Authorization
    );
    agent.command(AgentCommand::Reconnect).unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::CreateOffer { generation, .. })
            if generation != failed_generation
    ));
    assert_eq!(agent.snapshot().connection, ConnectionState::CreatingOffer);
    assert_eq!(agent.snapshot().terminal_failure, None);
}

#[test]
fn signaling_messages_received_before_topic_channels_open_are_replayed_on_activation() {
    let mut agent = Agent::new(config()).unwrap();
    let mut state = desired(1);
    state
        .topics
        .publishers
        .push(publisher("events", TopicMode::Ordered));
    agent.command(AgentCommand::ReplaceDesired(state)).unwrap();
    let (generation, topic_label) = match next_effect(&mut agent) {
        Effect::Rtc(RtcEffect::CreateOffer {
            generation,
            data_channels,
            ..
        }) => (generation, data_channels[1].label.clone()),
        effect => panic!("expected offer, got {effect:?}"),
    };
    let signal = channel(40);
    let topic = channel(41);
    let mut offer_resources = resources(signal);
    offer_resources.data_channels.push(DataChannelBinding {
        label: topic_label,
        channel: topic,
    });
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: offer(),
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
            response: create_response("p1", "etag-1", PARTICIPANT_URI),
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
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: signal,
            payload: server_payload(server_message::Payload::Catalog(v1::Catalog {
                revision: 1,
                state: Some(v1::catalog::State::Snapshot(v1::CatalogSnapshot {
                    participants: vec![v1::Participant {
                        participant_id: "publisher".to_string(),
                        participant_external_id: "publisher-external".to_string(),
                    }],
                    tracks: vec![v1::RemoteTrack {
                        track_id: "video-track".to_string(),
                        participant_id: "publisher".to_string(),
                        kind: v1::TrackKind::Video.into(),
                        mode: 0,
                        label: "camera".to_string(),
                    }],
                })),
            })),
        }))
        .unwrap();
    assert_eq!(agent.snapshot().catalog_revision, 0);
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            channel: topic,
        }))
        .unwrap();
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert_eq!(agent.snapshot().catalog_revision, 1);
    assert_eq!(agent.snapshot().publications["video-track"].label, "camera");
}

#[test]
fn signaling_snapshot_diff_and_empty_binding_groups_are_exact() {
    let (mut agent, generation, cid, send) = connected_agent();
    acknowledge_send(&mut agent, generation, cid, send);
    let _ = drain_notifications(&mut agent);
    let initial = v1::Catalog {
        revision: 1,
        state: Some(v1::catalog::State::Snapshot(v1::CatalogSnapshot {
            participants: vec![v1::Participant {
                participant_id: "publisher".to_string(),
                participant_external_id: "publisher-external".to_string(),
            }],
            tracks: vec![
                v1::RemoteTrack {
                    track_id: "video-track".to_string(),
                    participant_id: "publisher".to_string(),
                    kind: v1::TrackKind::Video.into(),
                    mode: 0,
                    label: "camera".to_string(),
                },
                v1::RemoteTrack {
                    track_id: "audio-track".to_string(),
                    participant_id: "publisher".to_string(),
                    kind: v1::TrackKind::Audio.into(),
                    mode: 0,
                    label: "microphone".to_string(),
                },
            ],
        })),
    };
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: cid,
            payload: server_payload(server_message::Payload::Catalog(initial)),
        }))
        .unwrap();
    assert_eq!(agent.snapshot().catalog_revision, 1);
    assert_eq!(agent.snapshot().accepted_intent_revision, 0);
    assert_eq!(agent.snapshot().participants.len(), 1);
    assert_eq!(agent.snapshot().publications.len(), 2);
    assert_eq!(
        agent.snapshot().participants["publisher"].external_id,
        "publisher-external"
    );
    assert_eq!(agent.snapshot().publications["video-track"].label, "camera");
    assert_eq!(
        agent.snapshot().publications["audio-track"].label,
        "microphone"
    );
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: cid,
            payload: server_payload(server_message::Payload::Mapping(v1::Mapping {
                intent_revision: 1,
                video: Some(v1::TrackMappings {
                    tracks: vec![v1::TrackMapping {
                        receiver_index: 4,
                        track_id: "video-track".to_string(),
                    }],
                }),
                audio: Some(v1::TrackMappings {
                    tracks: vec![v1::TrackMapping {
                        receiver_index: 2,
                        track_id: "audio-track".to_string(),
                    }],
                }),
            })),
        }))
        .unwrap();
    assert_eq!(agent.snapshot().accepted_intent_revision, 1);
    assert_eq!(agent.snapshot().video_mapping[&4], "video-track");
    assert_eq!(agent.snapshot().audio_mapping[&2], "audio-track");
    assert_eq!(agent.snapshot().publications.len(), 2);
    assert!(drain_notifications(&mut agent).contains(&Notification::SnapshotChanged));

    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: cid,
            payload: server_payload(server_message::Payload::Mapping(v1::Mapping {
                intent_revision: 1,
                video: Some(v1::TrackMappings { tracks: vec![] }),
                audio: Some(v1::TrackMappings { tracks: vec![] }),
            })),
        }))
        .unwrap();
    assert!(agent.snapshot().video_mapping.is_empty());
    assert!(agent.snapshot().audio_mapping.is_empty());
    assert_eq!(
        drain_notifications(&mut agent),
        vec![Notification::SnapshotChanged]
    );

    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: cid,
            payload: server_payload(server_message::Payload::Catalog(v1::Catalog {
                revision: 2,
                state: Some(v1::catalog::State::Delta(v1::CatalogDelta {
                    removed_participant_ids: vec!["publisher".to_string()],
                    ..Default::default()
                })),
            })),
        }))
        .unwrap();
    assert!(agent.snapshot().participants.is_empty());
    assert!(agent.snapshot().publications.is_empty());
}

#[test]
fn mapping_omission_retains_desire_until_a_subsequent_mapping() {
    let (mut agent, generation, cid, send) = connected_agent();
    acknowledge_send(&mut agent, generation, cid, send);
    let deliver = |agent: &mut Agent, payload: server_message::Payload| {
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Message {
                generation,
                channel: cid,
                payload: server_payload(payload),
            }))
            .unwrap();
    };
    deliver(
        &mut agent,
        server_message::Payload::Catalog(v1::Catalog {
            revision: 1,
            state: Some(v1::catalog::State::Snapshot(v1::CatalogSnapshot {
                participants: vec![v1::Participant {
                    participant_id: "publisher".into(),
                    participant_external_id: "publisher-external".into(),
                }],
                tracks: ["audio-track", "audio-second"]
                    .into_iter()
                    .map(|id| v1::RemoteTrack {
                        track_id: id.into(),
                        participant_id: "publisher".into(),
                        kind: v1::TrackKind::Audio.into(),
                        mode: 0,
                        label: id.into(),
                    })
                    .collect(),
            })),
        }),
    );
    let mut selected = desired(2);
    selected.audio.pinned.push("audio-second".into());
    agent
        .command(AgentCommand::ReplaceDesired(selected))
        .unwrap();
    let Effect::DataChannel(DataChannelEffect::Send {
        operation, payload, ..
    }) = next_effect(&mut agent)
    else {
        panic!("expected A+B intent");
    };
    let audio = decode_intent(&payload)
        .receive
        .unwrap()
        .audio
        .unwrap()
        .tracks;
    assert_eq!(audio.len(), 2);
    assert_eq!(audio[0].track_id, "audio-track");
    assert_eq!(audio[1].track_id, "audio-second");
    acknowledge_send(&mut agent, generation, cid, operation);
    for (id, expected) in [
        ("audio-track", "audio-track"),
        ("audio-second", "audio-second"),
    ] {
        deliver(
            &mut agent,
            server_message::Payload::Mapping(v1::Mapping {
                intent_revision: 2,
                video: Some(v1::TrackMappings { tracks: vec![] }),
                audio: Some(v1::TrackMappings {
                    tracks: vec![v1::TrackMapping {
                        receiver_index: 2,
                        track_id: id.into(),
                    }],
                }),
            }),
        );
        assert_eq!(agent.snapshot().audio_mapping[&2], expected);
        assert_eq!(agent.snapshot().publications.len(), 2);
        assert!(agent.next_effect().is_none());
    }
    deliver(
        &mut agent,
        server_message::Payload::Mapping(v1::Mapping {
            intent_revision: 2,
            video: Some(v1::TrackMappings { tracks: vec![] }),
            audio: Some(v1::TrackMappings { tracks: vec![] }),
        }),
    );
    assert!(agent.snapshot().audio_mapping.is_empty());
    assert_eq!(agent.snapshot().publications.len(), 2);
    assert!(agent.next_effect().is_none());
}

#[test]
fn catalog_removal_retracts_direct_video_and_pinned_audio_intent() {
    let (mut agent, generation, cid, send) = connected_agent();
    acknowledge_send(&mut agent, generation, cid, send);
    let deliver = |agent: &mut Agent, catalog: v1::Catalog| {
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Message {
                generation,
                channel: cid,
                payload: server_payload(server_message::Payload::Catalog(catalog)),
            }))
            .unwrap();
    };
    deliver(
        &mut agent,
        v1::Catalog {
            revision: 1,
            state: Some(v1::catalog::State::Snapshot(v1::CatalogSnapshot {
                participants: vec![v1::Participant {
                    participant_id: "publisher".into(),
                    participant_external_id: "publisher-external".into(),
                }],
                tracks: vec![
                    v1::RemoteTrack {
                        track_id: "video-track".into(),
                        participant_id: "publisher".into(),
                        kind: v1::TrackKind::Video.into(),
                        mode: 0,
                        label: "camera".into(),
                    },
                    v1::RemoteTrack {
                        track_id: "audio-track".into(),
                        participant_id: "publisher".into(),
                        kind: v1::TrackKind::Audio.into(),
                        mode: 0,
                        label: "microphone".into(),
                    },
                ],
            })),
        },
    );
    assert!(agent.next_effect().is_none());
    deliver(
        &mut agent,
        v1::Catalog {
            revision: 2,
            state: Some(v1::catalog::State::Delta(v1::CatalogDelta {
                removed_track_ids: vec!["video-track".into(), "audio-track".into()],
                ..Default::default()
            })),
        },
    );
    let Effect::DataChannel(DataChannelEffect::Send {
        operation, payload, ..
    }) = next_effect(&mut agent)
    else {
        panic!("expected retracted intent");
    };
    let receive = decode_intent(&payload).receive.unwrap();
    assert!(receive.video.unwrap().tracks.is_empty());
    assert!(receive.audio.unwrap().tracks.is_empty());
    acknowledge_send(&mut agent, generation, cid, operation);
    assert!(agent.next_effect().is_none());

    deliver(
        &mut agent,
        v1::Catalog {
            revision: 3,
            state: Some(v1::catalog::State::Delta(v1::CatalogDelta {
                added_tracks: vec![v1::RemoteTrack {
                    track_id: "video-track".into(),
                    participant_id: "publisher".into(),
                    kind: v1::TrackKind::Video.into(),
                    mode: 0,
                    label: "camera".into(),
                }],
                ..Default::default()
            })),
        },
    );
    assert!(agent.next_effect().is_none());
    let mut changed = desired(2);
    changed.video[0].height = 1080;
    agent
        .command(AgentCommand::ReplaceDesired(changed))
        .unwrap();
    let Effect::DataChannel(DataChannelEffect::Send {
        operation, payload, ..
    }) = next_effect(&mut agent)
    else {
        panic!("expected intent after unrelated policy change");
    };
    assert!(
        decode_intent(&payload)
            .receive
            .unwrap()
            .video
            .unwrap()
            .tracks
            .is_empty()
    );
    acknowledge_send(&mut agent, generation, cid, operation);
    let mut cleared = desired(3);
    cleared.video.clear();
    cleared.audio.pinned.clear();
    agent
        .command(AgentCommand::ReplaceDesired(cleared))
        .unwrap();
    let Effect::DataChannel(DataChannelEffect::Send { operation, .. }) = next_effect(&mut agent)
    else {
        panic!("expected cleared desire");
    };
    acknowledge_send(&mut agent, generation, cid, operation);
    let mut fresh = desired(4);
    fresh.audio.pinned.clear();
    agent.command(AgentCommand::ReplaceDesired(fresh)).unwrap();
    let Effect::DataChannel(DataChannelEffect::Send { payload, .. }) = next_effect(&mut agent)
    else {
        panic!("expected fresh explicit selection");
    };
    assert_eq!(
        decode_intent(&payload)
            .receive
            .unwrap()
            .video
            .unwrap()
            .tracks[0]
            .track_id,
        "video-track"
    );
}

#[test]
fn catalog_selectors_remain_unresolved_then_follow_canonical_track_changes() {
    for owned in [false, true] {
        let mut selected = desired(1);
        selected.video[0].track_id.clear();
        selected.video[0].selector = Some(TrackSelector {
            participant_external_id: "remote-user".to_string(),
            label: "camera".to_string(),
        });
        selected.audio.pinned.clear();
        let audio_selector = TrackSelector {
            participant_external_id: "remote-user".to_string(),
            label: "microphone".to_string(),
        };
        selected.audio.selected.push(audio_selector.clone());
        selected.audio.selector_delays.insert(
            audio_selector,
            PlayoutDelay::Fixed {
                min_ms: 0,
                max_ms: 0,
            },
        );
        let mut agent = Agent::new(config()).unwrap();
        let view = if owned {
            let video = selected.video.remove(0);
            let ResourceResult::Owned { owner } = agent
                .resource(ResourceCommand::ClaimVideo {
                    selector: video.selector.unwrap(),
                })
                .unwrap()
            else {
                panic!("video lease")
            };
            agent
                .resource(ResourceCommand::ObserveVideo {
                    owner,
                    observation: VideoObservation {
                        visible: true,
                        height: video.height,
                        min_height: video.min_height,
                        min_fps: video.min_fps,
                        priority: video.priority,
                        playout_delay: video.playout_delay,
                    },
                })
                .unwrap();
            Some(owner)
        } else {
            None
        };
        let cid = channel(9);
        let (generation, operation) = begin_connect(&mut agent, selected, cid);
        let send = finish_connect(&mut agent, generation, operation, cid);
        acknowledge_send(&mut agent, generation, cid, send);

        let deliver = |agent: &mut Agent, catalog: v1::Catalog| {
            agent
                .handle(HostEvent::DataChannel(DataChannelEvent::Message {
                    generation,
                    channel: cid,
                    payload: server_payload(server_message::Payload::Catalog(catalog)),
                }))
                .unwrap();
        };
        deliver(
            &mut agent,
            v1::Catalog {
                revision: 1,
                state: Some(v1::catalog::State::Snapshot(v1::CatalogSnapshot {
                    participants: vec![v1::Participant {
                        participant_id: "remote".to_string(),
                        participant_external_id: "remote-user".to_string(),
                    }],
                    tracks: vec![],
                })),
            },
        );
        assert!(agent.next_effect().is_none());

        deliver(
            &mut agent,
            v1::Catalog {
                revision: 2,
                state: Some(v1::catalog::State::Delta(v1::CatalogDelta {
                    added_tracks: vec![
                        v1::RemoteTrack {
                            track_id: "video-one".to_string(),
                            participant_id: "remote".to_string(),
                            kind: v1::TrackKind::Video.into(),
                            mode: 0,
                            label: "camera".to_string(),
                        },
                        v1::RemoteTrack {
                            track_id: "audio-one".to_string(),
                            participant_id: "remote".to_string(),
                            kind: v1::TrackKind::Audio.into(),
                            mode: 0,
                            label: "microphone".to_string(),
                        },
                    ],
                    ..Default::default()
                })),
            },
        );
        let Effect::DataChannel(DataChannelEffect::Send {
            operation, payload, ..
        }) = next_effect(&mut agent)
        else {
            panic!("expected resolved intent");
        };
        let receive = decode_intent(&payload).receive.unwrap();
        assert_eq!(receive.video.unwrap().tracks[0].track_id, "video-one");
        let audio = receive.audio.unwrap().tracks;
        assert_eq!(audio[0].track_id, "audio-one");
        let delay = audio[0]
            .options
            .as_ref()
            .unwrap()
            .playout_delay
            .as_ref()
            .unwrap();
        assert_eq!((delay.min_ms, delay.max_ms), (0, 0));
        acknowledge_send(&mut agent, generation, cid, operation);

        deliver(
            &mut agent,
            v1::Catalog {
                revision: 3,
                state: Some(v1::catalog::State::Delta(v1::CatalogDelta {
                    removed_track_ids: vec!["video-one".to_string()],
                    ..Default::default()
                })),
            },
        );
        let Effect::DataChannel(DataChannelEffect::Send {
            operation, payload, ..
        }) = next_effect(&mut agent)
        else {
            panic!("expected cleared video intent");
        };
        assert!(
            decode_intent(&payload)
                .receive
                .unwrap()
                .video
                .unwrap()
                .tracks
                .is_empty()
        );
        acknowledge_send(&mut agent, generation, cid, operation);

        deliver(
            &mut agent,
            v1::Catalog {
                revision: 4,
                state: Some(v1::catalog::State::Delta(v1::CatalogDelta {
                    added_tracks: vec![v1::RemoteTrack {
                        track_id: "video-two".to_string(),
                        participant_id: "remote".to_string(),
                        kind: v1::TrackKind::Video.into(),
                        mode: 0,
                        label: "camera".to_string(),
                    }],
                    ..Default::default()
                })),
            },
        );
        let Effect::DataChannel(DataChannelEffect::Send {
            operation, payload, ..
        }) = next_effect(&mut agent)
        else {
            panic!("expected re-resolved video intent");
        };
        assert_eq!(
            decode_intent(&payload)
                .receive
                .unwrap()
                .video
                .unwrap()
                .tracks[0]
                .track_id,
            "video-two"
        );
        acknowledge_send(&mut agent, generation, cid, operation);
        assert!(agent.next_effect().is_none());
        if let Some(owner) = view {
            agent.resource(ResourceCommand::Release { owner }).unwrap();
            let Effect::DataChannel(DataChannelEffect::Send { payload, .. }) =
                next_effect(&mut agent)
            else {
                panic!("withdrawn view intent")
            };
            assert!(
                decode_intent(&payload)
                    .receive
                    .unwrap()
                    .video
                    .unwrap()
                    .tracks
                    .is_empty()
            );
        }
    }
}

#[test]
fn malformed_signaling_is_transactional_and_fences_an_in_flight_replacement() {
    let (mut agent, generation, cid, send) = connected_agent();
    acknowledge_send(&mut agent, generation, cid, send);
    let mut changed = desired(2);
    changed
        .topics
        .publishers
        .push(publisher("chat", TopicMode::Ordered));
    agent
        .command(AgentCommand::ReplaceDesired(changed))
        .unwrap();
    let (replacement, topic_label) = match next_effect(&mut agent) {
        Effect::Rtc(RtcEffect::CreateOffer {
            generation,
            data_channels,
            ..
        }) => (generation, data_channels[1].label.clone()),
        effect => panic!("expected replacement offer, got {effect:?}"),
    };
    let mut replacement_resources = resources(channel(13));
    replacement_resources
        .data_channels
        .push(DataChannelBinding {
            label: topic_label,
            channel: channel(14),
        });
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation: replacement,
            offer: offer(),
            resources: replacement_resources,
        }))
        .unwrap();
    let operation = match next_effect(&mut agent) {
        Effect::Http(HttpEffect::Request { operation, .. }) => operation,
        effect => panic!("expected replacement request, got {effect:?}"),
    };
    let before = agent.snapshot().clone();
    let malformed = v1::Catalog {
        revision: 1,
        state: Some(v1::catalog::State::Snapshot(v1::CatalogSnapshot {
            participants: vec![
                v1::Participant {
                    participant_id: "duplicate".to_string(),
                    participant_external_id: "a".to_string(),
                },
                v1::Participant {
                    participant_id: "duplicate".to_string(),
                    participant_external_id: "b".to_string(),
                },
            ],
            tracks: vec![],
        })),
    };
    assert!(matches!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: cid,
            payload: server_payload(server_message::Payload::Catalog(malformed)),
        })),
        Err(AgentError::InvalidSignaling(SignalingError::Invalid(
            "catalog"
        )))
    ));
    assert_eq!(agent.snapshot().participants, before.participants);
    assert_eq!(agent.snapshot().publications, before.publications);
    assert_eq!(agent.snapshot().video_mapping, before.video_mapping);
    assert_eq!(
        agent.snapshot().connection,
        ConnectionState::TerminalFailure
    );
    assert_eq!(
        agent
            .snapshot()
            .terminal_failure
            .as_ref()
            .map(|failure| &failure.class),
        Some(&FailureClass::Protocol)
    );
    let failure = agent.snapshot().terminal_failure.clone();
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close {
            generation: replacement
        })
    );
    assert_eq!(
        next_effect(&mut agent),
        Effect::Http(HttpEffect::Cancel { operation })
    );
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: update_response("p1"),
        }))
        .unwrap();
    for event in [
        RtcEvent::Connected {
            generation: replacement,
        },
        RtcEvent::AnswerApplied {
            generation: replacement,
        },
    ] {
        agent.handle(HostEvent::Rtc(event)).unwrap();
    }
    assert_eq!(
        agent.snapshot().connection,
        ConnectionState::TerminalFailure
    );
    assert_eq!(agent.snapshot().terminal_failure, failure);
    while let Some(effect) = agent.next_effect() {
        assert!(
            matches!(&effect, Effect::Http(HttpEffect::Request {request, ..}) if request.method == HttpMethod::Delete),
            "late completions can only clean orphaned resources, not activate or retry: {effect:?}"
        );
    }
    agent.command(AgentCommand::Reconnect).unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::CreateOffer { .. })
    ));
}

#[test]
fn complete_intent_retracts_omitted_state_and_allows_per_track_playout_changes() {
    let (mut agent, generation, cid, initial_send) = connected_agent();
    assert!(agent.next_effect().is_none());
    acknowledge_send(&mut agent, generation, cid, initial_send);

    let mut retracted = desired(2);
    retracted.publications.clear();
    retracted.video.clear();
    retracted.audio = AudioSubscription::default();
    agent
        .command(AgentCommand::ReplaceDesired(retracted))
        .unwrap();
    let (operation, intent) = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            operation, payload, ..
        }) => (operation, decode_intent(&payload)),
        effect => panic!("expected updated intent, got {effect:?}"),
    };
    assert_eq!(intent.revision, 2);
    assert!(intent.send.unwrap().tracks.is_empty());
    let receive = intent.receive.unwrap();
    assert!(receive.video.unwrap().tracks.is_empty());
    assert!(receive.audio.unwrap().tracks.is_empty());
    acknowledge_send(&mut agent, generation, cid, operation);

    let mut per_track = desired(3);
    per_track.video[0].playout_delay = PlayoutDelay::Fixed {
        min_ms: 20,
        max_ms: 100,
    };
    per_track.audio.playout_delays.insert(
        "audio-track".into(),
        PlayoutDelay::Fixed {
            min_ms: 0,
            max_ms: 0,
        },
    );
    agent
        .command(AgentCommand::ReplaceDesired(per_track))
        .unwrap();
    let next = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send { payload, .. }) => decode_intent(&payload),
        effect => panic!("expected track policy intent, got {effect:?}"),
    };
    let receive = next.receive.unwrap();
    assert_eq!(
        receive.video.unwrap().tracks[0]
            .options
            .as_ref()
            .unwrap()
            .playout_delay
            .as_ref()
            .unwrap()
            .min_ms,
        20
    );
    assert_eq!(
        receive.audio.unwrap().tracks[0]
            .options
            .as_ref()
            .unwrap()
            .playout_delay
            .as_ref()
            .unwrap()
            .max_ms,
        0
    );

    let disconnected = DesiredState {
        revision: 4,
        ..DesiredState::default()
    };
    agent
        .command(AgentCommand::ReplaceDesired(disconnected))
        .unwrap();
    assert_eq!(agent.snapshot().desired_revision, 4);
}

#[test]
fn pending_signaling_coalesces_superseded_state_and_fences_retired_completion() {
    let (mut agent, generation, cid, first_send) = connected_agent();
    for (revision, height) in [(2, 360), (3, 1080)] {
        let mut replacement = desired(revision);
        replacement.video[0].height = height;
        agent
            .command(AgentCommand::ReplaceDesired(replacement))
            .unwrap();
        assert!(agent.next_effect().is_none());
    }
    acknowledge_send(&mut agent, generation, cid, first_send);
    let (operation, intent) = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            operation, payload, ..
        }) => (operation, decode_intent(&payload)),
        effect => panic!("expected latest complete intent, got {effect:?}"),
    };
    assert_eq!(
        intent.receive.unwrap().video.unwrap().tracks[0]
            .options
            .as_ref()
            .unwrap()
            .height,
        1080
    );
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 4,
            ..DesiredState::default()
        }))
        .unwrap();
    while agent.next_effect().is_some() {}
    acknowledge_send(&mut agent, generation, cid, operation);
    assert!(agent.next_effect().is_none());
    assert_eq!(agent.snapshot().desired_revision, 4);
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
    assert_eq!(intent.revision, 2);
    assert_eq!(
        intent.receive.unwrap().video.unwrap().tracks[0]
            .options
            .as_ref()
            .unwrap()
            .height,
        360
    );
}

#[test]
fn topic_registrations_validate_declare_and_retract_complete_channels() {
    let registrations = TopicRegistrations {
        publishers: vec![
            publisher("state", TopicMode::Latest),
            publisher("state", TopicMode::Lossy),
            publisher("chat", TopicMode::Ordered),
        ],
        subscribers: vec![
            subscriber("state", TopicMode::Latest),
            subscriber("state", TopicMode::Lossy),
            subscriber("chat", TopicMode::Ordered),
        ],
    };
    let (mut agent, _, _, _) = connect_with_topics(registrations);
    assert_eq!(agent.snapshot().topics.publishers.len(), 3);
    assert_eq!(agent.snapshot().topics.subscribers.len(), 3);
    assert!(
        agent
            .snapshot()
            .topics
            .publishers
            .iter()
            .all(|status| status.channel.is_some() && status.data_track_id.is_some())
    );
    let mut retracted = desired(2);
    retracted.topics = TopicRegistrations {
        publishers: vec![publisher("chat", TopicMode::Ordered)],
        subscribers: vec![subscriber("chat", TopicMode::Ordered)],
    };
    agent
        .command(AgentCommand::ReplaceDesired(retracted))
        .unwrap();
    let Effect::Rtc(RtcEffect::CreateOffer { data_channels, .. }) = next_effect(&mut agent) else {
        panic!("replacement offer");
    };
    assert_eq!(
        data_channels
            .iter()
            .map(|spec| spec.label.as_str())
            .collect::<Vec<_>>(),
        vec![
            "v1/sys/signaling",
            "v2/ordered/pub/chat",
            "v2/ordered/sub/chat"
        ]
    );
    assert_eq!(agent.snapshot().topics.publishers.len(), 1);
    assert_eq!(agent.snapshot().topics.subscribers.len(), 1);
    let mut invalid = desired(3);
    invalid.topics.publishers = vec![publisher("bad/topic", TopicMode::Latest)];
    assert!(matches!(
        agent.command(AgentCommand::ReplaceDesired(invalid)),
        Err(AgentError::InvalidConfiguration(ValidationError::Topic(_)))
    ));
    let mut duplicate = desired(3);
    duplicate.topics.subscribers = vec![
        subscriber("chat", TopicMode::Ordered),
        subscriber("chat", TopicMode::Ordered),
    ];
    assert!(matches!(
        agent.command(AgentCommand::ReplaceDesired(duplicate)),
        Err(AgentError::InvalidConfiguration(
            ValidationError::Duplicate { .. }
        ))
    ));
}

#[test]
fn connected_subscription_bootstrap_is_bounded_and_completed_timers_are_retired() {
    for complete in [false, true] {
        let mut timers = Vec::new();
        let (mut agent, generation, _, _) = connect_with_topic_seed(
            TopicRegistrations {
                subscribers: vec![subscriber("events", TopicMode::Ordered)],
                ..TopicRegistrations::default()
            },
            complete,
            &mut timers,
        );
        let timer = timers
            .iter()
            .find_map(|(timer, after)| (*after == Duration::from_secs(30)).then_some(*timer))
            .expect("connected bootstrap has a bounded deadline");
        agent
            .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
            .unwrap();
        assert!(agent.snapshot().terminal_failure.is_none());
        if complete {
            assert!(
                agent.next_effect().is_none(),
                "retired completion deadline cannot reset a call"
            );
            assert_eq!(agent.snapshot().generation, Some(generation));
        } else {
            assert!(
                matches!(next_effect(&mut agent), Effect::Rtc(RtcEffect::CreateOffer { generation: next, .. }) if next != generation)
            );
            assert!(drain_notifications(&mut agent).iter().any(|notification|
                matches!(notification, Notification::Failure(failure) if failure.message.contains("subscription seed"))
            ));
        }
    }
}

#[test]
fn latest_topic_send_admission_freshness_and_failures_are_explicit() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("state", TopicMode::Latest)],
        subscribers: vec![subscriber("state", TopicMode::Latest)],
    };
    let (mut agent, generation, _, channels) = connect_with_topics(registrations);
    let publish_channel = channels["v2/latest/pub/state"];
    let subscribe_channel = channels["v2/latest/sub/state"];
    let publish = publisher("state", TopicMode::Latest);
    for value in [b"first".as_slice(), b"second", b"third"] {
        agent
            .command(AgentCommand::SendTopic(TopicSend {
                publisher: publish.clone(),
                payload: value.to_vec(),
            }))
            .unwrap();
    }
    let (first, _, body) = next_topic_send(&mut agent);
    let TopicBody::Message(first_message) = body else {
        panic!("message envelope");
    };
    assert_eq!(first_message.payload, b"first");
    assert_eq!(first_message.sequence, 0);
    assert_eq!(
        first_message.data_track_id,
        data_id(true, "state", TopicMode::Latest, "first")
    );
    assert!(agent.next_effect().is_none());
    assert_eq!(agent.snapshot().topics.accepted_sends, 3);
    assert_eq!(agent.snapshot().topics.dropped_sends, 1);
    acknowledge_send(&mut agent, generation, publish_channel, first);
    let (third, _, body) = next_topic_send(&mut agent);
    let TopicBody::Message(third_message) = body else {
        panic!("message envelope");
    };
    assert_eq!(
        (third_message.sequence, third_message.payload),
        (2, b"third".to_vec())
    );
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::SendFailed {
            operation: third,
            generation,
            channel: publish_channel,
            message: "host queue full".into(),
        }))
        .unwrap();
    assert_eq!(agent.snapshot().topics.dropped_sends, 2);
    assert_eq!(agent.snapshot().topics.channel_failures, 1);
    let notifications = drain_notifications(&mut agent);
    assert!(notifications.iter().any(|event| matches!(
        event,
        Notification::Topic(TopicNotification::SendDropped {
            reason: TopicDropReason::HostRejected,
            ..
        })
    )));
    assert!(notifications.iter().any(|event| matches!(
        event,
        Notification::Topic(TopicNotification::ChannelFailed { .. })
    )));
    let id = data_id(false, "state", TopicMode::Latest, "first");
    for sequence in [5, 3, 5, 6] {
        topic_input(
            &mut agent,
            generation,
            subscribe_channel,
            wire_message(&id, sequence, b"latest"),
        )
        .unwrap();
    }
    let messages = drain_notifications(&mut agent)
        .into_iter()
        .filter_map(|event| match event {
            Notification::Topic(TopicNotification::Message(message)) => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        messages
            .iter()
            .map(|message| message.sequence)
            .collect::<Vec<_>>(),
        vec![5, 6]
    );
    assert!(messages.iter().all(|message| message.data_track_id == id
        && message.participant_id == "remote-app-user"
        && message.mode == TopicMode::Latest));
    assert!(matches!(
        agent.command(AgentCommand::SendTopic(TopicSend {
            publisher: publish,
            payload: vec![0; MAX_TOPIC_PAYLOAD_BYTES + 1]
        })),
        Err(AgentError::InvalidTopic(TopicError::PayloadTooLarge { .. }))
    ));
}

#[test]
fn owned_publications_reconcile_without_legacy_lifetime_binding_or_cross_owner_cleanup() {
    let mut configuration = config();
    configuration.topology.local_video = 1;
    let mut agent = Agent::new(configuration).unwrap();
    let spec = TrackSpec::Video {
        label: "camera".into(),
    };
    let ResourceResult::Publication {
        owner,
        slot,
        after_generation,
    } = agent
        .resource(ResourceCommand::ClaimPublication { spec: spec.clone() })
        .unwrap()
    else {
        panic!("publication lease")
    };
    assert_eq!(slot, "v0");
    assert_eq!(after_generation, None);
    let sender = MediaSlot::LocalVideo(slot.clone());
    assert!(!agent.is_publishing(&sender));
    agent
        .resource(ResourceCommand::UpdatePublication {
            owner,
            present: true,
        })
        .unwrap();
    assert!(agent.is_publishing(&sender));
    assert_eq!(
        agent.resource(ResourceCommand::ClaimPublication { spec }),
        Err(AgentError::InvalidResource(
            TrackResourceError::AlreadyOwned
        ))
    );
    let legacy = DesiredState {
        revision: 1,
        publications: vec![PublicationIntent {
            slot: slot.clone(),
            label: "legacy".into(),
            active: true,
        }],
        ..DesiredState::default()
    };
    assert_eq!(
        agent.command(AgentCommand::ReplaceDesired(legacy)),
        Err(AgentError::InvalidResource(
            TrackResourceError::AlreadyOwned
        ))
    );
    assert!(agent.is_publishing(&sender));
    agent.resource(ResourceCommand::Release { owner }).unwrap();
    assert!(!agent.is_publishing(&sender));
    let ResourceResult::Publication {
        owner: next,
        slot: reused,
        after_generation: next_generation,
    } = agent
        .resource(ResourceCommand::ClaimPublication {
            spec: TrackSpec::Video {
                label: "screen".into(),
            },
        })
        .unwrap()
    else {
        panic!("replacement lease")
    };
    assert_eq!(reused, slot);
    assert_eq!(next_generation, None);
    assert_ne!(next, owner);
    agent
        .resource(ResourceCommand::UpdatePublication {
            owner: next,
            present: true,
        })
        .unwrap();
    assert_eq!(
        agent.resource(ResourceCommand::UpdatePublication {
            owner,
            present: false
        }),
        Err(AgentError::InvalidResource(TrackResourceError::Released))
    );
    agent.resource(ResourceCommand::Release { owner }).unwrap();
    assert!(agent.is_publishing(&sender));
}

#[test]
fn owned_publication_relabel_replaces_the_wire_session_and_fences_the_new_source() {
    let mut agent = Agent::new(config()).unwrap();
    let camera = TrackSpec::Video {
        label: "camera".into(),
    };
    let ResourceResult::Publication {
        owner,
        slot,
        after_generation,
    } = agent
        .resource(ResourceCommand::ClaimPublication { spec: camera })
        .unwrap()
    else {
        panic!("camera owner")
    };
    assert_eq!(after_generation, None);
    agent
        .resource(ResourceCommand::UpdatePublication {
            owner,
            present: true,
        })
        .unwrap();

    let channel = channel(21);
    let (generation, request) = begin_connect(
        &mut agent,
        DesiredState {
            revision: 1,
            connected: true,
            ..DesiredState::default()
        },
        channel,
    );
    let signal = finish_connect(&mut agent, generation, request, channel);
    acknowledge_send(&mut agent, generation, channel, signal);

    agent.resource(ResourceCommand::Release { owner }).unwrap();
    let omitted = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            operation,
            generation: actual,
            channel: actual_channel,
            payload,
            ..
        }) => {
            assert_eq!(actual, generation);
            assert_eq!(actual_channel, channel);
            assert!(decode_intent(&payload).send.unwrap().tracks.is_empty());
            operation
        }
        effect => panic!("expected publication withdrawal, got {effect:?}"),
    };
    acknowledge_send(&mut agent, generation, channel, omitted);

    let ResourceResult::Publication {
        owner: replacement,
        slot: reused,
        after_generation,
    } = agent
        .resource(ResourceCommand::ClaimPublication {
            spec: TrackSpec::Video {
                label: "screen".into(),
            },
        })
        .unwrap()
    else {
        panic!("screen owner")
    };
    assert_eq!(reused, slot);
    assert_eq!(after_generation, Some(generation));
    agent
        .resource(ResourceCommand::UpdatePublication {
            owner: replacement,
            present: true,
        })
        .unwrap();

    let next_generation = match next_effect(&mut agent) {
        Effect::Rtc(RtcEffect::CreateOffer {
            generation: next, ..
        }) => next,
        effect => panic!("expected fresh wire session, got {effect:?}"),
    };
    assert_ne!(next_generation, generation);
    assert!(
        agent.next_effect().is_none(),
        "relabel must not reach the bound session"
    );
}

#[test]
fn owned_receive_uses_transport_deduplication_and_last_owner_registration_lifetime() {
    let registrations = TopicRegistrations {
        publishers: vec![],
        subscribers: vec![subscriber("events", TopicMode::Lossy)],
    };
    let (mut agent, generation, _, channels) = connect_with_topics(registrations);
    let spec = TrackSpec::Data {
        label: "events".into(),
        delivery: TopicMode::Lossy,
    };
    let mut owners = Vec::new();
    for participant_id in [None, Some("remote-app-user".into()), Some("bob".into())] {
        let ResourceResult::Owned { owner } = agent
            .resource(ResourceCommand::ReceiveData {
                spec: spec.clone(),
                participant_id,
            })
            .unwrap()
        else {
            panic!("receive lease")
        };
        owners.push(owner);
    }
    agent
        .command(AgentCommand::ReplaceDesired(desired(2)))
        .unwrap();
    assert_eq!(agent.snapshot().topics.subscribers.len(), 1);
    assert!(agent.next_effect().is_none());
    drain_notifications(&mut agent);
    let remote = data_id(false, "events", TopicMode::Lossy, "first");
    for _ in 0..2 {
        topic_input(
            &mut agent,
            generation,
            channels["v2/lossy/sub/events"],
            wire_message(&remote, 0, &[255, 0]),
        )
        .unwrap();
    }
    let notifications = drain_notifications(&mut agent);
    assert_eq!(notifications.iter().filter(|notification| matches!(notification, Notification::DataReady { owners: ready } if ready == &owners[..2])).count(), 1);
    for owner in &owners[..2] {
        assert_eq!(
            agent
                .resource(ResourceCommand::ReadData { owner: *owner })
                .unwrap(),
            ResourceResult::Event {
                event: Some(DataEvent::Message {
                    participant_id: "remote-app-user".into(),
                    track_id: remote.clone(),
                    sequence: 0,
                    data: vec![255, 0],
                })
            }
        );
        assert_eq!(
            agent
                .resource(ResourceCommand::ReadData { owner: *owner })
                .unwrap(),
            ResourceResult::Event { event: None }
        );
    }
    assert_eq!(
        agent
            .resource(ResourceCommand::ReadData { owner: owners[2] })
            .unwrap(),
        ResourceResult::Event { event: None }
    );
    for owner in &owners[..2] {
        agent
            .resource(ResourceCommand::Release { owner: *owner })
            .unwrap();
        assert_eq!(agent.snapshot().topics.subscribers.len(), 1);
    }
    agent
        .resource(ResourceCommand::Release { owner: owners[2] })
        .unwrap();
    assert!(agent.snapshot().topics.subscribers.is_empty());
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::CreateOffer { .. })
    ));
}

#[test]
fn ordered_topics_reorder_deduplicate_nack_replay_and_emit_inline_gaps() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("chat", TopicMode::Ordered)],
        subscribers: vec![subscriber("chat", TopicMode::Ordered)],
    };
    let (mut agent, generation, _, channels) = connect_with_topics(registrations);
    let publish_channel = channels["v2/ordered/pub/chat"];
    let subscribe_channel = channels["v2/ordered/sub/chat"];
    let local = data_id(true, "chat", TopicMode::Ordered, "first");
    let remote = data_id(false, "chat", TopicMode::Ordered, "first");
    for sequence in 0..3 {
        agent
            .command(AgentCommand::SendTopic(TopicSend {
                publisher: publisher("chat", TopicMode::Ordered),
                payload: vec![u8::try_from(sequence).unwrap()],
            }))
            .unwrap();
        let (operation, channel, body) = next_topic_send(&mut agent);
        let TopicBody::Message(message) = body else {
            panic!("message");
        };
        assert_eq!(
            (message.data_track_id, message.sequence),
            (local.clone(), sequence)
        );
        acknowledge_send(&mut agent, generation, channel, operation);
    }
    assert_eq!(agent.snapshot().topics.publishers[0].accepted_history, 3);
    drain_notifications(&mut agent);
    topic_input(
        &mut agent,
        generation,
        subscribe_channel,
        TopicBody::Frontier(TopicFrontier {
            data_track_id: remote.clone(),
            next_sequence: 0,
        }),
    )
    .unwrap();
    topic_input(
        &mut agent,
        generation,
        subscribe_channel,
        wire_message(&remote, 0, b"zero"),
    )
    .unwrap();
    topic_input(
        &mut agent,
        generation,
        subscribe_channel,
        wire_message(&remote, 2, b"two"),
    )
    .unwrap();
    let (operation, channel, body) = next_topic_send(&mut agent);
    assert_eq!(channel, subscribe_channel);
    assert_eq!(
        body,
        TopicBody::Nack(TopicNack {
            data_track_id: remote.clone(),
            from_sequence: 1
        })
    );
    acknowledge_send(&mut agent, generation, channel, operation);
    topic_input(
        &mut agent,
        generation,
        subscribe_channel,
        wire_message(&remote, 1, b"one"),
    )
    .unwrap();
    let delivered = drain_notifications(&mut agent)
        .into_iter()
        .filter_map(|event| match event {
            Notification::Topic(TopicNotification::Message(message)) => Some(message.sequence),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(delivered, vec![0, 1, 2]);
    topic_input(
        &mut agent,
        generation,
        subscribe_channel,
        wire_message(&remote, 2, b"duplicate"),
    )
    .unwrap();
    assert!(drain_notifications(&mut agent).is_empty());
    topic_input(
        &mut agent,
        generation,
        publish_channel,
        TopicBody::Nack(TopicNack {
            data_track_id: local,
            from_sequence: 1,
        }),
    )
    .unwrap();
    for sequence in 1..3 {
        let (operation, channel, body) = next_topic_send(&mut agent);
        assert!(matches!(body, TopicBody::Message(message) if message.sequence == sequence));
        acknowledge_send(&mut agent, generation, channel, operation);
    }
    topic_input(
        &mut agent,
        generation,
        subscribe_channel,
        wire_message(&remote, 259, b"far"),
    )
    .unwrap();
    assert!(drain_notifications(&mut agent).is_empty());
    let (operation, channel, body) = next_topic_send(&mut agent);
    assert_eq!(
        body,
        TopicBody::Nack(TopicNack {
            data_track_id: remote.clone(),
            from_sequence: 3
        })
    );
    acknowledge_send(&mut agent, generation, channel, operation);
    topic_input(
        &mut agent,
        generation,
        subscribe_channel,
        TopicBody::Floor(TopicFloor {
            data_track_id: remote,
            available_sequence: 259,
        }),
    )
    .unwrap();
    assert!(matches!(
        drain_notifications(&mut agent).as_slice(),
        [
            Notification::Topic(TopicNotification::RecoveryGap {
                expected_sequence: 3,
                available_sequence: 259,
                ..
            }),
            Notification::Topic(TopicNotification::Message(TopicMessage {
                sequence: 259,
                ..
            }))
        ]
    ));
}

#[test]
fn late_receiver_starts_live_and_existing_receiver_replays_from_its_checkpoint() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("events", TopicMode::Ordered)],
        subscribers: vec![],
    };
    let (mut agent, generation, _, channels) = connect_with_topics(registrations);
    let channel = channels["v2/ordered/pub/events"];
    let id = data_id(true, "events", TopicMode::Ordered, "first");
    for sequence in 0..3 {
        agent
            .command(AgentCommand::SendTopic(TopicSend {
                publisher: publisher("events", TopicMode::Ordered),
                payload: vec![sequence],
            }))
            .unwrap();
        let (operation, _, _) = next_topic_send(&mut agent);
        acknowledge_send(&mut agent, generation, channel, operation);
    }
    topic_input(
        &mut agent,
        generation,
        channel,
        TopicBody::Probe(TopicProbe {
            data_track_id: id.clone(),
            from_sequence: 0,
            live: true,
        }),
    )
    .unwrap();
    let (operation, _, body) = next_topic_send(&mut agent);
    assert_eq!(
        body,
        TopicBody::Frontier(TopicFrontier {
            data_track_id: id.clone(),
            next_sequence: 3
        })
    );
    acknowledge_send(&mut agent, generation, channel, operation);
    assert!(agent.next_effect().is_none());
    topic_input(
        &mut agent,
        generation,
        channel,
        TopicBody::Probe(TopicProbe {
            data_track_id: id,
            from_sequence: 1,
            live: false,
        }),
    )
    .unwrap();
    for sequence in 1..3 {
        let (operation, _, body) = next_topic_send(&mut agent);
        assert!(matches!(body, TopicBody::Message(message) if message.sequence == sequence));
        acknowledge_send(&mut agent, generation, channel, operation);
    }
    assert!(agent.next_effect().is_none());
}

#[test]
fn recovery_floor_reports_only_missing_confirmed_sequences() {
    let registrations = TopicRegistrations {
        publishers: vec![],
        subscribers: vec![subscriber("events", TopicMode::Ordered)],
    };
    let (mut agent, generation, _, channels) = connect_with_topics(registrations);
    let channel = channels["v2/ordered/sub/events"];
    let id = data_id(false, "events", TopicMode::Ordered, "first");
    topic_input(
        &mut agent,
        generation,
        channel,
        wire_message(&id, 4, b"before-frontier"),
    )
    .unwrap();
    topic_input(
        &mut agent,
        generation,
        channel,
        TopicBody::Floor(TopicFloor {
            data_track_id: id.clone(),
            available_sequence: 5,
        }),
    )
    .unwrap();
    assert!(drain_notifications(&mut agent).is_empty());
    topic_input(
        &mut agent,
        generation,
        channel,
        TopicBody::Frontier(TopicFrontier {
            data_track_id: id.clone(),
            next_sequence: 5,
        }),
    )
    .unwrap();
    topic_input(
        &mut agent,
        generation,
        channel,
        wire_message(&id, 5, b"live"),
    )
    .unwrap();
    assert!(matches!(
        drain_notifications(&mut agent).as_slice(),
        [Notification::Topic(TopicNotification::Message(
            TopicMessage { sequence: 5, .. }
        ))]
    ));
    topic_input(
        &mut agent,
        generation,
        channel,
        TopicBody::Frontier(TopicFrontier {
            data_track_id: id.clone(),
            next_sequence: 99,
        }),
    )
    .unwrap();
    topic_input(
        &mut agent,
        generation,
        channel,
        TopicBody::Floor(TopicFloor {
            data_track_id: id.clone(),
            available_sequence: 9,
        }),
    )
    .unwrap();
    topic_input(
        &mut agent,
        generation,
        channel,
        wire_message(&id, 9, b"resumed"),
    )
    .unwrap();
    assert!(matches!(
        drain_notifications(&mut agent).as_slice(),
        [
            Notification::Topic(TopicNotification::RecoveryGap {
                expected_sequence: 6,
                available_sequence: 9,
                ..
            }),
            Notification::Topic(TopicNotification::Message(TopicMessage { sequence: 9, .. }))
        ]
    ));
}

#[test]
fn ordered_replay_exhaustion_emits_floor_before_the_retained_window() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("events", TopicMode::Ordered)],
        subscribers: vec![],
    };
    let (mut agent, generation, _, channels) = connect_with_topics(registrations);
    let channel = channels["v2/ordered/pub/events"];
    let id = data_id(true, "events", TopicMode::Ordered, "first");
    for sequence in 0..=TOPIC_HISTORY_CAPACITY {
        agent
            .command(AgentCommand::SendTopic(TopicSend {
                publisher: publisher("events", TopicMode::Ordered),
                payload: vec![u8::try_from(sequence).unwrap_or(u8::MAX)],
            }))
            .unwrap();
        let (operation, _, _) = next_topic_send(&mut agent);
        acknowledge_send(&mut agent, generation, channel, operation);
        drain_notifications(&mut agent);
    }
    assert_eq!(
        agent.snapshot().topics.publishers[0].accepted_history,
        TOPIC_HISTORY_CAPACITY
    );
    topic_input(
        &mut agent,
        generation,
        channel,
        TopicBody::Nack(TopicNack {
            data_track_id: id.clone(),
            from_sequence: 0,
        }),
    )
    .unwrap();
    let (operation, _, body) = next_topic_send(&mut agent);
    assert_eq!(
        body,
        TopicBody::Floor(TopicFloor {
            data_track_id: id.clone(),
            available_sequence: 1
        })
    );
    acknowledge_send(&mut agent, generation, channel, operation);
    let mut recovered = Vec::new();
    while let Some(effect) = agent.next_effect() {
        let Effect::DataChannel(DataChannelEffect::Send {
            operation, payload, ..
        }) = effect
        else {
            panic!("replay send");
        };
        let Some(TopicBody::Message(message)) = TopicFrame::decode_bounded(&payload).unwrap().body
        else {
            panic!("replay message");
        };
        recovered.push(message.sequence);
        acknowledge_send(&mut agent, generation, channel, operation);
    }
    assert_eq!(recovered, (1..256).collect::<Vec<_>>());
    topic_input(
        &mut agent,
        generation,
        channel,
        TopicBody::Probe(TopicProbe {
            data_track_id: id,
            from_sequence: 256,
            live: false,
        }),
    )
    .unwrap();
    let (operation, _, body) = next_topic_send(&mut agent);
    assert!(matches!(body, TopicBody::Message(message) if message.sequence == 256));
    acknowledge_send(&mut agent, generation, channel, operation);
    assert!(agent.next_effect().is_none());
}

#[test]
fn topic_send_queue_overflow_and_channel_close_drop_every_admitted_message() {
    for mode in [TopicMode::Lossy, TopicMode::Ordered] {
        let registrations = TopicRegistrations {
            publishers: vec![publisher("state", mode)],
            subscribers: vec![],
        };
        let (mut agent, generation, _, channels) = connect_with_topics(registrations);
        let channel = *channels.values().next().unwrap();
        for _ in 0..TOPIC_SEND_QUEUE_CAPACITY {
            agent
                .command(AgentCommand::SendTopic(TopicSend {
                    publisher: publisher("state", mode),
                    payload: b"queued".to_vec(),
                }))
                .unwrap();
        }
        assert_eq!(
            agent.snapshot().topics.publishers[0].queued_messages,
            TOPIC_SEND_QUEUE_CAPACITY - 1
        );
        assert!(matches!(
            agent.command(AgentCommand::SendTopic(TopicSend {
                publisher: publisher("state", mode),
                payload: b"overflow".to_vec()
            })),
            Err(AgentError::InvalidTopic(TopicError::SendQueueFull(_)))
        ));
        next_topic_send(&mut agent);
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Closed {
                generation,
                channel,
            }))
            .unwrap();
        assert_eq!(
            agent.snapshot().topics.dropped_sends,
            TOPIC_SEND_QUEUE_CAPACITY as u64 + 1
        );
        assert_eq!(agent.snapshot().topics.publishers[0].queued_messages, 0);
        assert_eq!(agent.snapshot().topics.publishers[0].accepted_history, 0);
        assert_eq!(agent.snapshot().topics.publishers[0].data_track_id, None);
        assert_eq!(agent.snapshot().topics.channel_failures, 1);
        assert!(matches!(
            next_effect(&mut agent),
            Effect::Rtc(RtcEffect::CreateOffer { .. })
        ));
    }
}

#[test]
fn explicit_disconnect_cancels_pending_topic_sends_and_releases_binding() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("chat", TopicMode::Ordered)],
        subscribers: vec![],
    };
    let (mut agent, _, _, _) = connect_with_topics(registrations.clone());
    for _ in 0..2 {
        agent
            .command(AgentCommand::SendTopic(TopicSend {
                publisher: publisher("chat", TopicMode::Ordered),
                payload: b"queued".to_vec(),
            }))
            .unwrap();
    }
    next_topic_send(&mut agent);
    let mut disconnected = desired(2);
    disconnected.connected = false;
    disconnected.topics = registrations;
    agent
        .command(AgentCommand::ReplaceDesired(disconnected))
        .unwrap();
    assert_eq!(agent.snapshot().topics.dropped_sends, 2);
    assert_eq!(agent.snapshot().topics.publishers[0].queued_messages, 0);
    assert_eq!(agent.snapshot().topics.publishers[0].data_track_id, None);
    assert_eq!(agent.snapshot().topics.publishers[0].accepted_history, 0);
    assert!(
        drain_notifications(&mut agent)
            .iter()
            .filter(|event| matches!(
                event,
                Notification::Topic(TopicNotification::SendDropped {
                    reason: TopicDropReason::ChannelClosed,
                    ..
                })
            ))
            .count()
            >= 2
    );
}

#[test]
fn topic_boundaries_reject_malformed_cross_mode_unknown_and_oversized_input() {
    let registrations = TopicRegistrations {
        publishers: vec![
            publisher("state", TopicMode::Latest),
            publisher("chat", TopicMode::Ordered),
        ],
        subscribers: vec![subscriber("state", TopicMode::Latest)],
    };
    let (mut agent, generation, _, channels) = connect_with_topics(registrations);
    let before = agent.snapshot().clone();
    assert_eq!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: channels["v2/ordered/pub/chat"],
            payload: b"not-protobuf".to_vec()
        })),
        Err(AgentError::InvalidTopic(TopicError::MalformedMessage))
    );
    assert_eq!(
        topic_input(
            &mut agent,
            generation,
            channels["v2/latest/pub/state"],
            TopicBody::Nack(TopicNack {
                data_track_id: data_id(true, "state", TopicMode::Latest, "first"),
                from_sequence: 0
            })
        ),
        Err(AgentError::InvalidTopic(TopicError::InvalidControl))
    );
    assert_eq!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: channel(99),
            payload: vec![]
        })),
        Err(AgentError::InvalidTopic(TopicError::UnknownChannel))
    );
    assert_eq!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            channel: channel(99)
        })),
        Err(AgentError::InvalidTopic(TopicError::UnknownChannel))
    );
    assert_eq!(
        topic_input(
            &mut agent,
            generation,
            channels["v2/latest/sub/state"],
            wire_message(
                &data_id(false, "state", TopicMode::Latest, "first"),
                0,
                &vec![0; MAX_TOPIC_PAYLOAD_BYTES + 1]
            )
        ),
        Err(AgentError::InvalidTopic(TopicError::MalformedMessage))
    );
    assert_eq!(agent.snapshot(), &before);
    assert!(drain_notifications(&mut agent).is_empty());
}

#[test]
fn lossy_keeps_distinct_messages_without_duplicates_coalescing_or_ordering() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("events", TopicMode::Lossy)],
        subscribers: vec![subscriber("events", TopicMode::Lossy)],
    };
    let (mut agent, generation, _, channels) = connect_with_topics(registrations);
    for value in 0..3 {
        agent
            .command(AgentCommand::SendTopic(TopicSend {
                publisher: publisher("events", TopicMode::Lossy),
                payload: vec![value],
            }))
            .unwrap();
    }
    for sequence in 0..3 {
        let (operation, channel, body) = next_topic_send(&mut agent);
        assert_eq!(
            body,
            wire_message(
                &data_id(true, "events", TopicMode::Lossy, "first"),
                sequence,
                &[u8::try_from(sequence).unwrap()]
            )
        );
        acknowledge_send(&mut agent, generation, channel, operation);
    }
    assert_eq!(agent.snapshot().topics.dropped_sends, 0);
    drain_notifications(&mut agent);
    let id = data_id(false, "events", TopicMode::Lossy, "first");
    for sequence in [2, 2, 0, 1, 0] {
        topic_input(
            &mut agent,
            generation,
            channels["v2/lossy/sub/events"],
            wire_message(&id, sequence, &[u8::try_from(sequence).unwrap()]),
        )
        .unwrap();
    }
    let events = drain_notifications(&mut agent);
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                Notification::Topic(TopicNotification::Message(message)) => Some(message.sequence),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![2, 0, 1]
    );
    assert_eq!(agent.snapshot().topics.delivered_messages, 3);
    assert!(events.iter().all(|event| !matches!(
        event,
        Notification::Topic(TopicNotification::RecoveryGap { .. })
    )));
}

#[test]
fn catalog_races_and_retirement_never_guess_or_revive_sender_metadata() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("state", TopicMode::Latest)],
        subscribers: vec![subscriber("state", TopicMode::Latest)],
    };
    let (mut agent, generation, signal, channels) = connect_with_topics(registrations.clone());
    let channel = channels["v2/latest/sub/state"];
    let old = data_id(false, "state", TopicMode::Latest, "first");
    let new = data_id(false, "state", TopicMode::Latest, "second");
    topic_input(
        &mut agent,
        generation,
        channel,
        wire_message(&new, 0, b"unresolved"),
    )
    .unwrap();
    assert!(drain_notifications(&mut agent).is_empty());
    agent
        .command(AgentCommand::SendTopic(TopicSend {
            publisher: publisher("state", TopicMode::Latest),
            payload: b"old-pending".to_vec(),
        }))
        .unwrap();
    next_topic_send(&mut agent);
    inject_topic_catalog(&mut agent, generation, signal, &registrations, 2, "second");
    assert_eq!(agent.snapshot().topics.publishers[0].next_sequence, 0);
    assert_eq!(agent.snapshot().topics.publishers[0].accepted_history, 0);
    assert!(!agent.snapshot().topics.publishers[0].send_pending);
    drain_notifications(&mut agent);
    topic_input(
        &mut agent,
        generation,
        channel,
        wire_message(&old, 999, b"retired"),
    )
    .unwrap();
    topic_input(
        &mut agent,
        generation,
        channel,
        wire_message(&new, 0, b"new"),
    )
    .unwrap();
    assert!(
        matches!(drain_notifications(&mut agent).as_slice(), [Notification::Topic(TopicNotification::Message(TopicMessage { participant_id, data_track_id, sequence: 0, payload, .. }))] if participant_id == "remote-app-user" && data_track_id == &new && payload == b"new")
    );
    agent
        .command(AgentCommand::SendTopic(TopicSend {
            publisher: publisher("state", TopicMode::Latest),
            payload: b"new-local".to_vec(),
        }))
        .unwrap();
    let (_, _, body) = next_topic_send(&mut agent);
    assert!(
        matches!(body, TopicBody::Message(message) if message.data_track_id == data_id(true, "state", TopicMode::Latest, "second") && message.sequence == 0)
    );
}

#[test]
fn reconnect_starts_fresh_publications_without_old_history_or_generation_output() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("chat", TopicMode::Ordered)],
        subscribers: vec![subscriber("chat", TopicMode::Ordered)],
    };
    let (mut agent, old_generation, _, old_channels) = connect_with_topics(registrations.clone());
    let old_publish_channel = old_channels["v2/ordered/pub/chat"];
    let old_subscribe_channel = old_channels["v2/ordered/sub/chat"];
    let publish = publisher("chat", TopicMode::Ordered);
    agent
        .command(AgentCommand::SendTopic(TopicSend {
            publisher: publish.clone(),
            payload: b"accepted-before-reconnect".to_vec(),
        }))
        .unwrap();
    let (accepted, _, body) = next_topic_send(&mut agent);
    let TopicBody::Message(old_message) = body else {
        panic!("message");
    };
    acknowledge_send(&mut agent, old_generation, old_publish_channel, accepted);
    drain_notifications(&mut agent);
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
            offer: offer(),
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
            response: update_response(LOCAL_PUBLISHER),
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
    assert!(
        matches!(next_effect(&mut agent), Effect::Timer(TimerEffect::Schedule { after, .. }) if after == Duration::from_secs(30))
    );
    let (operation, seed_id) = match next_effect(&mut agent) {
        Effect::DataChannel(DataChannelEffect::Send {
            operation, payload, ..
        }) => {
            let Some(v1::client_message::Payload::SubscriptionSeed(seed)) =
                pulsebeam_proto::codec::decode_client(&payload)
                    .unwrap()
                    .payload
            else {
                panic!("expected replacement subscription seed");
            };
            (operation, seed.seed_id)
        }
        effect => panic!("expected subscription seed, got {effect:?}"),
    };
    acknowledge_send(&mut agent, generation, signal, operation);
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
    assert_eq!(agent.snapshot().topics.publishers[0].data_track_id, None);
    assert_eq!(agent.snapshot().topics.publishers[0].next_sequence, 0);
    assert_eq!(agent.snapshot().topics.publishers[0].accepted_history, 0);
    assert!(matches!(
        agent.command(AgentCommand::SendTopic(TopicSend {
            publisher: publish.clone(),
            payload: b"before-binding".to_vec()
        })),
        Err(AgentError::InvalidTopic(TopicError::PublisherUnavailable(
            _
        )))
    ));
    inject_topic_catalog(&mut agent, generation, signal, &registrations, 1, "second");
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: signal,
            payload: server_payload(v1::server_message::Payload::SubscriptionSeedComplete(
                v1::SubscriptionSeedComplete {
                    seed_id,
                    catalog_revision: 1,
                },
            )),
        }))
        .unwrap();
    while let Some(effect) = agent.next_effect() {
        if matches!(effect, Effect::Timer(TimerEffect::Cancel { .. })) {
            continue;
        }
        let Effect::DataChannel(DataChannelEffect::Send {
            operation, channel, ..
        }) = effect
        else {
            panic!("probe");
        };
        acknowledge_send(&mut agent, generation, channel, operation);
    }
    drain_notifications(&mut agent);
    topic_input(
        &mut agent,
        old_generation,
        old_subscribe_channel,
        wire_message(
            &data_id(false, "chat", TopicMode::Ordered, "first"),
            0,
            b"stale-generation",
        ),
    )
    .unwrap();
    assert!(drain_notifications(&mut agent).is_empty());
    agent
        .command(AgentCommand::SendTopic(TopicSend {
            publisher: publish,
            payload: b"after-reconnect".to_vec(),
        }))
        .unwrap();
    let (_, new_publish_channel, body) = next_topic_send(&mut agent);
    assert_eq!(new_publish_channel, new_channels["v2/ordered/pub/chat"]);
    let TopicBody::Message(new_message) = body else {
        panic!("new message");
    };
    assert_ne!(new_message.data_track_id, old_message.data_track_id);
    assert_eq!(new_message.sequence, 0);
    assert_eq!(
        topic_input(
            &mut agent,
            generation,
            new_publish_channel,
            TopicBody::Nack(TopicNack {
                data_track_id: old_message.data_track_id,
                from_sequence: 0
            })
        ),
        Err(AgentError::InvalidTopic(TopicError::InvalidControl))
    );
    topic_input(
        &mut agent,
        generation,
        new_publish_channel,
        TopicBody::Nack(TopicNack {
            data_track_id: new_message.data_track_id.clone(),
            from_sequence: 0,
        }),
    )
    .unwrap();
    assert_eq!(
        next_topic_send(&mut agent).2,
        TopicBody::Message(new_message)
    );
    assert!(agent.next_effect().is_none());
}

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
    topic::{
        TopicFloor, TopicFrame, TopicFrontier, TopicMessage as WireTopicMessage, TopicNack,
        TopicProbe, topic_frame::Body as TopicBody,
    },
};

use crate::*;

mod data_send;
pub(crate) mod native_connection;
mod native_media;

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
    let mut initial = desired(1);
    initial
        .topics
        .publishers
        .push(publisher("state", TopicMode::Latest));
    let _ = take_video_intent_logs();
    let cid = channel(12);
    let (generation, operation) = begin_connect(&mut agent, initial, cid);
    activate_connection(&mut agent, generation, operation, cid);
    native_connection::observe(
        &mut agent,
        generation,
        cid,
        vec![native_connection::grant(1)],
    );
    let Effect::Rtc(RtcEffect::RetireMedia {
        operation, mids, ..
    }) = next_effect(&mut agent)
    else {
        panic!("unprepared local publication retirement")
    };
    assert_eq!(mids, ["lv0"]);
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
            generation,
            operation,
            slots: Vec::new(),
            outcome: crate::client_sdp::ApplyOutcome::NotApplied,
            preparation: Err(crate::client_sdp::Error::Observation),
        }))
        .unwrap();
    let (_, send, payload) = native_connection::prepare_data_pending(&mut agent, generation, cid);
    assert_eq!(take_video_intent_logs().len(), 1);
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::SendFailed {
            operation: send,
            generation,
            channel: cid,
            message: "full".to_string(),
        }))
        .unwrap();
    let Effect::Timer(TimerEffect::Schedule { timer, .. }) = next_effect(&mut agent) else {
        panic!("retry")
    };
    agent
        .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
        .unwrap();
    let Effect::DataChannel(DataChannelEffect::Send {
        payload: retried, ..
    }) = next_effect(&mut agent)
    else {
        panic!("serialized retry")
    };
    assert_eq!(retried, payload);
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
            kind: MediaKind::Video,
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

pub(crate) fn create_response(participant: &str, _etag: &str, uri: &str) -> HttpResponse {
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
            "profile": "pulsebeam.media.v1",
            "rtp_capabilities": {},
            "data_delivery": ["LATEST", "LOSSY", "ORDERED"],
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

#[track_caller]
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
            data_channels,
        }) => {
            assert_eq!(data_channels.len(), 1);
            assert_eq!(data_channels[0].label, "v1/sys/signaling");
            assert_eq!(data_channels[0].negotiated_id, Some(0));
            assert!(data_channels[0].ordered);
            assert_eq!(
                data_channels[0].reliability,
                DataChannelReliability::Reliable
            );
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
                serde_json::json!({"profile": "pulsebeam.media.v1", "offer": offer()}),
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

fn activate_connection(
    agent: &mut Agent,
    generation: Generation,
    operation: OperationId,
    cid: ChannelId,
) {
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
            capabilities: BootstrapCapabilities {
                rtp: Default::default(),
                data_delivery: vec![
                    pulsebeam_proto::state::DataDelivery::Latest,
                    pulsebeam_proto::state::DataDelivery::Lossy,
                    pulsebeam_proto::state::DataDelivery::Ordered
                ],
            },
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
}

fn finish_connect(
    agent: &mut Agent,
    generation: Generation,
    operation: OperationId,
    cid: ChannelId,
) -> OperationId {
    activate_connection(agent, generation, operation, cid);
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

pub(super) fn connected_agent() -> (Agent, Generation, ChannelId, OperationId) {
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
fn native_resource_rejection_is_nonfatal_without_advancing_accepted_state() {
    use pulsebeam_proto::state::{self, codec, validation};

    for valid in [true, false] {
        let (mut agent, generation, channel, accepted) =
            native_connection::connected_publisher(TopicMode::Latest);
        let before = agent.snapshot().clone();
        let desired = DesiredState {
            revision: 2,
            connected: true,
            topics: TopicRegistrations {
                publishers: vec![
                    publisher("state", TopicMode::Latest),
                    publisher("unsupported", TopicMode::Latest),
                ],
                subscribers: Vec::new(),
            },
            ..Default::default()
        };
        agent
            .command(AgentCommand::ReplaceDesired(desired.clone()))
            .unwrap();
        let rejected = native_connection::prepare_data(&mut agent, generation, channel);
        let mut record = native_connection::confirmed(
            state::resource_key::Key::Owned(rejected.reference),
            rejected.revision + u64::from(!valid),
            state::confirmed_state::Value::Absent(state::AbsentState {
                reason: state::AbsenceReason::CreateRejected as i32,
            }),
        );
        let Some(state::state_record::State::Confirmed(confirmation)) = &mut record.state else {
            panic!("confirmation")
        };
        confirmation.rejection = Some(state::Failure {
            code: state::FailureCode::Unsupported as i32,
            diagnostic: "unsupported resource settings".into(),
        });
        let event = HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            payload: codec::encode(
                &state::StateBatch {
                    records: vec![record],
                    initial_complete: false,
                },
                validation::Direction::Server,
            )
            .unwrap(),
        });
        let result = agent.handle(event.clone());
        assert_eq!(result.is_ok(), valid);
        if !valid {
            assert_eq!(
                agent.snapshot().connection,
                ConnectionState::TerminalFailure
            );
            continue;
        }
        assert_eq!(
            agent.snapshot().accepted_intent_revision,
            before.accepted_intent_revision
        );
        assert_eq!(agent.snapshot().connection, before.connection);
        assert_eq!(agent.snapshot().generation, before.generation);
        assert!(agent.snapshot().terminal_failure.is_none());
        assert!(
            drain_notifications(&mut agent).contains(&Notification::ServerError(
                "unsupported resource settings".into()
            ))
        );
        assert_eq!(
            next_effect(&mut agent),
            Effect::Rtc(RtcEffect::CloseDataChannel {
                generation,
                channel: rejected.channel,
            })
        );
        agent.handle(event).unwrap();
        assert!(
            drain_notifications(&mut agent)
                .iter()
                .all(|notification| !matches!(notification, Notification::ServerError(_)))
        );
        agent
            .command(AgentCommand::ReplaceDesired(desired))
            .unwrap();
        assert!(agent.next_effect().is_none());
        agent
            .command(AgentCommand::SendTopic(TopicSend {
                publisher: publisher("state", TopicMode::Latest),
                payload: b"still accepted".to_vec(),
            }))
            .unwrap();
        let (_, output, body) = next_topic_send(&mut agent);
        assert_eq!(output, accepted.channel);
        assert!(matches!(body, TopicBody::Message(message)
            if message.data_track_id == native_connection::SOURCE
                && message.payload == b"still accepted"));
    }
}

#[test]
fn rejected_renewal_preserves_the_committed_credential_and_expiry() {
    let (mut agent, _, _) = native_connection::connected(Default::default());
    assert_eq!(agent.snapshot().authorization_expires_at, Some(10_001));
    let command = AgentCommand::RenewAuthorization("new-secret".to_string());
    assert!(!format!("{command:?}").contains("new-secret"));
    agent.command(command).unwrap();
    let operation = match next_effect(&mut agent) {
        Effect::Http(HttpEffect::Request {
            operation, request, ..
        }) => {
            assert_eq!(request.method, HttpMethod::Patch);
            assert_eq!(request.uri, PARTICIPANT_URI);
            assert!(request.headers.iter().any(
                |header| header.name == "Authorization" && header.value == "Bearer new-secret"
            ));
            assert!(
                request
                    .headers
                    .iter()
                    .any(|header| header.name == "If-Match" && header.value == "\"auth-1\"")
            );
            assert!(!format!("{request:?}").contains("new-secret"));
            operation
        }
        effect => panic!("expected renewal PATCH, got {effect:?}"),
    };
    assert_eq!(
        agent.command(AgentCommand::RenewAuthorization("second".to_string())),
        Err(AgentError::RenewalBusy)
    );
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
    assert_eq!(agent.snapshot().authorization_expires_at, Some(10_001));
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert!(agent.snapshot().terminal_failure.is_none());
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 2,
            ..DesiredState::default()
        }))
        .unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close { .. })
    ));
    let Effect::Http(HttpEffect::Request {
        request: delete, ..
    }) = next_effect(&mut agent)
    else {
        panic!("delete request")
    };
    assert_eq!(delete.method, HttpMethod::Delete);
    assert_eq!(delete.uri, PARTICIPANT_URI);
    assert!(
        delete
            .headers
            .iter()
            .any(|header| header.name == "Authorization" && header.value == "Bearer private-token")
    );
    assert!(!format!("{delete:?}").contains("private-token"));
}

#[test]
fn accepted_renewal_commits_the_new_credential_only_on_authorization() {
    let (mut agent, generation, control) = native_connection::connected(Default::default());
    agent
        .command(AgentCommand::RenewAuthorization("new-secret".to_string()))
        .unwrap();
    let Effect::Http(HttpEffect::Request {
        operation, request, ..
    }) = next_effect(&mut agent)
    else {
        panic!("renewal PATCH")
    };
    assert_eq!(request.method, HttpMethod::Patch);
    assert_eq!(request.uri, PARTICIPANT_URI);
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: HttpResponse {
                status: 204,
                headers: vec![HttpHeader {
                    name: "ETag".into(),
                    value: "\"auth-2\"".into(),
                }],
                body: vec![],
            },
        }))
        .unwrap();
    assert_eq!(agent.snapshot().authorization_expires_at, Some(10_001));
    native_connection::observe(
        &mut agent,
        generation,
        control,
        vec![native_connection::grant(2)],
    );
    assert_eq!(agent.snapshot().authorization_expires_at, Some(10_002));
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 2,
            ..DesiredState::default()
        }))
        .unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close { .. })
    ));
    let Effect::Http(HttpEffect::Request {
        request: delete, ..
    }) = next_effect(&mut agent)
    else {
        panic!("delete request")
    };
    assert_eq!(delete.method, HttpMethod::Delete);
    assert_eq!(delete.uri, PARTICIPANT_URI);
    assert!(
        delete
            .headers
            .iter()
            .any(|header| header.name == "Authorization" && header.value == "Bearer new-secret")
    );
    assert!(!format!("{delete:?}").contains("new-secret"));
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
    state.publications[0].slot.clear();
    assert!(
        agent
            .command(AgentCommand::ReplaceDesired(state.clone()))
            .is_err()
    );
    assert!(agent.is_publishing(&video));
    state.publications[0].slot = "v0".into();
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
fn publication_labels_can_change_after_desire_is_cleared() {
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
    agent
        .command(AgentCommand::ReplaceDesired(relabelled))
        .unwrap();
    assert_eq!(agent.snapshot().desired_revision, 3);
    assert!(agent.is_publishing(&MediaSlot::LocalVideo("v0".into())));

    let mut audio = original;
    audio.revision = 4;
    audio.publications = vec![PublicationIntent {
        slot: "a0".into(),
        kind: MediaKind::Audio,
        label: "camera".into(),
        active: true,
    }];
    agent.command(AgentCommand::ReplaceDesired(audio)).unwrap();
    assert!(!agent.is_publishing(&MediaSlot::LocalVideo("v0".into())));
    assert!(agent.is_publishing(&MediaSlot::LocalAudio("a0".into())));
}

#[test]
fn construction_and_desired_state_validate_complete_external_input() {
    let mut invalid_endpoint = config();
    invalid_endpoint.endpoint = "https://".to_string();
    assert!(matches!(
        Agent::new(invalid_endpoint),
        Err(AgentError::InvalidConfiguration(ValidationError::Endpoint))
    ));

    let mut unconstrained = desired(1);
    unconstrained.publications[0].slot = "arbitrary-sender-coordinate".into();
    unconstrained.video[0].slot = u32::MAX;
    assert!(unconstrained.validate().is_ok());
    let mut same_coordinate_audio = unconstrained.publications[0].clone();
    same_coordinate_audio.kind = MediaKind::Audio;
    unconstrained.publications.push(same_coordinate_audio);
    assert!(unconstrained.validate().is_ok());

    let mut invalid_slot = desired(1);
    invalid_slot.publications[0].slot.clear();
    assert_eq!(
        invalid_slot.validate(),
        Err(ValidationError::Identifier {
            field: "publication slot"
        })
    );

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
            "SDP RTP media section is missing a slot binding"
        ))
    );
    let mut partial = resources(channel(3));
    partial.slots.pop();
    assert_eq!(
        agent.handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: offer(),
            resources: partial,
        })),
        Err(AgentError::InvalidOffer(
            "SDP RTP media section is missing a slot binding"
        ))
    );
    assert_eq!(agent.snapshot().connection, ConnectionState::CreatingOffer);
    assert!(agent.next_effect().is_none());

    assert_eq!(
        agent.handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: " ".repeat(131_073),
            resources: resources(channel(3)),
        })),
        Err(AgentError::InvalidOffer("SDP offer is empty or too large"))
    );
    assert_eq!(agent.snapshot().connection, ConnectionState::CreatingOffer);
    assert!(agent.next_effect().is_none());

    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer:
                "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:data-channel\r\n"
                    .into(),
            resources: OfferResources {
                slots: vec![],
                signaling_channel: channel(3),
                data_channels: vec![],
            },
        }))
        .unwrap();
    let Effect::Http(HttpEffect::Request { operation, .. }) = next_effect(&mut agent) else {
        panic!("data-only session request")
    };
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: create_response("p1", "unused", PARTICIPANT_URI),
        }))
        .unwrap();
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::ApplyAnswer { .. })
    ));
    agent
        .handle(HostEvent::Rtc(RtcEvent::BootstrapMediaPrepared {
            generation,
            preparation: Ok(client_sdp::Bindings::default()),
        }))
        .unwrap();
    for event in [
        HostEvent::Rtc(RtcEvent::AnswerApplied { generation }),
        HostEvent::Rtc(RtcEvent::Connected { generation }),
        HostEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            channel: channel(3),
        }),
    ] {
        agent.handle(event).unwrap();
    }
    let mut allocations = Vec::new();
    while let Some(effect) = agent.next_effect() {
        if let Effect::Rtc(RtcEffect::AllocateMedia {
            allocations: requested,
            ..
        }) = effect
        {
            allocations.extend(requested);
        }
    }
    assert_eq!(
        allocations,
        [client_sdp::Allocation {
            slot: MediaSlot::LocalVideo("v0".into()),
            format: None,
        }]
    );
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
fn unavailable_bootstrap_rtp_does_not_retry_or_reject_the_transport() {
    let mut agent = Agent::new(config()).unwrap();
    let cid = channel(9);
    let (generation, operation) = begin_connect(&mut agent, desired(1), cid);
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
        .handle(HostEvent::Rtc(RtcEvent::BootstrapMediaPrepared {
            generation,
            preparation: Err(client_sdp::Error::Observation),
        }))
        .unwrap();
    assert!(agent.next_effect().is_none());
    for event in [
        HostEvent::Rtc(RtcEvent::AnswerApplied { generation }),
        HostEvent::Rtc(RtcEvent::Connected { generation }),
        HostEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            channel: cid,
        }),
    ] {
        agent.handle(event).unwrap();
    }
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert!(agent.snapshot().terminal_failure.is_none());
    while let Some(effect) = agent.next_effect() {
        assert!(!matches!(
            effect,
            Effect::Rtc(RtcEffect::CreateOffer { .. })
                | Effect::Timer(TimerEffect::Schedule { .. })
        ));
    }
}

#[test]
fn rejected_bootstrap_is_terminal_and_late_completion_cannot_activate() {
    let mut agent = Agent::new(config()).unwrap();
    let cid = channel(9);
    let (generation, operation) = begin_connect(&mut agent, desired(1), cid);
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
        .handle(HostEvent::Rtc(RtcEvent::BootstrapRejected {
            generation,
            message: "unsupported applied bootstrap SDP".into(),
        }))
        .unwrap();
    let mut effects = Vec::new();
    while let Some(effect) = agent.next_effect() {
        effects.push(effect);
    }
    assert!(effects.contains(&Effect::Rtc(RtcEffect::Close { generation })));
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Timer(TimerEffect::Schedule { .. })))
    );
    let terminal = agent.snapshot().clone();
    for event in [
        HostEvent::Rtc(RtcEvent::AnswerApplied { generation }),
        HostEvent::Rtc(RtcEvent::Connected { generation }),
        HostEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            channel: cid,
        }),
    ] {
        agent.handle(event).unwrap();
    }
    assert_eq!(
        agent.snapshot().connection,
        ConnectionState::TerminalFailure
    );
    assert_eq!(
        agent.snapshot().terminal_failure.as_ref().unwrap().class,
        FailureClass::Protocol
    );
    assert_eq!(agent.snapshot(), &terminal);
    assert!(agent.snapshot().participant_id.is_none());
    assert!(agent.next_effect().is_none());
}

#[test]
fn connection_waits_for_every_host_boundary_and_closes_both_resources() {
    let (mut agent, generation, cid) = native_connection::joining(Default::default());
    assert_eq!(agent.snapshot().connection, ConnectionState::ApplyingAnswer);
    assert!(agent.snapshot().participant_id.is_none());
    agent
        .handle(HostEvent::Rtc(RtcEvent::AnswerApplied { generation }))
        .unwrap();
    assert_eq!(
        agent.snapshot().connection,
        ConnectionState::WaitingForTransport
    );
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            channel: cid,
        }))
        .unwrap();
    assert_eq!(
        agent.snapshot().connection,
        ConnectionState::WaitingForTransport
    );
    assert!(agent.snapshot().participant_id.is_none());
    assert!(agent.next_effect().is_none());
    agent
        .handle(HostEvent::Rtc(RtcEvent::Connected { generation }))
        .unwrap();
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert_eq!(agent.snapshot().participant_id.as_deref(), Some("p1"));
    assert_eq!(
        agent.snapshot().participant_external_id.as_deref(),
        Some("participant")
    );
    assert_eq!(agent.snapshot().room_external_id.as_deref(), Some("room"));
    assert!(agent.next_effect().is_none());

    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 2,
            ..DesiredState::default()
        }))
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
    for ending in 0..3 {
        let (mut agent, generation, control) = native_connection::connected(Default::default());
        agent
            .command(AgentCommand::RenewAuthorization("uncommitted".into()))
            .unwrap();
        let Effect::Http(HttpEffect::Request {
            operation, request, ..
        }) = next_effect(&mut agent)
        else {
            panic!("PATCH")
        };
        assert_eq!(request.method, HttpMethod::Patch);
        native_connection::observe(
            &mut agent,
            generation,
            control,
            vec![native_connection::grant(2)],
        );
        let event = match ending {
            0 => HostEvent::Rtc(RtcEvent::Disconnected { generation }),
            1 => HostEvent::Rtc(RtcEvent::Failed {
                generation,
                message: "transport failed".into(),
            }),
            _ => HostEvent::DataChannel(DataChannelEvent::Closed {
                generation,
                channel: control,
                reset_complete: true,
            }),
        };
        agent.handle(event).unwrap();
        assert_eq!(
            next_effect(&mut agent),
            Effect::Http(HttpEffect::Cancel { operation })
        );
        let Effect::Rtc(RtcEffect::CreateOffer {
            generation: replacement,
            ..
        }) = next_effect(&mut agent)
        else {
            panic!("replacement")
        };
        assert_ne!(replacement, generation);
        let notifications = drain_notifications(&mut agent);
        assert_eq!(
            notifications
                .iter()
                .any(|notification| matches!(notification, Notification::Failure(_))),
            ending == 1
        );
        let before = agent.snapshot().clone();
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Message {
                generation,
                channel: control,
                payload: vec![0xff],
            }))
            .unwrap();
        agent
            .handle(HostEvent::Http(HttpEvent::Response {
                operation,
                response: HttpResponse {
                    status: 204,
                    headers: vec![HttpHeader {
                        name: "ETag".into(),
                        value: "\"auth-2\"".into(),
                    }],
                    body: vec![],
                },
            }))
            .unwrap();
        assert_eq!(agent.snapshot(), &before);
        assert!(agent.next_notification().is_none());
        assert!(agent.next_effect().is_none());
        agent
            .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
                generation: replacement,
                offer: native_connection::OFFER.into(),
                resources: OfferResources {
                    slots: vec![],
                    signaling_channel: channel(15),
                    data_channels: vec![],
                },
            }))
            .unwrap();
        let Effect::Http(HttpEffect::Request { request, .. }) = next_effect(&mut agent) else {
            panic!("POST")
        };
        assert_eq!(request.method, HttpMethod::Post);
        assert!(
            request
                .headers
                .iter()
                .any(|header| header.name == "Authorization"
                    && header.value == "Bearer private-token")
        );
        assert!(agent.next_effect().is_none());
    }
}

#[test]
fn reconnect_posts_a_new_resource_and_swaps_only_after_the_candidate_is_ready() {
    use pulsebeam_proto::state;
    let (mut agent, old_generation, control) = native_connection::connected(Default::default());
    native_connection::observe(
        &mut agent,
        old_generation,
        control,
        vec![
            native_connection::confirmed(
                state::resource_key::Key::Participant("remote".into()),
                0,
                state::confirmed_state::Value::Participant(state::Participant {
                    participant_id: "remote".into(),
                    participant_external_id: "remote-external".into(),
                }),
            ),
            native_connection::confirmed(
                state::resource_key::Key::Track("old-video".into()),
                0,
                state::confirmed_state::Value::Track(native_media::track(
                    "old-video",
                    "remote",
                    "camera",
                    MediaKind::Video,
                )),
            ),
        ],
    );
    assert!(agent.snapshot().publications.contains_key("old-video"));
    assert!(agent.snapshot().remote_tracks.contains_key("old-video"));
    agent
        .handle(HostEvent::Rtc(RtcEvent::Disconnected {
            generation: old_generation,
        }))
        .unwrap();
    let replacement_generation = match next_effect(&mut agent) {
        Effect::Rtc(RtcEffect::CreateOffer {
            generation,
            data_channels,
            ..
        }) => {
            assert_eq!(data_channels.len(), 1);
            assert_eq!(data_channels[0].negotiated_id, Some(0));
            generation
        }
        effect => panic!("expected replacement offer, got {effect:?}"),
    };
    assert_ne!(replacement_generation, old_generation);
    assert!(agent.snapshot().publications.contains_key("old-video"));
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
            offer: native_connection::OFFER.into(),
            resources: OfferResources {
                slots: vec![],
                signaling_channel: replacement_cid,
                data_channels: vec![],
            },
        }))
        .unwrap();
    let update = match next_effect(&mut agent) {
        Effect::Http(HttpEffect::Request {
            operation,
            generation: Some(actual_generation),
            request,
        }) => {
            assert_eq!(actual_generation, replacement_generation);
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
    assert!(
        matches!(next_effect(&mut agent), Effect::Rtc(RtcEffect::ApplyAnswer { generation, .. }) if generation == replacement_generation)
    );
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
    assert!(agent.next_effect().is_none());
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Opened {
            generation: replacement_generation,
            channel: replacement_cid,
        }))
        .unwrap();
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close {
            generation: old_generation
        })
    );
    assert!(
        agent.next_effect().is_none(),
        "no declarations without prepared resources"
    );
    assert_eq!(agent.snapshot().generation, Some(replacement_generation));
    assert!(agent.snapshot().participants.is_empty());
    assert!(agent.snapshot().publications.is_empty());
    assert!(agent.snapshot().remote_tracks.is_empty());
    assert!(agent.snapshot().video_mapping.is_empty());
    assert!(agent.snapshot().audio_mapping.is_empty());
    assert_eq!(agent.snapshot().participant_id.as_deref(), Some("p1"));
    assert_eq!(
        agent.snapshot().participant_external_id.as_deref(),
        Some("participant")
    );
    assert_eq!(agent.snapshot().room_external_id.as_deref(), Some("room"));
}

#[test]
fn rejected_reconnect_is_terminal_without_a_legacy_fallback() {
    let (mut agent, old_generation, _) = native_connection::connected(Default::default());
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
            offer: native_connection::OFFER.into(),
            resources: OfferResources {
                slots: vec![],
                signaling_channel: channel(11),
                data_channels: vec![],
            },
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
            generation: replacement_generation
        })
    );
    assert_eq!(agent.snapshot().generation, Some(old_generation));
    assert_eq!(
        agent.snapshot().connection,
        ConnectionState::TerminalFailure
    );
    assert!(
        drain_notifications(&mut agent)
            .iter()
            .any(|notification| matches!(
                notification,
                Notification::Failure(Failure {
                    class: FailureClass::InvalidConfiguration,
                    ..
                })
            ))
    );
    let failure = agent.snapshot().terminal_failure.clone();
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 2,
            connected: true,
            topics: TopicRegistrations {
                publishers: vec![publisher("chat", TopicMode::Ordered)],
                ..Default::default()
            },
            ..Default::default()
        }))
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
fn native_state_received_before_activation_is_replayed_at_the_host_boundary() {
    use native_connection::{confirmed, grant};
    use pulsebeam_proto::state::{self, codec, validation};
    let (mut agent, generation, signal) = native_connection::joining(Default::default());
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
            payload: codec::encode(
                &state::StateBatch {
                    initial_complete: true,
                    records: vec![
                        grant(1),
                        confirmed(
                            state::resource_key::Key::Participant("publisher".into()),
                            0,
                            state::confirmed_state::Value::Participant(state::Participant {
                                participant_id: "publisher".into(),
                                participant_external_id: "publisher-external".into(),
                            }),
                        ),
                        confirmed(
                            state::resource_key::Key::Track("video-track".into()),
                            0,
                            state::confirmed_state::Value::Track(state::Track {
                                track_id: "video-track".into(),
                                participant_id: "publisher".into(),
                                label: "camera".into(),
                                kind: Some(state::track::Kind::Video(state::VideoTrack {
                                    format: Some(state::CodecFormat {
                                        codec: Some(state::codec_format::Codec::Vp8(
                                            state::Vp8Parameters {},
                                        )),
                                    }),
                                })),
                            }),
                        ),
                    ],
                },
                validation::Direction::Server,
            )
            .unwrap(),
        }))
        .unwrap();
    assert_eq!(agent.snapshot().connection, ConnectionState::ApplyingAnswer);
    assert!(agent.snapshot().participant_id.is_none());
    assert!(agent.snapshot().authorization_expires_at.is_none());
    assert!(agent.snapshot().publications.is_empty());
    assert!(agent.next_effect().is_none());
    agent
        .handle(HostEvent::Rtc(RtcEvent::AnswerApplied { generation }))
        .unwrap();
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert_eq!(agent.snapshot().authorization_expires_at, Some(10_001));
    assert_eq!(
        agent.snapshot().participants["publisher"].external_id,
        "publisher-external"
    );
    assert_eq!(agent.snapshot().publications["video-track"].label, "camera");
    assert_eq!(
        agent.snapshot().remote_tracks["video-track"].participant_id,
        "publisher-external"
    );
    let activated = agent.snapshot().clone();
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            channel: signal,
        }))
        .unwrap();
    assert_eq!(agent.snapshot(), &activated);
    assert!(agent.next_effect().is_none());
}

#[test]
fn pending_sender_identities_preserve_the_physical_slot_until_application_withdrawal() {
    use pulsebeam_proto::state;
    let mut initial = desired(1);
    initial.video.clear();
    initial.audio = Default::default();
    initial.audio.automatic = false;
    let mut audio = initial.publications[0].clone();
    audio.slot = "a0".into();
    audio.kind = MediaKind::Audio;
    audio.label = "voice".into();
    initial.publications.push(audio);
    let mut actual = None;
    let (mut agent, generation, channel) =
        native_media::begin(Agent::new(config()).unwrap(), initial.clone(), |bindings| {
            actual = Some(bindings.clone());
            let video = bindings
                .publications
                .iter_mut()
                .find(|binding| binding.mid == "lv0")
                .unwrap();
            video.rtcp.cname = None;
            video.encodings[0].primary = None;
            video.encodings[0].repair = None;
        });
    let available = native_media::next(&mut agent, generation, channel);
    assert_eq!(available.len(), 1);
    assert!(matches!(&available[0].spec().declaration,
        Some(state::resource_spec::Declaration::Publication(publication))
            if publication.label == "voice" && matches!(publication.kind, Some(state::publication_spec::Kind::Audio(_)))));
    let participant = state::Participant {
        participant_id: agent.snapshot().participant_id.clone().unwrap(),
        participant_external_id: agent.snapshot().participant_external_id.clone().unwrap(),
    };
    native_connection::observe(
        &mut agent,
        generation,
        channel,
        vec![available[0].installed(
            participant.clone(),
            native_media::track(
                "local-voice-source",
                &participant.participant_id,
                "voice",
                MediaKind::Audio,
            ),
        )],
    );
    assert!(
        agent.next_effect().is_none(),
        "pending facts cannot retire or reallocate the accepted physical slot"
    );
    initial.revision += 1;
    initial
        .publications
        .retain(|publication| publication.slot != "v0");
    agent
        .command(AgentCommand::ReplaceDesired(initial))
        .unwrap();
    native_media::finish_retirements(&mut agent, generation, &["lv0"], &[]);
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaObserved {
            generation,
            bindings: actual.unwrap(),
        }))
        .unwrap();
    assert!(agent.next_effect().is_none());
    assert_eq!(agent.snapshot().generation, Some(generation));
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
}

#[test]
fn late_sender_observations_complete_only_matching_pending_publications() {
    let mut initial = desired(1);
    initial.video.clear();
    initial.audio = Default::default();
    initial.audio.automatic = false;
    let mut audio = initial.publications[0].clone();
    audio.slot = "a0".into();
    audio.kind = MediaKind::Audio;
    audio.label = "voice".into();
    initial.publications.push(audio);
    let mut actual = None;
    let (mut agent, generation, channel) =
        native_media::begin(Agent::new(config()).unwrap(), initial, |bindings| {
            actual = Some(bindings.clone());
            let video = bindings
                .publications
                .iter_mut()
                .find(|binding| binding.mid == "lv0")
                .unwrap();
            video.rtcp.cname = None;
            video.encodings[0].primary = None;
            video.encodings[0].repair = None;
        });
    let available = native_media::next(&mut agent, generation, channel);
    assert_eq!(available.len(), 1);
    let participant = pulsebeam_proto::state::Participant {
        participant_id: agent.snapshot().participant_id.clone().unwrap(),
        participant_external_id: agent.snapshot().participant_external_id.clone().unwrap(),
    };
    native_connection::observe(
        &mut agent,
        generation,
        channel,
        vec![available[0].installed(
            participant.clone(),
            native_media::track(
                "local-voice",
                &participant.participant_id,
                "voice",
                MediaKind::Audio,
            ),
        )],
    );
    let actual = actual.unwrap();
    let mut ids = crate::id::IdGenerator::new();
    let mut wrong_generation = ids.generation();
    if wrong_generation == generation {
        wrong_generation = ids.generation();
    }
    let mut wrong_mid = actual.clone();
    wrong_mid
        .publications
        .iter_mut()
        .find(|binding| binding.mid == "lv0")
        .unwrap()
        .mid = "unowned".into();
    let mut pending = actual.clone();
    pending
        .publications
        .iter_mut()
        .find(|binding| binding.mid == "lv0")
        .unwrap()
        .rtcp
        .cname = None;
    for (generation, bindings) in [
        (wrong_generation, actual.clone()),
        (generation, wrong_mid),
        (generation, pending),
    ] {
        agent
            .handle(HostEvent::Rtc(RtcEvent::MediaObserved {
                generation,
                bindings,
            }))
            .unwrap();
        assert!(agent.next_effect().is_none());
    }
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaObserved {
            generation,
            bindings: actual.clone(),
        }))
        .unwrap();
    let targets = native_media::next(&mut agent, generation, channel);
    assert_eq!(targets.len(), 1);
    let Some(pulsebeam_proto::state::resource_spec::Declaration::Publication(publication)) =
        &targets[0].spec().declaration
    else {
        panic!("publication")
    };
    assert_eq!(publication.label, "camera");
    let Some(pulsebeam_proto::state::publication_spec::Kind::Video(video)) = &publication.kind
    else {
        panic!("video")
    };
    let Some(pulsebeam_proto::state::track_binding::Transport::Rtp(rtp)) = video
        .binding
        .as_ref()
        .and_then(|binding| binding.transport.as_ref())
    else {
        panic!("RTP")
    };
    let observed = actual
        .publications
        .iter()
        .find(|binding| binding.mid == "lv0")
        .unwrap();
    assert_eq!(rtp.encodings[0].ssrc, observed.encodings[0].primary);
    assert_eq!(rtp.encodings[0].rtx_ssrc, observed.encodings[0].repair);
    assert_eq!(
        rtp.rtcp.as_ref().unwrap().cname,
        observed.rtcp.cname.as_deref().unwrap()
    );
    let mut changed = actual;
    changed
        .publications
        .iter_mut()
        .find(|binding| binding.mid == "lv0")
        .unwrap()
        .encodings[0]
        .primary = Some(123456);
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaObserved {
            generation,
            bindings: changed,
        }))
        .unwrap();
    assert!(
        agent.next_effect().is_none(),
        "observations cannot reinterpret admitted source identities"
    );
}

#[test]
fn native_snapshot_discovery_and_owned_receiver_lifetimes_are_independent() {
    use native_connection::{confirmed, observe};
    use pulsebeam_proto::state;

    let (mut agent, generation, channel, targets) = native_media::connected();
    drain_notifications(&mut agent);
    let (participant, tracks) = native_media::discover_receivers(&mut agent, generation, channel);
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
    assert!(agent.snapshot().video_mapping.is_empty());
    assert!(agent.snapshot().audio_mapping.is_empty());
    assert!(agent.next_effect().is_none());
    assert_eq!(targets.len(), 2);
    let installations = targets
        .iter()
        .map(|target| target.receiver_installed(&participant, &tracks))
        .collect();
    observe(&mut agent, generation, channel, installations);
    assert_eq!(agent.snapshot().video_mapping[&4], "video-track");
    assert_eq!(agent.snapshot().audio_mapping[&2], "audio-track");
    assert!(drain_notifications(&mut agent).contains(&Notification::SnapshotChanged));
    assert!(agent.next_effect().is_none());

    observe(
        &mut agent,
        generation,
        channel,
        targets
            .iter()
            .map(|target| {
                confirmed(
                    state::resource_key::Key::Owned(target.reference),
                    target.desired.revision,
                    state::confirmed_state::Value::Absent(state::AbsentState {
                        reason: state::AbsenceReason::SourceEnded as i32,
                    }),
                )
            })
            .collect(),
    );
    assert!(agent.snapshot().video_mapping.is_empty());
    assert!(agent.snapshot().audio_mapping.is_empty());
    assert_eq!(agent.snapshot().publications.len(), 2);
    let mut withdrawal = tracks
        .iter()
        .map(|track| {
            confirmed(
                state::resource_key::Key::Track(track.track_id.clone()),
                0,
                state::confirmed_state::Value::Absent(state::AbsentState {
                    reason: state::AbsenceReason::Removed as i32,
                }),
            )
        })
        .collect::<Vec<_>>();
    withdrawal.push(confirmed(
        state::resource_key::Key::Participant(participant.participant_id),
        0,
        state::confirmed_state::Value::Absent(state::AbsentState {
            reason: state::AbsenceReason::Removed as i32,
        }),
    ));
    observe(&mut agent, generation, channel, withdrawal);
    assert!(
        agent.next_effect().is_none(),
        "source absence does not release logical receive ownership"
    );
    let mut waiting = desired(2);
    waiting.video[0].track_id = "future-video".into();
    agent
        .command(AgentCommand::ReplaceDesired(waiting))
        .unwrap();
    native_media::finish_retirements(&mut agent, generation, &["rv0"], &[]);
    assert!(
        agent.next_effect().is_none(),
        "unknown source intent waits without retaining another source's receiver"
    );
    let mut stopped = desired(3);
    stopped.video.clear();
    stopped.audio = AudioSubscription::default();
    agent
        .command(AgentCommand::ReplaceDesired(stopped))
        .unwrap();
    native_media::finish_retirements(&mut agent, generation, &["ra0"], &[]);
    assert!(agent.snapshot().participants.is_empty());
    assert!(agent.snapshot().publications.is_empty());
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert!(agent.next_effect().is_none());
}

#[test]
fn partial_native_observations_preserve_other_receivers_and_declared_desire() {
    use pulsebeam_proto::state;

    let (mut agent, generation, channel, targets) = native_media::connected();
    let (participant, tracks) = native_media::discover_receivers(&mut agent, generation, channel);
    for target in &targets {
        native_connection::observe(
            &mut agent,
            generation,
            channel,
            vec![target.receiver_installed(&participant, &tracks)],
        );
    }
    assert_eq!(agent.snapshot().video_mapping[&4], "video-track");
    assert_eq!(agent.snapshot().audio_mapping[&2], "audio-track");
    let video = targets
        .iter()
        .find(|target| {
            matches!(&target.spec().declaration,
            Some(state::resource_spec::Declaration::Subscription(subscription))
                if subscription.kind == state::TrackKind::Video as i32)
        })
        .unwrap();
    let installed = video.installed(participant, tracks[0].clone());
    let mut waiting = installed.clone();
    let Some(state::state_record::State::Confirmed(confirmation)) = &mut waiting.state else {
        panic!("confirmation")
    };
    let Some(state::confirmed_state::Value::Owned(live)) = &mut confirmation.value else {
        panic!("owned receiver")
    };
    live.execution = state::ExecutionStatus::WaitingForSource as i32;
    native_connection::observe(&mut agent, generation, channel, vec![waiting]);
    assert!(agent.snapshot().video_mapping.is_empty());
    assert_eq!(agent.snapshot().audio_mapping[&2], "audio-track");
    agent
        .command(AgentCommand::ReplaceDesired(desired(1)))
        .unwrap();
    assert!(agent.next_effect().is_none());
    native_connection::observe(&mut agent, generation, channel, vec![installed]);
    assert_eq!(agent.snapshot().video_mapping[&4], "video-track");
    assert_eq!(agent.snapshot().audio_mapping[&2], "audio-track");
    assert_eq!(agent.snapshot().publications.len(), 2);
    assert_eq!(agent.snapshot().generation, Some(generation));
    assert!(agent.next_effect().is_none());
}

#[test]
fn discovery_withdrawal_preserves_owned_receivers_until_explicit_retirement() {
    use native_connection::{confirmed, observe};
    use pulsebeam_proto::state;

    let (mut agent, generation, channel, targets) = native_media::connected();
    let (participant, tracks) = native_media::discover_receivers(&mut agent, generation, channel);
    observe(
        &mut agent,
        generation,
        channel,
        targets
            .iter()
            .map(|target| target.receiver_installed(&participant, &tracks))
            .collect(),
    );
    let mut withdrawals = tracks
        .iter()
        .map(|track| {
            confirmed(
                state::resource_key::Key::Track(track.track_id.clone()),
                0,
                state::confirmed_state::Value::Absent(state::AbsentState {
                    reason: state::AbsenceReason::Removed as i32,
                }),
            )
        })
        .collect::<Vec<_>>();
    withdrawals.push(confirmed(
        state::resource_key::Key::Participant(participant.participant_id.clone()),
        0,
        state::confirmed_state::Value::Absent(state::AbsentState {
            reason: state::AbsenceReason::Removed as i32,
        }),
    ));
    observe(&mut agent, generation, channel, withdrawals);
    assert!(agent.snapshot().participants.is_empty());
    assert!(agent.snapshot().publications.is_empty());
    assert_eq!(agent.snapshot().video_mapping[&4], "video-track");
    assert_eq!(agent.snapshot().audio_mapping[&2], "audio-track");
    assert!(agent.next_effect().is_none());
    let mut changed = desired(2);
    changed.video[0].height = 1080;
    agent
        .command(AgentCommand::ReplaceDesired(changed.clone()))
        .unwrap();
    let updated = native_media::next(&mut agent, generation, channel);
    assert_eq!(updated.len(), 1);
    let video = targets
        .iter()
        .find(|target| {
            matches!(&target.spec().declaration,
            Some(state::resource_spec::Declaration::Subscription(subscription))
                if subscription.kind == state::TrackKind::Video as i32)
        })
        .unwrap();
    assert_eq!(updated[0].reference, video.reference);
    assert_eq!(updated[0].spec().declaration, video.spec().declaration);
    assert!(
        matches!(&updated[0].spec().settings.as_ref().unwrap().receive,
        Some(state::resource_settings::Receive::Video(options)) if options.max_height == Some(1080))
    );
    observe(
        &mut agent,
        generation,
        channel,
        vec![updated[0].receiver_installed(&participant, &tracks)],
    );
    native_media::discover_receivers(&mut agent, generation, channel);
    assert!(agent.next_effect().is_none());
    assert_eq!(agent.snapshot().video_mapping[&4], "video-track");
    assert_eq!(agent.snapshot().audio_mapping[&2], "audio-track");
    changed.revision = 3;
    changed.video.clear();
    changed.audio.pinned.clear();
    changed.audio.automatic = false;
    agent
        .command(AgentCommand::ReplaceDesired(changed))
        .unwrap();
    let retired = native_media::next(&mut agent, generation, channel);
    assert_eq!(retired.len(), 2);
    for target in &retired {
        assert!(
            targets
                .iter()
                .any(|previous| previous.reference == target.reference)
        );
        assert_eq!(
            target.desired.value,
            Some(state::desired_state::Value::Absent(true))
        );
    }
    assert!(
        agent.next_effect().is_none(),
        "physical retirement waits for authoritative cutoff"
    );
    observe(
        &mut agent,
        generation,
        channel,
        retired
            .iter()
            .map(|target| {
                confirmed(
                    state::resource_key::Key::Owned(target.reference),
                    target.desired.revision,
                    state::confirmed_state::Value::Absent(state::AbsentState {
                        reason: state::AbsenceReason::Removed as i32,
                    }),
                )
            })
            .collect(),
    );
    native_media::finish_retirements(&mut agent, generation, &["ra0", "rv0"], &[]);
    assert!(agent.snapshot().video_mapping.is_empty());
    assert!(agent.snapshot().audio_mapping.is_empty());
    assert_eq!(agent.snapshot().publications.len(), 2);
    assert_eq!(agent.snapshot().generation, Some(generation));
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert!(agent.next_effect().is_none());
}

#[test]
fn native_selectors_create_a_fresh_receiver_after_source_cutoff() {
    use native_connection::{confirmed, observe};
    use pulsebeam_proto::state;
    for owned in [false, true] {
        let mut selected = desired(1);
        selected.video[0].track_id.clear();
        selected.video[0].selector = Some(TrackSelector {
            participant_external_id: "remote-user".to_string(),
            label: "camera".to_string(),
        });
        selected.audio.pinned.clear();
        selected.audio.automatic = false;
        let audio_selector = TrackSelector {
            participant_external_id: "remote-user".to_string(),
            label: "microphone".to_string(),
        };
        selected.audio.selected.push(audio_selector.clone());
        selected.audio.selector_delays.insert(
            audio_selector.clone(),
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
        let audio_view = if owned {
            selected.audio = AudioSubscription::default();
            let ResourceResult::Owned { owner } = agent
                .resource(ResourceCommand::ClaimAudioSubscription {
                    selector: audio_selector,
                })
                .unwrap()
            else {
                panic!("audio owner")
            };
            agent
                .resource(ResourceCommand::ConfigureAudio {
                    owner,
                    playout_delay: PlayoutDelay::Fixed {
                        min_ms: 0,
                        max_ms: 0,
                    },
                })
                .unwrap();
            let mut conflict = selected.clone();
            conflict.audio.automatic = true;
            assert!(matches!(
                agent.command(AgentCommand::ReplaceDesired(conflict)),
                Err(AgentError::InvalidResource(
                    TrackResourceError::AlreadyOwned
                ))
            ));
            Some(owner)
        } else {
            None
        };
        let (mut agent, generation, channel, unresolved) =
            native_media::connected_with(agent, selected);
        assert!(unresolved.is_empty());
        let participant = state::Participant {
            participant_id: "remote".into(),
            participant_external_id: "remote-user".into(),
        };
        observe(
            &mut agent,
            generation,
            channel,
            vec![confirmed(
                state::resource_key::Key::Participant(participant.participant_id.clone()),
                0,
                state::confirmed_state::Value::Participant(participant.clone()),
            )],
        );
        assert!(agent.next_effect().is_none());
        let tracks = [
            native_media::track("video-one", "remote", "camera", MediaKind::Video),
            native_media::track("audio-one", "remote", "microphone", MediaKind::Audio),
        ];
        observe(
            &mut agent,
            generation,
            channel,
            tracks
                .iter()
                .map(|track| {
                    confirmed(
                        state::resource_key::Key::Track(track.track_id.clone()),
                        0,
                        state::confirmed_state::Value::Track(track.clone()),
                    )
                })
                .collect(),
        );
        let resolved = native_media::next(&mut agent, generation, channel);
        assert_eq!(resolved.len(), 2);
        for target in &resolved {
            let Some(state::resource_spec::Declaration::Subscription(subscription)) =
                &target.spec().declaration
            else {
                panic!("resolved receiver")
            };
            match state::TrackKind::try_from(subscription.kind).unwrap() {
                state::TrackKind::Video => assert_eq!(subscription.track_id, "video-one"),
                state::TrackKind::Audio => {
                    assert_eq!(subscription.track_id, "audio-one");
                    assert!(matches!(&target.spec().settings.as_ref().unwrap().receive,
                        Some(state::resource_settings::Receive::Audio(options))
                            if options.playout_delay == Some(state::PlayoutDelay { min_ms: 0, max_ms: 0 })));
                }
                _ => panic!("media receiver"),
            }
        }
        let video = resolved
            .iter()
            .find(|target| {
                matches!(&target.spec().declaration,
            Some(state::resource_spec::Declaration::Subscription(subscription))
                if subscription.kind == state::TrackKind::Video as i32)
            })
            .unwrap();
        observe(
            &mut agent,
            generation,
            channel,
            vec![
                video.receiver_installed(&participant, &tracks),
                resolved
                    .iter()
                    .find(|target| target.reference != video.reference)
                    .unwrap()
                    .receiver_installed(&participant, &tracks),
            ],
        );
        assert_eq!(agent.snapshot().video_mapping[&4], "video-one");
        let observation = view.map(|owner| {
            let ResourceResult::MediaObservation(id) = agent
                .resource(ResourceCommand::BeginMediaObservation { owner })
                .unwrap()
            else {
                panic!("observation request")
            };
            let Effect::MediaObservation(control) = next_effect(&mut agent) else {
                panic!("observation target")
            };
            assert_eq!(control.observation, id);
            let target = control.target.as_ref().unwrap();
            assert_eq!(
                (target.generation, target.reference),
                (generation, video.reference)
            );
            assert_eq!(target.source, "video-one");
            assert_eq!(target.binding.mid, "rv0");
            assert_eq!(target.binding.media_index, 4);
            assert_eq!(
                agent
                    .resource(ResourceCommand::CompleteMediaObservation {
                        control: control.clone(),
                        succeeded: true
                    })
                    .unwrap(),
                ResourceResult::Updated
            );
            let settings = desired(1).video.remove(0);
            agent
                .resource(ResourceCommand::ObserveVideo {
                    owner,
                    observation: VideoObservation {
                        visible: true,
                        height: settings.height + 1,
                        min_height: settings.min_height,
                        min_fps: settings.min_fps,
                        priority: settings.priority,
                        playout_delay: settings.playout_delay,
                    },
                })
                .unwrap();
            let pending = native_media::next(&mut agent, generation, channel);
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].reference, video.reference);
            assert!(pending[0].desired.revision > video.desired.revision);
            assert_eq!(
                agent
                    .resource(ResourceCommand::AuthorizeMediaObservation {
                        control: control.clone()
                    })
                    .unwrap(),
                ResourceResult::Updated
            );
            assert!(
                agent.next_effect().is_none(),
                "pending settings do not replace an installed observation"
            );
            control
        });
        let audio_observation = audio_view.map(|owner| {
            let ResourceResult::MediaObservation(id) = agent
                .resource(ResourceCommand::BeginAudioSourceObservation {
                    owner,
                    source: "audio-one".into(),
                })
                .unwrap()
            else {
                panic!("audio observation")
            };
            let Effect::MediaObservation(control) = next_effect(&mut agent) else {
                panic!("audio target")
            };
            assert_eq!(control.observation, id);
            let target = control.target.as_ref().unwrap();
            assert_eq!(target.source, "audio-one");
            assert_eq!(target.generation, generation);
            assert_eq!(target.binding.slot, MediaSlot::RemoteAudio(0));
            assert_eq!(
                target.reference,
                resolved
                    .iter()
                    .find(|target| target.reference != video.reference)
                    .unwrap()
                    .reference
            );
            assert_eq!(
                agent
                    .resource(ResourceCommand::CompleteMediaObservation {
                        control: control.clone(),
                        succeeded: true
                    })
                    .unwrap(),
                ResourceResult::Updated
            );
            control
        });
        let replacement = native_media::track("video-two", "remote", "camera", MediaKind::Video);
        observe(
            &mut agent,
            generation,
            channel,
            vec![
                confirmed(
                    state::resource_key::Key::Track("video-one".into()),
                    0,
                    state::confirmed_state::Value::Absent(state::AbsentState {
                        reason: state::AbsenceReason::Removed as i32,
                    }),
                ),
                confirmed(
                    state::resource_key::Key::Track(replacement.track_id.clone()),
                    0,
                    state::confirmed_state::Value::Track(replacement.clone()),
                ),
            ],
        );
        assert_eq!(agent.snapshot().video_mapping[&4], "video-one");
        assert!(
            agent.next_effect().is_none(),
            "discovery changes do not end the installed source"
        );
        if let Some(control) = &observation {
            assert_eq!(
                agent
                    .resource(ResourceCommand::AuthorizeMediaObservation {
                        control: control.clone()
                    })
                    .unwrap(),
                ResourceResult::Updated
            );
        }
        let mut waiting = video.receiver_installed(&participant, &tracks);
        let Some(state::state_record::State::Confirmed(confirmation)) = &mut waiting.state else {
            panic!("confirmation")
        };
        let Some(state::confirmed_state::Value::Owned(live)) = &mut confirmation.value else {
            panic!("owned receiver")
        };
        live.execution = state::ExecutionStatus::WaitingForSource as i32;
        observe(&mut agent, generation, channel, vec![waiting]);
        let cutoff = native_media::next(&mut agent, generation, channel);
        assert_eq!(cutoff.len(), 1);
        assert_eq!(cutoff[0].reference, video.reference);
        assert_eq!(
            cutoff[0].desired.value,
            Some(state::desired_state::Value::Absent(true))
        );
        let fresh_observation = observation.as_ref().map(|previous| {
            let Effect::MediaObservation(closed) = next_effect(&mut agent) else { panic!("retired observation") };
            assert!(closed.closed && closed.target.is_none());
            assert_eq!(closed.observation, previous.observation);
            assert_eq!(agent.resource(ResourceCommand::CompleteMediaObservation { control: previous.clone(), succeeded: false }).unwrap(), ResourceResult::Superseded);
            agent.resource(ResourceCommand::Release { owner: previous.observation.request() }).unwrap();
            assert!(matches!(next_effect(&mut agent), Effect::MediaObservation(control) if control.closed && control.observation == previous.observation));
            let ResourceResult::MediaObservation(id) = agent.resource(ResourceCommand::BeginMediaObservation { owner: previous.observation.owner() }).unwrap() else { panic!("fresh observation") };
            assert_ne!(id, previous.observation);
            assert!(matches!(next_effect(&mut agent), Effect::MediaObservation(control) if !control.closed && control.target.is_none() && control.observation == id));
            id
        });
        observe(
            &mut agent,
            generation,
            channel,
            vec![confirmed(
                state::resource_key::Key::Owned(video.reference),
                video.desired.revision,
                state::confirmed_state::Value::Absent(state::AbsentState {
                    reason: state::AbsenceReason::SourceEnded as i32,
                }),
            )],
        );
        assert!(agent.snapshot().video_mapping.is_empty());
        native_media::finish_retirements(&mut agent, generation, &["rv0"], &[]);
        let Effect::Rtc(RtcEffect::AllocateMedia { operation, .. }) = next_effect(&mut agent)
        else {
            panic!("fresh receiver allocation")
        };
        agent
            .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
                generation,
                operation,
                slots: Vec::new(),
                outcome: client_sdp::ApplyOutcome::NotApplied,
                preparation: Err(client_sdp::Error::Observation),
            }))
            .unwrap();
        assert!(
            agent.next_effect().is_none(),
            "failed allocation neither revives the retired receiver nor spins"
        );
        let replacement = native_media::track("video-three", "remote", "camera", MediaKind::Video);
        observe(
            &mut agent,
            generation,
            channel,
            vec![
                confirmed(
                    state::resource_key::Key::Track("video-two".into()),
                    0,
                    state::confirmed_state::Value::Absent(state::AbsentState {
                        reason: state::AbsenceReason::Removed as i32,
                    }),
                ),
                confirmed(
                    state::resource_key::Key::Track(replacement.track_id.clone()),
                    0,
                    state::confirmed_state::Value::Track(replacement.clone()),
                ),
            ],
        );
        let assigned = SlotBinding {
            slot: MediaSlot::RemoteVideo(0),
            mid: "fresh-rv0".into(),
            media_index: 5,
            kind: MediaKind::Video,
            direction: MediaDirection::ReceiveOnly,
        };
        native_media::finish_allocation(&mut agent, generation, assigned.clone(), &["rv0"], &[]);
        let fresh = native_media::next(&mut agent, generation, channel);
        assert_eq!(fresh.len(), 1);
        assert!(fresh[0].reference > video.reference);
        let Some(state::resource_spec::Declaration::Subscription(subscription)) =
            &fresh[0].spec().declaration
        else {
            panic!("replacement receiver")
        };
        assert_eq!(subscription.track_id, "video-three");
        let Some(state::resource_spec::Declaration::Subscription(previous)) =
            &video.spec().declaration
        else {
            panic!("previous receiver")
        };
        let Some(state::track_binding::Transport::Rtp(previous)) = previous
            .binding
            .as_ref()
            .and_then(|binding| binding.transport.as_ref())
        else {
            panic!("previous RTP facts")
        };
        assert!(
            matches!(subscription.binding.as_ref().and_then(|binding| binding.transport.as_ref()),
            Some(state::track_binding::Transport::Rtp(rtp)) if rtp.mid == "fresh-rv0" && rtp.mid != previous.mid && rtp.encodings.iter().all(|encoding| previous.encodings.iter().all(|old| old.ssrc != encoding.ssrc)))
        );
        observe(
            &mut agent,
            generation,
            channel,
            vec![fresh[0].installed(participant.clone(), replacement)],
        );
        assert_eq!(agent.snapshot().video_mapping[&5], "video-three");
        assert!(!agent.snapshot().video_mapping.contains_key(&4));
        let fresh_control = fresh_observation.map(|id| {
            let Effect::MediaObservation(control) = next_effect(&mut agent) else {
                panic!("fresh observation target")
            };
            assert_eq!(control.observation, id);
            let target = control.target.as_ref().unwrap();
            assert_eq!(target.reference, fresh[0].reference);
            assert_eq!(target.generation, generation);
            assert_eq!(target.source, "video-three");
            assert_eq!(target.binding.mid, "fresh-rv0");
            assert_eq!(target.binding.media_index, 5);
            assert_eq!(
                agent
                    .resource(ResourceCommand::CompleteMediaObservation {
                        control: observation.as_ref().unwrap().clone(),
                        succeeded: true
                    })
                    .unwrap(),
                ResourceResult::Superseded
            );
            assert_eq!(
                agent
                    .resource(ResourceCommand::CompleteMediaObservation {
                        control: control.clone(),
                        succeeded: true
                    })
                    .unwrap(),
                ResourceResult::Updated
            );
            control
        });
        assert!(agent.next_effect().is_none());
        if let Some(owner) = view {
            agent.resource(ResourceCommand::Release { owner }).unwrap();
            assert!(
                matches!(next_effect(&mut agent), Effect::MediaObservation(control) if control.closed && control.observation == fresh_control.as_ref().unwrap().observation)
            );
            assert_eq!(
                agent
                    .resource(ResourceCommand::CompleteMediaObservation {
                        control: fresh_control.unwrap(),
                        succeeded: false
                    })
                    .unwrap(),
                ResourceResult::Superseded
            );
            let withdrawn = native_media::next(&mut agent, generation, channel);
            assert_eq!(withdrawn.len(), 1);
            assert_eq!(withdrawn[0].reference, fresh[0].reference);
            assert_eq!(
                withdrawn[0].desired.value,
                Some(state::desired_state::Value::Absent(true))
            );
        }
        let audio = resolved
            .iter()
            .find(|target| target.reference != video.reference)
            .unwrap();
        let replacement =
            native_media::track("audio-two", "remote", "microphone", MediaKind::Audio);
        observe(
            &mut agent,
            generation,
            channel,
            vec![
                confirmed(
                    state::resource_key::Key::Track("audio-one".into()),
                    0,
                    state::confirmed_state::Value::Absent(state::AbsentState {
                        reason: state::AbsenceReason::Removed as i32,
                    }),
                ),
                confirmed(
                    state::resource_key::Key::Track(replacement.track_id.clone()),
                    0,
                    state::confirmed_state::Value::Track(replacement.clone()),
                ),
            ],
        );
        assert!(
            agent.next_effect().is_none(),
            "catalog replacement alone does not retire installed audio"
        );
        let mut waiting = audio.receiver_installed(&participant, &tracks);
        let Some(state::state_record::State::Confirmed(confirmation)) = &mut waiting.state else {
            panic!("audio confirmation")
        };
        let Some(state::confirmed_state::Value::Owned(live)) = &mut confirmation.value else {
            panic!("owned audio")
        };
        live.execution = state::ExecutionStatus::WaitingForSource as i32;
        observe(&mut agent, generation, channel, vec![waiting]);
        let cutoff = native_media::next(&mut agent, generation, channel);
        assert_eq!(cutoff.len(), 1);
        assert_eq!(cutoff[0].reference, audio.reference);
        assert_eq!(
            cutoff[0].desired.value,
            Some(state::desired_state::Value::Absent(true))
        );
        let fresh_audio_request = audio_observation.as_ref().map(|old| {
            assert!(matches!(next_effect(&mut agent), Effect::MediaObservation(control) if control.closed && control.observation == old.observation));
            assert_eq!(agent.resource(ResourceCommand::CompleteMediaObservation { control: old.clone(), succeeded: true }).unwrap(), ResourceResult::Superseded);
            agent.resource(ResourceCommand::Release { owner: old.observation.request() }).unwrap();
            assert!(matches!(next_effect(&mut agent), Effect::MediaObservation(control) if control.closed && control.observation == old.observation));
            let ResourceResult::MediaObservation(id) = agent.resource(ResourceCommand::BeginMediaObservation { owner: old.observation.owner() }).unwrap() else { panic!("fresh audio request") };
            assert_ne!(id, old.observation);
            assert!(matches!(next_effect(&mut agent), Effect::MediaObservation(control) if !control.closed && control.target.is_none() && control.observation == id));
            id
        });
        observe(
            &mut agent,
            generation,
            channel,
            vec![confirmed(
                state::resource_key::Key::Owned(audio.reference),
                audio.desired.revision,
                state::confirmed_state::Value::Absent(state::AbsentState {
                    reason: state::AbsenceReason::SourceEnded as i32,
                }),
            )],
        );
        native_media::finish_retirements(
            &mut agent,
            generation,
            &["ra0"],
            core::slice::from_ref(&assigned),
        );
        native_media::finish_allocation(
            &mut agent,
            generation,
            SlotBinding {
                slot: MediaSlot::RemoteAudio(0),
                mid: "fresh-ra0".into(),
                media_index: 7,
                kind: MediaKind::Audio,
                direction: MediaDirection::ReceiveOnly,
            },
            &["ra0"],
            core::slice::from_ref(&assigned),
        );
        let fresh_audio = native_media::next(&mut agent, generation, channel);
        assert_eq!(fresh_audio.len(), 1);
        assert!(fresh_audio[0].reference > audio.reference);
        assert!(matches!(&fresh_audio[0].spec().declaration,
            Some(state::resource_spec::Declaration::Subscription(subscription)) if subscription.track_id == "audio-two"));
        observe(
            &mut agent,
            generation,
            channel,
            vec![fresh_audio[0].installed(participant, replacement)],
        );
        assert_eq!(agent.snapshot().audio_mapping[&7], "audio-two");
        if let Some(id) = fresh_audio_request {
            let Effect::MediaObservation(control) = next_effect(&mut agent) else {
                panic!("fresh audio target")
            };
            assert_eq!(control.observation, id);
            let target = control.target.as_ref().unwrap();
            assert_eq!(target.source, "audio-two");
            assert_eq!(target.reference, fresh_audio[0].reference);
            assert_eq!(target.binding.mid, "fresh-ra0");
            assert_eq!(target.binding.media_index, 7);
            assert_eq!(
                agent
                    .resource(ResourceCommand::CompleteMediaObservation {
                        control: audio_observation.unwrap(),
                        succeeded: false
                    })
                    .unwrap(),
                ResourceResult::Superseded
            );
            assert_eq!(
                agent
                    .resource(ResourceCommand::CompleteMediaObservation {
                        control: control.clone(),
                        succeeded: true
                    })
                    .unwrap(),
                ResourceResult::Updated
            );
            agent
                .resource(ResourceCommand::Release { owner: id.owner() })
                .unwrap();
            assert!(
                matches!(next_effect(&mut agent), Effect::MediaObservation(control) if control.closed && control.observation == id)
            );
            let withdrawn = native_media::next(&mut agent, generation, channel);
            assert_eq!(withdrawn.len(), 1);
            assert_eq!(withdrawn[0].reference, fresh_audio[0].reference);
        }
        assert_eq!(agent.snapshot().generation, Some(generation));
        assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
        assert!(agent.next_effect().is_none());
    }
}

#[test]
fn malformed_native_state_is_transactional_and_cancels_a_pending_renewal() {
    use pulsebeam_proto::state;
    let (mut agent, generation, control) = native_connection::connected(Default::default());
    native_connection::observe(
        &mut agent,
        generation,
        control,
        vec![
            native_connection::confirmed(
                state::resource_key::Key::Participant("remote".into()),
                0,
                state::confirmed_state::Value::Participant(state::Participant {
                    participant_id: "remote".into(),
                    participant_external_id: "stable-external".into(),
                }),
            ),
            native_connection::confirmed(
                state::resource_key::Key::Track("video-track".into()),
                0,
                state::confirmed_state::Value::Track(state::Track {
                    track_id: "video-track".into(),
                    participant_id: "remote".into(),
                    label: "camera".into(),
                    kind: Some(state::track::Kind::Video(state::VideoTrack {
                        format: Some(state::CodecFormat {
                            codec: Some(state::codec_format::Codec::Vp8(state::Vp8Parameters {})),
                        }),
                    })),
                }),
            ),
        ],
    );
    agent
        .command(AgentCommand::RenewAuthorization("new-secret".into()))
        .unwrap();
    let Effect::Http(HttpEffect::Request {
        operation, request, ..
    }) = next_effect(&mut agent)
    else {
        panic!("PATCH")
    };
    assert_eq!(request.method, HttpMethod::Patch);
    native_connection::observe(
        &mut agent,
        generation,
        control,
        vec![native_connection::grant(2)],
    );
    let before = agent.snapshot().clone();
    assert_eq!(before.participants.len(), 1);
    assert_eq!(before.publications.len(), 1);
    let malformed = state::StateBatch {
        records: ["first", "second"]
            .into_iter()
            .map(|external| {
                native_connection::confirmed(
                    state::resource_key::Key::Participant("prospective".into()),
                    0,
                    state::confirmed_state::Value::Participant(state::Participant {
                        participant_id: "prospective".into(),
                        participant_external_id: external.into(),
                    }),
                )
            })
            .collect(),
        initial_complete: false,
    }
    .encode_to_vec();
    assert!(matches!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: control,
            payload: malformed,
        })),
        Err(AgentError::InvalidSignaling(SignalingError::Malformed))
    ));
    assert_eq!(agent.snapshot().participants, before.participants);
    assert_eq!(agent.snapshot().publications, before.publications);
    assert_eq!(agent.snapshot().remote_tracks, before.remote_tracks);
    assert_eq!(agent.snapshot().video_mapping, before.video_mapping);
    assert_eq!(agent.snapshot().audio_mapping, before.audio_mapping);
    assert_eq!(
        agent.snapshot().authorization_expires_at,
        before.authorization_expires_at
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
        Some(FailureClass::Protocol)
    );
    assert_eq!(
        next_effect(&mut agent),
        Effect::Http(HttpEffect::Cancel { operation })
    );
    assert!(agent.next_effect().is_none());
    let terminal = agent.snapshot().clone();
    drain_notifications(&mut agent);
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: HttpResponse {
                status: 204,
                headers: vec![HttpHeader {
                    name: "ETag".into(),
                    value: "\"auth-2\"".into(),
                }],
                body: vec![],
            },
        }))
        .unwrap();
    native_connection::observe(
        &mut agent,
        generation,
        control,
        vec![native_connection::grant(3)],
    );
    agent
        .handle(HostEvent::Rtc(RtcEvent::Connected { generation }))
        .unwrap();
    assert_eq!(agent.snapshot(), &terminal);
    assert!(agent.next_effect().is_none());
    assert!(agent.next_notification().is_none());
    agent.command(AgentCommand::Reconnect).unwrap();
    let Effect::Rtc(RtcEffect::CreateOffer {
        generation: replacement,
        ..
    }) = next_effect(&mut agent)
    else {
        panic!("explicit recovery")
    };
    assert_ne!(replacement, generation);
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation: replacement,
            offer: native_connection::OFFER.into(),
            resources: OfferResources {
                slots: vec![],
                signaling_channel: channel(13),
                data_channels: vec![],
            },
        }))
        .unwrap();
    let Effect::Http(HttpEffect::Request { request, .. }) = next_effect(&mut agent) else {
        panic!("POST")
    };
    assert_eq!(request.method, HttpMethod::Post);
    assert!(
        request
            .headers
            .iter()
            .any(|header| header.name == "Authorization" && header.value == "Bearer private-token"),
        "late renewal cannot replace committed credentials"
    );
}

#[test]
fn cancelled_native_replacement_cleans_orphaned_http_resources_without_activating() {
    let (mut agent, old, control) = native_connection::connected(Default::default());
    agent.command(AgentCommand::Reconnect).unwrap();
    let Effect::Rtc(RtcEffect::CreateOffer {
        generation: replacement,
        ..
    }) = next_effect(&mut agent)
    else {
        panic!("replacement")
    };
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation: replacement,
            offer: native_connection::OFFER.into(),
            resources: OfferResources {
                slots: vec![],
                signaling_channel: channel(14),
                data_channels: vec![],
            },
        }))
        .unwrap();
    let Effect::Http(HttpEffect::Request {
        operation, request, ..
    }) = next_effect(&mut agent)
    else {
        panic!("POST")
    };
    assert_eq!(request.method, HttpMethod::Post);
    let pending = agent.snapshot().clone();
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation: old,
            channel: control,
            payload: vec![0xff],
        }))
        .unwrap();
    assert_eq!(
        agent.snapshot(),
        &pending,
        "fenced incumbent cannot poison a candidate"
    );
    assert!(agent.next_effect().is_none());
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 2,
            ..Default::default()
        }))
        .unwrap();
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
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close { generation: old })
    );
    let Effect::Http(HttpEffect::Request {
        operation: delete,
        request,
        ..
    }) = next_effect(&mut agent)
    else {
        panic!("incumbent delete")
    };
    assert_eq!(request.method, HttpMethod::Delete);
    assert_eq!(request.uri, PARTICIPANT_URI);
    let closing = agent.snapshot().clone();
    assert_eq!(closing.connection, ConnectionState::Closing);
    const ORPHAN_URI: &str = "https://sfu.test/native/orphan";
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: create_response("p2", "unused", ORPHAN_URI),
        }))
        .unwrap();
    let Effect::Http(HttpEffect::Request { request, .. }) = next_effect(&mut agent) else {
        panic!("orphan cleanup")
    };
    assert_eq!(request.method, HttpMethod::Delete);
    assert_eq!(request.uri, ORPHAN_URI);
    for event in [
        RtcEvent::Connected {
            generation: replacement,
        },
        RtcEvent::AnswerApplied {
            generation: replacement,
        },
        RtcEvent::Closed {
            generation: replacement,
        },
    ] {
        agent.handle(HostEvent::Rtc(event)).unwrap();
    }
    assert_eq!(agent.snapshot(), &closing);
    assert!(agent.next_effect().is_none());
    agent
        .handle(HostEvent::Rtc(RtcEvent::Closed { generation: old }))
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
    assert!(agent.snapshot().generation.is_none());
    assert!(agent.snapshot().participant_id.is_none());
}

#[test]
fn native_settings_replace_per_resource_and_omitted_declarations_retire_owned_lifetimes() {
    use native_connection::{confirmed, observe};
    use pulsebeam_proto::state;

    let (mut agent, generation, channel, targets) = native_media::connected();
    let mut per_track = desired(2);
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
    let fixed = native_media::next(&mut agent, generation, channel);
    assert_eq!(fixed.len(), 2);
    for target in &fixed {
        assert!(
            targets
                .iter()
                .any(|previous| previous.reference == target.reference
                    && previous.spec().declaration == target.spec().declaration)
        );
        match target
            .spec()
            .settings
            .as_ref()
            .unwrap()
            .receive
            .as_ref()
            .unwrap()
        {
            state::resource_settings::Receive::Video(options) => assert_eq!(
                options.playout_delay,
                Some(state::PlayoutDelay {
                    min_ms: 20,
                    max_ms: 100
                })
            ),
            state::resource_settings::Receive::Audio(options) => assert_eq!(
                options.playout_delay,
                Some(state::PlayoutDelay {
                    min_ms: 0,
                    max_ms: 0
                })
            ),
        }
    }
    agent
        .command(AgentCommand::ReplaceDesired(desired(3)))
        .unwrap();
    let adaptive = native_media::next(&mut agent, generation, channel);
    assert_eq!(adaptive.len(), 2);
    for target in &adaptive {
        assert!(
            fixed
                .iter()
                .any(|previous| previous.reference == target.reference
                    && previous.spec().declaration == target.spec().declaration)
        );
        match target
            .spec()
            .settings
            .as_ref()
            .unwrap()
            .receive
            .as_ref()
            .unwrap()
        {
            state::resource_settings::Receive::Video(options) => {
                assert!(options.playout_delay.is_none());
            }
            state::resource_settings::Receive::Audio(options) => {
                assert!(options.playout_delay.is_none());
            }
        }
    }
    let mut retracted = desired(4);
    retracted.publications.clear();
    retracted.video.clear();
    retracted.audio = AudioSubscription::default();
    agent
        .command(AgentCommand::ReplaceDesired(retracted))
        .unwrap();
    let withdrawals = native_media::next(&mut agent, generation, channel);
    assert_eq!(withdrawals.len(), 3);
    assert!(
        withdrawals
            .iter()
            .all(|target| target.desired.value == Some(state::desired_state::Value::Absent(true)))
    );
    assert!(agent.next_effect().is_none());
    observe(
        &mut agent,
        generation,
        channel,
        withdrawals
            .iter()
            .map(|target| {
                confirmed(
                    state::resource_key::Key::Owned(target.reference),
                    target.desired.revision,
                    state::confirmed_state::Value::Absent(state::AbsentState {
                        reason: state::AbsenceReason::Removed as i32,
                    }),
                )
            })
            .collect(),
    );
    native_media::finish_retirements(&mut agent, generation, &["lv0", "ra0", "rv0"], &[]);
    assert!(agent.next_effect().is_none());
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 5,
            ..Default::default()
        }))
        .unwrap();
    assert_eq!(agent.snapshot().desired_revision, 5);
}

#[test]
fn pending_control_fences_completion_before_a_replacement_lifetime() {
    use pulsebeam_proto::state::{self, codec, validation};
    let registrations = TopicRegistrations {
        publishers: vec![publisher("state", TopicMode::Latest)],
        subscribers: vec![],
    };
    let (mut agent, generation, control) = native_connection::connected(registrations.clone());
    let (first, first_send, _) =
        native_connection::prepare_data_pending(&mut agent, generation, control);
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 2,
            connected: true,
            ..Default::default()
        }))
        .unwrap();
    assert!(agent.next_effect().is_none());
    acknowledge_send(&mut agent, generation, control, first_send);
    let Effect::DataChannel(DataChannelEffect::Send {
        operation: second_send,
        payload,
        ..
    }) = next_effect(&mut agent)
    else {
        panic!("queued retirement")
    };
    let batch = codec::decode(&payload, validation::Direction::Client).unwrap();
    let [record] = batch.records.as_slice() else {
        panic!("one retirement")
    };
    assert_eq!(
        record.key.as_ref().unwrap().key,
        Some(state::resource_key::Key::Owned(first.reference))
    );
    let Some(state::state_record::State::Desired(desired)) = &record.state else {
        panic!("desired")
    };
    assert!(matches!(
        desired.value,
        Some(state::desired_state::Value::Absent(true))
    ));
    let retirement_revision = desired.revision;
    assert!(retirement_revision > first.revision);
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 3,
            connected: true,
            topics: registrations,
            ..Default::default()
        }))
        .unwrap();
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::SendFailed {
            operation: second_send,
            generation,
            channel: control,
            message: "full".into(),
        }))
        .unwrap();
    let Effect::Timer(TimerEffect::Schedule { timer, .. }) = next_effect(&mut agent) else {
        panic!("retry")
    };
    agent
        .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
        .unwrap();
    let Effect::DataChannel(DataChannelEffect::Send {
        operation: retry,
        payload: retried,
        ..
    }) = next_effect(&mut agent)
    else {
        panic!("serialized retry")
    };
    assert_eq!(retried, payload);
    acknowledge_send(&mut agent, generation, control, retry);
    assert_ne!(retirement_revision, agent.snapshot().desired_revision);
    assert!(
        agent.next_effect().is_none(),
        "fresh lifetime waits for cutoff"
    );
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 4,
            ..Default::default()
        }))
        .unwrap();
    acknowledge_send(&mut agent, generation, control, retry);
    assert_eq!(agent.snapshot().desired_revision, 4);
}

#[test]
fn failed_control_send_retries_its_frame_before_coalesced_successors() {
    use pulsebeam_proto::state::{self, codec, validation};
    let initial = publisher("state", TopicMode::Latest);
    let (mut agent, generation, control) = native_connection::connected(TopicRegistrations {
        publishers: vec![initial.clone()],
        subscribers: vec![],
    });
    let (first, operation, payload) =
        native_connection::prepare_data_pending(&mut agent, generation, control);
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::SendFailed {
            operation,
            generation,
            channel: control,
            message: "full".into(),
        }))
        .unwrap();
    let timer = match next_effect(&mut agent) {
        Effect::Timer(TimerEffect::Schedule { timer, after }) => {
            assert_eq!(after, Duration::from_millis(100));
            timer
        }
        effect => panic!("expected retry, got {effect:?}"),
    };
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 2,
            connected: true,
            topics: TopicRegistrations {
                publishers: vec![initial.clone(), publisher("superseded", TopicMode::Latest)],
                subscribers: vec![],
            },
            ..Default::default()
        }))
        .unwrap();
    let (superseded, _) = native_connection::prepare_channel(&mut agent, generation, control);
    assert!(agent.next_effect().is_none());
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 3,
            connected: true,
            topics: TopicRegistrations {
                publishers: vec![initial, publisher("final", TopicMode::Latest)],
                subscribers: vec![],
            },
            ..Default::default()
        }))
        .unwrap();
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::CloseDataChannel {
            generation,
            channel: superseded
        })
    );
    let (final_channel, _) = native_connection::prepare_channel(&mut agent, generation, control);
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Closed {
            generation,
            channel: superseded,
            reset_complete: true,
        }))
        .unwrap();
    assert!(agent.next_effect().is_none());
    agent
        .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
        .unwrap();
    let Effect::DataChannel(DataChannelEffect::Send {
        operation: retry,
        channel,
        payload: retried,
        ..
    }) = next_effect(&mut agent)
    else {
        panic!("retry")
    };
    assert_eq!(channel, control);
    assert_ne!(retry, operation);
    assert_eq!(retried, payload);
    acknowledge_send(&mut agent, generation, control, operation);
    assert!(agent.next_effect().is_none());
    acknowledge_send(&mut agent, generation, control, retry);
    let Effect::DataChannel(DataChannelEffect::Send {
        operation: successor,
        channel,
        payload,
        ..
    }) = next_effect(&mut agent)
    else {
        panic!("coalesced successor")
    };
    assert_eq!(channel, control);
    let batch = codec::decode(&payload, validation::Direction::Client).unwrap();
    let [record] = batch.records.as_slice() else {
        panic!("only final target is serialized")
    };
    let Some(state::resource_key::Key::Owned(reference)) =
        record.key.as_ref().and_then(|key| key.key.as_ref())
    else {
        panic!("owned")
    };
    let Some(state::state_record::State::Desired(desired)) = &record.state else {
        panic!("desired")
    };
    assert_eq!(*reference, first.reference + 1);
    assert_eq!(desired.revision, first.revision + 1);
    let Some(state::desired_state::Value::Present(spec)) = &desired.value else {
        panic!("present")
    };
    let Some(state::resource_spec::Declaration::Publication(publication)) = &spec.declaration
    else {
        panic!("publication")
    };
    assert_eq!(publication.label, "final");
    let Some(state::publication_spec::Kind::Data(data)) = &publication.kind else {
        panic!("data")
    };
    assert_eq!(data.delivery, state::DataDelivery::Latest as i32);
    assert_ne!(final_channel, superseded);
    acknowledge_send(&mut agent, generation, control, successor);
    assert!(agent.next_effect().is_none());
}

#[test]
fn topic_registrations_validate_and_retire_only_omitted_sources() {
    use alloc::collections::BTreeSet;
    use pulsebeam_proto::state::{self, codec, validation};
    let (mut agent, generation, control) =
        native_connection::connected(TopicRegistrations::default());
    let mut registrations = TopicRegistrations::default();
    let mut omitted = Vec::new();
    for (index, (label, mode, local, remote)) in [
        ("state", TopicMode::Latest, "local-a", "remote-a"),
        ("telemetry", TopicMode::Lossy, "local-b", "remote-b"),
        ("chat/Δ", TopicMode::Ordered, "local-c", "remote-c"),
    ]
    .into_iter()
    .enumerate()
    {
        registrations.publishers.push(publisher(label, mode));
        registrations.subscribers.push(subscriber(label, mode));
        agent
            .command(AgentCommand::ReplaceDesired(DesiredState {
                revision: 2 + u64::try_from(index).unwrap(),
                connected: true,
                topics: registrations.clone(),
                ..Default::default()
            }))
            .unwrap();
        if mode == TopicMode::Ordered {
            assert!(
                matches!(next_effect(&mut agent), Effect::Timer(TimerEffect::Schedule { after, .. }) if after == Duration::from_secs(2))
            );
        }
        let publication = native_connection::prepare_data(&mut agent, generation, control);
        assert!(
            matches!(&publication.spec.declaration, Some(state::resource_spec::Declaration::Publication(spec)) if spec.label == label)
        );
        native_connection::confirm_publication(
            &mut agent,
            generation,
            control,
            &publication,
            local,
        );
        let receiving = native_connection::prepare_subscription(
            &mut agent,
            generation,
            control,
            remote,
            &subscriber(label, mode),
        );
        for prepared in [&publication, &receiving] {
            assert!(
                prepared
                    .requested_channel
                    .negotiated_id
                    .is_some_and(|stream| stream != 0)
            );
            assert_eq!(
                prepared.requested_channel.ordered,
                mode == TopicMode::Ordered
            );
            assert_eq!(
                prepared.requested_channel.reliability,
                if mode == TopicMode::Ordered {
                    DataChannelReliability::Reliable
                } else {
                    DataChannelReliability::MaxRetransmits(0)
                }
            );
            if index < 2 {
                omitted.push((prepared.reference, prepared.channel));
            }
        }
    }
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
    let retained = DesiredState {
        revision: 5,
        connected: true,
        topics: TopicRegistrations {
            publishers: vec![publisher("chat/Δ", TopicMode::Ordered)],
            subscribers: vec![subscriber("chat/Δ", TopicMode::Ordered)],
        },
        ..Default::default()
    };
    agent
        .command(AgentCommand::ReplaceDesired(retained.clone()))
        .unwrap();
    let Effect::DataChannel(DataChannelEffect::Send {
        operation,
        channel,
        payload,
        ..
    }) = next_effect(&mut agent)
    else {
        panic!("owned retirements")
    };
    assert_eq!(channel, control);
    let batch = codec::decode(&payload, validation::Direction::Client).unwrap();
    assert_eq!(batch.records.len(), 4);
    let mut refs = BTreeSet::new();
    let confirmed = batch
        .records
        .iter()
        .map(|record| {
            let Some(state::resource_key::Key::Owned(reference)) =
                record.key.as_ref().and_then(|key| key.key.as_ref())
            else {
                panic!("owned")
            };
            refs.insert(*reference);
            let Some(state::state_record::State::Desired(desired)) = &record.state else {
                panic!("desired")
            };
            assert!(matches!(
                desired.value,
                Some(state::desired_state::Value::Absent(true))
            ));
            native_connection::confirmed(
                state::resource_key::Key::Owned(*reference),
                desired.revision,
                state::confirmed_state::Value::Absent(state::AbsentState {
                    reason: state::AbsenceReason::Removed as i32,
                }),
            )
        })
        .collect();
    assert_eq!(
        refs,
        omitted.iter().map(|(reference, _)| *reference).collect()
    );
    assert_eq!(agent.snapshot().topics.publishers.len(), 1);
    assert_eq!(agent.snapshot().topics.subscribers.len(), 1);
    acknowledge_send(&mut agent, generation, control, operation);
    assert!(
        agent.next_effect().is_none(),
        "physical channels await authoritative cutoff"
    );
    native_connection::observe(&mut agent, generation, control, confirmed);
    let closed = (0..4)
        .map(|_| {
            let Effect::Rtc(RtcEffect::CloseDataChannel {
                generation: actual,
                channel,
            }) = next_effect(&mut agent)
            else {
                panic!("scoped close")
            };
            assert_eq!(actual, generation);
            channel
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        closed,
        omitted.iter().map(|(_, channel)| *channel).collect()
    );
    for channel in closed {
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Closed {
                generation,
                channel,
                reset_complete: true,
            }))
            .unwrap();
    }
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert_eq!(agent.snapshot().generation, Some(generation));
    assert!(agent.next_effect().is_none());
    let before = agent.snapshot().clone();
    let mut invalid = retained.clone();
    invalid.revision = 6;
    invalid
        .topics
        .publishers
        .push(publisher("", TopicMode::Latest));
    assert!(matches!(
        agent.command(AgentCommand::ReplaceDesired(invalid)),
        Err(AgentError::InvalidConfiguration(ValidationError::Topic(_)))
    ));
    let mut duplicate = retained.clone();
    duplicate.revision = 6;
    duplicate
        .topics
        .subscribers
        .push(subscriber("chat/Δ", TopicMode::Ordered));
    assert!(matches!(
        agent.command(AgentCommand::ReplaceDesired(duplicate)),
        Err(AgentError::InvalidConfiguration(
            ValidationError::Duplicate { .. }
        ))
    ));
    let mut conflicting_delivery = retained;
    conflicting_delivery.revision = 6;
    conflicting_delivery
        .topics
        .publishers
        .push(publisher("chat/Δ", TopicMode::Lossy));
    assert!(matches!(
        agent.command(AgentCommand::ReplaceDesired(conflicting_delivery)),
        Err(AgentError::InvalidConfiguration(
            ValidationError::Duplicate { .. }
        ))
    ));
    assert_eq!(agent.snapshot(), &before);
    assert!(agent.next_effect().is_none());
}

#[test]
fn native_owned_source_readiness_does_not_wait_for_discovery_completion() {
    use pulsebeam_proto::state::{codec, validation};

    for discovery_complete in [false, true] {
        let subscriber = subscriber("events", TopicMode::Ordered);
        let (mut agent, generation, channel) = native_connection::joining(TopicRegistrations {
            subscribers: vec![subscriber.clone()],
            ..Default::default()
        });
        for event in [
            HostEvent::Rtc(RtcEvent::Connected { generation }),
            HostEvent::DataChannel(DataChannelEvent::Opened {
                generation,
                channel,
            }),
            HostEvent::Rtc(RtcEvent::AnswerApplied { generation }),
        ] {
            agent.handle(event).unwrap();
        }
        native_connection::observe(
            &mut agent,
            generation,
            channel,
            vec![native_connection::grant(1)],
        );
        if discovery_complete {
            agent
                .handle(HostEvent::DataChannel(DataChannelEvent::Message {
                    generation,
                    channel,
                    payload: codec::encode(
                        &pulsebeam_proto::state::StateBatch {
                            records: Vec::new(),
                            initial_complete: true,
                        },
                        validation::Direction::Server,
                    )
                    .unwrap(),
                }))
                .unwrap();
        }
        let Effect::Timer(TimerEffect::Schedule { timer, after }) = next_effect(&mut agent) else {
            panic!("source recovery timer")
        };
        assert_eq!(after, Duration::from_secs(2));
        assert!(agent.next_effect().is_none());
        let receiving = native_connection::prepare_subscription(
            &mut agent,
            generation,
            channel,
            native_connection::REMOTE_SOURCE,
            &subscriber,
        );
        agent
            .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
            .unwrap();
        let mut effects = Vec::new();
        while let Some(effect) = agent.next_effect() {
            effects.push(effect);
        }
        assert!(effects.iter().any(|effect| matches!(effect,
            Effect::DataChannel(DataChannelEffect::Send { channel, payload, .. })
                if *channel == receiving.channel
                    && matches!(TopicFrame::decode_bounded(payload).unwrap().body,
                        Some(TopicBody::Probe(probe)) if probe.data_track_id == native_connection::REMOTE_SOURCE))));
        assert!(effects.iter().all(|effect| match effect {
            Effect::Rtc(RtcEffect::CreateOffer { .. }) => false,
            Effect::Timer(TimerEffect::Schedule { after, .. }) => *after == Duration::from_secs(2),
            _ => true,
        }));
        assert_eq!(agent.snapshot().generation, Some(generation));
        assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
        assert!(agent.snapshot().terminal_failure.is_none());
        assert!(
            drain_notifications(&mut agent)
                .iter()
                .all(|notification| !matches!(notification, Notification::Failure(_)))
        );
    }
}

#[test]
fn latest_topic_send_admission_freshness_and_failures_are_explicit() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("state", TopicMode::Latest)],
        subscribers: vec![subscriber("state", TopicMode::Latest)],
    };
    let (mut agent, generation, control) = native_connection::connected(registrations);
    let prepared = native_connection::prepare_data(&mut agent, generation, control);
    native_connection::confirm_publication(
        &mut agent,
        generation,
        control,
        &prepared,
        native_connection::SOURCE,
    );
    let receiving = native_connection::prepare_subscription(
        &mut agent,
        generation,
        control,
        native_connection::REMOTE_SOURCE,
        &subscriber("state", TopicMode::Latest),
    );
    let publish_channel = prepared.channel;
    let subscribe_channel = receiving.channel;
    drain_notifications(&mut agent);
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
    assert_eq!(first_message.data_track_id, native_connection::SOURCE);
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
    let id = native_connection::REMOTE_SOURCE.to_string();
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
    let configuration = config();
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
            kind: MediaKind::Video,
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
fn owned_publication_replacement_ends_the_lifetime_but_input_loss_does_not() {
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

    let (mut agent, generation, channel, receivers) = native_media::connected_with(
        agent,
        DesiredState {
            revision: 1,
            connected: true,
            ..Default::default()
        },
    );
    assert!(receivers.is_empty());
    agent.resource(ResourceCommand::Release { owner }).unwrap();
    let retired = native_media::next(&mut agent, generation, channel);
    assert_eq!(retired.len(), 1);
    assert_eq!(
        retired[0].desired.value,
        Some(pulsebeam_proto::state::desired_state::Value::Absent(true))
    );
    assert!(agent.next_effect().is_none());

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
    assert_ne!(replacement, owner);
    assert_eq!(after_generation, None);
    agent
        .resource(ResourceCommand::UpdatePublication {
            owner: replacement,
            present: true,
        })
        .unwrap();
    assert!(
        agent.next_effect().is_none(),
        "replacement waits for the old owned cutoff"
    );
    use pulsebeam_proto::state;
    native_connection::observe(
        &mut agent,
        generation,
        channel,
        vec![native_connection::confirmed(
            state::resource_key::Key::Owned(retired[0].reference),
            retired[0].desired.revision,
            state::confirmed_state::Value::Absent(state::AbsentState {
                reason: state::AbsenceReason::Removed as i32,
            }),
        )],
    );
    let operations = native_media::finish_retirements(&mut agent, generation, &["lv0"], &[]);
    let assigned = SlotBinding {
        slot: MediaSlot::LocalVideo(reused),
        mid: "fresh-sender".into(),
        media_index: 5,
        kind: MediaKind::Video,
        direction: MediaDirection::SendOnly,
    };
    native_media::finish_allocation(&mut agent, generation, assigned, &["lv0"], &[]);
    let fresh = native_media::next(&mut agent, generation, channel);
    assert_eq!(fresh.len(), 1);
    assert!(fresh[0].reference > retired[0].reference);
    let Some(state::resource_spec::Declaration::Publication(publication)) =
        &fresh[0].spec().declaration
    else {
        panic!("screen publication")
    };
    assert_eq!(publication.label, "screen");
    let Some(state::publication_spec::Kind::Video(video)) = &publication.kind else {
        panic!("video publication")
    };
    assert!(
        matches!(video.binding.as_ref().and_then(|binding| binding.transport.as_ref()),
        Some(state::track_binding::Transport::Rtp(rtp)) if rtp.mid == "fresh-sender")
    );
    let participant = state::Participant {
        participant_id: agent.snapshot().participant_id.clone().unwrap(),
        participant_external_id: agent.snapshot().participant_external_id.clone().unwrap(),
    };
    native_connection::observe(
        &mut agent,
        generation,
        channel,
        vec![fresh[0].installed(
            participant.clone(),
            native_media::track(
                "replacement-screen-source",
                &participant.participant_id,
                "screen",
                MediaKind::Video,
            ),
        )],
    );
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
            generation,
            operation: operations[0],
            slots: Vec::new(),
            outcome: crate::client_sdp::ApplyOutcome::Applied,
            preparation: Ok(Default::default()),
        }))
        .unwrap();
    assert!(agent.next_effect().is_none());
    agent
        .resource(ResourceCommand::UpdatePublication {
            owner: replacement,
            present: false,
        })
        .unwrap();
    let disabled = native_media::next(&mut agent, generation, channel);
    assert_eq!(disabled.len(), 1);
    assert_eq!(disabled[0].reference, fresh[0].reference);
    assert_eq!(disabled[0].spec().declaration, fresh[0].spec().declaration);
    assert_eq!(
        disabled[0].spec().settings.as_ref().unwrap().enabled,
        Some(false)
    );
    native_connection::observe(
        &mut agent,
        generation,
        channel,
        vec![disabled[0].installed(
            participant.clone(),
            native_media::track(
                "replacement-screen-source",
                &participant.participant_id,
                "screen",
                MediaKind::Video,
            ),
        )],
    );
    assert!(agent.next_effect().is_none());
    agent
        .resource(ResourceCommand::UpdatePublication {
            owner: replacement,
            present: true,
        })
        .unwrap();
    let resumed = native_media::next(&mut agent, generation, channel);
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].reference, fresh[0].reference);
    assert_eq!(resumed[0].spec(), fresh[0].spec());
    native_connection::observe(
        &mut agent,
        generation,
        channel,
        vec![resumed[0].installed(
            participant.clone(),
            native_media::track(
                "replacement-screen-source",
                &participant.participant_id,
                "screen",
                MediaKind::Video,
            ),
        )],
    );
    assert!(agent.next_effect().is_none());
    agent
        .resource(ResourceCommand::Release { owner: replacement })
        .unwrap();
    let withdrawn = native_media::next(&mut agent, generation, channel);
    assert_eq!(withdrawn.len(), 1);
    assert_eq!(withdrawn[0].reference, fresh[0].reference);
    assert_eq!(
        withdrawn[0].desired.value,
        Some(state::desired_state::Value::Absent(true))
    );
    assert_eq!(agent.snapshot().generation, Some(generation));
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert!(agent.next_effect().is_none());
}

#[test]
fn owned_receive_uses_transport_deduplication_and_last_owner_registration_lifetime() {
    use pulsebeam_proto::state::{self, codec, validation};
    let registrations = TopicRegistrations {
        publishers: vec![],
        subscribers: vec![subscriber("events", TopicMode::Lossy)],
    };
    let (mut agent, generation, control) = native_connection::connected(registrations);
    let receiving = native_connection::prepare_subscription(
        &mut agent,
        generation,
        control,
        native_connection::REMOTE_SOURCE,
        &subscriber("events", TopicMode::Lossy),
    );
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
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 2,
            connected: true,
            ..Default::default()
        }))
        .unwrap();
    assert_eq!(agent.snapshot().topics.subscribers.len(), 1);
    assert!(agent.next_effect().is_none());
    drain_notifications(&mut agent);
    let remote = native_connection::REMOTE_SOURCE.to_string();
    for _ in 0..2 {
        topic_input(
            &mut agent,
            generation,
            receiving.channel,
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
    let Effect::DataChannel(DataChannelEffect::Send {
        operation,
        channel,
        payload,
        ..
    }) = next_effect(&mut agent)
    else {
        panic!("owned retirement")
    };
    assert_eq!(channel, control);
    let batch = codec::decode(&payload, validation::Direction::Client).unwrap();
    let [record] = batch.records.as_slice() else {
        panic!("one source retirement")
    };
    assert_eq!(
        record.key.as_ref().unwrap().key,
        Some(state::resource_key::Key::Owned(receiving.reference))
    );
    let Some(state::state_record::State::Desired(desired)) = &record.state else {
        panic!("desired")
    };
    assert!(matches!(
        desired.value,
        Some(state::desired_state::Value::Absent(true))
    ));
    acknowledge_send(&mut agent, generation, control, operation);
    assert!(agent.next_effect().is_none());
    native_connection::observe(
        &mut agent,
        generation,
        control,
        vec![native_connection::confirmed(
            state::resource_key::Key::Owned(receiving.reference),
            desired.revision,
            state::confirmed_state::Value::Absent(state::AbsentState {
                reason: state::AbsenceReason::Removed as i32,
            }),
        )],
    );
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::CloseDataChannel {
            generation,
            channel: receiving.channel
        })
    );
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Closed {
            generation,
            channel: receiving.channel,
            reset_complete: true,
        }))
        .unwrap();
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert_eq!(agent.snapshot().generation, Some(generation));
    assert!(agent.next_effect().is_none());
}

#[test]
fn ordered_topics_reorder_deduplicate_nack_replay_and_emit_inline_gaps() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("chat", TopicMode::Ordered)],
        subscribers: vec![subscriber("chat", TopicMode::Ordered)],
    };
    let (mut agent, generation, control) = native_connection::connected(registrations);
    let prepared = native_connection::prepare_data(&mut agent, generation, control);
    native_connection::confirm_publication(
        &mut agent,
        generation,
        control,
        &prepared,
        native_connection::SOURCE,
    );
    let receiving = native_connection::prepare_subscription(
        &mut agent,
        generation,
        control,
        native_connection::REMOTE_SOURCE,
        &subscriber("chat", TopicMode::Ordered),
    );
    let publish_channel = prepared.channel;
    let subscribe_channel = receiving.channel;
    let local = native_connection::SOURCE.to_string();
    let remote = native_connection::REMOTE_SOURCE.to_string();
    drain_notifications(&mut agent);
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
    let (mut agent, generation, control) = native_connection::connected(registrations);
    let prepared = native_connection::prepare_data(&mut agent, generation, control);
    native_connection::confirm_publication(
        &mut agent,
        generation,
        control,
        &prepared,
        native_connection::SOURCE,
    );
    let channel = prepared.channel;
    let id = native_connection::SOURCE.to_string();
    drain_notifications(&mut agent);
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
    let (mut agent, generation, control) = native_connection::connected(registrations);
    let receiving = native_connection::prepare_subscription(
        &mut agent,
        generation,
        control,
        native_connection::REMOTE_SOURCE,
        &subscriber("events", TopicMode::Ordered),
    );
    let channel = receiving.channel;
    let id = native_connection::REMOTE_SOURCE.to_string();
    drain_notifications(&mut agent);
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
    let (mut agent, generation, control) = native_connection::connected(registrations);
    let prepared = native_connection::prepare_data(&mut agent, generation, control);
    native_connection::confirm_publication(
        &mut agent,
        generation,
        control,
        &prepared,
        native_connection::SOURCE,
    );
    let channel = prepared.channel;
    let id = native_connection::SOURCE.to_string();
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
    use pulsebeam_proto::state::{self, codec, validation};
    for mode in [TopicMode::Lossy, TopicMode::Ordered] {
        for cutoff_first in [false, true] {
            let (mut agent, generation, control, old) =
                native_connection::connected_publisher(mode);
            let channel = old.channel;
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
                    payload: b"overflow".to_vec(),
                })),
                Err(AgentError::InvalidTopic(TopicError::SendQueueFull(_)))
            ));
            let (late, _, _) = next_topic_send(&mut agent);
            if cutoff_first {
                native_connection::observe(
                    &mut agent,
                    generation,
                    control,
                    vec![native_connection::confirmed(
                        state::resource_key::Key::Owned(old.reference),
                        old.revision,
                        state::confirmed_state::Value::Absent(state::AbsentState {
                            reason: state::AbsenceReason::SourceEnded as i32,
                        }),
                    )],
                );
                assert_eq!(
                    next_effect(&mut agent),
                    Effect::Rtc(RtcEffect::CloseDataChannel {
                        generation,
                        channel
                    })
                );
                assert!(
                    agent.next_effect().is_none(),
                    "cutoff cannot substitute for actual closure"
                );
            }
            agent
                .handle(HostEvent::DataChannel(DataChannelEvent::Closed {
                    generation,
                    channel,
                    reset_complete: true,
                }))
                .unwrap();
            assert_eq!(
                agent.snapshot().topics.dropped_sends,
                TOPIC_SEND_QUEUE_CAPACITY as u64 + 1
            );
            assert_eq!(agent.snapshot().topics.publishers[0].queued_messages, 0);
            assert_eq!(agent.snapshot().topics.publishers[0].accepted_history, 0);
            assert_eq!(agent.snapshot().topics.publishers[0].data_track_id, None);
            assert_eq!(
                agent.snapshot().topics.channel_failures,
                u64::from(!cutoff_first)
            );
            assert_eq!(agent.snapshot().generation, Some(generation));
            assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
            if !cutoff_first {
                let Effect::DataChannel(DataChannelEffect::Send {
                    operation,
                    channel: output,
                    payload,
                    ..
                }) = next_effect(&mut agent)
                else {
                    panic!("retirement frame")
                };
                assert_eq!(output, control);
                let batch = codec::decode(&payload, validation::Direction::Client).unwrap();
                let [record] = batch.records.as_slice() else {
                    panic!("one retirement")
                };
                assert_eq!(
                    record.key.as_ref().unwrap().key,
                    Some(state::resource_key::Key::Owned(old.reference))
                );
                let Some(state::state_record::State::Desired(desired)) = &record.state else {
                    panic!("desired retirement")
                };
                assert!(matches!(
                    desired.value,
                    Some(state::desired_state::Value::Absent(true))
                ));
                assert!(desired.revision > old.revision);
                acknowledge_send(&mut agent, generation, control, operation);
                assert!(
                    agent.next_effect().is_none(),
                    "closure cannot substitute for authoritative cutoff"
                );
                native_connection::observe(
                    &mut agent,
                    generation,
                    control,
                    vec![native_connection::confirmed(
                        state::resource_key::Key::Owned(old.reference),
                        desired.revision,
                        state::confirmed_state::Value::Absent(state::AbsentState {
                            reason: state::AbsenceReason::Removed as i32,
                        }),
                    )],
                );
            }
            let fresh = native_connection::prepare_data(&mut agent, generation, control);
            assert_ne!(fresh.channel, channel);
            assert!(fresh.reference > old.reference);
            let sctp = |spec: &state::ResourceSpec| {
                let Some(state::resource_spec::Declaration::Publication(publication)) =
                    &spec.declaration
                else {
                    panic!("publication")
                };
                let Some(state::publication_spec::Kind::Data(data)) = &publication.kind else {
                    panic!("data")
                };
                let Some(state::track_binding::Transport::Sctp(binding)) = data
                    .binding
                    .as_ref()
                    .and_then(|binding| binding.transport.as_ref())
                else {
                    panic!("SCTP")
                };
                binding.stream_id
            };
            assert_eq!(sctp(&fresh.spec), sctp(&old.spec));
            native_connection::confirm_publication(
                &mut agent,
                generation,
                control,
                &fresh,
                "opaque/recovered/source",
            );
            assert!(
                agent.next_effect().is_none(),
                "old queued bytes must not be replayed"
            );
            agent
                .command(AgentCommand::SendTopic(TopicSend {
                    publisher: publisher("state", mode),
                    payload: b"fresh".to_vec(),
                }))
                .unwrap();
            let (operation, output, body) = next_topic_send(&mut agent);
            assert_eq!(output, fresh.channel);
            assert!(
                matches!(body, TopicBody::Message(message) if message.data_track_id == "opaque/recovered/source" && message.sequence == 0 && message.payload == b"fresh")
            );
            let before = agent.snapshot().clone();
            acknowledge_send(&mut agent, generation, channel, late);
            assert_eq!(agent.snapshot(), &before);
            assert!(agent.next_effect().is_none());
            acknowledge_send(&mut agent, generation, fresh.channel, operation);
            assert!(!agent.snapshot().topics.publishers[0].send_pending);
            assert!(agent.next_effect().is_none());
            native_connection::observe(
                &mut agent,
                generation,
                control,
                vec![native_connection::confirmed(
                    state::resource_key::Key::Owned(fresh.reference),
                    fresh.revision,
                    state::confirmed_state::Value::Absent(state::AbsentState {
                        reason: state::AbsenceReason::PermissionRevoked as i32,
                    }),
                )],
            );
            assert_eq!(
                next_effect(&mut agent),
                Effect::Rtc(RtcEffect::CloseDataChannel {
                    generation,
                    channel: fresh.channel
                })
            );
            agent
                .handle(HostEvent::DataChannel(DataChannelEvent::Closed {
                    generation,
                    channel: fresh.channel,
                    reset_complete: true,
                }))
                .unwrap();
            assert!(
                agent.next_effect().is_none(),
                "permission loss cannot implicitly reconstruct"
            );
            assert_eq!(agent.snapshot().generation, Some(generation));
            assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
        }
    }
}

#[test]
fn ended_exact_source_stays_terminal_without_poisoning_a_shared_subscriber() {
    use pulsebeam_proto::state;
    let (mut agent, generation, control) = native_connection::connected(TopicRegistrations {
        publishers: vec![],
        subscribers: vec![subscriber("state", TopicMode::Lossy)],
    });
    let participant = state::Participant {
        participant_id: "remote".into(),
        participant_external_id: "remote-app-user".into(),
    };
    let mut prepared = Vec::new();
    for source in ["left", "right"] {
        let track = state::Track {
            track_id: source.into(),
            participant_id: participant.participant_id.clone(),
            label: "state".into(),
            kind: Some(state::track::Kind::Data(state::DataTrack {
                delivery: state::DataDelivery::Lossy as i32,
            })),
        };
        native_connection::observe(
            &mut agent,
            generation,
            control,
            vec![
                native_connection::confirmed(
                    state::resource_key::Key::Participant(participant.participant_id.clone()),
                    0,
                    state::confirmed_state::Value::Participant(participant.clone()),
                ),
                native_connection::confirmed(
                    state::resource_key::Key::Track(source.into()),
                    0,
                    state::confirmed_state::Value::Track(track.clone()),
                ),
            ],
        );
        let resource = native_connection::prepare_data(&mut agent, generation, control);
        native_connection::observe(
            &mut agent,
            generation,
            control,
            vec![native_connection::confirmed(
                state::resource_key::Key::Owned(resource.reference),
                resource.revision,
                state::confirmed_state::Value::Owned(alloc::boxed::Box::new(state::LiveResource {
                    spec: Some(resource.spec.clone()),
                    participant: Some(participant.clone()),
                    track: Some(track),
                    execution: state::ExecutionStatus::Installed as i32,
                    execution_failure: None,
                    activity: None,
                })),
            )],
        );
        assert!(agent.next_effect().is_none());
        prepared.push(resource);
    }
    let [left, right] = prepared.as_slice() else {
        panic!("two exact sources")
    };
    agent
        .command(AgentCommand::RenewAuthorization(
            "surviving-credential".into(),
        ))
        .unwrap();
    let Effect::Http(HttpEffect::Request {
        operation: renewal,
        request,
        ..
    }) = next_effect(&mut agent)
    else {
        panic!("PATCH")
    };
    assert_eq!(request.method, HttpMethod::Patch);
    drain_notifications(&mut agent);
    topic_input(
        &mut agent,
        generation,
        right.channel,
        wire_message("right", 7, b"before"),
    )
    .unwrap();
    assert!(matches!(
        drain_notifications(&mut agent).as_slice(),
        [Notification::Topic(TopicNotification::Message(
            TopicMessage { sequence: 7, .. }
        ))]
    ));
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Closed {
            generation,
            channel: left.channel,
            reset_complete: true,
        }))
        .unwrap();
    let Effect::DataChannel(DataChannelEffect::Send {
        operation,
        channel,
        payload,
        ..
    }) = next_effect(&mut agent)
    else {
        panic!("scoped retirement")
    };
    assert_eq!(channel, control);
    let batch = state::codec::decode(&payload, state::validation::Direction::Client).unwrap();
    let [record] = batch.records.as_slice() else {
        panic!("one retirement")
    };
    assert_eq!(
        record.key.as_ref().unwrap().key,
        Some(state::resource_key::Key::Owned(left.reference))
    );
    let Some(state::state_record::State::Desired(desired)) = &record.state else {
        panic!("desired")
    };
    assert!(matches!(
        desired.value,
        Some(state::desired_state::Value::Absent(true))
    ));
    acknowledge_send(&mut agent, generation, control, operation);
    assert!(agent.next_effect().is_none());
    native_connection::observe(
        &mut agent,
        generation,
        control,
        vec![native_connection::confirmed(
            state::resource_key::Key::Owned(left.reference),
            desired.revision,
            state::confirmed_state::Value::Absent(state::AbsentState {
                reason: state::AbsenceReason::SourceEnded as i32,
            }),
        )],
    );
    assert!(
        agent.next_effect().is_none(),
        "an ended exact source cannot reconstruct"
    );
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert_eq!(agent.snapshot().generation, Some(generation));
    assert_eq!(agent.snapshot().topics.subscribers[0].publishers, 1);
    assert!(agent.snapshot().terminal_failure.is_none());
    drain_notifications(&mut agent);
    topic_input(
        &mut agent,
        generation,
        left.channel,
        wire_message("left", 0, b"late"),
    )
    .unwrap();
    topic_input(
        &mut agent,
        generation,
        right.channel,
        wire_message("right", 7, b"duplicate"),
    )
    .unwrap();
    assert!(drain_notifications(&mut agent).is_empty());
    topic_input(
        &mut agent,
        generation,
        right.channel,
        wire_message("right", 8, b"after"),
    )
    .unwrap();
    assert!(
        matches!(drain_notifications(&mut agent).as_slice(), [Notification::Topic(TopicNotification::Message(TopicMessage { data_track_id, sequence: 8, payload, .. }))] if data_track_id == "right" && payload == b"after")
    );
    assert!(agent.next_effect().is_none());
    native_connection::observe(
        &mut agent,
        generation,
        control,
        vec![native_connection::grant(2)],
    );
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation: renewal,
            response: HttpResponse {
                status: 204,
                headers: vec![HttpHeader {
                    name: "ETag".into(),
                    value: "\"auth-2\"".into(),
                }],
                body: vec![],
            },
        }))
        .unwrap();
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 2,
            ..Default::default()
        }))
        .unwrap();
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close { generation })
    );
    let Effect::Http(HttpEffect::Request { request, .. }) = next_effect(&mut agent) else {
        panic!("DELETE")
    };
    assert_eq!(request.method, HttpMethod::Delete);
    assert!(
        request
            .headers
            .iter()
            .any(|header| header.name == "Authorization"
                && header.value == "Bearer surviving-credential"),
        "payload closure must not cancel the root renewal"
    );
}

#[test]
fn explicit_disconnect_cancels_pending_topic_sends_and_releases_binding() {
    let registrations = TopicRegistrations {
        publishers: vec![publisher("chat", TopicMode::Ordered)],
        subscribers: vec![],
    };
    let (mut agent, generation, control) = native_connection::connected(registrations.clone());
    let prepared = native_connection::prepare_data(&mut agent, generation, control);
    native_connection::confirm_publication(
        &mut agent,
        generation,
        control,
        &prepared,
        native_connection::SOURCE,
    );
    for _ in 0..2 {
        agent
            .command(AgentCommand::SendTopic(TopicSend {
                publisher: publisher("chat", TopicMode::Ordered),
                payload: b"queued".to_vec(),
            }))
            .unwrap();
    }
    next_topic_send(&mut agent);
    let disconnected = DesiredState {
        revision: 2,
        topics: registrations,
        ..Default::default()
    };
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
        publishers: vec![publisher("state", TopicMode::Latest)],
        subscribers: vec![subscriber("state", TopicMode::Latest)],
    };
    let (mut agent, generation, control) = native_connection::connected(registrations);
    let latest = native_connection::prepare_data(&mut agent, generation, control);
    native_connection::confirm_publication(
        &mut agent,
        generation,
        control,
        &latest,
        native_connection::SOURCE,
    );
    let receiving = native_connection::prepare_subscription(
        &mut agent,
        generation,
        control,
        native_connection::REMOTE_SOURCE,
        &subscriber("state", TopicMode::Latest),
    );
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 2,
            connected: true,
            topics: TopicRegistrations {
                publishers: vec![
                    publisher("state", TopicMode::Latest),
                    publisher("chat", TopicMode::Ordered),
                ],
                subscribers: vec![subscriber("state", TopicMode::Latest)],
            },
            ..Default::default()
        }))
        .unwrap();
    let ordered = native_connection::prepare_data(&mut agent, generation, control);
    native_connection::confirm_publication(
        &mut agent,
        generation,
        control,
        &ordered,
        "opaque/server/chat/source",
    );
    drain_notifications(&mut agent);
    let before = agent.snapshot().clone();
    assert_eq!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: ordered.channel,
            payload: b"not-protobuf".to_vec(),
        })),
        Err(AgentError::InvalidTopic(TopicError::MalformedMessage))
    );
    assert_eq!(
        topic_input(
            &mut agent,
            generation,
            latest.channel,
            TopicBody::Nack(TopicNack {
                data_track_id: native_connection::SOURCE.into(),
                from_sequence: 0,
            })
        ),
        Err(AgentError::InvalidTopic(TopicError::InvalidControl))
    );
    assert_eq!(
        topic_input(
            &mut agent,
            generation,
            receiving.channel,
            wire_message(native_connection::SOURCE, 0, b"wrong source")
        ),
        Err(AgentError::InvalidTopic(TopicError::InvalidControl))
    );
    assert_eq!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel: channel(99),
            payload: vec![],
        })),
        Ok(()),
        "messages without a live source binding are inert, including late channel callbacks"
    );
    assert_eq!(
        agent.handle(HostEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            channel: channel(99)
        })),
        Ok(()),
        "open callbacks without a live channel lease are inert"
    );
    assert_eq!(
        topic_input(
            &mut agent,
            generation,
            receiving.channel,
            wire_message(
                native_connection::REMOTE_SOURCE,
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
    let (mut agent, generation, control) = native_connection::connected(registrations);
    let prepared = native_connection::prepare_data(&mut agent, generation, control);
    native_connection::confirm_publication(
        &mut agent,
        generation,
        control,
        &prepared,
        native_connection::SOURCE,
    );
    let receiving = native_connection::prepare_subscription(
        &mut agent,
        generation,
        control,
        native_connection::REMOTE_SOURCE,
        &subscriber("events", TopicMode::Lossy),
    );
    drain_notifications(&mut agent);
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
                native_connection::SOURCE,
                sequence,
                &[u8::try_from(sequence).unwrap()]
            )
        );
        acknowledge_send(&mut agent, generation, channel, operation);
    }
    assert_eq!(agent.snapshot().topics.dropped_sends, 0);
    drain_notifications(&mut agent);
    let id = native_connection::REMOTE_SOURCE.to_string();
    for sequence in [2, 2, 0, 1, 0] {
        topic_input(
            &mut agent,
            generation,
            receiving.channel,
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
fn discovery_withdrawal_does_not_rebind_and_retired_sources_do_not_revive() {
    use pulsebeam_proto::state;
    let (mut agent, generation, control) = native_connection::connected(TopicRegistrations {
        publishers: vec![publisher("state", TopicMode::Latest)],
        subscribers: vec![subscriber("state", TopicMode::Latest)],
    });
    let old_publication = native_connection::prepare_data(&mut agent, generation, control);
    native_connection::confirm_publication(
        &mut agent,
        generation,
        control,
        &old_publication,
        native_connection::SOURCE,
    );
    let old_subscription = native_connection::prepare_subscription(
        &mut agent,
        generation,
        control,
        native_connection::REMOTE_SOURCE,
        &subscriber("state", TopicMode::Latest),
    );
    const NEW_REMOTE: &str = "opaque/remote/replacement";
    const NEW_LOCAL: &str = "opaque/local/replacement";
    drain_notifications(&mut agent);
    assert_eq!(
        topic_input(
            &mut agent,
            generation,
            old_subscription.channel,
            wire_message(NEW_REMOTE, 0, b"unresolved")
        ),
        Err(AgentError::InvalidTopic(TopicError::InvalidControl))
    );
    assert!(drain_notifications(&mut agent).is_empty());
    native_connection::observe(
        &mut agent,
        generation,
        control,
        vec![native_connection::confirmed(
            state::resource_key::Key::Track(native_connection::REMOTE_SOURCE.into()),
            0,
            state::confirmed_state::Value::Absent(state::AbsentState {
                reason: state::AbsenceReason::Removed as i32,
            }),
        )],
    );
    assert!(agent.next_effect().is_none());
    drain_notifications(&mut agent);
    topic_input(
        &mut agent,
        generation,
        old_subscription.channel,
        wire_message(native_connection::REMOTE_SOURCE, 5, b"still-owned"),
    )
    .unwrap();
    assert!(
        matches!(drain_notifications(&mut agent).as_slice(), [Notification::Topic(TopicNotification::Message(TopicMessage { data_track_id, sequence: 5, .. }))] if data_track_id == native_connection::REMOTE_SOURCE)
    );
    agent
        .command(AgentCommand::SendTopic(TopicSend {
            publisher: publisher("state", TopicMode::Latest),
            payload: b"old-pending".to_vec(),
        }))
        .unwrap();
    let (old_operation, old_channel, _) = next_topic_send(&mut agent);
    assert_eq!(old_channel, old_publication.channel);
    native_connection::observe(
        &mut agent,
        generation,
        control,
        vec![
            native_connection::confirmed(
                state::resource_key::Key::Owned(old_publication.reference),
                old_publication.revision,
                state::confirmed_state::Value::Absent(state::AbsentState {
                    reason: state::AbsenceReason::SourceEnded as i32,
                }),
            ),
            native_connection::confirmed(
                state::resource_key::Key::Owned(old_subscription.reference),
                old_subscription.revision,
                state::confirmed_state::Value::Absent(state::AbsentState {
                    reason: state::AbsenceReason::SourceEnded as i32,
                }),
            ),
        ],
    );
    assert_eq!(agent.snapshot().topics.publishers[0].next_sequence, 0);
    assert_eq!(agent.snapshot().topics.publishers[0].accepted_history, 0);
    assert!(!agent.snapshot().topics.publishers[0].send_pending);
    for channel in [old_publication.channel, old_subscription.channel] {
        assert_eq!(
            next_effect(&mut agent),
            Effect::Rtc(RtcEffect::CloseDataChannel {
                generation,
                channel
            })
        );
    }
    assert!(
        agent.next_effect().is_none(),
        "cutoff does not substitute for closure"
    );
    for channel in [old_publication.channel, old_subscription.channel] {
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Closed {
                generation,
                channel,
                reset_complete: true,
            }))
            .unwrap();
    }
    let publication = native_connection::prepare_data(&mut agent, generation, control);
    assert_ne!(publication.channel, old_publication.channel);
    assert!(publication.reference > old_publication.reference);
    native_connection::confirm_publication(
        &mut agent,
        generation,
        control,
        &publication,
        NEW_LOCAL,
    );
    let receiving = native_connection::prepare_subscription(
        &mut agent,
        generation,
        control,
        NEW_REMOTE,
        &subscriber("state", TopicMode::Latest),
    );
    assert_ne!(receiving.channel, old_subscription.channel);
    assert!(receiving.reference > old_subscription.reference);
    assert!(
        agent.next_effect().is_none(),
        "old pending bytes cannot replay"
    );
    drain_notifications(&mut agent);
    topic_input(
        &mut agent,
        generation,
        old_subscription.channel,
        wire_message(native_connection::REMOTE_SOURCE, 999, b"retired"),
    )
    .unwrap();
    topic_input(
        &mut agent,
        generation,
        receiving.channel,
        wire_message(NEW_REMOTE, 0, b"new"),
    )
    .unwrap();
    assert!(
        matches!(drain_notifications(&mut agent).as_slice(), [Notification::Topic(TopicNotification::Message(TopicMessage { participant_id, data_track_id, sequence: 0, payload, .. }))] if participant_id == "remote-app-user" && data_track_id == NEW_REMOTE && payload == b"new")
    );
    agent
        .command(AgentCommand::SendTopic(TopicSend {
            publisher: publisher("state", TopicMode::Latest),
            payload: b"new-local".to_vec(),
        }))
        .unwrap();
    let (operation, channel, body) = next_topic_send(&mut agent);
    assert_eq!(channel, publication.channel);
    assert!(
        matches!(body, TopicBody::Message(message) if message.data_track_id == NEW_LOCAL && message.sequence == 0)
    );
    let before = agent.snapshot().clone();
    acknowledge_send(&mut agent, generation, old_channel, old_operation);
    assert_eq!(agent.snapshot(), &before);
    assert!(agent.next_effect().is_none());
    acknowledge_send(&mut agent, generation, channel, operation);
}

#[test]
fn reconnect_starts_fresh_publications_without_old_history_or_generation_output() {
    use pulsebeam_proto::state::{self, codec, validation};
    let registrations = TopicRegistrations {
        publishers: vec![publisher("chat", TopicMode::Ordered)],
        subscribers: vec![subscriber("chat", TopicMode::Ordered)],
    };
    let (mut agent, old_generation, old_control) = native_connection::connected(registrations);
    let old_publication = native_connection::prepare_data(&mut agent, old_generation, old_control);
    native_connection::confirm_publication(
        &mut agent,
        old_generation,
        old_control,
        &old_publication,
        native_connection::SOURCE,
    );
    let old_subscription = native_connection::prepare_subscription(
        &mut agent,
        old_generation,
        old_control,
        native_connection::REMOTE_SOURCE,
        &subscriber("chat", TopicMode::Ordered),
    );
    let publish = publisher("chat", TopicMode::Ordered);
    agent
        .command(AgentCommand::SendTopic(TopicSend {
            publisher: publish.clone(),
            payload: b"accepted-before-reconnect".to_vec(),
        }))
        .unwrap();
    let (accepted, _, body) = next_topic_send(&mut agent);
    let TopicBody::Message(old_message) = body else {
        panic!("message")
    };
    acknowledge_send(
        &mut agent,
        old_generation,
        old_publication.channel,
        accepted,
    );
    drain_notifications(&mut agent);
    agent
        .handle(HostEvent::Rtc(RtcEvent::Disconnected {
            generation: old_generation,
        }))
        .unwrap();
    let Effect::Rtc(RtcEffect::CreateOffer {
        generation,
        data_channels,
        ..
    }) = next_effect(&mut agent)
    else {
        panic!("replacement offer")
    };
    assert_ne!(generation, old_generation);
    assert_eq!(data_channels.len(), 1);
    assert_eq!(data_channels[0].negotiated_id, Some(0));
    let signal = channel(50);
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: native_connection::OFFER.into(),
            resources: OfferResources {
                slots: Vec::new(),
                signaling_channel: signal,
                data_channels: Vec::new(),
            },
        }))
        .unwrap();
    let Effect::Http(HttpEffect::Request { operation, .. }) = next_effect(&mut agent) else {
        panic!("replacement POST")
    };
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: create_response("p2", "unused", "https://sfu.test/sessions/p2"),
        }))
        .unwrap();
    assert!(
        matches!(next_effect(&mut agent), Effect::Rtc(RtcEffect::ApplyAnswer { generation: current, .. }) if current == generation)
    );
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
            payload: codec::encode(
                &state::StateBatch {
                    records: vec![native_connection::grant(1)],
                    initial_complete: true,
                },
                validation::Direction::Server,
            )
            .unwrap(),
        }))
        .unwrap();
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close {
            generation: old_generation
        })
    );

    assert!(agent.next_effect().is_none());
    assert_eq!(agent.snapshot().participant_id.as_deref(), Some("p2"));
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
    let publication = native_connection::prepare_data(&mut agent, generation, signal);
    native_connection::confirm_publication(
        &mut agent,
        generation,
        signal,
        &publication,
        "opaque/local/root-two",
    );
    let receiving = native_connection::prepare_subscription(
        &mut agent,
        generation,
        signal,
        "opaque/remote/root-two",
        &subscriber("chat", TopicMode::Ordered),
    );
    drain_notifications(&mut agent);
    topic_input(
        &mut agent,
        old_generation,
        old_subscription.channel,
        wire_message(native_connection::REMOTE_SOURCE, 0, b"stale-generation"),
    )
    .unwrap();
    acknowledge_send(
        &mut agent,
        old_generation,
        old_publication.channel,
        accepted,
    );
    assert!(drain_notifications(&mut agent).is_empty());
    assert!(agent.next_effect().is_none());
    agent
        .command(AgentCommand::SendTopic(TopicSend {
            publisher: publish,
            payload: b"after-reconnect".to_vec(),
        }))
        .unwrap();
    let (_, new_publish_channel, body) = next_topic_send(&mut agent);
    assert_eq!(new_publish_channel, publication.channel);
    assert_ne!(publication.channel, old_publication.channel);
    assert_ne!(receiving.channel, old_subscription.channel);
    let TopicBody::Message(new_message) = body else {
        panic!("new message")
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
    assert_eq!(agent.snapshot().generation, Some(generation));
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert!(agent.next_effect().is_none());
}

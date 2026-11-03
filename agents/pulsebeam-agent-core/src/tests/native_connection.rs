use super::*;
use pulsebeam_proto::state::{self, codec, validation};

pub(crate) fn observe(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
    records: Vec<state::StateRecord>,
) {
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            payload: codec::encode(
                &state::StateBatch {
                    records,
                    initial_complete: false,
                },
                validation::Direction::Server,
            )
            .unwrap(),
        }))
        .unwrap();
}

pub(crate) fn confirmed(
    key: state::resource_key::Key,
    revision: u64,
    value: state::confirmed_state::Value,
) -> state::StateRecord {
    state::StateRecord {
        key: Some(state::ResourceKey { key: Some(key) }),
        state: Some(state::state_record::State::Confirmed(
            state::ConfirmedState {
                evaluated_revision: revision,
                rejection: None,
                value: Some(value),
            },
        )),
    }
}

pub(crate) fn grant(revision: u64) -> state::StateRecord {
    confirmed(
        state::resource_key::Key::Session(true),
        0,
        state::confirmed_state::Value::Session(state::SessionState {
            authorization: Some(state::Authorization {
                expires_at_unix_seconds: 10_000 + i64::try_from(revision).unwrap(),
                revision,
            }),
            closed: None,
            diagnostic: String::new(),
        }),
    )
}

pub(crate) const OFFER: &str =
    "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:data\r\n";

pub(crate) fn joining(topics: crate::TopicRegistrations) -> (Agent, Generation, ChannelId) {
    let mut agent = Agent::new(AgentConfig {
        endpoint: "https://sfu.test/".into(),
        token: "private-token".into(),
        topology: crate::MediaTopology {
            local_video: 0,
            local_audio: 0,
            remote_video: 0,
            remote_audio: 0,
        },
        retry: Default::default(),
        log_level: Default::default(),
    })
    .unwrap();
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 1,
            connected: true,
            topics,
            ..Default::default()
        }))
        .unwrap();
    let Some(Effect::Rtc(RtcEffect::CreateOffer {
        generation,
        data_channels,
        ..
    })) = agent.next_effect()
    else {
        panic!("offer")
    };
    assert_eq!(data_channels.len(), 1);
    assert_eq!(data_channels[0].negotiated_id, Some(0));
    let channel = ChannelId::new(400).unwrap();
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: OFFER.into(),
            resources: OfferResources {
                slots: Vec::new(),
                signaling_channel: channel,
                data_channels: Vec::new(),
            },
        }))
        .unwrap();
    let Some(Effect::Http(HttpEffect::Request { operation, .. })) = agent.next_effect() else {
        panic!("POST")
    };
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: crate::tests::create_response("p1", "unused", PARTICIPANT_URI),
        }))
        .unwrap();
    assert!(matches!(
        agent.next_effect(),
        Some(Effect::Rtc(RtcEffect::ApplyAnswer { .. }))
    ));
    (agent, generation, channel)
}

pub(crate) fn connected(topics: crate::TopicRegistrations) -> (Agent, Generation, ChannelId) {
    let ordered = topics
        .subscribers
        .iter()
        .any(|subscriber| subscriber.mode == TopicMode::Ordered);
    let (mut agent, generation, channel) = joining(topics);
    agent
        .handle(HostEvent::Rtc(RtcEvent::Connected { generation }))
        .unwrap();
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            channel,
        }))
        .unwrap();
    agent
        .handle(HostEvent::Rtc(RtcEvent::AnswerApplied { generation }))
        .unwrap();
    observe(&mut agent, generation, channel, vec![grant(1)]);
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            payload: codec::encode(
                &state::StateBatch {
                    records: Vec::new(),
                    initial_complete: true,
                },
                validation::Direction::Server,
            )
            .unwrap(),
        }))
        .unwrap();
    if ordered {
        assert!(
            matches!(agent.next_effect(), Some(Effect::Timer(TimerEffect::Schedule { after, .. })) if after == Duration::from_secs(2))
        );
    }
    assert!(agent.next_effect().is_none());
    (agent, generation, channel)
}

pub(crate) const SOURCE: &str = "opaque/server/data/source";
pub(crate) const REMOTE_SOURCE: &str = "opaque/server/remote/source";

pub(crate) struct PreparedData {
    pub channel: ChannelId,
    pub requested_channel: DataChannelSpec,
    pub reference: u64,
    pub revision: u64,
    pub spec: state::ResourceSpec,
}

pub(crate) fn prepare_channel(
    agent: &mut Agent,
    generation: Generation,
    control: ChannelId,
) -> (ChannelId, DataChannelSpec) {
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Prepared {
            generation,
            channel: control,
            preparation: Ok(PreparedDataChannel {
                stream_id: 0,
                negotiated: true,
                ordered: true,
                reliability: DataChannelReliability::Reliable,
                association_max_channels: Some(8),
            }),
        }))
        .unwrap();
    let effect = next_effect(agent);
    let Effect::Rtc(RtcEffect::CreateDataChannel {
        generation: actual,
        channel,
        specification,
    }) = effect
    else {
        panic!("payload allocation, got {effect:?}")
    };
    assert_eq!(actual, generation);
    let stream_id = specification.negotiated_id.unwrap();
    assert_ne!(stream_id, 0);
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Prepared {
            generation,
            channel,
            preparation: Ok(PreparedDataChannel {
                stream_id,
                negotiated: true,
                ordered: specification.ordered,
                reliability: specification.reliability,
                association_max_channels: Some(8),
            }),
        }))
        .unwrap();
    (channel, specification)
}

pub(crate) fn prepare_data_pending(
    agent: &mut Agent,
    generation: Generation,
    control: ChannelId,
) -> (PreparedData, OperationId, Vec<u8>) {
    let (channel, channel_spec) = prepare_channel(agent, generation, control);
    let Effect::DataChannel(DataChannelEffect::Send {
        operation,
        channel: output,
        payload,
        ..
    }) = next_effect(agent)
    else {
        panic!("StateBatch")
    };
    assert_eq!(output, control);
    let batch = codec::decode(&payload, validation::Direction::Client).unwrap();
    assert_eq!(batch.records.len(), 1);
    let record = &batch.records[0];
    let Some(state::resource_key::Key::Owned(reference)) =
        record.key.as_ref().and_then(|key| key.key.as_ref())
    else {
        panic!("owned reference")
    };
    let Some(state::state_record::State::Desired(desired)) = &record.state else {
        panic!("desired")
    };
    let Some(state::desired_state::Value::Present(spec)) = &desired.value else {
        panic!("present")
    };
    let publication = PreparedData {
        channel,
        requested_channel: channel_spec,
        reference: *reference,
        revision: desired.revision,
        spec: spec.as_ref().clone(),
    };
    (publication, operation, payload)
}

pub(crate) fn prepare_data(
    agent: &mut Agent,
    generation: Generation,
    control: ChannelId,
) -> PreparedData {
    let (prepared, operation, _) = prepare_data_pending(agent, generation, control);
    acknowledge_send(agent, generation, control, operation);
    prepared
}

pub(crate) fn confirm_publication(
    agent: &mut Agent,
    generation: Generation,
    control: ChannelId,
    publication: &PreparedData,
    source: &str,
) {
    let Some(state::resource_spec::Declaration::Publication(declaration)) =
        &publication.spec.declaration
    else {
        panic!("publication")
    };
    let Some(state::publication_spec::Kind::Data(data)) = &declaration.kind else {
        panic!("data")
    };
    let participant = state::Participant {
        participant_id: agent.snapshot().participant_id.clone().unwrap(),
        participant_external_id: agent.snapshot().participant_external_id.clone().unwrap(),
    };
    observe(
        agent,
        generation,
        control,
        vec![confirmed(
            state::resource_key::Key::Owned(publication.reference),
            publication.revision,
            state::confirmed_state::Value::Owned(alloc::boxed::Box::new(state::LiveResource {
                spec: Some(publication.spec.clone()),
                participant: Some(participant.clone()),
                track: Some(state::Track {
                    track_id: source.into(),
                    participant_id: participant.participant_id,
                    label: declaration.label.clone(),
                    kind: Some(state::track::Kind::Data(state::DataTrack {
                        delivery: data.delivery,
                    })),
                }),
                execution: state::ExecutionStatus::Installed as i32,
                execution_failure: None,
                activity: None,
            })),
        )],
    );
}

pub(crate) fn prepare_subscription(
    agent: &mut Agent,
    generation: Generation,
    control: ChannelId,
    source: &str,
    subscriber: &TopicSubscriber,
) -> PreparedData {
    let participant = state::Participant {
        participant_id: REMOTE_PUBLISHER.into(),
        participant_external_id: "remote-app-user".into(),
    };
    let track = state::Track {
        track_id: source.into(),
        participant_id: participant.participant_id.clone(),
        label: subscriber.topic.clone(),
        kind: Some(state::track::Kind::Data(state::DataTrack {
            delivery: subscriber.mode.delivery() as i32,
        })),
    };
    observe(
        agent,
        generation,
        control,
        vec![
            confirmed(
                state::resource_key::Key::Participant(participant.participant_id.clone()),
                0,
                state::confirmed_state::Value::Participant(participant.clone()),
            ),
            confirmed(
                state::resource_key::Key::Track(source.into()),
                0,
                state::confirmed_state::Value::Track(track.clone()),
            ),
        ],
    );
    let prepared = prepare_data(agent, generation, control);
    assert!(
        matches!(&prepared.spec.declaration, Some(state::resource_spec::Declaration::Subscription(subscription)) if subscription.track_id == source && subscription.kind == state::TrackKind::Data as i32)
    );
    observe(
        agent,
        generation,
        control,
        vec![confirmed(
            state::resource_key::Key::Owned(prepared.reference),
            prepared.revision,
            state::confirmed_state::Value::Owned(alloc::boxed::Box::new(state::LiveResource {
                spec: Some(prepared.spec.clone()),
                participant: Some(participant),
                track: Some(track),
                execution: state::ExecutionStatus::Installed as i32,
                execution_failure: None,
                activity: None,
            })),
        )],
    );
    if subscriber.mode == TopicMode::Ordered {
        let (operation, channel, body) = next_topic_send(agent);
        assert_eq!(channel, prepared.channel);
        assert_eq!(
            body,
            TopicBody::Probe(TopicProbe {
                data_track_id: source.into(),
                from_sequence: 0,
                live: true
            })
        );
        acknowledge_send(agent, generation, channel, operation);
    }
    assert!(agent.next_effect().is_none());
    prepared
}

pub(crate) fn connected_publisher(mode: TopicMode) -> (Agent, Generation, ChannelId, PreparedData) {
    let (mut agent, generation, control) = connected(TopicRegistrations {
        publishers: vec![publisher("state", mode)],
        subscribers: Vec::new(),
    });
    let publication = prepare_data(&mut agent, generation, control);
    confirm_publication(&mut agent, generation, control, &publication, SOURCE);
    assert!(agent.next_effect().is_none());
    let _ = drain_notifications(&mut agent);
    (agent, generation, control, publication)
}

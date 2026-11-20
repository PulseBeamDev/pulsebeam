use super::*;
use pulsebeam_proto::state::{self, codec};

pub(crate) fn observe(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
    records: Vec<state::ServerStateRecord>,
) {
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            payload: codec::encode_server(&state::ServerStateBatch {
                records,
                initial_complete: false,
                ..Default::default()
            })
            .unwrap(),
        }))
        .unwrap();
}

pub(crate) fn drain_batches(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
) -> Vec<state::ClientStateBatch> {
    let mut batches = Vec::new();
    while let Some(effect) = agent.next_effect_after_turn() {
        if let Effect::DataChannel(DataChannelEffect::Send {
            operation,
            generation: actual_generation,
            channel: actual_channel,
            payload,
            ..
        }) = effect
        {
            assert_eq!((actual_generation, actual_channel), (generation, channel));
            batches.push(codec::decode_client(&payload).unwrap());
            acknowledge_send(agent, generation, channel, operation);
        }
    }
    batches
}

pub(crate) fn catalog_page(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
    watch_id: u64,
    room: &str,
) {
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            payload: codec::encode_server(&state::ServerStateBatch {
                catalog_records: vec![state::CatalogServerRecord {
                    response: Some(state::catalog_server_record::Response::Page(
                        state::CatalogPageRecord {
                            watch_id,
                            room: room.into(),
                            revision: 1,
                            complete: true,
                            ..Default::default()
                        },
                    )),
                }],
                ..Default::default()
            })
            .unwrap(),
        }))
        .unwrap();
}

pub(crate) fn confirmed(
    key: state::resource_key::Key,
    revision: u64,
    value: state::confirmed_state::Value,
) -> state::ServerStateRecord {
    state::ServerStateRecord {
        key: Some(state::ResourceKey { key: Some(key) }),
        confirmed: Some(state::ConfirmedState {
            evaluated_revision: revision,
            rejection: None,
            value: Some(value),
        }),
    }
}

fn lookup_record(
    reference: u64,
    revision: u64,
    spec: state::ResourceSpec,
    publication_id: &str,
    mode: Option<TopicMode>,
) -> state::ServerStateRecord {
    let Some(state::resource_spec::Declaration::LogicalSubscription(lookup)) = &spec.declaration
    else {
        panic!("source lookup")
    };
    let Some(address) = &lookup.source else {
        panic!("logical source")
    };
    let label = address.track_id.clone();
    let participant = state::Participant {
        participant_id: address.participant_id.clone(),

        room_id: address.room.clone(),
    };
    let track_kind = match state::TrackKind::try_from(address.kind).unwrap() {
        state::TrackKind::Video => state::track::Kind::Video(state::VideoTrack {
            format: Some(state::CodecFormat::from(state::codec_format::Codec::Vp8(
                state::Vp8Parameters {},
            ))),
        }),
        state::TrackKind::Audio => state::track::Kind::Audio(state::AudioTrack {
            format: Some(state::CodecFormat::from(state::codec_format::Codec::Opus(
                state::OpusParameters::default(),
            ))),
        }),
        state::TrackKind::Data => state::track::Kind::Data(state::DataTrack {
            delivery: mode.unwrap_or(TopicMode::Ordered).delivery() as i32,
        }),
        _ => panic!("unsupported lookup kind"),
    };
    confirmed(
        state::resource_key::Key::Owned(reference),
        revision,
        state::confirmed_state::Value::LogicalSubscription(Box::new(
            state::LogicalSubscriptionState {
                spec: Some(spec),
                source_revision: 1,
                execution: None,
                source: Some(state::Track {
                    publication_id: publication_id.into(),
                    participant_id: participant.participant_id,
                    room_id: participant.room_id,
                    track_id: label,
                    kind: Some(track_kind),
                }),
            },
        )),
    )
}

pub(crate) fn answer_source_lookup_batch(
    agent: &mut Agent,
    generation: Generation,
    control: ChannelId,
    topics: &crate::TopicRegistrations,
    remote_source_override: Option<&str>,
) -> Vec<u64> {
    let effect = next_effect(agent);
    let Effect::DataChannel(DataChannelEffect::Send {
        operation,
        generation: actual_generation,
        channel: actual_channel,
        payload,
        ..
    }) = effect
    else {
        panic!("SourceLookup StateBatch, got {effect:?}")
    };
    assert_eq!((actual_generation, actual_channel), (generation, control));
    let batch = codec::decode_client(&payload).unwrap();
    let registrations = &topics.subscribers;
    let mut lookup_references = Vec::new();
    let records = batch
        .records
        .into_iter()
        .filter_map(|record| {
            let desired = record.desired?;
            let revision = desired.revision;
            match desired.value? {
                state::desired_state::Value::Absent(_) => Some(confirmed(
                    state::resource_key::Key::Owned(record.owned),
                    revision,
                    state::confirmed_state::Value::Absent(state::AbsentState {
                        reason: state::AbsenceReason::Removed as i32,
                    }),
                )),
                state::desired_state::Value::Present(spec) => {
                    let Some(state::resource_spec::Declaration::LogicalSubscription(lookup)) =
                        &spec.declaration
                    else {
                        return None;
                    };
                    let address = lookup
                        .source
                        .as_ref()
                        .filter(|address| address.kind == state::TrackKind::Data as i32)?;
                    lookup_references.push(record.owned);
                    let mode = registrations
                        .iter()
                        .find(|subscriber| {
                            subscriber.room == address.room
                                && subscriber.participant_id == address.participant_id
                                && subscriber.topic == address.track_id
                        })
                        .map(|subscriber| subscriber.mode);
                    let publication_id = remote_source_override.unwrap_or_else(|| {
                        if address.participant_id == "remote-app-user" {
                            REMOTE_SOURCE
                        } else {
                            &address.participant_id
                        }
                    });
                    Some(lookup_record(
                        record.owned,
                        revision,
                        spec.as_ref().clone(),
                        publication_id,
                        mode,
                    ))
                }
            }
        })
        .collect::<Vec<_>>();
    assert!(!records.is_empty(), "SourceLookup request in StateBatch");
    acknowledge_send(agent, generation, control, operation);
    observe(agent, generation, control, records);
    lookup_references
}

pub(crate) fn grant(revision: u64) -> state::ServerStateRecord {
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
            successor_receipt: Some(state::SuccessorReceipt {
                proof: vec![17, 29],
                expires_at_unix_seconds: 10_000 + revision,
            }),
        }),
    )
}

pub(crate) const OFFER: &str =
    "v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:data\r\n";

pub(crate) fn joining(topics: crate::TopicRegistrations) -> (Agent, Generation, ChannelId) {
    let mut agent = Agent::new(AgentConfig {
        endpoint: "https://sfu.test/".into(),
        token: "private-token".into(),
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
    agent.command(AgentCommand::Join("room".into())).unwrap();
    let Some(Effect::Rtc(RtcEffect::CreateOffer {
        generation,
        data_channels,
        ..
    })) = agent.next_effect_after_turn()
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
    let (operation, _) = native_request(&mut agent, generation);
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: crate::tests::create_response("p1", "unused", PARTICIPANT_URI),
        }))
        .unwrap();
    assert!(matches!(
        agent.next_effect_after_turn(),
        Some(Effect::Rtc(RtcEffect::ApplyAnswer { .. }))
    ));
    (agent, generation, channel)
}

pub(crate) fn connected(topics: crate::TopicRegistrations) -> (Agent, Generation, ChannelId) {
    let ordered = topics
        .subscribers
        .iter()
        .any(|subscriber| subscriber.mode == TopicMode::Ordered);
    let has_subscribers = !topics.subscribers.is_empty();
    let fixture_topics = topics.clone();
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
            payload: codec::encode_server(&state::ServerStateBatch {
                records: Vec::new(),
                initial_complete: true,
                ..Default::default()
            })
            .unwrap(),
        }))
        .unwrap();
    if ordered {
        assert!(
            matches!(agent.next_effect_after_turn(), Some(Effect::Timer(TimerEffect::Schedule { after, .. })) if after == Duration::from_secs(2))
        );
    }
    if has_subscribers {
        let _ = answer_source_lookup_batch(&mut agent, generation, channel, &fixture_topics, None);
    } else {
        assert!(agent.next_effect_after_turn().is_none());
    }
    (agent, generation, channel)
}

pub(crate) const SOURCE: &str = "opaque/server/data/source";
pub(crate) const REMOTE_SOURCE: &str = "opaque/server/remote/source";

#[derive(Clone)]
pub(crate) struct PreparedData {
    pub channel: ChannelId,
    pub requested_channel: DataChannelSpec,
    pub reference: u64,
    pub revision: u64,
    pub spec: state::ResourceSpec,
    pub additional: Vec<PreparedData>,
    logical: Option<state::LogicalSubscriptionState>,
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
    let effect = agent
        .next_effect_after_turn()
        .unwrap_or_else(|| panic!("accepted SourceLookup produced no selected payload receiver"));
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
    let mut channels = vec![(channel, channel_spec)];
    let mut effect = next_effect(agent);
    let (operation, output, payload) = loop {
        match effect {
            Effect::Rtc(RtcEffect::CreateDataChannel {
                generation: actual_generation,
                channel: pending_channel,
                specification,
            }) => {
                assert_eq!(actual_generation, generation);
                channels.push((pending_channel, specification.clone()));
                let stream_id = specification.negotiated_id.unwrap();
                assert_ne!(stream_id, 0);
                agent
                    .handle(HostEvent::DataChannel(DataChannelEvent::Prepared {
                        generation,
                        channel: pending_channel,
                        preparation: Ok(PreparedDataChannel {
                            stream_id,
                            negotiated: true,
                            ordered: specification.ordered,
                            reliability: specification.reliability,
                            association_max_channels: Some(8),
                        }),
                    }))
                    .unwrap();
                effect = next_effect(agent);
            }
            Effect::DataChannel(DataChannelEffect::Send {
                operation,
                channel,
                payload,
                ..
            }) => break (operation, channel, payload),
            other => panic!("StateBatch after physical payload preparation, got {other:?}"),
        }
    };
    assert_eq!(output, control);
    let batch = codec::decode_client(&payload).unwrap();
    assert_eq!(
        batch.records.len(),
        channels.len(),
        "records: {:#?}",
        batch.records
    );
    let mut prepared = batch
        .records
        .iter()
        .map(|record| {
            let (revision, spec, logical, binding) = if let Some(desired) = &record.desired {
                let Some(state::desired_state::Value::Present(spec)) = &desired.value else {
                    panic!("present resource")
                };
                let Some(state::resource_spec::Declaration::Publication(publication)) =
                    &spec.declaration
                else {
                    panic!("data publication")
                };
                let Some(state::publication_spec::Kind::Data(data)) = &publication.kind else {
                    panic!("data publication")
                };
                (
                    desired.revision,
                    spec.as_ref().clone(),
                    None,
                    data.binding.clone().unwrap(),
                )
            } else {
                let Some(state::execution_report::Outcome::LogicalReceiver(report)) = record
                    .execution
                    .as_ref()
                    .and_then(|execution| execution.outcome.as_ref())
                else {
                    panic!("logical receiver report")
                };
                let Some(state::logical_receiver_report::Outcome::Prepared(binding)) =
                    &report.outcome
                else {
                    panic!("prepared payload channel")
                };
                let confirmed = agent.test_native_owned(record.owned).expect("source offer");
                let Some(state::confirmed_state::Value::LogicalSubscription(logical)) =
                    &confirmed.value
                else {
                    panic!("logical data state")
                };
                assert_eq!(logical.source_revision, report.source_revision);
                let mut logical = logical.as_ref().clone();
                logical.execution = Some(state::logical_subscription_state::Execution::Installed(
                    binding.clone(),
                ));
                (
                    confirmed.evaluated_revision,
                    logical.spec.clone().unwrap(),
                    Some(logical),
                    binding.clone(),
                )
            };
            let Some(state::track_binding::Transport::Sctp(sctp)) = binding.transport else {
                panic!("SCTP binding")
            };
            let index = channels
                .iter()
                .position(|(_, spec)| spec.negotiated_id.map(u32::from) == sctp.stream_id)
                .expect("prepared stream has a local channel");
            let (channel, requested_channel) = channels.swap_remove(index);
            PreparedData {
                channel,
                requested_channel,
                reference: record.owned,
                revision,
                spec,
                additional: Vec::new(),
                logical,
            }
        })
        .collect::<Vec<_>>();
    let primary = prepared
        .iter()
        .position(|resource| {
            matches!(
                resource.spec.declaration,
                Some(state::resource_spec::Declaration::Publication(_))
            )
        })
        .unwrap_or(0);
    let mut primary = prepared.remove(primary);
    primary.additional = prepared;
    (primary, operation, payload)
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

        room_id: agent.snapshot().room_id.clone().unwrap(),
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
                    publication_id: source.into(),
                    participant_id: participant.participant_id,
                    room_id: participant.room_id,
                    track_id: declaration.track_id.clone(),
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
    prepare_subscription_staged(agent, generation, control, source, subscriber, None)
}

pub(crate) fn prepare_subscription_staged(
    agent: &mut Agent,
    generation: Generation,
    control: ChannelId,
    source: &str,
    subscriber: &TopicSubscriber,
    staged: Option<&PreparedData>,
) -> PreparedData {
    let prepared = if let Some(staged) = staged {
        staged
            .additional
            .iter()
            .find(|resource| {
                resource
                    .logical
                    .as_ref()
                    .and_then(|l| l.source.as_ref())
                    .is_some_and(|track| track.publication_id == source)
            })
            .expect("staged data subscription")
            .clone()
    } else {
        prepare_data(agent, generation, control)
    };
    let logical = prepared.logical.clone().expect("data source report");
    let track = logical.source.as_ref().expect("data source");
    assert_eq!(track.publication_id, source);
    assert_eq!(track.participant_id, subscriber.participant_id);
    assert_eq!(track.track_id, subscriber.topic);
    observe(
        agent,
        generation,
        control,
        vec![confirmed(
            state::resource_key::Key::Owned(prepared.reference),
            prepared.revision,
            state::confirmed_state::Value::LogicalSubscription(Box::new(logical)),
        )],
    );
    if subscriber.mode == TopicMode::Ordered {
        let (operation, channel, body) = next_topic_send(agent);
        assert_eq!(channel, prepared.channel);
        assert!(
            matches!(body, TopicBody::Probe(TopicProbe { publication_id, .. }) if publication_id == source)
        );
        acknowledge_send(agent, generation, channel, operation);
    }
    assert!(agent.next_effect_after_turn().is_none());
    prepared
}

pub(crate) fn connected_publisher(mode: TopicMode) -> (Agent, Generation, ChannelId, PreparedData) {
    let (mut agent, generation, control) = connected(TopicRegistrations {
        publishers: vec![publisher("state", mode)],
        subscribers: Vec::new(),
    });
    let publication = prepare_data(&mut agent, generation, control);
    confirm_publication(&mut agent, generation, control, &publication, SOURCE);
    assert!(agent.next_effect_after_turn().is_none());
    let _ = drain_notifications(&mut agent);
    (agent, generation, control, publication)
}

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
            binary: true,
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
            binary: true,
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
    data_v1: Option<state::DataV1ReceiverOffer>,
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
            capture_clock: data_v1
                .as_ref()
                .and_then(|offer| offer.capture_clock.clone()),
            delivery: mode.unwrap_or(TopicMode::Ordered).delivery() as i32,
        }),
        _ => panic!("unsupported lookup kind"),
    };
    confirmed(
        state::resource_key::Key::Owned(reference),
        revision,
        state::confirmed_state::Value::LogicalSubscription(Box::new(
            state::LogicalSubscriptionState {
                data_v1,
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
    answer_source_lookup(
        agent,
        generation,
        control,
        topics,
        remote_source_override,
        None,
    )
}

fn answer_source_lookup(
    agent: &mut Agent,
    generation: Generation,
    control: ChannelId,
    topics: &crate::TopicRegistrations,
    remote_source_override: Option<&str>,
    data_v1: Option<state::DataV1ReceiverOffer>,
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
                    let offer = data_v1.clone().or_else(|| {
                        (lookup.data_version == Some(DATA_V1)).then_some(
                            state::DataV1ReceiverOffer {
                                source: record.owned,
                                branch: record.owned,
                                capture_clock: None,
                            },
                        )
                    });
                    Some(lookup_record(
                        record.owned,
                        revision,
                        spec.as_ref().clone(),
                        publication_id,
                        mode,
                        offer,
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

const DATA_V1: u32 = pulsebeam_proto::data_endpoint::DATA_V1 as u32;

pub(crate) fn joining(topics: crate::TopicRegistrations) -> (Agent, Generation, ChannelId) {
    joining_with_data_version(topics, Some(DATA_V1))
}

fn joining_with_data_version(
    topics: crate::TopicRegistrations,
    data_version: Option<u32>,
) -> (Agent, Generation, ChannelId) {
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
    let mut response = crate::tests::create_response("p1", "unused", PARTICIPANT_URI);
    let mut json: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    let fields = json.as_object_mut().unwrap();
    match data_version {
        Some(version) => {
            fields.insert("data_version".into(), version.into());
        }
        None => {
            fields.remove("data_version");
        }
    }
    response.body = serde_json::to_vec(&json).unwrap();
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response,
        }))
        .unwrap();
    assert!(matches!(
        agent.next_effect_after_turn(),
        Some(Effect::Rtc(RtcEffect::ApplyAnswer { .. }))
    ));
    (agent, generation, channel)
}

pub(super) fn complete_initial_state(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
) {
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
    observe(agent, generation, channel, vec![grant(1)]);
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            binary: true,
            payload: codec::encode_server(&state::ServerStateBatch {
                records: Vec::new(),
                initial_complete: true,
                ..Default::default()
            })
            .unwrap(),
        }))
        .unwrap();
}

pub(crate) fn connected(topics: crate::TopicRegistrations) -> (Agent, Generation, ChannelId) {
    let reliable = topics
        .publishers
        .iter()
        .any(|publisher| publisher.mode == TopicMode::Ordered)
        || topics
            .subscribers
            .iter()
            .any(|subscriber| subscriber.mode == TopicMode::Ordered);
    let (mut agent, generation, control) = connected_with_data_profile(topics, Some(DATA_V1), None);
    if reliable {
        agent.enable_reliable_data_fixture();
    }
    (agent, generation, control)
}

fn connected_with_data_profile(
    topics: crate::TopicRegistrations,
    data_version: Option<u32>,
    data_v1: Option<state::DataV1ReceiverOffer>,
) -> (Agent, Generation, ChannelId) {
    let has_subscribers = !topics.subscribers.is_empty();
    let fixture_topics = topics.clone();
    let (mut agent, generation, channel) = joining_with_data_version(topics, data_version);
    complete_initial_state(&mut agent, generation, channel);
    if has_subscribers {
        let _ = answer_source_lookup(
            &mut agent,
            generation,
            channel,
            &fixture_topics,
            None,
            data_v1,
        );
    } else {
        assert!(agent.next_effect_after_turn().is_none());
    }
    (agent, generation, channel)
}

#[test]
fn native_without_data_v1_does_not_allocate_a_legacy_payload_channel() {
    let topics = crate::TopicRegistrations {
        publishers: vec![crate::TopicPublisher {
            topic: "updates".into(),
            mode: TopicMode::Latest,
        }],
        ..Default::default()
    };
    let (mut agent, generation, _) = connected_with_data_profile(topics, None, None);
    assert!(agent.next_effect_after_turn().is_none());
    assert!(
        agent
            .snapshot()
            .topics
            .publishers
            .iter()
            .all(|publisher| publisher.channel.is_none() && publisher.publication_id.is_none())
    );
    assert_eq!(agent.snapshot().generation, Some(generation));
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
    let effect = next_without_data_timer(agent);
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

#[test]
fn reliable_receiver_uses_active_native_signaling_for_expectations_and_receipts() {
    use pulsebeam_proto::{
        data_control::{
            self, BranchUnit, Expectation, SourceUnit, SubscriberControl, SubscriberObservation,
            subscriber_control, subscriber_observation,
        },
        data_endpoint::DATA_V1,
        data_record::{Header, Record},
    };
    let selector = TrackSelector {
        room: "room".into(),
        participant_id: "remote-app-user".into(),
        track_id: "cursor".into(),
    };
    let topics = crate::TopicRegistrations {
        subscribers: vec![crate::TopicSubscriber {
            room: selector.room.clone(),
            participant_id: selector.participant_id.clone(),
            topic: selector.track_id.clone(),
            mode: TopicMode::Ordered,
        }],
        ..Default::default()
    };
    let offer = state::DataV1ReceiverOffer {
        source: 11,
        branch: 7,
        capture_clock: None,
    };
    let endpoint = offer.endpoint().unwrap();
    let (mut agent, generation, control) =
        connected_with_data_profile(topics, Some(u32::from(DATA_V1)), Some(offer));
    agent.enable_reliable_data_fixture();
    let ResourceResult::Owned { owner } = agent
        .resource(ResourceCommand::ReceiveData {
            spec: TrackSpec::Data {
                track_id: selector.track_id.clone(),
                delivery: TopicMode::Ordered,
            },
            source: selector,
        })
        .unwrap()
    else {
        panic!("data owner")
    };
    let prepared = prepare_data(&mut agent, generation, control);
    assert!(!prepared.requested_channel.ordered);
    assert_eq!(
        prepared.requested_channel.reliability,
        DataChannelReliability::MaxRetransmits(0)
    );
    observe(
        &mut agent,
        generation,
        control,
        vec![confirmed(
            state::resource_key::Key::Owned(prepared.reference),
            prepared.revision,
            state::confirmed_state::Value::LogicalSubscription(Box::new(prepared.logical.unwrap())),
        )],
    );
    let record = Record::new(
        Header {
            unit_seq: 2,
            index: 0,
            count: 1,
            unit_len: 1,
            capture: None,
        },
        b"x",
        None,
    )
    .unwrap();
    let mut bytes = vec![0; record.encoded_len()];
    record.write(&mut bytes).unwrap();
    let message = |channel, payload| {
        HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            binary: true,
            payload,
        })
    };
    agent.handle(message(prepared.channel, bytes)).unwrap();
    let read = |agent: &mut Agent| agent.resource(ResourceCommand::ReadData { owner }).unwrap();
    assert_eq!(read(&mut agent), ResourceResult::Event { event: None });

    let expectation = Expectation {
        unit: Some(BranchUnit {
            source: Some(SourceUnit {
                source_incarnation: 11,
                unit_seq: Some(2),
            }),
            branch_incarnation: 7,
        }),
        fragment_count: 1,
    };
    let control_frame = |record| {
        codec::encode_data_control(&state::NativeDataControl {
            endpoint: Some(state::native_data_control::Endpoint::Subscriber(
                state::OwnedDataControl {
                    owned: prepared.reference,
                    record: data_control::encode(
                        endpoint,
                        &SubscriberObservation {
                            record: Some(record),
                        },
                    )
                    .unwrap(),
                },
            )),
        })
        .unwrap()
    };
    agent
        .handle(message(
            control,
            control_frame(subscriber_observation::Record::Expectation(expectation)),
        ))
        .unwrap();
    assert_eq!(
        read(&mut agent),
        ResourceResult::Event {
            event: Some(DataEvent::Message {
                participant_id: "remote-app-user".into(),
                publication_id: REMOTE_SOURCE.into(),
                sequence: 2,
                data: b"x".to_vec(),
                captured_at: None,
            })
        }
    );
    let next_control = |agent: &mut Agent| {
        for _ in 0..8 {
            match next_effect(agent) {
                Effect::DataChannel(DataChannelEffect::Send {
                    operation,
                    channel,
                    payload,
                    ..
                }) => {
                    assert_eq!(channel, control);
                    let codec::NativeFrame::Data(frame) =
                        codec::decode_client_frame(&payload).unwrap()
                    else {
                        panic!("DATA control")
                    };
                    let Some(state::native_data_control::Endpoint::Subscriber(frame)) =
                        frame.endpoint
                    else {
                        panic!("subscriber control")
                    };
                    assert_eq!(frame.owned, prepared.reference);
                    return (
                        operation,
                        data_control::decode::<SubscriberControl>(endpoint, &frame.record)
                            .unwrap()
                            .record
                            .unwrap(),
                    );
                }
                Effect::Timer(TimerEffect::Schedule { timer, after })
                    if after == Duration::ZERO =>
                {
                    agent
                        .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
                        .unwrap();
                }
                Effect::Timer(_) => {}
                other => panic!("unexpected effect {other:?}"),
            }
        }
        panic!("Reliable receiver did not send control")
    };
    let (operation, propagated) = next_control(&mut agent);
    assert!(
        matches!(propagated, subscriber_control::Record::ExpectationOutcome(outcome) if outcome.expectation == Some(expectation) && outcome.outcome == data_control::ControlOutcome::Confirmed as i32)
    );
    acknowledge_send(&mut agent, generation, control, operation);
    let (operation, received) = next_control(&mut agent);
    assert_eq!(
        received,
        subscriber_control::Record::AssemblyReceipt(expectation)
    );
    agent
        .handle(message(
            control,
            control_frame(subscriber_observation::Record::ReceiptAck(expectation)),
        ))
        .unwrap();
    acknowledge_send(&mut agent, generation, control, operation);
    assert_eq!(read(&mut agent), ResourceResult::Event { event: None });
}

#[test]
fn explicit_fabric_offer_delivers_only_on_the_exact_installed_data_v1_channel() {
    let selector = TrackSelector {
        room: "room".into(),
        participant_id: "remote-app-user".into(),
        track_id: "cursor".into(),
    };
    let topics = crate::TopicRegistrations {
        subscribers: vec![crate::TopicSubscriber {
            room: selector.room.clone(),
            participant_id: selector.participant_id.clone(),
            topic: selector.track_id.clone(),
            mode: TopicMode::Latest,
        }],
        ..Default::default()
    };
    let stamp = crate::ffi::CaptureTimestamp {
        clock: crate::ffi::CaptureClock {
            id: "capture-source".into(),
            epoch: u64::MAX,
        },
        nanos: i64::MIN,
        uncertainty_ns: Some(u32::MAX),
    };
    let offer = state::DataV1ReceiverOffer {
        source: u64::MAX,
        branch: 3,
        capture_clock: Some(state::DataV1CaptureClock {
            id: stamp.clock.id.clone(),
            epoch: stamp.clock.epoch,
        }),
    };
    let (mut agent, generation, control) = connected_with_data_profile(
        topics,
        Some(u32::from(pulsebeam_proto::data_endpoint::DATA_V1)),
        Some(offer.clone()),
    );
    let ResourceResult::Owned { owner } = agent
        .resource(ResourceCommand::ReceiveData {
            spec: TrackSpec::Data {
                track_id: selector.track_id.clone(),
                delivery: TopicMode::Latest,
            },
            source: selector.clone(),
        })
        .unwrap()
    else {
        panic!("data owner")
    };
    let (channel, specification) = prepare_channel(&mut agent, generation, control);
    assert_ne!(channel, control);
    assert!(!specification.ordered);
    assert_eq!(
        specification.reliability,
        DataChannelReliability::MaxRetransmits(0)
    );
    let Effect::DataChannel(DataChannelEffect::Send {
        operation,
        channel: output,
        payload,
        ..
    }) = next_effect(&mut agent)
    else {
        panic!("logical receiver preparation")
    };
    assert_eq!(output, control);
    let batch = codec::decode_client(&payload).unwrap();
    let [record] = batch.records.as_slice() else {
        panic!("one prepared binding")
    };
    assert!(record.desired.is_none());
    let Some(state::execution_report::Outcome::LogicalReceiver(report)) = record
        .execution
        .as_ref()
        .and_then(|report| report.outcome.as_ref())
    else {
        panic!("logical receiver report")
    };
    assert_eq!(report.source_revision, 1);
    let parameters = state::SctpDataV1Parameters {
        stream_id: specification.negotiated_id.map(u32::from),
        version: u32::from(pulsebeam_proto::data_endpoint::DATA_V1),
        channel_incarnation: channel.get(),
        delivery: state::DataV1Delivery::Latest as i32,
    };
    let transport = |parameters| state::TrackBinding {
        transport: Some(state::track_binding::Transport::SctpDataV1(parameters)),
    };
    assert_eq!(
        report.outcome,
        Some(state::logical_receiver_report::Outcome::Prepared(
            transport(parameters)
        ))
    );
    acknowledge_send(&mut agent, generation, control, operation);
    let install =
        |binding| fabric_installed(record.owned, selector.clone(), offer.clone(), binding);
    observe(
        &mut agent,
        generation,
        control,
        vec![install(transport(state::SctpDataV1Parameters {
            channel_incarnation: parameters.channel_incarnation + 1,
            ..parameters
        }))],
    );
    let message = |index, count, data: &[u8], unit_seq| {
        use pulsebeam_proto::data_record::{Capture, Clock, Header, Record};
        let clock = Clock {
            id: &stamp.clock.id,
            epoch: stamp.clock.epoch,
        };
        let record = Record::new(
            Header {
                unit_seq,
                index,
                count,
                unit_len: if count == 2 { 1025 } else { 1 },
                capture: Some(Capture {
                    clock,
                    nanos: stamp.nanos,
                    uncertainty_ns: stamp.uncertainty_ns,
                }),
            },
            data,
            Some(clock),
        )
        .unwrap();
        let mut payload = vec![0; record.encoded_len()];
        record.write(&mut payload).unwrap();
        HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            payload,
            binary: true,
        })
    };
    let read = |agent: &mut Agent| agent.resource(ResourceCommand::ReadData { owner }).unwrap();
    agent
        .handle_at(message(0, 1, b"x", 0), Duration::from_secs(1))
        .unwrap();
    assert_eq!(read(&mut agent), ResourceResult::Event { event: None });
    observe(
        &mut agent,
        generation,
        control,
        vec![install(transport(parameters))],
    );
    agent
        .handle_at(message(1, 2, b"A", 1), Duration::from_secs(2))
        .unwrap();
    assert_eq!(read(&mut agent), ResourceResult::Event { event: None });
    agent
        .handle_at(message(0, 1, b"B", 2), Duration::from_secs(3))
        .unwrap();
    agent
        .handle_at(message(0, 2, &[0; 1024], 1), Duration::from_secs(3))
        .unwrap();
    assert_eq!(
        read(&mut agent),
        ResourceResult::Event {
            event: Some(DataEvent::Message {
                participant_id: selector.participant_id,
                publication_id: REMOTE_SOURCE.into(),
                sequence: 2,
                data: b"B".to_vec(),
                captured_at: Some(stamp),
            })
        }
    );
    assert_eq!(read(&mut agent), ResourceResult::Event { event: None });
}

#[test]
fn negotiated_publisher_admits_only_on_the_installed_lease_and_emits_records() {
    use pulsebeam_proto::{data_endpoint::DATA_V1, data_record::Record};
    let publisher = TopicPublisher {
        topic: "cursor".into(),
        mode: TopicMode::Latest,
    };
    let spec = TrackSpec::Data {
        track_id: publisher.topic.clone(),
        delivery: publisher.mode,
    };
    let topics = crate::TopicRegistrations {
        publishers: vec![publisher],
        ..Default::default()
    };
    let (mut agent, generation, control) =
        connected_with_data_profile(topics, Some(u32::from(DATA_V1)), None);
    let prepared = prepare_data(&mut agent, generation, control);
    assert!(matches!(
        agent.send_data(spec.clone(), vec![1]),
        Err(AgentError::InvalidTopic(TopicError::PublisherUnavailable(
            _
        )))
    ));
    assert_eq!(agent.snapshot().topics.publishers[0].channel, None);
    assert_eq!(agent.snapshot().topics.publishers[0].publication_id, None);
    confirm_publication(&mut agent, generation, control, &prepared, SOURCE);
    assert_eq!(
        agent.snapshot().topics.publishers[0].channel,
        Some(prepared.channel)
    );
    assert_eq!(
        agent.snapshot().topics.publishers[0]
            .publication_id
            .as_deref(),
        Some(SOURCE)
    );
    let send = agent.send_data(spec, vec![255; 1025]).unwrap();
    assert!(
        drain_notifications(&mut agent)
            .contains(&Notification::DataSendFinished { send, error: None })
    );
    assert!(matches!(
        next_effect(&mut agent),
        Effect::Timer(TimerEffect::Schedule { after, .. }) if after == Duration::from_secs(2)
    ));
    for (index, length) in [(0, 1024), (1, 1)] {
        let effect = next_effect(&mut agent);
        let Effect::DataChannel(DataChannelEffect::Send {
            operation,
            channel,
            binary,
            payload,
            ..
        }) = effect
        else {
            panic!("expected binary DATA record {index}, got {effect:?}");
        };
        assert!(binary);
        assert_eq!(channel, prepared.channel);
        let record = Record::decode(&payload, None).unwrap();
        assert_eq!(record.header().index, index);
        assert_eq!(record.header().unit_seq, 0);
        assert_eq!(record.bytes(), vec![255; length]);
        assert!(agent.next_effect().is_none());
        acknowledge_send(&mut agent, generation, prepared.channel, operation);
    }
    assert!(matches!(
        agent.next_effect(),
        Some(Effect::Timer(TimerEffect::Cancel { .. }))
    ));
    assert!(agent.next_effect().is_none());
}

#[test]
fn reliable_publisher_uses_active_signaling_for_admission_repair_and_delivery() {
    use crate::ffi::{ReliableDelivery, ReliablePublishEvent};
    use core::num::NonZeroU64;
    use pulsebeam_proto::{
        data_control::{
            self, BranchUnit, ControlOutcome, DeliveryOutcome, Expectation, OriginalDemand,
            PublisherBinding, PublisherObservation, SourceUnit, publisher_observation::Record,
        },
        data_endpoint::DATA_V1,
    };
    let publisher = TopicPublisher {
        topic: "cursor".into(),
        mode: TopicMode::Ordered,
    };
    let spec = TrackSpec::Data {
        track_id: publisher.topic.clone(),
        delivery: publisher.mode,
    };
    let (mut agent, generation, signaling) = connected_with_data_profile(
        crate::TopicRegistrations {
            publishers: vec![publisher],
            ..Default::default()
        },
        Some(u32::from(DATA_V1)),
        None,
    );
    agent.enable_reliable_data_fixture();
    let prepared = prepare_data(&mut agent, generation, signaling);
    assert_eq!(agent.snapshot().topics.publishers[0].publication_id, None);
    confirm_publication(&mut agent, generation, signaling, &prepared, SOURCE);
    assert_eq!(
        agent.snapshot().topics.publishers[0].channel,
        Some(prepared.channel)
    );
    assert_eq!(
        agent.snapshot().topics.publishers[0]
            .publication_id
            .as_deref(),
        Some(SOURCE)
    );
    let notify = |event| Notification::ReliableData {
        track_id: "cursor".into(),
        generation,
        source: 3,
        event,
    };
    let sent = agent.send_data(spec.clone(), b"message".to_vec()).unwrap();
    assert!(drain_notifications(&mut agent).contains(&notify(
        ReliablePublishEvent::LocallyAdmitted {
            send: sent.get(),
            unit_seq: 0
        }
    )));

    fn next_record(agent: &mut Agent, channel: ChannelId) -> (OperationId, Vec<u8>) {
        loop {
            match next_effect(agent) {
                Effect::DataChannel(DataChannelEffect::Send {
                    operation,
                    channel: actual,
                    payload,
                    ..
                }) if actual == channel => return (operation, payload),
                Effect::Timer(_) => {}
                effect => panic!("unexpected {effect:?}"),
            }
        }
    }
    let (operation, original) = next_record(&mut agent, prepared.channel);
    acknowledge_send(&mut agent, generation, prepared.channel, operation);
    let retry = loop {
        match next_effect(&mut agent) {
            Effect::Timer(TimerEffect::Schedule { timer, after })
                if after == Duration::from_millis(100) =>
            {
                break timer;
            }
            Effect::Timer(_) => {}
            effect => panic!("unexpected {effect:?}"),
        }
    };
    agent
        .handle(HostEvent::Timer(TimerEvent::Fired { timer: retry }))
        .unwrap();
    let (operation, retry) = next_record(&mut agent, prepared.channel);
    assert_eq!(retry, original);
    acknowledge_send(&mut agent, generation, prepared.channel, operation);

    let endpoint = PublisherBinding {
        source: NonZeroU64::new(3).unwrap(),
    };
    let report = |agent: &mut Agent, channel, record| {
        let payload = codec::encode_data_control(&state::NativeDataControl {
            endpoint: Some(state::native_data_control::Endpoint::Publisher(
                state::OwnedDataControl {
                    owned: prepared.reference,
                    record: data_control::encode(
                        endpoint,
                        &PublisherObservation {
                            record: Some(record),
                        },
                    )
                    .unwrap(),
                },
            )),
        })
        .unwrap();
        agent
            .handle(HostEvent::DataChannel(DataChannelEvent::Message {
                generation,
                channel,
                binary: true,
                payload,
            }))
            .unwrap();
    };
    let unit = SourceUnit {
        unit_seq: Some(0),
        source_incarnation: 3,
    };
    let ack = notify(ReliablePublishEvent::SourceAdmitted { unit_seq: 0 });
    report(&mut agent, prepared.channel, Record::Admission(unit));
    assert!(!drain_notifications(&mut agent).contains(&ack));
    report(&mut agent, signaling, Record::Admission(unit));
    assert!(drain_notifications(&mut agent).contains(&ack));
    report(&mut agent, signaling, Record::Admission(unit));
    assert!(!drain_notifications(&mut agent).contains(&ack));
    let delivery = DeliveryOutcome {
        expectation: Some(Expectation {
            unit: Some(BranchUnit {
                source: Some(unit),
                branch_incarnation: 7,
            }),
            fragment_count: 1,
        }),
        outcome: ControlOutcome::Confirmed as i32,
    };
    report(&mut agent, signaling, Record::Delivery(delivery));
    assert!(
        drain_notifications(&mut agent).contains(&notify(ReliablePublishEvent::Delivery {
            unit_seq: 0,
            branch: 7,
            outcome: ReliableDelivery::Confirmed
        }))
    );

    let demand = OriginalDemand {
        unit: Some(unit),
        fragment_index: Some(0),
        request_id: 1,
    };
    report(
        &mut agent,
        prepared.channel,
        Record::RequestOriginal(demand),
    );
    while let Some(effect) = agent.next_effect_after_turn() {
        assert!(
            matches!(effect, Effect::Timer(_)),
            "wrong-channel request produced {effect:?}"
        );
    }
    report(&mut agent, signaling, Record::RequestOriginal(demand));
    report(&mut agent, signaling, Record::RequestOriginal(demand));
    let (operation, repair) = next_record(&mut agent, prepared.channel);
    assert_eq!(repair, original);
    report(&mut agent, signaling, Record::RequestOriginal(demand));
    acknowledge_send(&mut agent, generation, prepared.channel, operation);

    let missing = OriginalDemand {
        fragment_index: Some(1),
        request_id: 2,
        ..demand
    };
    report(&mut agent, signaling, Record::RequestOriginal(missing));
    let (operation, response) = next_record(&mut agent, signaling);
    let codec::NativeFrame::Data(response) = codec::decode_client_frame(&response).unwrap() else {
        panic!("expected protected repair response")
    };
    let Some(state::native_data_control::Endpoint::Publisher(response)) = response.endpoint else {
        panic!("expected publisher response")
    };
    assert_eq!(response.owned, prepared.reference);
    assert_eq!(
        data_control::decode::<data_control::PublisherControl>(endpoint, &response.record).unwrap(),
        data_control::PublisherControl {
            record: Some(data_control::publisher_control::Record::Unavailable(
                data_control::OriginalUnavailable {
                    demand: Some(missing)
                }
            )),
        }
    );
    acknowledge_send(&mut agent, generation, signaling, operation);
    while let Some(effect) = agent.next_effect_after_turn() {
        assert!(
            matches!(effect, Effect::Timer(_)),
            "duplicate request sent again: {effect:?}"
        );
    }

    agent.send_data(spec, b"unconfirmed".to_vec()).unwrap();
    let _ = next_record(&mut agent, prepared.channel);
    let close = || {
        HostEvent::DataChannel(DataChannelEvent::Closed {
            generation,
            channel: prepared.channel,
            reset_complete: true,
        })
    };
    let unknown = notify(ReliablePublishEvent::AdmissionUnknown { unit_seq: 1 });
    agent.handle(close()).unwrap();
    assert!(drain_notifications(&mut agent).contains(&unknown));
    agent.handle(close()).unwrap();
    report(
        &mut agent,
        signaling,
        Record::Admission(SourceUnit {
            unit_seq: Some(1),
            ..unit
        }),
    );
    assert!(
        !drain_notifications(&mut agent)
            .iter()
            .any(|note| matches!(note, Notification::ReliableData { .. }))
    );
}

fn fabric_installed(
    reference: u64,
    selector: TrackSelector,
    offer: state::DataV1ReceiverOffer,
    binding: state::TrackBinding,
) -> state::ServerStateRecord {
    let spec = state::ResourceSpec {
        settings: Some(state::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(state::resource_spec::Declaration::LogicalSubscription(
            state::LogicalSubscriptionSpec {
                data_version: Some(u32::from(pulsebeam_proto::data_endpoint::DATA_V1)),
                source: Some(state::LogicalSourceAddress {
                    room: selector.room,
                    participant_id: selector.participant_id,
                    track_id: selector.track_id,
                    kind: state::TrackKind::Data as i32,
                }),
            },
        )),
    };
    let mut record = lookup_record(
        reference,
        reference,
        spec,
        REMOTE_SOURCE,
        Some(TopicMode::Latest),
        Some(offer),
    );
    let Some(state::confirmed_state::Value::LogicalSubscription(logical)) = record
        .confirmed
        .as_mut()
        .and_then(|confirmed| confirmed.value.as_mut())
    else {
        panic!("logical receiver state")
    };
    logical.execution = Some(state::logical_subscription_state::Execution::Installed(
        binding,
    ));
    record
}

pub(crate) fn prepare_data_pending(
    agent: &mut Agent,
    generation: Generation,
    control: ChannelId,
) -> (PreparedData, OperationId, Vec<u8>) {
    let (channel, channel_spec) = prepare_channel(agent, generation, control);
    let mut channels = vec![(channel, channel_spec)];
    let mut effect = next_without_data_timer(agent);
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
                effect = next_without_data_timer(agent);
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
            let stream_id = match binding.transport {
                Some(state::track_binding::Transport::Sctp(sctp)) => sctp.stream_id,
                Some(state::track_binding::Transport::SctpDataV1(sctp)) => sctp.stream_id,
                _ => panic!("SCTP binding"),
            };
            let index = channels
                .iter()
                .position(|(_, spec)| spec.negotiated_id.map(u32::from) == stream_id)
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
                data_v1_source: matches!(
                    data.binding
                        .as_ref()
                        .and_then(|binding| binding.transport.as_ref()),
                    Some(state::track_binding::Transport::SctpDataV1(_))
                )
                .then_some(3),
                spec: Some(publication.spec.clone()),
                participant: Some(participant.clone()),
                track: Some(state::Track {
                    publication_id: source.into(),
                    participant_id: participant.participant_id,
                    room_id: participant.room_id,
                    track_id: declaration.track_id.clone(),
                    kind: Some(state::track::Kind::Data(state::DataTrack {
                        capture_clock: data.capture_clock.clone(),
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

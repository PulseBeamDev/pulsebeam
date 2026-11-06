use super::*;
use crate::media::ChannelId;
use crate::participant::event::test_utils::MockParticipantSink;
use crate::participant::signaling::{SignalingChannel, SignalingProgress};
use pulsebeam_proto::state::{codec, resource_spec};

pub(in crate::participant::core) struct NativeParticipant {
    endpoint: Connection,
    pub(in crate::participant::core) signaling: crate::control::native::fixture::NativeControl,
    sources: BTreeMap<[u8; 16], (TrackHandle, Track)>,
}

impl std::ops::Deref for NativeParticipant {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &self.endpoint
    }
}

impl std::ops::DerefMut for NativeParticipant {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.endpoint
    }
}

impl NativeParticipant {
    fn service_control(&mut self) {
        let sources = &self.sources;
        if let Err(error) = self
            .signaling
            .service(|source| sources.get(source.as_bytes()).map(|(_, track)| track))
        {
            self.endpoint.disconnect(
                if matches!(error, crate::control::native::InputError::Output(_)) {
                    DisconnectReason::SignalingOutput
                } else {
                    DisconnectReason::SignalingInput
                },
            );
        }
    }

    fn pump(&mut self, sink: &mut impl ParticipantSink) {
        for _ in 0..32 {
            self.service_control();
            if self.endpoint.disconnect_reason.is_some() {
                return;
            }
            if !self
                .endpoint
                .native_handoff
                .as_mut()
                .unwrap()
                .commands_pending()
            {
                return;
            }
            self.endpoint
                .begin_native_control_turn(Instant::now(), sink);
            for (handle, track) in self.sources.values() {
                if self.endpoint.native_receivers.contains_source(track.id())
                    && self.endpoint.downstream.track_candidate(*handle).is_none()
                {
                    self.endpoint.add_track_candidate(*handle, track.clone());
                }
            }
        }
        panic!("bounded controller/endpoint fixture did not quiesce");
    }

    pub(in crate::participant::core) fn handle_native_signaling(
        &mut self,
        bytes: &[u8],
        sink: &mut impl ParticipantSink,
    ) {
        self.endpoint.handle_event(
            Instant::now(),
            Event::DataChannel(DataChannelEvent::Message {
                channel: ChannelId::from_value(0),
                message: DataMessage::Binary(bytes.to_vec().into()),
            }),
            sink,
        );
        self.pump(sink);
    }

    pub(in crate::participant::core) fn add_track_candidate(
        &mut self,
        handle: TrackHandle,
        track: Track,
    ) {
        self.signaling.observe_candidate(&track).unwrap();
        self.sources
            .insert(*track.id().as_bytes(), (handle, track.clone()));
        self.endpoint.add_track_candidate(handle, track);
    }

    pub(in crate::participant::core) fn remove_track_candidate(
        &mut self,
        handle: TrackHandle,
        track: TrackId,
    ) {
        self.signaling.remove_track(track.as_str()).unwrap();
        self.sources.remove(track.as_bytes());
        self.endpoint.remove_track_candidate(handle, track);
    }

    pub(in crate::participant::core) fn advance_native_source(
        &mut self,
        sink: &mut impl ParticipantSink,
    ) -> bool {
        self.pump(sink);
        let progressed = self.endpoint.advance_native_source(sink);
        self.service_control();
        progressed
    }

    pub(in crate::participant::core) fn execute_native_command(
        &mut self,
        command: ExecutionCommand,
        sink: &mut impl ParticipantSink,
    ) {
        self.signaling.command(command);
        self.pump(sink);
    }

    pub(in crate::participant::core) fn retire_native_resource(
        &mut self,
        reference: u64,
        sink: &mut impl ParticipantSink,
    ) {
        let command = self.signaling.retirement_command(reference).unwrap();
        self.execute_native_command(command, sink);
    }

    fn complete_native_preparation(
        &mut self,
        work: Preparation,
        result: Result<ResourceExecution, ExecutionFailure>,
    ) {
        self.endpoint.complete_native_preparation(work, result);
        self.service_control();
    }
}

pub(in crate::participant::core) fn participant() -> NativeParticipant {
    participant_with_expiry(unix_now() + 60)
}

pub(in crate::participant::core) fn participant_with_expiry(expiry: u64) -> NativeParticipant {
    participant_with_capabilities(expiry, None)
}

pub(in crate::participant::core) fn participant_with_capabilities(
    expiry: u64,
    capabilities: Option<wire::RtpCapabilities>,
) -> NativeParticipant {
    let (mut rtc, mut resources, _) =
        crate::control::application_only_test_connection_and_binding();
    let capabilities = capabilities.unwrap_or_else(crate::control::rtp::capabilities);
    crate::control::rtp::initialize(&mut rtc.connection, &capabilities).unwrap();
    resources.signaling_channel = Some(SignalingChannel::compile(
        &DataTrackIntent::native_signaling_config(),
    ));
    let mut config = ConnectionConfig {
        manual_sub: true,
        room_id: entity::RoomId::from_external(
            &entity::RoomExternalId::new("native-data").unwrap(),
        ),
        participant_id: entity::ParticipantId::new(),
        participant_external_id: entity::ParticipantExternalId::new("publisher").unwrap(),
        connection_id: entity::ConnectionId::new(),
        profile: SignalingProtocol::State,
        allow_topic_channels: false,
        authorization_expiry: Some(
            pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds(expiry),
        ),
        rtc,
        resources,
    };
    let (signaling, endpoint) = crate::control::native::fixture::NativeControl::new(
        config.participant_id,
        config.connection_id,
        config.room_id,
        config.participant_external_id.clone(),
        Some(capabilities),
        expiry,
    );
    config.resources.native_handoff = Some(endpoint);
    let mut participant = NativeParticipant {
        endpoint: Connection::new(config, ShardId::new(0), 8),
        signaling,
        sources: BTreeMap::new(),
    };
    participant.handle_event(
        Instant::now(),
        Event::DataChannel(DataChannelEvent::Opened {
            channel: ChannelId::from_value(0),
            config: DataTrackIntent::native_signaling_config(),
        }),
        &mut MockParticipantSink::new(),
    );
    participant.service_control();
    participant
}

fn declaration() -> wire::ResourceSpec {
    wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(resource_spec::Declaration::Publication(
            wire::PublicationSpec {
                label: "camera/updates".into(),
                kind: Some(wire::publication_spec::Kind::Data(wire::DataPublication {
                    delivery: wire::DataDelivery::Ordered as i32,
                    binding: Some(wire::TrackBinding {
                        transport: Some(wire::track_binding::Transport::Sctp(
                            wire::SctpParameters { stream_id: Some(1) },
                        )),
                    }),
                })),
            },
        )),
    }
}

pub(in crate::participant::core) fn frame(
    revision: u64,
    value: wire::desired_state::Value,
) -> Vec<u8> {
    frame_for(1, revision, value)
}

pub(in crate::participant::core) fn frame_for(
    reference: u64,
    revision: u64,
    value: wire::desired_state::Value,
) -> Vec<u8> {
    codec::encode_client(&wire::ClientStateBatch {
        records: vec![wire::ClientStateRecord {
            owned: reference,
            execution: None,
            desired: Some(wire::DesiredState {
                revision,
                value: Some(value),
            }),
        }],
    })
    .unwrap()
}

pub(in crate::participant::core) fn confirmations(
    participant: &mut NativeParticipant,
) -> Vec<wire::ConfirmedState> {
    confirmations_for(participant, 1)
}

pub(in crate::participant::core) fn confirmations_for(
    participant: &mut NativeParticipant,
    reference: u64,
) -> Vec<wire::ConfirmedState> {
    let mut values = Vec::new();
    for _ in 0..32 {
        participant.service_control();
        if let Some(output) = participant
            .endpoint
            .native_handoff
            .as_mut()
            .unwrap()
            .output()
        {
            let batch = codec::decode_server(&output.bytes).unwrap();
            for record in batch.records {
                if record.key.and_then(|key| key.key)
                    == Some(wire::resource_key::Key::Owned(reference))
                    && let Some(value) = record.confirmed
                {
                    values.push(value);
                }
            }
            // Projection fixture injects admission; connected replay proves real RTC admission.
            assert!(
                participant
                    .endpoint
                    .native_handoff
                    .as_ref()
                    .unwrap()
                    .progress(SignalingProgress {
                        sequence: output.sequence,
                        cid: output.cid,
                        at: Instant::now(),
                    })
            );
        } else if !participant.signaling.needs_poll() {
            return values;
        }
    }
    panic!("bounded control output did not quiesce");
}

#[test]
fn reserved_channel_uses_control_contract_without_label_grammar() {
    let mut participant = participant();
    let mut sink = MockParticipantSink::new();
    let mut config = DataTrackIntent::native_signaling_config();
    config.label = "opaque control channel".into();
    config.protocol = "opaque transport protocol".into();
    participant.negotiated.signaling_channel = Some(SignalingChannel::compile(&config));
    config.priority = pulsebeam_rtc::DataChannelPriority::HIGH;
    participant.handle_event(
        Instant::now(),
        Event::DataChannel(DataChannelEvent::Opened {
            channel: ChannelId::from_value(0),
            config: config.clone(),
        }),
        &mut sink,
    );
    assert!(participant.disconnect_reason.is_none());
    assert_eq!(participant.signaling.cid, Some(ChannelId::from_value(0)));
    config.protocol = "wrong protocol".into();
    participant.handle_event(
        Instant::now(),
        Event::DataChannel(DataChannelEvent::Opened {
            channel: ChannelId::from_value(0),
            config,
        }),
        &mut sink,
    );
    assert!(matches!(
        participant.disconnect_reason,
        Some(DisconnectReason::SignalingInput)
    ));
}

#[test]
fn declaration_without_a_usable_binding_is_not_invented_execution() {
    for invalid_spec in [false, true] {
        let mut participant = participant();
        let mut sink = MockParticipantSink::new();
        let mut spec = declaration();
        if invalid_spec {
            let Some(resource_spec::Declaration::Publication(publication)) =
                spec.declaration.as_mut()
            else {
                panic!("publication");
            };
            let Some(wire::publication_spec::Kind::Data(data)) = publication.kind.as_mut() else {
                panic!("data");
            };
            data.binding = None;
        }
        participant.handle_native_signaling(
            &frame(1, wire::desired_state::Value::Present(Box::new(spec))),
            &mut sink,
        );
        assert_eq!(
            participant.native_execution.sequence,
            if invalid_spec { 0 } else { 1 }
        );
        assert!(participant.native_execution.pending.is_empty());
        assert!(participant.disconnect_reason.is_none());
        assert!(participant.native_data.publications.is_empty());
        assert!(participant.native_data.channels.is_empty());
        assert!(participant.native_data.tracks.is_empty());
        assert!(participant.native_data.fanouts.is_empty());
        assert!(participant.native_data.deadlines.is_empty());
        assert!(sink.publish_track_calls.is_empty());
        let values = confirmations(&mut participant);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].evaluated_revision, 1);
        assert_eq!(
            values[0].rejection.as_ref().unwrap().code,
            if invalid_spec {
                wire::FailureCode::InvalidConfiguration
            } else {
                wire::FailureCode::Incompatible
            } as i32
        );
        assert!(matches!(
            values[0].value,
            Some(wire::confirmed_state::Value::Absent(_))
        ));
    }
}

#[test]
fn contradictory_execution_fact_terminates_its_connection() {
    let mut participant = participant();
    participant
        .signaling
        .apply_input(&frame(
            1,
            wire::desired_state::Value::Present(Box::new(declaration())),
        ))
        .unwrap();
    let work = participant.signaling.take_preparation().unwrap().unwrap();
    let _command = participant
        .signaling
        .prepare_execution(&work, unix_now(), None, |_| panic!("publication context"))
        .unwrap()
        .unwrap();
    assert!(participant.admit_native_preparation(work));
    participant.complete_native_preparation(
        work,
        Ok(ResourceExecution::Receiver(
            crate::participant::policy::ReceiverExecution {
                source: participant
                    .connection_id
                    .derive_track_id(TrackKind::Data, "contradiction"),
                enabled: true,
                ready: true,
                failed: false,
            },
        )),
    );
    assert!(participant.native_execution.pending.is_empty());
    assert!(matches!(
        participant.disconnect_reason,
        Some(DisconnectReason::SignalingOutput)
    ));
}

#[test]
fn repeated_retirement_keeps_the_receipt_and_does_not_erase_a_reused_channel_index() {
    let mut participant = participant();
    participant
        .signaling
        .apply_input(&frame(
            1,
            wire::desired_state::Value::Present(Box::new(declaration())),
        ))
        .unwrap();
    let work = participant.signaling.take_preparation().unwrap().unwrap();
    let in_flight_duplicate = participant
        .signaling
        .prepare_execution(&work, unix_now(), None, |_| panic!("publication context"))
        .unwrap()
        .unwrap();
    assert_eq!(participant.native_execution.sequence, 0);
    assert!(!participant.native_execution.contains(work.scope()));
    assert!(participant.admit_native_preparation(work));
    assert!(!participant.admit_native_preparation(work));
    assert!(participant.native_execution.contains(work.scope()));
    assert!(
        participant
            .signaling
            .preparation_current(&work, unix_now())
            .unwrap()
    );
    assert!(participant.disconnect_reason.is_none());
    let delayed = participant
        .signaling
        .prepare_execution(&work, unix_now(), None, |_| panic!("publication context"))
        .unwrap()
        .unwrap();
    let duplicate = participant
        .signaling
        .prepare_execution(&work, unix_now(), None, |_| panic!("publication context"))
        .unwrap()
        .unwrap();
    let id = participant.connection_id.derive_publication_track_id(
        TrackKind::Data,
        "camera/updates",
        NonZeroU64::new(1).unwrap(),
    );
    let channel = crate::media::ChannelId::from_value(1);
    let track = Track::data_publication(
        track::TrackMeta {
            room_id: participant.room_id,
            shard_id: participant.shard_id,
            id,
            origin: participant.participant_id,
            connection_id: participant.connection_id,
            label: Some("camera/updates".into()),
        },
        DataLane::Ordered,
    );
    assert_eq!(track.publication_label().as_deref(), Some("camera/updates"));
    assert!(track.requires_reverse_route());
    // Exercise the retained runtime receipt directly, not SCTP admission or an Installed claim.
    participant.native_data.channels.insert(channel, 1);
    participant.native_data.tracks.insert(*id.as_bytes(), 1);
    let installation = participant
        .transport
        .install_data_channel(crate::participant::transport::native_data_config(
            channel,
            DataLane::Ordered,
        ))
        .unwrap();
    participant.native_data.publications.insert(
        1,
        DataPublication {
            channel,
            track,
            source_id: id.as_str(),
            fanout: None,
            enabled: false,
            opened: false,
            installation,
            published: false,
            retiring: false,
            preparation: Some(work),
            storage_sequence: work.scope().sequence,
            deadline: None,
        },
    );
    let mut sink = MockParticipantSink::new();
    participant.execute_native_command(in_flight_duplicate, &mut sink);
    assert!(participant.native_execution.contains(work.scope()));
    assert!(
        participant
            .signaling
            .preparation_current(&work, unix_now())
            .unwrap()
    );
    assert!(
        participant.native_data.publications[&1]
            .preparation
            .is_some()
    );
    assert!(!participant.native_data.publications[&1].retiring);
    let sequence = participant.native_execution.sequence;
    participant.native_data_closed(channel, &mut sink);
    assert_eq!(participant.native_execution.sequence, sequence);
    participant.native_data_opened(
        channel,
        crate::participant::transport::native_data_config(channel, DataLane::Ordered),
        &mut sink,
    );
    let pending = participant.native_data.publications.get_mut(&1).unwrap();
    assert!(
        !pending.opened && !pending.published && !pending.retiring && pending.preparation.is_some()
    );
    pending.enabled = true;
    pending.opened = true;
    pending.published = true;
    use pulsebeam_proto::prelude::Message;
    let mut handles = slotmap::SlotMap::<TrackHandle, ()>::with_key();
    let handle = handles.insert(());
    participant.native_data.bind_source(id, handle);
    let message = |source| {
        TopicFrame {
            body: Some(pulsebeam_proto::topic::topic_frame::Body::Message(
                pulsebeam_proto::topic::TopicMessage {
                    data_track_id: source,
                    sequence: 0,
                    payload: vec![42],
                },
            )),
        }
        .encode_to_vec()
    };
    participant.native_data_message(channel, &message("foreign".into()), &mut sink);
    assert!(sink.publish_track_packet_calls.is_empty());
    participant.native_data_message(channel, &message(id.as_str()), &mut sink);
    assert_eq!(sink.publish_track_packet_calls, vec![handle]);
    let probe = |source| {
        TopicFrame {
            body: Some(pulsebeam_proto::topic::topic_frame::Body::Probe(
                pulsebeam_proto::topic::TopicProbe {
                    data_track_id: source,
                    from_sequence: 0,
                    live: true,
                },
            )),
        }
        .encode_to_vec()
    };
    participant.native_data_message(channel, &probe(id.as_str()), &mut sink);
    assert_eq!(sink.publish_track_packet_calls, vec![handle]);
    participant.on_reverse(
        handle,
        ReversePacket::reliable_control(probe("foreign".into())),
    );
    assert!(
        participant
            .transport
            .apply_next_mutation(Instant::now(), |_| true)
            .is_none()
    );
    participant.on_reverse(handle, ReversePacket::reliable_control(probe(id.as_str())));
    assert!(matches!(
        participant
            .transport
            .apply_next_mutation(Instant::now(), |_| true),
        Some(AppliedMutation::ControlNotWritten)
    ));
    participant.on_reverse(handle, ReversePacket::reliable_control(probe(id.as_str())));
    let sequence = participant.native_execution.sequence;
    assert!(!participant.native_callback_fenced(1));
    participant.fence_native_resource(1);
    assert!(participant.native_callback_fenced(1));
    assert!(
        participant
            .signaling
            .preparation_current(&work, unix_now())
            .unwrap()
    );
    assert_eq!(participant.native_execution.sequence, sequence);
    assert!(!participant.native_data.can_write(channel));
    assert!(!participant.native_execution.contains(work.scope()));
    participant.defer_native_preparation(work);
    participant.service_control();
    participant
        .endpoint
        .begin_native_control_turn(Instant::now(), &mut sink);
    assert!(participant.native_execution.sequence > sequence);
    assert!(matches!(
        participant.native_execution.pending.get(&1),
        Some(NativeExecutionState::Fenced)
    ));
    assert!(!participant.native_data.publications[&1].enabled);
    assert_eq!(
        participant.native_data.publications[&1].storage_sequence,
        work.scope().sequence
    );
    participant
        .transport
        .cancel_data_channel_installation(installation);
    let sequence = participant.native_execution.sequence;
    participant.native_data_closed(channel, &mut sink);
    participant.pump(&mut sink);
    assert_eq!(participant.native_execution.sequence, sequence + 1);
    assert!(participant.native_execution.pending.is_empty());
    assert!(participant.native_data.fanouts.is_empty());
    participant.on_reverse(handle, ReversePacket::reliable_control(probe(id.as_str())));
    assert!(
        participant
            .transport
            .apply_next_mutation(Instant::now(), |_| true)
            .is_none()
    );
    participant.native_data_message(channel, &message(id.as_str()), &mut sink);
    assert_eq!(sink.publish_track_packet_calls, vec![handle]);
    assert!(participant.native_data.publications[&1].retiring);
    assert!(participant.current_native_preparation(work).is_none());
    let sequence = participant.native_execution.sequence;
    participant.execute_native_command(delayed, &mut sink);
    assert_eq!(participant.native_execution.sequence, sequence);
    assert!(participant.native_data.publications[&1].retiring);
    assert!(!participant.native_data.publications[&1].enabled);
    for revision in [2, 3] {
        participant.handle_native_signaling(
            &frame(revision, wire::desired_state::Value::Absent(true)),
            &mut sink,
        );
        assert!(participant.native_data.publications[&1].retiring);
        assert!(!participant.native_data.publications[&1].enabled);
        assert!(participant.native_data.channels.is_empty());
        assert!(
            confirmations(&mut participant)
                .iter()
                .all(|value| value.evaluated_revision < 2)
        );
    }
    assert_eq!(sink.unpublish_track_calls, vec![id]);
    // A replacement SID owner can exist after reset while the old graph receipt is pending.
    participant.native_data.channels.insert(channel, 5);
    participant.complete_native_data_cutoff(1, true);
    assert_eq!(participant.native_data.channels.get(&channel), Some(&5));
    participant.execute_native_command(duplicate, &mut sink);
    assert_eq!(participant.native_data.channels.get(&channel), Some(&5));
    assert!(!participant.native_data.publications.contains_key(&1));
    let values = confirmations(&mut participant);
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].evaluated_revision, 3);
    assert!(
        matches!(&values[0].value, Some(wire::confirmed_state::Value::Absent(absent)) if absent.reason == wire::AbsenceReason::Removed as i32)
    );
    assert!(participant.disconnect_reason.is_none());
    assert!(
        participant
            .transport
            .enqueue_mutation(TransportMutation::Data {
                channel,
                bytes: probe(id.as_str()),
                required: true
            })
    );
    participant.authorization_expiry =
        Some(pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds(unix_now()));
    assert!(!participant.apply_one_rtc_mutation(Instant::now()));
    assert!(participant.disconnect_reason.is_none());
    assert!(
        participant
            .transport
            .apply_next_mutation(Instant::now(), |_| true)
            .is_none()
    );
}

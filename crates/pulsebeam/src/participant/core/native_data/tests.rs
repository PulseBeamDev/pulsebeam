use super::*;
use crate::participant::event::test_utils::MockParticipantSink;
use pulsebeam_proto::state::codec;

pub(in crate::participant::core) fn participant() -> Participant {
    participant_with_expiry(unix_now() + 60)
}

pub(in crate::participant::core) fn participant_with_expiry(expiry: u64) -> Participant {
    participant_with_capabilities(expiry, None)
}

pub(in crate::participant::core) fn participant_with_capabilities(
    expiry: u64,
    capabilities: Option<wire::RtpCapabilities>,
) -> Participant {
    let (mut rtc, mut resources, _) =
        crate::control::application_only_test_connection_and_binding();
    let capabilities = capabilities.unwrap_or_else(crate::participant::rtp::capabilities);
    crate::participant::rtp::initialize(&mut rtc.connection, &capabilities).unwrap();
    resources.native_rtp_capabilities = Some(capabilities);
    let mut participant = Participant::new(
        ParticipantConfig {
            manual_sub: true,
            room_id: entity::RoomId::from_external(
                &entity::RoomExternalId::new("native-data").unwrap(),
            ),
            participant_id: entity::ParticipantId::new(),
            participant_external_id: entity::ParticipantExternalId::new("publisher").unwrap(),
            connection_id: entity::ConnectionId::new(),
            profile: ConnectionProfile::Native,
            authorization_expiry: Some(
                pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds(expiry),
            ),
            rtc,
            resources,
        },
        ShardId::new(0),
        8,
    );
    participant
        .signaling
        .set_cid(crate::media::ChannelId::from_value(0));
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
    participant: &mut Participant,
) -> Vec<wire::ConfirmedState> {
    confirmations_for(participant, 1)
}

pub(in crate::participant::core) fn confirmations_for(
    participant: &mut Participant,
    reference: u64,
) -> Vec<wire::ConfirmedState> {
    let mut values = Vec::new();
    for _ in 0..32 {
        if !participant.signaling.needs_poll() {
            return values;
        }
        if let Some(output) = participant.signaling.poll().unwrap() {
            let batch = codec::decode_server(&output.bytes).unwrap();
            for record in batch.records {
                if record.key.and_then(|key| key.key)
                    == Some(wire::resource_key::Key::Owned(reference))
                    && let Some(value) = record.confirmed
                {
                    values.push(value);
                }
            }
            participant.signaling.commit_sent();
        }
    }
    panic!("bounded control output did not quiesce");
}

#[test]
fn declaration_without_a_connected_sctp_association_is_not_invented_execution() {
    let mut participant = participant();
    let mut sink = MockParticipantSink::new();
    participant.handle_native_signaling(
        &frame(
            1,
            wire::desired_state::Value::Present(Box::new(declaration())),
        ),
        &mut sink,
    );
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
        wire::FailureCode::Incompatible as i32
    );
    assert!(matches!(
        values[0].value,
        Some(wire::confirmed_state::Value::Absent(_))
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
    participant
        .signaling
        .reserve_execution(1, execution_bytes("camera/updates"))
        .unwrap();
    // Exercise the retained runtime receipt directly, not SCTP admission or an Installed claim.
    participant.native_data.channels.insert(channel, 1);
    participant.native_data.tracks.insert(*id.as_bytes(), 1);
    participant.native_data.publications.insert(
        1,
        DataPublication {
            channel,
            track,
            source_id: id.as_str(),
            fanout: None,
            enabled: false,
            opened: false,
            reset_pending: true,
            published: false,
            retiring: false,
            preparation: Some(work),
            deadline: None,
        },
    );
    let mut sink = MockParticipantSink::new();
    participant.native_data_closed(channel, &mut sink);
    participant.native_data_opened(
        channel,
        crate::participant::transport::native_data_config(channel, DataLane::Ordered),
        &mut sink,
    );
    let pending = participant.native_data.publications.get_mut(&1).unwrap();
    assert!(
        !pending.opened && !pending.published && !pending.retiring && pending.preparation.is_some()
    );
    pending.reset_pending = false;
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
    participant.native_data_reverse(handle, probe("foreign".into()));
    assert!(
        participant
            .transport
            .apply_next_mutation(Instant::now(), |_| true)
            .is_none()
    );
    participant.native_data_reverse(handle, probe(id.as_str()));
    assert!(matches!(
        participant
            .transport
            .apply_next_mutation(Instant::now(), |_| true),
        Some(AppliedMutation::ControlNotWritten)
    ));
    participant.native_data_reverse(handle, probe(id.as_str()));
    participant.native_data_closed(channel, &mut sink);
    assert!(participant.native_data.fanouts.is_empty());
    assert!(
        participant
            .transport
            .apply_next_mutation(Instant::now(), |_| true)
            .is_none()
    );
    participant.native_data_message(channel, &message(id.as_str()), &mut sink);
    assert_eq!(sink.publish_track_packet_calls, vec![handle]);
    assert!(participant.native_data.publications[&1].retiring);
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

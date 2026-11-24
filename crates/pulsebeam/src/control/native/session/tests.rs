use super::*;

mod catalog_watch;
mod discovery;
mod permissions;

#[test]
fn quic_native_session_rejects_wrong_carrier_as_a_scoped_proposal() {
    let mut model = NativeSession::with_binding_profile(validation::BindingProfile::Quic);
    model
        .authorization(900, 1, ResourcePermissions::RoomBound)
        .unwrap();
    input(&mut model, vec![present(1, 1, subscription("source"))]).unwrap();
    let row = &model.rows[&Key::Owned(1)].value;
    assert_eq!(row.evaluated_revision, 1);
    assert_eq!(
        row.rejection.as_ref().unwrap().code,
        wire::FailureCode::InvalidConfiguration as i32
    );
    assert!(model.take_preparation().unwrap().is_none());
    assert!(!model.closed);
}

#[test]
fn successor_receipt_updates_independently_of_grant_and_is_cleared_on_close() {
    let mut session = NativeSession::default();
    let receipt = |value, expires| wire::SuccessorReceipt {
        proof: vec![value],
        expires_at_unix_seconds: expires,
    };
    session
        .authorization(900, 1, ResourcePermissions::RoomBound)
        .unwrap();
    session.successor_receipt(receipt(1, 400)).unwrap();
    session.successor_receipt(receipt(2, 460)).unwrap();
    let state = session.session_state().unwrap();
    assert_eq!(state.authorization.as_ref().unwrap().revision, 1);
    assert_eq!(state.successor_receipt, Some(receipt(2, 460)));
    session
        .authorization(1200, 2, ResourcePermissions::RoomBound)
        .unwrap();
    assert_eq!(
        session.session_state().unwrap().successor_receipt,
        Some(receipt(2, 460))
    );
    session
        .close(wire::CloseReason::ParticipantReplaced)
        .unwrap();
    assert!(session.session_state().unwrap().successor_receipt.is_none());
    assert!(matches!(
        session.successor_receipt(receipt(3, 520)),
        Err(OutputError::InvalidObservation)
    ));
}

#[test]
fn missing_source_preparation_deadlines_expire_cancel_or_resolve_without_ending_the_source() {
    use crate::entity::{ConnectionId, ParticipantId, RoomId, TrackKind};
    use crate::participant::policy::ResourceObservation;
    use crate::track::{DataLane, TrackMeta};
    use pulsebeam_runtime::time::Instant;
    use std::{num::NonZeroU64, time::Duration};

    enum Outcome {
        Expire,
        Cancel,
        SourceArrives,
        SourceArrivesWithoutDiscovery,
        SourceArrivesAfterRenewal,
    }
    for outcome in [
        Outcome::Expire,
        Outcome::Cancel,
        Outcome::SourceArrives,
        Outcome::SourceArrivesWithoutDiscovery,
        Outcome::SourceArrivesAfterRenewal,
    ] {
        let mut model = NativeSession::default();
        model
            .authorization(99, 1, ResourcePermissions::RoomBound)
            .unwrap();
        let connection = ConnectionId::new();
        let source = connection.fixture_publication_at(
            TrackKind::Data,
            "not-yet-observed",
            NonZeroU64::new(1).unwrap(),
        );
        let spec = wire::ResourceSpec {
            settings: Some(wire::ResourceSettings {
                enabled: Some(true),
                receive: None,
            }),
            declaration: Some(wire::resource_spec::Declaration::Subscription(
                wire::SubscriptionSpec {
                    publication_id: source.as_str(),
                    kind: wire::TrackKind::Data as i32,
                    source_room: "room".into(),
                    binding: Some(crate::control::native::fixture::data_v1_binding(1)),
                },
            )),
        };
        let count = if matches!(outcome, Outcome::Expire) {
            SELECT_WORK.saturating_mul(2).saturating_add(1)
        } else {
            1
        };
        for index in 0..count {
            let reference = u64::try_from(index)
                .unwrap()
                .saturating_mul(2)
                .saturating_add(1);
            input(
                &mut model,
                vec![present(reference, reference, spec.clone())],
            )
            .unwrap();
        }
        assert!(!model.preparation_pending(100));
        assert!(model.source_waits.is_empty());
        model
            .authorization(10_000, 2, ResourcePermissions::RoomBound)
            .unwrap();
        let before = Instant::now();
        while model.preparation_pending(100) {
            for _ in 0..SELECT_WORK {
                let Some(work) = model.take_preparation().unwrap() else {
                    break;
                };
                assert!(
                    model
                        .prepare_execution(&work, 100, None, |_| None)
                        .unwrap()
                        .is_none()
                );
            }
        }
        assert_eq!(model.source_waits.len(), count);
        let wait = model.source_waits[&1].clone();
        assert!(wait.deadline >= before + Duration::from_secs(30));
        assert!(wait.deadline <= Instant::now() + Duration::from_secs(30));
        assert_eq!(
            model.deadline(),
            model.source_deadlines.first().map(|(at, _)| *at)
        );
        assert_eq!(
            model.rows[&Key::Owned(1)]
                .owned
                .as_ref()
                .unwrap()
                .execution_bytes,
            4096
        );
        model
            .service_source_waits(100, wait.deadline - Duration::from_nanos(1), |_| false)
            .unwrap();
        assert_eq!(model.source_waits.len(), count);
        assert!(!matches!(
            model.rows[&Key::Owned(1)].value.value,
            Some(wire::confirmed_state::Value::Owned(_))
        ));
        match outcome {
            Outcome::Expire => {
                let at = model.source_deadlines.last().unwrap().0;
                while !model.source_waits.is_empty() {
                    let remaining = model.source_waits.len();
                    model.service_source_waits(131, at, |_| false).unwrap();
                    assert_eq!(model.source_waits.len(), remaining.saturating_sub(16));
                }
                assert_eq!(
                    model.rows[&Key::Owned(1)]
                        .value
                        .rejection
                        .as_ref()
                        .unwrap()
                        .code,
                    wire::FailureCode::InternalFailure as i32
                );
                assert_eq!(
                    model.rows[&Key::Owned(1)]
                        .owned
                        .as_ref()
                        .unwrap()
                        .execution_bytes,
                    0
                );
            }
            Outcome::Cancel => {
                assert_eq!(input(&mut model, vec![absent(1, 1000)]).unwrap(), vec![1]);
                model
                    .service_source_waits(100, Instant::now(), |_| false)
                    .unwrap();
                model
                    .observe_resource(ResourceObservation::Released { binding: 1 })
                    .unwrap();
                assert_eq!(
                    model.rows[&Key::Owned(1)]
                        .owned
                        .as_ref()
                        .unwrap()
                        .execution_bytes,
                    0
                );
                assert!(
                    matches!(&model.rows[&Key::Owned(1)].value.value, Some(wire::confirmed_state::Value::Absent(value)) if value.reason == wire::AbsenceReason::Removed as i32)
                );
            }
            Outcome::SourceArrives
            | Outcome::SourceArrivesWithoutDiscovery
            | Outcome::SourceArrivesAfterRenewal => {
                let revision = if matches!(outcome, Outcome::SourceArrivesAfterRenewal) {
                    3
                } else {
                    2
                };
                if revision == 3 {
                    model
                        .authorization(10_000, revision, ResourcePermissions::RoomBound)
                        .unwrap();
                }
                let owner = ParticipantId::new("source").unwrap();
                let track = Track::data_publication(
                    TrackMeta {
                        room: crate::entity::ProjectRoomKey::fixture(RoomId::new("room").unwrap()),
                        id: source,
                        origin: owner,
                        connection_id: connection,
                        track_id: Some(crate::entity::TrackId::new("not-yet-observed").unwrap()),
                    },
                    DataLane::Ordered,
                );
                let publisher = owner;
                if matches!(outcome, Outcome::SourceArrivesWithoutDiscovery) {
                    let mut incomplete = track.clone();
                    incomplete.meta_mut().track_id = None;
                    let unresolved = super::super::ResolvedSource {
                        track: &incomplete,
                        publisher: &publisher,
                    };
                    assert!(!unresolved.is_available());
                    model.source_wait_changed();
                    model
                        .service_source_waits(100, Instant::now(), |_| unresolved.is_available())
                        .unwrap();
                    assert_eq!(model.source_waits[&1].deadline, wait.deadline);
                    model.source_wait_changed();
                } else {
                    model
                        .observe_participant(wire::Participant {
                            participant_id: owner.as_str().to_owned(),
                            room_id: "room".into(),
                        })
                        .unwrap();
                    model
                        .observe_candidate(super::super::ResolvedSource {
                            track: &track,
                            publisher: &publisher,
                        })
                        .unwrap();
                }
                model
                    .service_source_waits(100, Instant::now(), |id| id == source)
                    .unwrap();
                assert!(model.source_waits.is_empty());
                let work = model.take_preparation().unwrap().unwrap();
                assert!(work.scope().sequence > wait.work.scope().sequence);
                assert_eq!(work.grant_revision, revision);
                if matches!(outcome, Outcome::SourceArrivesWithoutDiscovery) {
                    let mut replacement = track.clone();
                    let replacement_connection = ConnectionId::new();
                    replacement.meta_mut().connection_id = replacement_connection;
                    replacement.meta_mut().id = replacement_connection.fixture_publication_at(
                        TrackKind::Data,
                        "not-yet-observed",
                        NonZeroU64::new(1).unwrap(),
                    );
                    let before_rejection = model.retained_bytes;
                    assert!(
                        matches!(model.compile_execution(&work, None, |_| Some(super::super::ResolvedSource { track: &replacement, publisher: &publisher })), Err(error) if error.code == wire::FailureCode::Incompatible as i32)
                    );
                    assert_eq!(model.retained_bytes, before_rejection);
                    assert!(
                        model.rows[&Key::Owned(1)]
                            .owned
                            .as_ref()
                            .unwrap()
                            .receiver_source
                            .is_none()
                    );
                }
                let before = model.retained_bytes;
                assert!(matches!(
                    model.compile_execution(&work, None, |_| Some(super::super::ResolvedSource {
                        track: &track,
                        publisher: &publisher,
                    })),
                    Err(error) if error.code() == wire::FailureCode::Incompatible
                ));
                assert_eq!(model.retained_bytes, before);
            }
        }
        assert!(model.source_waits.is_empty());
        assert!(model.source_deadlines.is_empty());
        assert!(!model.source_scan);
        model
            .service_source_waits(131, wait.deadline, |_| false)
            .unwrap();
        assert!(model.source_waits.is_empty());
    }
}

fn session() -> NativeSession {
    let mut session = NativeSession::default();
    session
        .authorization(10_000, 1, ResourcePermissions::RoomBound)
        .unwrap();
    session.set_cid(
        ChannelId::from_value(0),
        pulsebeam_runtime::time::Instant::now(),
    );
    session
}

#[test]
fn quic_opus_preparation_checks_capabilities_and_receiver_source_format() {
    use crate::control::native::{Publication, publication as prepare};

    let format = wire::CodecFormat::from(wire::codec_format::Codec::Opus(
        wire::OpusParameters::default(),
    ));
    let parameters = wire::QuicRtpParameters {
        binding_id: 7,
        incarnation: 9,
        primary_ssrc: 11,
        publishes_h264_dd: false,
        codec: Some(wire::RtpCodecParameters {
            payload_type: Some(111),
            format: Some(format.clone()),
            ..Default::default()
        }),
    };
    let spec = wire::ResourceSpec {
        declaration: Some(wire::resource_spec::Declaration::Publication(
            wire::PublicationSpec {
                track_id: "voice".into(),
                kind: Some(wire::publication_spec::Kind::Audio(
                    wire::AudioPublication {
                        binding: Some(wire::TrackBinding {
                            transport: Some(wire::track_binding::Transport::QuicRtp(
                                parameters.clone(),
                            )),
                        }),
                    },
                )),
            },
        )),
        ..Default::default()
    };
    assert!(matches!(
        prepare(&spec, None),
        Err(wire::FailureCode::Unsupported)
    ));
    assert!(matches!(
        prepare(&spec, Some(&wire::RtpCapabilities::default())),
        Err(wire::FailureCode::Incompatible)
    ));
    let capabilities = wire::RtpCapabilities {
        codecs: vec![wire::RtpCodecCapability {
            format: Some(format),
            ..Default::default()
        }],
        ..Default::default()
    };
    let Some((Publication::QuicRtp(plan), bytes)) = prepare(&spec, Some(&capabilities)).unwrap()
    else {
        panic!("QUIC RTP preparation")
    };
    assert_eq!(plan.track_id.as_str(), "voice");
    assert_eq!(plan.kind, crate::entity::TrackKind::Audio);
    assert_eq!(plan.parameters, parameters);
    assert_eq!(plan.enabled, crate::control::native::enabled(&spec));
    assert!(bytes >= spec.encoded_len());

    use crate::control::native::{ReceiveTransport, receiver};
    use crate::entity::{ConnectionId, ParticipantId, ProjectRoomKey, RoomId, TrackKind};
    use crate::track::{AudioTrack, TrackMeta};
    let connection = ConnectionId::new();
    let meta = TrackMeta {
        room: ProjectRoomKey::fixture(RoomId::new("room").unwrap()),
        id: connection.fixture_publication(TrackKind::Audio, "voice"),
        origin: ParticipantId::new("source").unwrap(),
        connection_id: connection,
        track_id: Some(plan.track_id.clone()),
    };
    let audio = AudioTrack {
        meta,
        format: capabilities
            .codecs
            .first()
            .and_then(|codec| codec.format.clone()),
        reverse: None,
    };
    let source = Track::Audio(audio.clone());
    let track = track_descriptor(&source).unwrap();
    let spec = wire::ResourceSpec {
        declaration: Some(wire::resource_spec::Declaration::Subscription(
            wire::SubscriptionSpec {
                publication_id: track.publication_id.clone(),
                kind: wire::TrackKind::Audio as i32,
                source_room: "room".into(),
                binding: Some(wire::TrackBinding {
                    transport: Some(wire::track_binding::Transport::QuicRtp(parameters.clone())),
                }),
            },
        )),
        ..Default::default()
    };
    assert!(matches!(
        receiver(&spec, &source, &track, None, None),
        Err(wire::FailureCode::Unsupported)
    ));
    let (ReceiveTransport::QuicRtp(prepared), _) =
        receiver(&spec, &source, &track, Some(&capabilities), None).unwrap()
    else {
        panic!("QUIC RTP receiver preparation")
    };
    assert_eq!(*prepared, parameters);
    let incompatible = Track::Audio(AudioTrack {
        format: None,
        ..audio
    });
    assert!(matches!(
        receiver(&spec, &incompatible, &track, Some(&capabilities), None),
        Err(wire::FailureCode::Incompatible)
    ));
}

fn publication(label: &str, enabled: bool) -> wire::ResourceSpec {
    wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(enabled),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::Publication(
            wire::PublicationSpec {
                track_id: label.to_owned(),
                kind: Some(wire::publication_spec::Kind::Data(wire::DataPublication {
                    capture_clock: None,
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

#[test]
fn data_v1_publication_keeps_its_exact_transport_in_the_execution_plan() {
    use crate::control::native::Publication;
    let version = u32::from(pulsebeam_proto::data_endpoint::DATA_V1);
    let parameters = wire::SctpDataV1Parameters {
        stream_id: Some(1),
        version,
        channel_incarnation: u64::MAX,
        delivery: wire::DataV1Delivery::Unreliable as i32,
    };
    let sctp = wire::track_binding::Transport::SctpDataV1(parameters);
    let quic_parameters = wire::QuicDataV1Parameters {
        binding_id: 1,
        version,
        incarnation: u64::MAX,
        delivery: wire::DataV1Delivery::Unreliable as i32,
    };
    let quic = wire::track_binding::Transport::QuicDataV1(quic_parameters);
    let spec = |delivery: wire::DataDelivery, transport| {
        let mut spec = publication("events", true);
        let Some(wire::resource_spec::Declaration::Publication(publication)) =
            &mut spec.declaration
        else {
            panic!("publication")
        };
        let Some(wire::publication_spec::Kind::Data(data)) = &mut publication.kind else {
            panic!("DATA publication")
        };
        data.delivery = delivery as i32;
        data.binding = Some(wire::TrackBinding {
            transport: Some(transport),
        });
        spec
    };
    let Some((Publication::Data(data), _)) =
        crate::control::native::publication(&spec(wire::DataDelivery::Lossy, sctp.clone()), None)
            .unwrap()
    else {
        panic!("DATA execution plan")
    };
    assert_eq!(data.parameters, wire::DataV1Binding::Sctp(parameters));
    assert!(matches!(
        crate::control::native::publication(&spec(wire::DataDelivery::Latest, sctp.clone()), None),
        Err(wire::FailureCode::InvalidConfiguration)
    ));

    let mut session = session();
    input(
        &mut session,
        vec![present(1, 1, spec(wire::DataDelivery::Lossy, sctp))],
    )
    .unwrap();
    let work = session.take_preparation().unwrap().unwrap();
    assert!(
        matches!(session.compile_execution(&work, None, |_| None).unwrap().into_action(),
        ExecutionAction::Prepare { resource: crate::control::native::PreparedResource::Publication(Publication::Data(data)), .. }
            if data.parameters == wire::DataV1Binding::Sctp(parameters))
    );

    let mut model = NativeSession::with_binding_profile(validation::BindingProfile::Quic);
    model
        .authorization(900, 1, ResourcePermissions::RoomBound)
        .unwrap();
    model.set_cid(
        ChannelId::from_value(0),
        pulsebeam_runtime::time::Instant::now(),
    );
    input(
        &mut model,
        vec![present(1, 1, spec(wire::DataDelivery::Lossy, quic))],
    )
    .unwrap();
    let work = model.take_preparation().unwrap().unwrap();
    assert!(
        matches!(model.compile_execution(&work, None, |_| None).unwrap().into_action(),
        ExecutionAction::Prepare { resource: crate::control::native::PreparedResource::Publication(Publication::Data(data)), .. }
            if data.parameters == wire::DataV1Binding::Quic(quic_parameters))
    );
}

fn subscription(publication_id: &str) -> wire::ResourceSpec {
    wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::Subscription(
            wire::SubscriptionSpec {
                publication_id: publication_id.to_owned(),
                kind: wire::TrackKind::Data as i32,
                source_room: "room".into(),
                binding: Some(wire::TrackBinding {
                    transport: Some(wire::track_binding::Transport::Sctp(wire::SctpParameters {
                        stream_id: Some(1),
                    })),
                }),
            },
        )),
    }
}

fn present(reference: u64, revision: u64, spec: wire::ResourceSpec) -> wire::ClientStateRecord {
    desired(
        reference,
        revision,
        wire::desired_state::Value::Present(Box::new(spec)),
    )
}

fn absent(reference: u64, revision: u64) -> wire::ClientStateRecord {
    desired(
        reference,
        revision,
        wire::desired_state::Value::Absent(true),
    )
}

fn desired(
    reference: u64,
    revision: u64,
    value: wire::desired_state::Value,
) -> wire::ClientStateRecord {
    wire::ClientStateRecord {
        owned: reference,
        execution: None,
        desired: Some(wire::DesiredState {
            revision,
            value: Some(value),
        }),
    }
}

fn input(
    session: &mut NativeSession,
    records: Vec<wire::ClientStateRecord>,
) -> Result<Vec<u64>, InputError> {
    let bytes = codec::encode_client(&wire::ClientStateBatch {
        records,
        ..Default::default()
    })
    .unwrap();
    session.apply_input(&bytes).map(|input| input.cutoffs)
}

fn source(reference: u64, label: &str) -> wire::Track {
    wire::Track {
        publication_id: format!("source-{reference}"),
        participant_id: "owner".into(),
        room_id: "room".into(),
        track_id: label.into(),
        kind: Some(wire::track::Kind::Data(wire::DataTrack {
            capture_clock: None,
            delivery: wire::DataDelivery::Ordered as i32,
        })),
    }
}

fn live(session: &NativeSession, work: &Preparation) -> wire::LiveResource {
    let spec = session.preparation_spec(work).unwrap();
    let Some(wire::resource_spec::Declaration::Publication(publication)) = &spec.declaration else {
        panic!("publication fixture")
    };
    let mut track = source(work.reference, &publication.track_id);
    if let Some(wire::publication_spec::Kind::Data(data)) = &publication.kind {
        track.kind = Some(wire::track::Kind::Data(wire::DataTrack {
            capture_clock: data.capture_clock.clone(),
            delivery: data.delivery,
        }));
    }
    wire::LiveResource {
        data_v1_source: None,
        spec: Some(spec.clone()),
        participant: Some(wire::Participant {
            participant_id: "owner".into(),

            room_id: "room".into(),
        }),
        track: Some(track),
        execution: if spec.settings.as_ref().unwrap().enabled == Some(true) {
            wire::ExecutionStatus::Installed
        } else {
            wire::ExecutionStatus::Disabled
        } as i32,
        execution_failure: None,
        activity: None,
    }
}

fn admit(session: &mut NativeSession, reference: u64, spec: wire::ResourceSpec) {
    input(session, vec![present(reference, reference, spec)]).unwrap();
    let work = session.take_preparation().unwrap().unwrap();
    let value = live(session, &work);
    assert!(session.complete(work, Ok(value), 100).unwrap());
}

fn output(session: &mut NativeSession) -> Option<wire::ServerStateBatch> {
    session.poll().unwrap().map(|output| {
        assert!(output.bytes.len() <= codec::MAX_FRAME_SIZE);
        codec::decode_server(&output.bytes).unwrap()
    })
}

fn commit_sent(session: &mut NativeSession) {
    let output = session.take_output().expect("selected output");
    assert!(
        session
            .commit_sent(SignalingProgress {
                sequence: output.sequence,
                cid: output.cid,
                at: pulsebeam_runtime::time::Instant::now(),
            })
            .unwrap()
    );
}

fn drain(session: &mut NativeSession) -> Vec<wire::ServerStateBatch> {
    let mut batches = Vec::new();
    for _ in 0..2048 {
        if !session.needs_poll() {
            return batches;
        }
        if let Some(batch) = output(session) {
            assert!(batch.records.len() <= codec::MAX_RECORDS);
            batches.push(batch);
            commit_sent(session);
        }
    }
    panic!("bounded fixture did not drain");
}

fn owned(session: &NativeSession, reference: u64) -> &wire::ConfirmedState {
    &session.rows.get(&Key::Owned(reference)).unwrap().value
}

#[test]
fn proposals_use_one_connection_counter_without_legacy_fallback() {
    let mut session = session();
    input(
        &mut session,
        vec![
            present(1, 1, publication("first", true)),
            present(9, 9, publication("second", true)),
        ],
    )
    .unwrap();
    assert_eq!(session.last_revision, 9);
    assert!(matches!(
        input(
            &mut session,
            vec![present(1, 8, publication("first", false))]
        ),
        Err(InputError::Revision)
    ));
    assert!(matches!(
        session.apply_input(b"legacy intent"),
        Err(InputError::Decode(_))
    ));
    assert_eq!(
        session
            .rows
            .get(&Key::Owned(1))
            .unwrap()
            .owned
            .as_ref()
            .unwrap()
            .target
            .revision,
        1
    );
    assert_eq!(session.take_preparation().unwrap().unwrap().reference, 1);
    assert_eq!(session.take_preparation().unwrap().unwrap().reference, 9);
}

#[test]
fn evaluated_proposal_allows_the_next_target_without_waiting_for_output() {
    let mut session = session();
    input(
        &mut session,
        vec![present(1, 1, publication("topic", true))],
    )
    .unwrap();
    let old = session.take_preparation().unwrap().unwrap();
    let old_value = live(&session, &old);
    assert!(session.complete(old, Ok(old_value), 100).unwrap());
    input(
        &mut session,
        vec![present(1, 2, publication("topic", false))],
    )
    .unwrap();
    let latest = session.take_preparation().unwrap().unwrap();
    assert_eq!(latest.revision, 2);
    let latest_value = live(&session, &latest);
    assert!(session.complete(latest, Ok(latest_value), 100).unwrap());
    let value = owned(&session, 1);
    assert_eq!(value.evaluated_revision, 2);
    let Some(wire::confirmed_state::Value::Owned(live)) = &value.value else {
        panic!("accepted value")
    };
    assert_eq!(live.execution, wire::ExecutionStatus::Disabled as i32);
    assert_eq!(
        live.spec
            .as_ref()
            .unwrap()
            .settings
            .as_ref()
            .unwrap()
            .enabled,
        Some(false)
    );
    assert!(
        !session
            .rows
            .contains_key(&Key::Publication("source-1".into()))
    );
}

#[test]
fn overlapping_proposals_cannot_replace_queued_or_active_targets() {
    for start_preparation in [false, true] {
        for next in [publication("topic", false), publication("relabel", true)] {
            let mut session = session();
            input(
                &mut session,
                vec![present(1, 1, publication("topic", true))],
            )
            .unwrap();
            let work = if start_preparation {
                Some(session.take_preparation().unwrap().unwrap())
            } else {
                None
            };
            let report = wire::ExecutionReport {
                revision: 2,
                outcome: Some(wire::execution_report::Outcome::Failure(failure(
                    wire::FailureCode::Unsupported,
                ))),
            };
            let bytes = codec::encode_client(&wire::ClientStateBatch {
                records: vec![wire::ClientStateRecord {
                    owned: 1,
                    desired: None,
                    execution: Some(report.clone()),
                }],
                ..Default::default()
            })
            .unwrap();
            assert!(session.receive_input(&bytes, 100).unwrap().is_empty());
            assert!(session.receiver_failure(1).is_none());
            assert_eq!(session.last_revision, 2);
            assert!(matches!(
                input(&mut session, vec![present(1, 3, next)]),
                Err(InputError::Revision)
            ));
            let owned = session.rows[&Key::Owned(1)].owned.as_ref().unwrap();
            assert_eq!(owned.target.revision, 1);
            assert_eq!(owned.declaration, publication("topic", true));
            if let Some(work) = work {
                assert_eq!(owned.active, Some(work.stamp()));
                assert!(session.preparation_current(&work, 100).unwrap());
            }
        }
    }
    let mut session = session();
    input(
        &mut session,
        vec![
            present(1, 1, publication("first", true)),
            present(2, 2, publication("second", true)),
        ],
    )
    .unwrap();
    let bytes = codec::encode_client(&wire::ClientStateBatch {
        records: vec![absent(1, 3), present(2, 4, publication("second", false))],
        ..Default::default()
    })
    .unwrap();
    assert!(matches!(
        session.receive_input(&bytes, 100),
        Err(InputError::Revision)
    ));
    assert_eq!(session.execution_sequence, 0);
    assert!(session.rows[&Key::Owned(1)].owned.as_ref().unwrap().retired);
    assert_eq!(
        session.rows[&Key::Owned(2)]
            .owned
            .as_ref()
            .unwrap()
            .target
            .revision,
        2
    );
}

#[test]
fn retirement_cuts_off_unevaluated_work_without_waiting_for_preparation() {
    let mut session = session();
    drain(&mut session);
    input(
        &mut session,
        vec![present(1, 1, publication("topic", true))],
    )
    .unwrap();
    let work = session.take_preparation().unwrap().unwrap();
    let value = live(&session, &work);
    assert_eq!(input(&mut session, vec![absent(1, 2)]).unwrap(), vec![1]);
    assert!(output(&mut session).is_none());
    assert!(
        session
            .observe_resource(crate::participant::policy::ResourceObservation::Released {
                binding: 1
            })
            .unwrap()
            .is_none()
    );
    assert!(!session.complete(work, Ok(value), 100).unwrap());
    assert_eq!(owned(&session, 1).evaluated_revision, 2);
    assert!(matches!(
        owned(&session, 1).value,
        Some(wire::confirmed_state::Value::Absent(_))
    ));
    assert!(session.take_preparation().unwrap().is_none());
    drain(&mut session);
    assert!(!session.rows.contains_key(&Key::Owned(1)));
}

#[test]
fn publication_admission_is_bounded_including_pending_and_releases_after_cutoff() {
    const LIMIT: u64 = 64;
    let capacity = usize::try_from(LIMIT).unwrap();
    let mut session = session();
    for reference in 1..=LIMIT {
        input(
            &mut session,
            vec![present(
                reference,
                reference,
                publication(&format!("topic-{reference}"), true),
            )],
        )
        .unwrap();
    }
    assert_eq!(session.publications.len(), capacity);
    assert!(
        session.rows[&Key::Owned(LIMIT)]
            .owned
            .as_ref()
            .unwrap()
            .active
            .is_none()
    );
    let revisions: Vec<_> = (1..=LIMIT)
        .map(|reference| {
            session.rows[&Key::Owned(reference)]
                .owned
                .as_ref()
                .unwrap()
                .target
                .revision
        })
        .collect();
    input(
        &mut session,
        vec![present(65, 65, subscription("source-65"))],
    )
    .unwrap();
    assert!(session.rows[&Key::Owned(65)].owned.as_ref().unwrap().queued);
    input(
        &mut session,
        vec![present(66, 66, publication("over-limit", true))],
    )
    .unwrap();
    assert_eq!(
        session.rows[&Key::Owned(66)]
            .value
            .rejection
            .as_ref()
            .unwrap()
            .code,
        wire::FailureCode::CapacityExceeded as i32
    );
    assert_eq!(session.publications.len(), capacity);
    for _ in 1..=LIMIT {
        let work = session.take_preparation().unwrap().unwrap();
        assert_ne!(work.reference, 65);
        let value = live(&session, &work);
        assert!(session.complete(work, Ok(value), 100).unwrap());
    }
    let subscription_work = session.take_preparation().unwrap().unwrap();
    assert_eq!(subscription_work.reference, 65);

    assert_eq!(session.publications.len(), capacity);
    assert_eq!(
        revisions,
        (1..=LIMIT)
            .map(|reference| session.rows[&Key::Owned(reference)]
                .owned
                .as_ref()
                .unwrap()
                .target
                .revision)
            .collect::<Vec<_>>()
    );
    assert!(!session.publications.values().any(|owner| *owner == 66));

    input(
        &mut session,
        vec![present(1, 67, publication("topic-1", true))],
    )
    .unwrap();
    assert_eq!(
        session.rows[&Key::Owned(1)]
            .owned
            .as_ref()
            .unwrap()
            .target
            .revision,
        67
    );
    input(&mut session, vec![absent(1, 68)]).unwrap();
    assert_eq!(session.publications.len(), capacity);
    session.cutoff_complete(1).unwrap();
    assert_eq!(session.publications.len(), capacity - 1);
    input(
        &mut session,
        vec![present(69, 69, publication("after-release", true))],
    )
    .unwrap();
    assert_eq!(session.publications.len(), capacity);
}

#[test]
fn rejected_update_preserves_acceptance_and_publication_reservation() {
    let mut session = session();
    admit(&mut session, 1, publication("topic", true));
    let accepted = owned(&session, 1).value.clone();
    let mut invalid = publication("topic", false);
    invalid.settings.as_mut().unwrap().enabled = None;
    input(&mut session, vec![present(1, 2, invalid)]).unwrap();
    assert_eq!(owned(&session, 1).value, accepted);
    assert_eq!(owned(&session, 1).evaluated_revision, 2);
    assert_eq!(
        owned(&session, 1).rejection.as_ref().unwrap().code,
        wire::FailureCode::InvalidConfiguration as i32
    );
    input(
        &mut session,
        vec![present(3, 3, publication("topic", true))],
    )
    .unwrap();
    assert!(matches!(
        owned(&session, 3).value,
        Some(wire::confirmed_state::Value::Absent(_))
    ));
    assert_eq!(
        session
            .publications
            .get(&(wire::TrackKind::Data as i32, "topic".into())),
        Some(&1)
    );
    use crate::participant::policy::{
        ExecutionFailure, ExecutionFailureContext as Context, ExecutionFailureKind as Kind,
    };
    let external_id = crate::entity::ParticipantId::new("Owner").unwrap();
    for (revision, (kind, context, code, diagnostic)) in (4..).zip([
        (
            Kind::Invalid,
            None,
            wire::FailureCode::InvalidConfiguration,
            "",
        ),
        (Kind::Unsupported, None, wire::FailureCode::Unsupported, ""),
        (
            Kind::Conflict,
            Some(Context::ReceiverVideoSlot),
            wire::FailureCode::Incompatible,
            "native receiver MID already has a video output",
        ),
        (
            Kind::Capacity,
            Some(Context::PrepareReceiver),
            wire::FailureCode::CapacityExceeded,
            "native receiver transport preparation failed",
        ),
        (
            Kind::Internal,
            Some(Context::InstallReceiver),
            wire::FailureCode::InternalFailure,
            "native receiver transport installation failed",
        ),
        (Kind::Retired, None, wire::FailureCode::ResourceGone, ""),
    ]) {
        input(
            &mut session,
            vec![present(1, revision, publication("topic", false))],
        )
        .unwrap();
        let work = session.take_preparation().unwrap().unwrap();
        session
            .observe_completion(
                crate::control::native::ExecutionCompletion {
                    work,
                    result: Err(ExecutionFailure { kind, context }),
                    released: false,
                    unix_now: 100,
                },
                &external_id,
            )
            .unwrap();
        let value = owned(&session, 1);
        assert_eq!(value.value, accepted);
        assert_eq!(value.evaluated_revision, revision);
        assert_eq!(value.rejection.as_ref().unwrap().code, code as i32);
        assert_eq!(value.rejection.as_ref().unwrap().diagnostic, diagnostic);
        assert!(
            session.rows[&Key::Owned(1)]
                .owned
                .as_ref()
                .unwrap()
                .active
                .is_none()
        );
        assert_eq!(
            session
                .publications
                .get(&(wire::TrackKind::Data as i32, "topic".into())),
            Some(&1)
        );
    }
    let lookup = wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::SourceLookup(
            wire::SourceLookupSpec {
                source: Some(wire::LogicalSourceAddress {
                    room: "room".into(),
                    participant_id: "Owner".into(),
                    kind: wire::TrackKind::Data as i32,
                    track_id: "topic".into(),
                }),
            },
        )),
    };
    assert!(
        input(&mut session, vec![present(16, 16, lookup)])
            .unwrap()
            .is_empty()
    );
    assert!(owned(&session, 16).rejection.is_none());
    assert!(matches!(
        owned(&session, 16).value,
        Some(wire::confirmed_state::Value::SourceLookup(_))
    ));
    assert!(session.take_preparation().unwrap().is_none());
    assert_eq!(owned(&session, 1).value, accepted);
}

#[test]
fn absence_waits_for_cutoff_and_preserves_newer_gone_evaluation() {
    let mut session = session();
    admit(&mut session, 1, publication("topic", true));
    drain(&mut session);
    assert_eq!(
        input(
            &mut session,
            vec![absent(1, 2), present(1, 3, publication("topic", true))]
        )
        .unwrap(),
        vec![1]
    );
    assert!(output(&mut session).is_none());
    session.cutoff_complete(1).unwrap();
    assert_eq!(owned(&session, 1).evaluated_revision, 3);
    assert_eq!(
        owned(&session, 1).rejection.as_ref().unwrap().code,
        wire::FailureCode::ResourceGone as i32
    );
    drain(&mut session);
    assert!(!session.rows.contains_key(&Key::Owned(1)));
    input(
        &mut session,
        vec![present(1, 4, publication("topic", true))],
    )
    .unwrap();
    assert!(session.take_preparation().unwrap().is_none());
    assert_eq!(
        owned(&session, 1).rejection.as_ref().unwrap().code,
        wire::FailureCode::ResourceGone as i32
    );
}

#[test]
fn old_retirement_cannot_release_a_new_lifetimes_label() {
    let mut session = session();
    admit(&mut session, 1, publication("topic", true));
    input(&mut session, vec![absent(1, 2)]).unwrap();
    session.cutoff_complete(1).unwrap();
    input(
        &mut session,
        vec![present(3, 3, publication("topic", true))],
    )
    .unwrap();
    input(&mut session, vec![absent(1, 4)]).unwrap();
    session.cutoff_complete(1).unwrap();
    assert_eq!(
        session
            .publications
            .get(&(wire::TrackKind::Data as i32, "topic".into())),
        Some(&3)
    );
}

#[test]
fn execution_attempts_and_grants_fence_stale_work_and_expired_commits() {
    let mut session = session();
    input(
        &mut session,
        vec![present(1, 1, publication("topic", true))],
    )
    .unwrap();
    let stale = session.take_preparation().unwrap().unwrap();
    let stale_value = live(&session, &stale);
    let prepaid = session.retained_bytes;
    session.defer_preparation(stale).unwrap();
    let old = session.take_preparation().unwrap().unwrap();
    assert!(old.scope().sequence > stale.scope().sequence);
    assert_eq!(old.revision, stale.revision);
    assert_eq!(old.grant_revision, stale.grant_revision);
    assert!(session.preparation_spec(&stale).is_none());
    assert!(
        !session
            .complete(stale, Ok(stale_value.clone()), 100)
            .unwrap()
    );
    session.defer_preparation(stale).unwrap();
    assert!(session.preparation_current(&old, 100).unwrap());
    assert_eq!(session.retained_bytes, prepaid);
    let old_value = live(&session, &old);
    session
        .authorization(9000, 2, ResourcePermissions::RoomBound)
        .unwrap();
    session
        .authorization(20000, 1, ResourcePermissions::RoomBound)
        .unwrap();
    assert_eq!(session.grant_revision, 2);
    assert!(session.preparation_spec(&old).is_none());
    assert!(!session.complete(old, Ok(old_value), 100).unwrap());
    let next = session.take_preparation().unwrap().unwrap();
    let next_value = live(&session, &next);
    assert!(!session.complete(next, Ok(next_value), 9000).unwrap());
    assert_eq!(owned(&session, 1).evaluated_revision, 0);
    assert!(owned(&session, 1).rejection.is_none());
    session
        .authorization(20000, 3, ResourcePermissions::RoomBound)
        .unwrap();
    let current = session.take_preparation().unwrap().unwrap();
    assert!(!session.complete(stale, Ok(stale_value), 9000).unwrap());
    assert!(session.preparation_current(&current, 9000).unwrap());
    let value = live(&session, &current);
    assert!(session.complete(current, Ok(value), 9000).unwrap());
    assert_eq!(owned(&session, 1).evaluated_revision, 1);
}

#[test]
fn terminal_lifetimes_release_rows_queues_and_byte_funding_without_history() {
    let mut session = session();
    drain(&mut session);
    let baseline = session.retained_bytes;
    for index in 0..512 {
        let reference = index * 2 + 1;
        let mut spec = publication("topic", true);
        let Some(wire::resource_spec::Declaration::Publication(publisher)) = &mut spec.declaration
        else {
            unreachable!()
        };
        let Some(wire::publication_spec::Kind::Data(data)) = &mut publisher.kind else {
            unreachable!()
        };
        data.delivery = wire::DataDelivery::Latest as i32;
        data.binding = Some(crate::control::native::fixture::data_v1_binding(1));
        input(&mut session, vec![present(reference, reference, spec)]).unwrap();
        let work = session.take_preparation().unwrap().unwrap();
        assert!(matches!(
            session
                .compile_execution(&work, None, |_| panic!(
                    "publication needs no receiver context"
                ))
                .unwrap().into_action(),
            ExecutionAction::Prepare { work: issued, resource: crate::control::native::PreparedResource::Publication(
                crate::control::native::Publication::Data(_)
            ) } if issued == work
        ));
        let prepaid = session.retained_bytes;
        session
            .compile_execution(&work, None, |_| {
                panic!("publication needs no receiver context")
            })
            .unwrap();
        assert_eq!(session.retained_bytes, prepaid);
        let work = if index == 0 {
            use crate::control::native::{ExecutionCompletion, ExecutionDeferral};
            session
                .observe_deferral(ExecutionDeferral {
                    work,
                    retained_sequence: Some(work.scope().sequence),
                })
                .unwrap();
            let current = session.take_preparation().unwrap().unwrap();
            session
                .compile_execution(&current, None, |_| panic!("publication context"))
                .unwrap();
            session
                .observe_deferral(ExecutionDeferral {
                    work,
                    retained_sequence: Some(work.scope().sequence),
                })
                .unwrap();
            session
                .observe_deferral(ExecutionDeferral {
                    work,
                    retained_sequence: None,
                })
                .unwrap();
            session
                .observe_completion(
                    ExecutionCompletion {
                        work,
                        result: Err(
                            crate::participant::policy::ExecutionFailureKind::Internal.into()
                        ),
                        released: true,
                        unix_now: 100,
                    },
                    &crate::entity::ParticipantId::new("Owner").unwrap(),
                )
                .unwrap();
            assert_eq!(session.retained_bytes, prepaid);
            assert_eq!(
                session.rows[&Key::Owned(reference)]
                    .owned
                    .as_ref()
                    .unwrap()
                    .active,
                Some(current.stamp())
            );
            session
                .observe_resource(
                    crate::participant::policy::ResourceObservation::StorageReleased {
                        binding: reference,
                        sequence: work.scope().sequence,
                    },
                )
                .unwrap();
            assert_eq!(session.retained_bytes, prepaid);
            session
                .observe_deferral(ExecutionDeferral {
                    work: current,
                    retained_sequence: Some(work.scope().sequence),
                })
                .unwrap();
            assert_eq!(
                session.rows[&Key::Owned(reference)]
                    .owned
                    .as_ref()
                    .unwrap()
                    .storage_sequence,
                work.scope().sequence
            );
            session
                .observe_resource(
                    crate::participant::policy::ResourceObservation::StorageReleased {
                        binding: reference,
                        sequence: current.scope().sequence,
                    },
                )
                .unwrap();
            assert_eq!(session.retained_bytes, prepaid - Preparation::bytes());
            session
                .observe_deferral(ExecutionDeferral {
                    work,
                    retained_sequence: None,
                })
                .unwrap();
            assert_eq!(
                session.rows[&Key::Owned(reference)]
                    .owned
                    .as_ref()
                    .unwrap()
                    .execution_bytes,
                0
            );
            let resumed = session.take_preparation().unwrap().unwrap();
            session
                .compile_execution(&resumed, None, |_| panic!("publication context"))
                .unwrap();
            session
                .observe_resource(
                    crate::participant::policy::ResourceObservation::StorageReleased {
                        binding: reference,
                        sequence: work.scope().sequence,
                    },
                )
                .unwrap();
            assert_eq!(session.retained_bytes, prepaid);
            resumed
        } else {
            work
        };
        let mut value = live(&session, &work);
        value.data_v1_source = Some(reference);
        assert!(session.complete(work, Ok(value), 100).unwrap());
        drain(&mut session);
        input(&mut session, vec![absent(reference, reference + 1)]).unwrap();
        session.cutoff_complete(reference).unwrap();
        drain(&mut session);
        assert!(session.rows.contains_key(&Key::Owned(reference)));
        session
            .observe_resource(
                crate::participant::policy::ResourceObservation::StorageReleased {
                    binding: reference,
                    sequence: work.scope().sequence,
                },
            )
            .unwrap();
        assert_eq!(session.rows.len(), 1);
        assert_eq!(session.order.len(), 1);
        assert!(session.dirty.is_empty());
        assert!(session.preparations.is_empty());
        assert!(session.publications.is_empty());
        assert_eq!(session.retained_bytes, baseline);
    }
}

#[test]
fn initial_enumeration_is_finite_fair_and_not_a_live_catchup_barrier() {
    let mut session = NativeSession::default();
    session
        .authorization(10000, 1, ResourcePermissions::RoomBound)
        .unwrap();
    for index in 0..512 {
        session
            .observe_participant(wire::Participant {
                participant_id: format!("initial-{index}"),

                room_id: "room".into(),
            })
            .unwrap();
    }
    session.set_cid(
        ChannelId::from_value(0),
        pulsebeam_runtime::time::Instant::now(),
    );
    let mut represented = BTreeSet::new();
    let mut marked = false;
    for index in 0..128 {
        session
            .observe_participant(wire::Participant {
                participant_id: format!("late-{index}"),

                room_id: "room".into(),
            })
            .unwrap();
        if let Some(batch) = output(&mut session) {
            if index == 0 {
                assert!(matches!(
                    batch.records[0].key.as_ref().unwrap().key,
                    Some(wire::resource_key::Key::Session(true))
                ));
            }
            for record in batch.records {
                if let Some(wire::resource_key::Key::Participant(id)) =
                    record.key.and_then(|key| key.key)
                    && id.participant_id.starts_with("initial-")
                {
                    represented.insert(id.participant_id);
                }
            }
            if batch.initial_complete {
                assert_eq!(represented.len(), 512);
                marked = true;
            }
            commit_sent(&mut session);
            if marked {
                break;
            }
        }
    }
    assert!(marked);
    assert!(
        drain(&mut session)
            .iter()
            .all(|batch| !batch.initial_complete)
    );
}

#[test]
fn output_admission_metric_requires_a_matching_receipt() {
    let recorder = crate::shard::recorder::ShardRecorder::new();
    metrics::with_local_recorder(&recorder, || {
        let mut session = session();
        session.poll().unwrap().unwrap();
        let output = session.take_output().unwrap();
        let receipt = SignalingProgress {
            sequence: output.sequence,
            cid: output.cid,
            at: pulsebeam_runtime::time::Instant::now(),
        };
        assert!(
            !session
                .commit_sent(SignalingProgress {
                    sequence: receipt.sequence + 1,
                    ..receipt
                })
                .unwrap()
        );
        assert!(session.commit_sent(receipt).unwrap());
        assert!(!session.commit_sent(receipt).unwrap());
    });
    let report = recorder.snapshot(crate::id::ShardId::new(0));
    let schema = report.schema.unwrap();
    let index = schema
        .histograms
        .iter()
        .position(|series| {
            series.name == "native_signaling_output_admission_us" && series.labels.is_empty()
        })
        .unwrap();
    assert_eq!(report.histograms[index].count, 1);
}

#[test]
fn selected_output_is_frozen_and_commit_does_not_clear_newer_state() {
    let mut session = session();
    let selected = session.poll().unwrap().unwrap();
    let pointer = selected.bytes.as_ptr();
    let deadline = selected.deadline;
    let early = SignalingProgress {
        sequence: selected.sequence,
        cid: selected.cid,
        at: pulsebeam_runtime::time::Instant::now(),
    };
    assert!(!session.commit_sent(early).unwrap());
    let pending = session.take_output().unwrap();
    assert_eq!(pending.bytes.as_ptr(), pointer);
    session.restore_output(pending).unwrap();
    assert_eq!(session.poll().unwrap().unwrap().bytes.as_ptr(), pointer);
    let pending = session.take_output().unwrap();
    let (ready, mut notifications) = pulsebeam_queue::bounded(core::num::NonZeroUsize::MIN);
    let connection = crate::entity::ConnectionId::new();
    let (mut controller, mut endpoint) = crate::control::native::handoff::pair(connection, ready);
    assert!(controller.output(pending).is_ok());
    let pending = endpoint.output().unwrap();
    assert_eq!(pending.bytes.as_ptr(), pointer);
    assert_eq!(pending.deadline, deadline);
    session
        .authorization(9000, 2, ResourcePermissions::RoomBound)
        .unwrap();
    assert!(session.poll().unwrap().is_none());
    assert!(!session.needs_poll());
    assert_eq!(session.enumeration_progress(), 0);
    assert!(!session.initial_complete);
    let progress = SignalingProgress {
        sequence: pending.sequence,
        cid: pending.cid,
        at: pulsebeam_runtime::time::Instant::now(),
    };
    assert!(
        !session
            .commit_sent(SignalingProgress {
                sequence: progress.sequence.saturating_add(1),
                ..progress
            })
            .unwrap()
    );
    assert!(endpoint.progress(progress));
    assert_eq!(notifications.try_recv().unwrap(), connection);
    let admitted = controller.progress().unwrap();
    assert_eq!(admitted.at, progress.at);
    assert!(session.commit_sent(admitted).unwrap());
    assert!(controller.rearm());
    let next = output(&mut session).unwrap();
    assert!(!session.commit_sent(progress).unwrap());
    assert!(!next.initial_complete);
    let Some(value) = &next.records[0].confirmed else {
        panic!("confirmed")
    };
    let Some(wire::confirmed_state::Value::Session(value)) = &value.value else {
        panic!("session")
    };
    assert_eq!(value.authorization.as_ref().unwrap().revision, 2);
}

#[test]
fn discovery_requires_real_immutable_descriptors_and_does_not_delay_initial_grant() {
    let mut session = session();
    session
        .unavailable(Key::Publication("source-1".into()))
        .unwrap();
    let first = output(&mut session).unwrap();
    assert_eq!(first.records.len(), 1);
    assert!(!first.initial_complete);
    commit_sent(&mut session);
    assert!(matches!(
        session.poll(),
        Err(OutputError::InvalidObservation)
    ));
    session
        .observe_track(
            source(1, "Literal_topic"),
            &crate::entity::ParticipantId::new("Owner").unwrap(),
        )
        .unwrap();
    assert_eq!(session.unavailable, 0);
    drain(&mut session);
    assert!(matches!(
        session.observe_track(
            source(1, "relabel"),
            &crate::entity::ParticipantId::new("Owner").unwrap()
        ),
        Err(OutputError::InvalidObservation)
    ));
    session
        .observe_participant(wire::Participant {
            participant_id: "peer".into(),

            room_id: "room".into(),
        })
        .unwrap();
    assert!(matches!(
        session.observe_participant(wire::Participant {
            participant_id: "peer".into(),
            room_id: "not/a/room".into()
        }),
        Err(OutputError::InvalidObservation)
    ));
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn unrelated_grant_churn_cannot_hide_a_stalled_selected_frame() {
    let mut session = session();
    let bytes = session.poll().unwrap().unwrap().bytes.clone();
    for revision in 2..=30 {
        pulsebeam_runtime::time::advance(std::time::Duration::from_secs(1)).await;
        session
            .authorization(10000, revision, ResourcePermissions::RoomBound)
            .unwrap();
        assert_eq!(session.poll().unwrap().unwrap().bytes, bytes);
    }
    pulsebeam_runtime::time::advance(std::time::Duration::from_secs(1)).await;
    assert!(matches!(session.poll(), Err(OutputError::ProgressExpired)));
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn delayed_admission_receipt_uses_original_open_and_admission_times() {
    use pulsebeam_runtime::time::{Instant, advance};
    use std::time::Duration;

    for before_deadline in [true, false] {
        let opened = Instant::now();
        let mut session = NativeSession::default();
        session
            .authorization(10_000, 1, ResourcePermissions::RoomBound)
            .unwrap();
        for index in 0..64 {
            session
                .observe_participant(wire::Participant {
                    participant_id: format!("initial-{index}"),

                    room_id: "room".into(),
                })
                .unwrap();
        }
        session.set_cid(ChannelId::from_value(0), opened);
        advance(Duration::from_secs(3)).await;
        session.poll().unwrap().unwrap();
        let output = session.take_output().unwrap();
        assert_eq!(output.deadline, opened + Duration::from_secs(30));
        assert!(!session.needs_poll());
        let progress = SignalingProgress {
            sequence: output.sequence,
            cid: output.cid,
            at: opened + Duration::from_secs(if before_deadline { 29 } else { 30 }),
        };
        advance(Duration::from_secs(28)).await;
        let result = session.commit_sent(progress);
        if before_deadline {
            assert!(result.unwrap());
            assert_eq!(session.deadline(), Some(opened + Duration::from_secs(59)));
            session.poll().unwrap().unwrap();
            advance(Duration::from_secs(28)).await;
            assert!(matches!(session.poll(), Err(OutputError::ProgressExpired)));
        } else {
            assert!(matches!(result, Err(OutputError::ProgressExpired)));
            assert_eq!(session.enumeration_progress(), 0);
        }
    }
}

#[test]
fn preparation_completion_cannot_replace_the_model_owned_target() {
    assert!(!std::mem::needs_drop::<Preparation>());
    for change_declaration in [false, true] {
        let mut session = session();
        let original = publication("topic", true);
        input(&mut session, vec![present(1, 1, original.clone())]).unwrap();
        let previous = session.take_preparation().unwrap().unwrap();
        let ExecutionAction::Retire(retirement) = session
            .retirement_command(previous.reference)
            .unwrap()
            .into_action()
        else {
            panic!("retirement command");
        };
        assert_eq!(retirement.sequence, previous.scope().sequence + 1);
        session.defer_preparation(previous).unwrap();
        let work = session.take_preparation().unwrap().unwrap();
        assert_eq!(work.scope().sequence, retirement.sequence + 1);
        let cross_scoped = ProjectedExecution {
            work: previous,
            live: live(&session, &work),
        };
        let before = session.retained_bytes;
        assert!(matches!(
            session.complete_projected(work, Ok(cross_scoped), 100),
            Err(OutputError::InvalidObservation)
        ));
        assert_eq!(session.retained_bytes, before);
        assert!(session.preparation_current(&work, 100).unwrap());
        assert_eq!(session.preparation_spec(&work), Some(&original));
        let mut fabricated = live(&session, &work);
        let spec = fabricated.spec.as_mut().unwrap();
        if change_declaration {
            let Some(wire::resource_spec::Declaration::Publication(declaration)) =
                &mut spec.declaration
            else {
                panic!("publication")
            };
            declaration.track_id = "different".into();
        } else {
            spec.settings.as_mut().unwrap().enabled = Some(false);
        }
        let before = session.retained_bytes;
        assert!(matches!(
            session.complete(work, Ok(fabricated), 100),
            Err(OutputError::InvalidObservation)
        ));
        assert_eq!(session.retained_bytes, before);
        assert_eq!(owned(&session, 1).evaluated_revision, 0);
        assert_eq!(session.preparation_spec(&work), Some(&original));
        assert!(session.preparation_current(&work, 100).unwrap());
        let accepted = live(&session, &work);
        assert!(session.complete(work, Ok(accepted), 100).unwrap());
        assert!(session.preparation_spec(&work).is_none());
    }
}

#[test]
fn data_projection_uses_actual_metadata_without_a_payload_format_or_activity_claim() {
    use crate::entity::{ConnectionId, ParticipantId, RoomId, TrackKind};
    use crate::track::{DataLane, Topic, Track, TrackMeta};
    let mut session = session();
    let connection = ConnectionId::new();
    let origin = crate::entity::fixture_participant_id();
    let room = RoomId::new("discovery").unwrap();
    let external_id = crate::entity::ParticipantId::new("Owner").unwrap();
    for (index, (lane, expected)) in [
        (DataLane::Latest, wire::DataDelivery::Latest),
        (DataLane::Lossy, wire::DataDelivery::Lossy),
        (DataLane::Ordered, wire::DataDelivery::Ordered),
    ]
    .into_iter()
    .enumerate()
    {
        let label = format!("Literal_{lane:?}");
        let id = connection.fixture_publication(TrackKind::Data, &label);
        let source = Track::data(
            TrackMeta {
                room: crate::entity::ProjectRoomKey::fixture(room),
                id,
                origin,
                connection_id: connection,
                track_id: Some(crate::entity::TrackId::new(&label).unwrap()),
            },
            Topic::for_test("application-topic"),
            lane,
            None,
        );
        session
            .observe_candidate(super::super::ResolvedSource {
                track: &source,
                publisher: &external_id,
            })
            .unwrap();
        let Some(wire::confirmed_state::Value::Track(observed)) = &session
            .rows
            .get(&Key::Publication(id.as_str()))
            .unwrap()
            .value
            .value
        else {
            panic!("discovered track")
        };
        assert_eq!(observed.publication_id, id.as_str());
        assert_eq!(observed.participant_id, origin.as_str());
        assert_eq!(observed.track_id, label);
        assert_eq!(
            observed.kind,
            Some(wire::track::Kind::Data(wire::DataTrack {
                capture_clock: None,
                delivery: expected as i32
            }))
        );
        let descriptor = observed.clone();
        let reference = index as u64 * 2 + 1;
        let mut spec = publication(&label, true);
        let Some(wire::resource_spec::Declaration::Publication(declaration)) =
            &mut spec.declaration
        else {
            panic!("publication")
        };
        let Some(wire::publication_spec::Kind::Data(data)) = &mut declaration.kind else {
            panic!("data")
        };
        data.delivery = expected as i32;
        input(
            &mut session,
            vec![present(reference, reference, spec.clone())],
        )
        .unwrap();
        let work = session.take_preparation().unwrap().unwrap();
        let fact = crate::participant::policy::PublicationExecution {
            data_v1_source: None,
            track: source,
            enabled: true,
            active: true,
        };
        let projected = session
            .project_publication(&work, &fact, &external_id)
            .unwrap();
        let live = &projected.live;
        assert_eq!(live.spec, Some(spec));
        assert_eq!(live.track, Some(descriptor));
        assert_eq!(
            live.participant.as_ref().unwrap().participant_id,
            origin.as_str()
        );
        assert_eq!(live.participant.as_ref().unwrap().room_id, room.as_str());
        assert_eq!(live.execution, wire::ExecutionStatus::Installed as i32);
        assert!(live.activity.is_none());
        assert!(matches!(
            session
                .complete_execution(
                    work,
                    Ok(ResourceExecution::Publication(Box::new(fact))),
                    &external_id,
                    100,
                )
                .unwrap(),
            ExecutionDisposition::Accepted
        ));
        assert!(matches!(
            session
                .complete_execution(
                    work,
                    Err(crate::participant::policy::ExecutionFailureKind::Internal.into()),
                    &external_id,
                    100,
                )
                .unwrap(),
            ExecutionDisposition::Deferred
        ));
        let report = codec::encode_client(&wire::ClientStateBatch {
            records: vec![wire::ClientStateRecord {
                owned: reference,
                desired: None,
                execution: Some(wire::ExecutionReport {
                    revision: reference + 1,
                    outcome: Some(wire::execution_report::Outcome::Failure(failure(
                        wire::FailureCode::InternalFailure,
                    ))),
                }),
            }],
            ..Default::default()
        })
        .unwrap();
        assert!(session.receive_input(&report, 100).unwrap().is_empty());
        assert!(session.receiver_failure(reference).is_none());
    }
    assert_eq!(session.unavailable, 0);
    assert!(
        drain(&mut session)
            .iter()
            .all(|batch| batch.records.len() <= codec::MAX_RECORDS)
    );
}

#[test]
fn source_end_preserves_latest_evaluation_and_funds_retained_execution() {
    let mut session = session();
    drain(&mut session);
    let base = session.retained_bytes;
    admit(&mut session, 1, publication("source", true));
    drain(&mut session);
    input(
        &mut session,
        vec![present(1, 2, publication("renamed", true))],
    )
    .unwrap();
    assert_eq!(session.rows[&Key::Owned(1)].value.evaluated_revision, 2);
    assert!(session.rows[&Key::Owned(1)].value.rejection.is_some());
    session.reserve_execution(1, 8192).unwrap();
    let command = session
        .observe_resource(crate::participant::policy::ResourceObservation::Ended { binding: 1 })
        .unwrap()
        .unwrap();
    assert!(
        matches!(command.into_action(), ExecutionAction::Retire(scope) if scope.binding == 1 && scope.authorization == 1 && scope.sequence == session.execution_sequence)
    );
    assert!(
        session.rows[&Key::Owned(1)]
            .owned
            .as_ref()
            .unwrap()
            .cutoff_pending
    );
    session.cutoff_complete(1).unwrap();
    let value = &session.rows[&Key::Owned(1)].value;
    assert_eq!(value.evaluated_revision, 2);
    assert!(value.rejection.is_some());
    assert!(
        matches!(&value.value, Some(wire::confirmed_state::Value::Absent(absent)) if absent.reason == wire::AbsenceReason::SourceEnded as i32)
    );
    drain(&mut session);
    assert!(session.rows.contains_key(&Key::Owned(1)));
    assert!(session.retained_bytes >= 8192);
    assert!(
        session
            .observe_resource(crate::participant::policy::ResourceObservation::Released {
                binding: 1
            })
            .unwrap()
            .is_none()
    );
    assert!(!session.rows.contains_key(&Key::Owned(1)));
    assert_eq!(session.retained_bytes, base);
}

#[test]
fn receiver_response_projection_is_prepaid_and_survives_catalog_removal_until_cutoff() {
    let mut session = session();
    drain(&mut session);
    let base = session.retained_bytes;
    session
        .authorization(99, 2, ResourcePermissions::RoomBound)
        .unwrap();
    let binding = |available| crate::participant::policy::ResourceObservation::ReceiverBinding {
        binding: 1,
        state: Some(crate::participant::policy::ReceiverBinding {
            available,
            rtp: None,
        }),
    };
    let mut track = source(1, "literal");
    let source_id = crate::entity::ConnectionId::new()
        .fixture_publication(crate::entity::TrackKind::Data, "literal");
    track.publication_id = source_id.as_str();
    let execution = crate::participant::policy::ReceiverExecution {
        source: source_id,
        enabled: true,
        ready: true,
        failed: true,
    };
    let owner = wire::Participant {
        participant_id: track.participant_id.clone(),

        room_id: "room".into(),
    };
    session.observe_participant(owner.clone()).unwrap();
    session
        .observe_track(
            track.clone(),
            &crate::entity::ParticipantId::new(&owner.participant_id).unwrap(),
        )
        .unwrap();
    drain(&mut session);
    let spec = wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::Subscription(
            wire::SubscriptionSpec {
                publication_id: track.publication_id.clone(),
                kind: wire::TrackKind::Data as i32,
                source_room: "room".into(),
                binding: Some(crate::control::native::fixture::data_v1_binding(1)),
            },
        )),
    };
    input(&mut session, vec![present(1, 1, spec.clone())]).unwrap();
    let work = session.take_preparation().unwrap().unwrap();
    let before = session.retained_bytes;
    assert!(session.preparation_current(&work, 98).unwrap());
    session.observe_resource(binding(true)).unwrap();
    assert!(matches!(
        session.compile_execution(&work, None, |_| panic!("opened binding needs no source lookup")),
        Err(error) if error.code == wire::FailureCode::ResourceGone as i32
    ));
    assert_eq!(session.retained_bytes, before);
    assert!(matches!(
        session.reserve_receiver_source(&work, STATE_BYTES, owner.clone(), track.clone()),
        Err(OutputError::Capacity)
    ));
    assert_eq!(session.retained_bytes, before);
    let owned = session.rows[&Key::Owned(1)].owned.as_ref().unwrap();
    assert!(owned.receiver_source.is_none());
    assert_eq!(owned.execution_bytes, 0);
    session
        .reserve_receiver_source(&work, 8192, owner.clone(), track.clone())
        .unwrap();
    let source_bytes = session.rows[&Key::Owned(1)]
        .owned
        .as_ref()
        .unwrap()
        .receiver_source
        .as_ref()
        .unwrap()
        .bytes();
    assert_eq!(session.retained_bytes, before + 8192 + source_bytes);
    session.remove_track(track.publication_id.clone()).unwrap();
    session
        .remove_participant(owner.room_id.clone(), owner.participant_id.clone())
        .unwrap();
    assert!(matches!(
        session.rows[&Key::Publication(track.publication_id.clone())]
            .value
            .value,
        Some(wire::confirmed_state::Value::Absent(_))
    ));
    let before_failure = session.retained_bytes;
    session.observe_receiver_failure(1, None).unwrap();
    let failure_bytes = receiver_failure_bytes(&failure(wire::FailureCode::InvalidConfiguration));
    assert_eq!(session.retained_bytes, before_failure + failure_bytes);
    session
        .observe_receiver_failure(1, Some(failure(wire::FailureCode::ResourceGone)))
        .unwrap();
    assert!(
        session
            .observe_resource(crate::participant::policy::ResourceObservation::Receiver {
                binding: 1,
                execution
            })
            .unwrap()
            .is_none()
    );
    assert_eq!(session.retained_bytes, before_failure + failure_bytes);
    let projected = session.project_receiver(&work, &execution).unwrap();
    let live = &projected.live;
    assert_eq!(live.execution, wire::ExecutionStatus::Failed as i32);
    assert_eq!(
        live.execution_failure.as_ref().unwrap().code,
        wire::FailureCode::InvalidConfiguration as i32
    );
    assert_eq!(live.spec, Some(spec.clone()));
    assert_eq!(live.participant, Some(owner.clone()));
    assert_eq!(live.track, Some(track.clone()));
    assert!(
        !session
            .complete_projected(work, Ok(projected), 100)
            .unwrap()
    );
    assert!(
        session.rows[&Key::Owned(1)]
            .owned
            .as_ref()
            .unwrap()
            .receiver_source
            .is_some()
    );
    assert!(!matches!(
        session.rows[&Key::Owned(1)].value.value,
        Some(wire::confirmed_state::Value::Owned(_))
    ));
    session
        .authorization(10_000, 3, ResourcePermissions::RoomBound)
        .unwrap();
    let work = session.take_preparation().unwrap().unwrap();
    assert!(matches!(
        session
            .compile_execution(&work, None, |_| panic!(
                "opened deferred binding must be reused"
            ))
            .unwrap()
            .into_action(),
        ExecutionAction::Prepare {
            resource: crate::control::native::PreparedResource::Receiver(
                crate::control::native::ReceiverPreparation::Policy(_)
            ),
            ..
        }
    ));
    let projected = session.project_receiver(&work, &execution).unwrap();
    assert!(
        session.rows[&Key::Owned(1)]
            .owned
            .as_ref()
            .unwrap()
            .receiver_failure
            .is_some()
    );
    assert!(
        session
            .complete_projected(work, Ok(projected), 100)
            .unwrap()
    );
    assert!(
        session.rows[&Key::Owned(1)]
            .owned
            .as_ref()
            .unwrap()
            .receiver_source
            .is_none()
    );
    assert!(
        session.rows[&Key::Owned(1)]
            .owned
            .as_ref()
            .unwrap()
            .receiver_failure
            .is_none()
    );
    let before_repeat = session.retained_bytes;
    session
        .observe_receiver_failure(1, Some(failure(wire::FailureCode::ResourceGone)))
        .unwrap();
    assert_eq!(session.retained_bytes, before_repeat);
    drain(&mut session);
    let mut disabled = spec;
    disabled.settings.as_mut().unwrap().enabled = Some(false);
    input(&mut session, vec![present(1, 2, disabled.clone())]).unwrap();
    let work = session.take_preparation().unwrap().unwrap();
    let before_policy = session.retained_bytes;
    let before_policy_sequence = session.rows[&Key::Owned(1)]
        .owned
        .as_ref()
        .unwrap()
        .storage_sequence;
    session.observe_resource(binding(false)).unwrap();
    assert!(
        matches!(session.compile_execution(&work, None, |_| panic!("unavailable binding needs no source lookup")), Err(error) if error.code == wire::FailureCode::Incompatible as i32)
    );
    assert_eq!(
        session.rows[&Key::Owned(1)]
            .owned
            .as_ref()
            .unwrap()
            .storage_sequence,
        before_policy_sequence
    );
    session.observe_resource(binding(true)).unwrap();
    let ExecutionAction::Prepare {
        work: issued,
        resource:
            crate::control::native::PreparedResource::Receiver(
                crate::control::native::ReceiverPreparation::Policy(policy),
            ),
    } = session
        .compile_execution(&work, None, |_| {
            panic!("opened binding needs no source lookup")
        })
        .unwrap()
        .into_action()
    else {
        panic!("policy-only command");
    };
    assert!(issued == work);
    assert!(!policy.enabled && policy.video.is_none());
    assert_eq!(session.retained_bytes, before_policy);
    let disabled_execution = crate::participant::policy::ReceiverExecution {
        enabled: false,
        ready: false,
        ..execution
    };
    let wrong_source = crate::participant::policy::ReceiverExecution {
        source: crate::entity::ConnectionId::new()
            .fixture_publication(crate::entity::TrackKind::Data, "wrong-source"),
        ..disabled_execution
    };
    assert!(
        matches!(session.project_receiver(&work, &wrong_source), Err(error) if error.code == wire::FailureCode::ResourceGone as i32)
    );
    let projected = session
        .project_receiver(&work, &disabled_execution)
        .unwrap();
    let live = &projected.live;
    assert_eq!(live.execution, wire::ExecutionStatus::Failed as i32);
    assert_eq!(
        live.execution_failure.as_ref().unwrap().code,
        wire::FailureCode::InvalidConfiguration as i32
    );
    assert_eq!(live.spec, Some(disabled.clone()));
    assert_eq!(live.participant, Some(owner));
    assert_eq!(live.track, Some(track));
    let external_id = crate::entity::ParticipantId::new("Owner").unwrap();
    assert!(matches!(
        session
            .complete_execution(
                work,
                Ok(ResourceExecution::Receiver(disabled_execution)),
                &external_id,
                100,
            )
            .unwrap(),
        ExecutionDisposition::Accepted
    ));
    drain(&mut session);
    input(&mut session, vec![present(1, 3, disabled.clone())]).unwrap();
    let work = session.take_preparation().unwrap().unwrap();
    assert!(matches!(
        session
            .complete_execution(
                work,
                Ok(ResourceExecution::Receiver(wrong_source)),
                &external_id,
                100,
            )
            .unwrap(),
        ExecutionDisposition::Rejected
    ));
    let row = &session.rows[&Key::Owned(1)];
    assert_eq!(row.value.evaluated_revision, 3);
    assert_eq!(
        row.value.rejection.as_ref().unwrap().code,
        wire::FailureCode::ResourceGone as i32
    );
    assert!(row.owned.as_ref().unwrap().active.is_none());
    drain(&mut session);
    input(&mut session, vec![present(1, 4, disabled)]).unwrap();
    let work = session.take_preparation().unwrap().unwrap();
    let before_rejection_sequence = session.rows[&Key::Owned(1)]
        .owned
        .as_ref()
        .unwrap()
        .storage_sequence;
    session.observe_resource(binding(false)).unwrap();
    assert!(
        session
            .prepare_execution(&work, 100, None, |_| panic!(
                "unavailable binding needs no source lookup"
            ))
            .unwrap()
            .is_none()
    );
    let value = &session.rows[&Key::Owned(1)].value;
    assert_eq!(value.evaluated_revision, 4);
    assert_eq!(
        session.rows[&Key::Owned(1)]
            .owned
            .as_ref()
            .unwrap()
            .storage_sequence,
        before_rejection_sequence
    );
    assert_eq!(
        value.rejection.as_ref().unwrap().code,
        wire::FailureCode::Incompatible as i32
    );
    assert!(
        session.rows[&Key::Owned(1)]
            .owned
            .as_ref()
            .unwrap()
            .active
            .is_none()
    );
    assert!(session.take_preparation().unwrap().is_none());
    drain(&mut session);
    assert_eq!(input(&mut session, vec![absent(1, 5)]).unwrap(), vec![1]);
    session.cutoff_complete(1).unwrap();
    drain(&mut session);
    session.reserve_execution(1, 0).unwrap();
    assert!(!session.rows.contains_key(&Key::Owned(1)));
    assert_eq!(session.retained_bytes, base);
}

#[test]
fn public_revision_and_private_counter_exhaustion_never_wrap() {
    let mut session = session();
    session.last_revision = u64::MAX - 1;
    input(
        &mut session,
        vec![present(u64::MAX, u64::MAX, publication("last", true))],
    )
    .unwrap();
    assert!(matches!(
        input(
            &mut session,
            vec![present(u64::MAX, u64::MAX, publication("last", true))]
        ),
        Err(InputError::Revision)
    ));
    session.version = u64::MAX;
    assert!(matches!(
        session.authorization(10000, 2, ResourcePermissions::RoomBound),
        Err(OutputError::Capacity)
    ));
    assert_eq!(session.version, u64::MAX);
    session.execution_sequence = u64::MAX;
    let retained = session.retained_bytes;
    assert!(matches!(
        session.retirement_command(1),
        Err(OutputError::Capacity)
    ));
    assert!(matches!(
        session.take_preparation(),
        Err(OutputError::Capacity)
    ));
    assert_eq!(session.execution_sequence, u64::MAX);
    assert_eq!(session.retained_bytes, retained);
}

#[test]
fn denied_source_lookup_never_resolves_or_prepares_even_in_another_room() {
    use pulsebeam_core::auth::grants::{Action, Grant, GrantSet, TrackKind};

    let mut model = session();
    model
        .authorization(
            10_000,
            2,
            ResourcePermissions::Grants {
                joined_room: Some(crate::entity::RoomId::new("room").unwrap()),
                grants: GrantSet::new(vec![
                    Grant::new(
                        vec!["room".into(), "other".into()],
                        vec![Action::Subscribe],
                        Some("allowed-publisher".into()),
                        Some(TrackKind::Data),
                        Some("label".into()),
                    )
                    .unwrap(),
                ])
                .unwrap(),
            },
        )
        .unwrap();
    for (reference, room) in [(81, "room"), (82, "other")] {
        let spec = wire::ResourceSpec {
            settings: Some(wire::ResourceSettings {
                enabled: Some(true),
                receive: None,
            }),
            declaration: Some(wire::resource_spec::Declaration::SourceLookup(
                wire::SourceLookupSpec {
                    source: Some(wire::LogicalSourceAddress {
                        room: room.into(),
                        participant_id: "denied-publisher".into(),
                        kind: wire::TrackKind::Data as i32,
                        track_id: "label".into(),
                    }),
                },
            )),
        };
        assert!(
            input(&mut model, vec![present(reference, reference, spec)])
                .unwrap()
                .is_empty()
        );
        let value = owned(&model, reference);
        assert_eq!(value.evaluated_revision, reference);
        assert_eq!(
            value.rejection.as_ref().unwrap().code,
            wire::FailureCode::PermissionDenied as i32
        );
        assert!(matches!(
            value.value,
            Some(wire::confirmed_state::Value::Absent(_))
        ));
        assert!(model.rows[&Key::Owned(reference)].owned.is_none());
    }
    model
        .refresh_source_lookups(32, 100, &DataBranchIds::default(), |_| {
            panic!("denied lookup reached source resolution")
        })
        .unwrap();
    assert!(!model.lookup_refresh_pending());
    assert!(model.take_preparation().unwrap().is_none());
}

#[test]
fn source_lookup_is_owned_nonforwarding_and_updates_atomic_borrowed_descriptors() {
    use crate::entity::{ConnectionId, ParticipantId, RoomId, TrackKind};
    use crate::track::{DataLane, TrackMeta};

    let mut model = session();
    let grant = pulsebeam_core::auth::grants::Grant::new(
        vec!["room".into()],
        vec![pulsebeam_core::auth::grants::Action::Subscribe],
        Some("publisher".into()),
        Some(pulsebeam_core::auth::grants::TrackKind::Data),
        Some("exact-label".into()),
    )
    .unwrap();
    model
        .authorization(
            10_000,
            2,
            ResourcePermissions::Grants {
                joined_room: Some(crate::entity::RoomId::new("room").unwrap()),
                grants: pulsebeam_core::auth::grants::GrantSet::new(vec![grant]).unwrap(),
            },
        )
        .unwrap();
    let room = RoomId::new("room").unwrap();
    let connection = ConnectionId::new();
    let publisher_id = ParticipantId::new("publisher").unwrap();
    let publisher = ParticipantId::new("publisher").unwrap();
    let label = "exact-label";
    let address = wire::LogicalSourceAddress {
        room: "room".into(),
        participant_id: publisher.as_str().into(),
        kind: wire::TrackKind::Data as i32,
        track_id: label.into(),
    };
    let lookup = |enabled| wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(enabled),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::SourceLookup(
            wire::SourceLookupSpec {
                source: Some(address.clone()),
            },
        )),
    };
    let data_track = |connection: ConnectionId| {
        let id = connection.fixture_publication(TrackKind::Data, label);
        Track::data_publication(
            TrackMeta {
                room: crate::entity::ProjectRoomKey::fixture(room),
                id,
                origin: publisher_id,
                connection_id: connection,
                track_id: Some(crate::entity::TrackId::new(&label).unwrap()),
            },
            DataLane::Ordered,
        )
    };

    assert!(
        input(&mut model, vec![present(51, 51, lookup(true))])
            .unwrap()
            .is_empty()
    );
    assert!(model.preparations.is_empty());
    let state = &model.rows[&Key::Owned(51)].value;
    let Some(wire::confirmed_state::Value::SourceLookup(initial)) = &state.value else {
        panic!("lookup must be projected immediately")
    };
    assert!(initial.participant.is_none() && initial.track.is_none());
    let initial_revision = state.evaluated_revision;

    let first = data_track(connection);
    model
        .observe_participant(wire::Participant {
            participant_id: publisher_id.as_str().to_owned(),
            room_id: "room".into(),
        })
        .unwrap();
    model
        .observe_candidate(super::super::ResolvedSource {
            track: &first,
            publisher: &publisher,
        })
        .unwrap();
    assert!(
        !model
            .permissions
            .can_discover_source(&super::super::ResolvedSource {
                track: &first,
                publisher: &publisher,
            })
    );
    assert!(
        !model
            .rows
            .keys()
            .any(|key| matches!(key, Key::Publication(_)))
    );
    model
        .refresh_source_lookups(32, 100, &DataBranchIds::default(), |_| {
            Some(super::super::ResolvedSource {
                track: &first,
                publisher: &publisher,
            })
        })
        .unwrap();
    let state = &model.rows[&Key::Owned(51)].value;
    let Some(wire::confirmed_state::Value::SourceLookup(available)) = &state.value else {
        panic!("lookup state")
    };
    assert_eq!(state.evaluated_revision, initial_revision);
    assert_eq!(
        available.participant.as_ref().unwrap().participant_id,
        "publisher"
    );
    assert_eq!(
        available.track.as_ref().unwrap().publication_id,
        first.id().as_str()
    );

    let invalid = wire::ResourceSpec {
        settings: None,
        declaration: Some(wire::resource_spec::Declaration::SourceLookup(
            wire::SourceLookupSpec {
                source: Some(address.clone()),
            },
        )),
    };
    input(&mut model, vec![present(51, 52, invalid)]).unwrap();
    let rejected = &model.rows[&Key::Owned(51)].value;
    assert_eq!(
        rejected.rejection.as_ref().unwrap().code,
        wire::FailureCode::InvalidConfiguration as i32
    );
    assert!(matches!(
        &rejected.value,
        Some(wire::confirmed_state::Value::SourceLookup(state))
            if state.participant.is_some() && state.track.is_some()
    ));

    input(&mut model, vec![present(51, 53, lookup(false))]).unwrap();
    let state = &model.rows[&Key::Owned(51)].value;
    let Some(wire::confirmed_state::Value::SourceLookup(disabled)) = &state.value else {
        panic!("disabled lookup state")
    };
    assert!(disabled.participant.is_none() && disabled.track.is_none());

    input(&mut model, vec![present(51, 54, lookup(true))]).unwrap();
    let replacement_connection = ConnectionId::new();
    let replacement = data_track(replacement_connection);
    model
        .refresh_source_lookups(32, 100, &DataBranchIds::default(), |_| {
            Some(super::super::ResolvedSource {
                track: &replacement,
                publisher: &publisher,
            })
        })
        .unwrap();
    let state = &model.rows[&Key::Owned(51)].value;
    let Some(wire::confirmed_state::Value::SourceLookup(republished)) = &state.value else {
        panic!("republished lookup state")
    };
    assert_eq!(
        republished.track.as_ref().unwrap().publication_id,
        replacement.id().as_str()
    );

    model
        .refresh_source_lookups(32, 100, &DataBranchIds::default(), |_| None)
        .unwrap();
    let state = &model.rows[&Key::Owned(51)].value;
    let Some(wire::confirmed_state::Value::SourceLookup(unpublished)) = &state.value else {
        panic!("unpublished lookup state")
    };
    assert!(unpublished.participant.is_none() && unpublished.track.is_none());
    assert!(input(&mut model, vec![absent(51, 55)]).unwrap().is_empty());
    assert!(matches!(
        model.rows[&Key::Owned(51)].value.value,
        Some(wire::confirmed_state::Value::Absent(_))
    ));
}

#[test]
fn fabric_receiver_offers_are_versioned_and_rotate_with_the_authenticated_source() {
    use crate::entity::{ConnectionId, ParticipantId, RoomId, TrackId, TrackKind};
    use crate::track::{DataLane, TrackMeta};
    use std::num::NonZeroU64;

    let mut model = session();
    let publisher = ParticipantId::new("publisher").unwrap();
    let room = RoomId::new("room").unwrap();
    let connection = ConnectionId::new();
    let meta = TrackMeta {
        room: crate::entity::ProjectRoomKey::fixture(room),
        id: connection.fixture_publication(TrackKind::Data, "cursor"),
        origin: publisher,
        connection_id: connection,
        track_id: Some(TrackId::new("cursor").unwrap()),
    };
    let fabric = |source| {
        Track::native_data_publication(
            meta.clone(),
            DataLane::Latest,
            NonZeroU64::new(source),
            NonZeroU64::new(source).map(|_| wire::DataV1CaptureClock {
                id: "source-clock".into(),
                epoch: source,
            }),
        )
    };
    let first = fabric(10);
    let legacy = fabric(0);
    let subscription = wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::LogicalSubscription(
            wire::LogicalSubscriptionSpec {
                data_version: Some(u32::from(pulsebeam_proto::data_endpoint::DATA_V1)),
                source: Some(wire::LogicalSourceAddress {
                    room: room.to_string(),
                    participant_id: publisher.to_string(),
                    kind: wire::TrackKind::Data as i32,
                    track_id: "cursor".into(),
                }),
            },
        )),
    };
    input(&mut model, vec![present(1, 1, subscription.clone())]).unwrap();
    let branches = DataBranchIds::default();
    let offer = |model: &mut NativeSession, track: &Track| {
        model
            .refresh_source_lookups(32, 100, &branches, |_| {
                Some(super::super::ResolvedSource {
                    track,
                    publisher: &publisher,
                })
            })
            .unwrap();
        let Some(wire::confirmed_state::Value::LogicalSubscription(state)) =
            &model.rows[&Key::Owned(1)].value.value
        else {
            panic!("logical subscription");
        };
        assert_eq!(state.source.is_some(), state.data_v1.is_some());
        if let Some(offer) = &state.data_v1 {
            assert_eq!(
                offer.capture_clock,
                Some(wire::DataV1CaptureClock {
                    id: "source-clock".into(),
                    epoch: offer.source
                })
            );
        }
        (
            state.source_revision,
            state
                .data_v1
                .as_ref()
                .map(|offer| (offer.source, offer.branch)),
        )
    };
    assert_eq!(offer(&mut model, &legacy), (0, None));
    assert_eq!(offer(&mut model, &first), (1, Some((10, 1))));
    assert_eq!(offer(&mut model, &first), (1, Some((10, 1))));
    assert_eq!(offer(&mut model, &fabric(11)), (2, Some((11, 2))));
    assert_eq!(offer(&mut model, &legacy), (3, None));
    assert_eq!(offer(&mut model, &first), (4, Some((10, 3))));
    let mut other_subscriber = session();
    input(
        &mut other_subscriber,
        vec![present(1, 1, subscription.clone())],
    )
    .unwrap();
    assert_eq!(offer(&mut other_subscriber, &first), (1, Some((10, 4))));
    assert_eq!(offer(&mut model, &first), (4, Some((10, 3))));

    let parameters = wire::SctpDataV1Parameters {
        stream_id: Some(2),
        version: u32::from(pulsebeam_proto::data_endpoint::DATA_V1),
        channel_incarnation: 19,
        delivery: wire::DataV1Delivery::Latest as i32,
    };
    let report = |source_revision, transport| wire::ClientStateBatch {
        records: vec![wire::ClientStateRecord {
            owned: 1,
            desired: None,
            execution: Some(wire::ExecutionReport {
                revision: 2,
                outcome: Some(wire::execution_report::Outcome::LogicalReceiver(
                    wire::LogicalReceiverReport {
                        source_revision,
                        outcome: Some(wire::logical_receiver_report::Outcome::Prepared(
                            wire::TrackBinding {
                                transport: Some(transport),
                            },
                        )),
                    },
                )),
            }),
        }],
        ..Default::default()
    };
    assert!(
        model
            .receive_input(
                &codec::encode_client(&report(
                    4,
                    wire::track_binding::Transport::SctpDataV1(parameters)
                ))
                .unwrap(),
                100
            )
            .unwrap()
            .is_empty()
    );
    let work = model.take_preparation().unwrap().unwrap();
    let stale = fabric(11);
    assert!(
        matches!(model.compile_execution(&work, None, |_| Some(super::super::ResolvedSource { track: &stale, publisher: &publisher })),
        Err(error) if error.code() == wire::FailureCode::Incompatible)
    );
    use crate::control::native::{PreparedResource, ReceiveTransport, ReceiverPreparation};
    let ExecutionAction::Prepare {
        resource: PreparedResource::Receiver(ReceiverPreparation::Ready(plan)),
        ..
    } = model
        .compile_execution(&work, None, |_| {
            Some(super::super::ResolvedSource {
                track: &first,
                publisher: &publisher,
            })
        })
        .unwrap()
        .into_action()
    else {
        panic!("exact receiver preparation")
    };
    let ReceiveTransport::FabricData(fabric) = plan.transport else {
        panic!("Fabric DATA receiver")
    };
    assert_eq!(fabric.parameters, parameters);
    assert_eq!(fabric.endpoint.source.get(), 10);
    assert_eq!(fabric.endpoint.branch.get(), 3);

    let mut quic = NativeSession::with_binding_profile(validation::BindingProfile::Quic);
    quic.authorization(900, 1, ResourcePermissions::RoomBound)
        .unwrap();
    quic.set_cid(
        ChannelId::from_value(0),
        pulsebeam_runtime::time::Instant::now(),
    );
    input(&mut quic, vec![present(1, 1, subscription)]).unwrap();
    assert_eq!(offer(&mut quic, &first), (1, Some((10, 5))));
    let parameters = wire::QuicDataV1Parameters {
        binding_id: 2,
        version: u32::from(pulsebeam_proto::data_endpoint::DATA_V1),
        incarnation: 19,
        delivery: wire::DataV1Delivery::Latest as i32,
    };
    quic.receive_input(
        &codec::encode_quic_client(&report(
            1,
            wire::track_binding::Transport::QuicDataV1(parameters),
        ))
        .unwrap(),
        100,
    )
    .unwrap();
    let work = quic.take_preparation().unwrap().unwrap();
    let ExecutionAction::Prepare {
        resource: PreparedResource::Receiver(ReceiverPreparation::Ready(plan)),
        ..
    } = quic
        .compile_execution(&work, None, |_| {
            Some(super::super::ResolvedSource {
                track: &first,
                publisher: &publisher,
            })
        })
        .unwrap()
        .into_action()
    else {
        panic!("QUIC receiver preparation")
    };
    let ReceiveTransport::QuicData(fabric) = plan.transport else {
        panic!("QUIC DATA receiver")
    };
    assert_eq!(fabric.parameters, parameters);
    assert_eq!(fabric.endpoint.source.get(), 10);
    assert_eq!(fabric.endpoint.branch.get(), 5);
}

#[test]
fn logical_subscription_keeps_one_reservation_across_scoped_receiver_replacement() {
    use crate::entity::{ConnectionId, ParticipantId, RoomId, TrackId, TrackKind};
    use crate::track::{DataLane, TrackMeta};
    use pulsebeam_core::auth::grants::{Action, Grant, GrantSet};
    use wire::confirmed_state::Value;

    let mut model = session();
    let room = RoomId::new("source-room").unwrap();
    let publisher = ParticipantId::new("publisher").unwrap();
    let grant = Grant::new(
        vec![room.to_string()],
        vec![Action::Subscribe],
        Some(publisher.to_string()),
        Some(pulsebeam_core::auth::grants::TrackKind::Data),
        Some("updates".into()),
    )
    .unwrap();
    model
        .authorization(
            10_000,
            2,
            ResourcePermissions::Grants {
                joined_room: None,
                grants: GrantSet::new(vec![grant]).unwrap(),
            },
        )
        .unwrap();
    let subscription = |enabled, track_id: &str| wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(enabled),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::LogicalSubscription(
            wire::LogicalSubscriptionSpec {
                data_version: Some(u32::from(pulsebeam_proto::data_endpoint::DATA_V1)),
                source: Some(wire::LogicalSourceAddress {
                    room: room.to_string(),
                    participant_id: publisher.to_string(),
                    kind: wire::TrackKind::Data as i32,
                    track_id: track_id.into(),
                }),
            },
        )),
    };
    let source_ids = std::cell::Cell::new(0);
    let data_track = |label: &str| {
        source_ids.set(source_ids.get() + 1);
        let connection = ConnectionId::new();
        Track::native_data_publication(
            TrackMeta {
                room: crate::entity::ProjectRoomKey::fixture(room),
                id: connection.fixture_publication(TrackKind::Data, label),
                origin: publisher,
                connection_id: connection,
                track_id: Some(TrackId::new(label).unwrap()),
            },
            DataLane::Latest,
            std::num::NonZeroU64::new(source_ids.get()),
            None,
        )
    };
    let assert_state =
        |model: &NativeSession, evaluated_revision, source_revision, source: Option<&Track>| {
            let row = &model.rows[&Key::Owned(1)];
            let Some(Value::LogicalSubscription(state)) = &row.value.value else {
                panic!("one admitted logical subscription");
            };
            assert_eq!(row.value.evaluated_revision, evaluated_revision);
            assert!(row.value.rejection.is_none());
            assert_eq!(state.source_revision, source_revision);
            assert_eq!(
                state
                    .source
                    .as_ref()
                    .map(|track| &track.publication_id)
                    .cloned(),
                source.map(|track| track.id().as_str()),
            );
        };
    let branches = DataBranchIds::default();
    let offer = |model: &mut NativeSession, track| {
        model
            .refresh_source_lookups(32, 100, &branches, |_| {
                Some(super::super::ResolvedSource {
                    track,
                    publisher: &publisher,
                })
            })
            .unwrap();
    };

    assert!(
        input(
            &mut model,
            vec![present(1, 1, subscription(true, "updates"))]
        )
        .unwrap()
        .is_empty()
    );
    assert_state(&model, 1, 0, None);
    let interests = model.take_lookup_changes();
    assert!(matches!(
        interests.as_slice(),
        [LookupInterestChange {
            reference: 1,
            active: true,
            ..
        }]
    ));
    assert!(model.take_preparation().unwrap().is_none());

    let unrelated = data_track("other");
    offer(&mut model, &unrelated);
    assert_state(&model, 1, 0, None);
    let first = data_track("updates");
    let second = data_track("updates");
    let retained = model.retained_bytes;
    offer(&mut model, &first);
    offer(&mut model, &first);
    assert_state(&model, 1, 1, Some(&first));
    let report = |model: &mut NativeSession, revision, source_revision, stream_id| {
        let batch = wire::ClientStateBatch {
            records: vec![wire::ClientStateRecord {
                owned: 1,
                desired: None,
                execution: Some(wire::ExecutionReport {
                    revision,
                    outcome: Some(wire::execution_report::Outcome::LogicalReceiver(
                        wire::LogicalReceiverReport {
                            source_revision,
                            outcome: Some(wire::logical_receiver_report::Outcome::Prepared(
                                crate::control::native::fixture::data_v1_binding(stream_id),
                            )),
                        },
                    )),
                }),
            }],
            ..Default::default()
        };
        let commands = model
            .receive_input(&codec::encode_client(&batch).unwrap(), 100)
            .unwrap();
        assert!(commands.is_empty());
    };
    let compile = |model: &mut NativeSession, track: &Track| {
        let work = model.take_preparation().unwrap().unwrap();
        let command = model
            .prepare_execution_in_room(&work, 100, None, |room, id| {
                assert_eq!(room, "source-room");
                assert_eq!(id, track.id());
                Some(super::super::ResolvedSource {
                    track,
                    publisher: &publisher,
                })
            })
            .unwrap()
            .unwrap();
        assert!(matches!(
            command.into_action(),
            ExecutionAction::Prepare { .. }
        ));
        work
    };
    let complete = |model: &mut NativeSession, work, track: &Track| {
        model
            .observe_completion(
                super::super::ExecutionCompletion {
                    work,
                    result: Ok(ResourceExecution::Receiver(
                        crate::participant::policy::ReceiverExecution {
                            source: track.id(),
                            enabled: true,
                            ready: true,
                            failed: false,
                        },
                    )),
                    released: false,
                    unix_now: 100,
                },
                &publisher,
            )
            .unwrap();
    };
    let retire = |model: &mut NativeSession, expected| {
        let command = model.take_logical_retirement().unwrap().unwrap();
        let ExecutionAction::Retire(scope) = command.into_action() else {
            panic!("physical receiver cutoff");
        };
        assert_eq!(scope.binding, expected);
    };
    let released = |model: &mut NativeSession, binding| {
        model
            .observe_resource(crate::participant::policy::ResourceObservation::Released { binding })
            .unwrap();
    };

    report(&mut model, 2, 1, 3);
    let old = compile(&mut model, &first);
    assert_eq!(old.scope().binding, 2);
    offer(&mut model, &second);
    assert_state(&model, 1, 2, None);
    retire(&mut model, 2);
    complete(&mut model, old, &first);
    offer(&mut model, &second);
    assert_state(&model, 1, 2, None);
    released(&mut model, 2);

    offer(&mut model, &second);
    assert_state(&model, 1, 3, Some(&second));
    assert_eq!(model.retained_bytes, retained);
    assert!(model.take_lookup_changes().is_empty());
    report(&mut model, 3, 1, 5);
    assert!(model.take_preparation().unwrap().is_none());
    report(&mut model, 4, 3, 7);
    let new = compile(&mut model, &second);
    assert_eq!(new.scope().binding, 4);
    complete(&mut model, new, &second);
    released(&mut model, 2);
    assert_state(&model, 1, 3, Some(&second));
    assert!(matches!(&model.rows[&Key::Owned(1)].value.value,
        Some(Value::LogicalSubscription(state)) if matches!(&state.execution,
            Some(wire::logical_subscription_state::Execution::Installed(binding))
                if matches!(&binding.transport, Some(wire::track_binding::Transport::SctpDataV1(sctp)) if sctp.stream_id == Some(7))
        )
    ));

    input(
        &mut model,
        vec![present(1, 5, subscription(false, "updates"))],
    )
    .unwrap();
    retire(&mut model, 4);
    offer(&mut model, &second);
    assert_state(&model, 5, 4, None);
    released(&mut model, 4);
    input(
        &mut model,
        vec![present(1, 6, subscription(true, "updates"))],
    )
    .unwrap();
    offer(&mut model, &second);
    assert_state(&model, 6, 5, Some(&second));

    assert!(input(&mut model, vec![absent(1, 7)]).unwrap().is_empty());
    assert!(matches!(
        model.rows[&Key::Owned(1)].value.value,
        Some(Value::Absent(_))
    ));
    assert!(model.lookup_interests.is_empty());
    assert!(model.take_preparation().unwrap().is_none());
}

#[test]
fn source_lookup_revocation_and_close_finish_locally_without_execution_commands() {
    let mut model = NativeSession::default();
    model
        .authorization(10_000, 1, ResourcePermissions::RoomBound)
        .unwrap();
    let spec = wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::SourceLookup(
            wire::SourceLookupSpec {
                source: Some(wire::LogicalSourceAddress {
                    room: "room".into(),
                    participant_id: "publisher".into(),
                    kind: wire::TrackKind::Data as i32,
                    track_id: "label".into(),
                }),
            },
        )),
    };
    input(&mut model, vec![present(1, 1, spec)]).unwrap();
    assert!(model.preparations.is_empty());
    model
        .authorization(10_000, 2, ResourcePermissions::Denied)
        .unwrap();
    let mut commands = Vec::new();
    let mut turns = 0;
    while model.revocation_pending() {
        commands.extend(
            model
                .service_revocations(32, |_| panic!("lookup revocation is local"))
                .unwrap(),
        );
        turns += 1;
        assert!(turns <= 4, "permission sweep failed to settle");
    }
    assert!(commands.is_empty());
    assert!(matches!(
        &model.rows[&Key::Owned(1)].value.value,
        Some(wire::confirmed_state::Value::Absent(absent))
            if absent.reason == wire::AbsenceReason::PermissionRevoked as i32
    ));
    assert!(
        !model.rows[&Key::Owned(1)]
            .owned
            .as_ref()
            .unwrap()
            .cutoff_pending
    );
    assert!(model.preparations.is_empty());

    let mut closing = NativeSession::default();
    closing
        .authorization(10_000, 1, ResourcePermissions::RoomBound)
        .unwrap();
    input(
        &mut closing,
        vec![present(
            3,
            3,
            wire::ResourceSpec {
                settings: Some(wire::ResourceSettings {
                    enabled: Some(true),
                    receive: None,
                }),
                declaration: Some(wire::resource_spec::Declaration::SourceLookup(
                    wire::SourceLookupSpec {
                        source: Some(wire::LogicalSourceAddress {
                            room: "room".into(),
                            participant_id: "publisher".into(),
                            kind: wire::TrackKind::Data as i32,
                            track_id: "label".into(),
                        }),
                    },
                )),
            },
        )],
    )
    .unwrap();
    closing.close(wire::CloseReason::ProtocolError).unwrap();
    closing.finish_closed_lookups(32).unwrap();
    assert!(!closing.closed_lookup_pending);
    assert!(matches!(
        &closing.rows[&Key::Owned(3)].value.value,
        Some(wire::confirmed_state::Value::Absent(absent))
            if absent.reason == wire::AbsenceReason::Removed as i32
    ));
    assert!(closing.preparations.is_empty());
}

#[test]
fn source_lookup_work_is_cursor_bounded_and_authorized_for_other_rooms() {
    use crate::entity::RoomId;
    use pulsebeam_core::auth::grants::{Action, Grant, GrantSet, TrackKind as GrantKind};

    let address = || wire::LogicalSourceAddress {
        room: "room".into(),
        participant_id: "publisher".into(),
        kind: wire::TrackKind::Data as i32,
        track_id: "label".into(),
    };
    let spec = || wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::SourceLookup(
            wire::SourceLookupSpec {
                source: Some(address()),
            },
        )),
    };
    let mut bounded = NativeSession::default();
    bounded
        .authorization(10_000, 1, ResourcePermissions::RoomBound)
        .unwrap();
    for start in (1..=40).step_by(16) {
        let end = (start + 15).min(40);
        input(
            &mut bounded,
            (start..=end)
                .map(|reference| present(reference, reference, spec()))
                .collect(),
        )
        .unwrap();
    }
    assert!(bounded.preparations.is_empty());
    let branches = DataBranchIds::default();
    bounded
        .refresh_source_lookups(8, 100, &branches, |_| None)
        .unwrap();
    assert!(bounded.lookup_refresh_pending());
    let mut turns = 1;
    while bounded.lookup_refresh_pending() {
        bounded
            .refresh_source_lookups(8, 100, &branches, |_| None)
            .unwrap();
        turns += 1;
        assert!(turns <= 8, "lookup cursor failed to make bounded progress");
    }
    assert!(turns > 1);
    assert!(bounded.retained_bytes < STATE_BYTES);

    let other_room_grant = Grant::new(
        vec!["other".into()],
        vec![Action::Subscribe],
        Some("publisher".into()),
        Some(GrantKind::Data),
        Some("label".into()),
    )
    .unwrap();
    let mut scoped = NativeSession::default();
    scoped
        .authorization(
            10_000,
            1,
            ResourcePermissions::Grants {
                joined_room: Some(RoomId::new("current").unwrap()),
                grants: GrantSet::new(vec![other_room_grant]).unwrap(),
            },
        )
        .unwrap();
    let mut other_room = spec();
    let Some(wire::resource_spec::Declaration::SourceLookup(lookup)) =
        other_room.declaration.as_mut()
    else {
        panic!("source lookup declaration")
    };
    lookup.source.as_mut().unwrap().room = "other".into();
    input(&mut scoped, vec![present(1, 1, other_room)]).unwrap();
    assert!(matches!(
        &scoped.rows[&Key::Owned(1)].value.value,
        Some(wire::confirmed_state::Value::SourceLookup(lookup))
            if lookup.spec.as_ref().and_then(|spec| match spec.declaration.as_ref() {
                Some(wire::resource_spec::Declaration::SourceLookup(value)) => value.source.as_ref(),
                _ => None,
            }).is_some_and(|source| source.room == "other")
    ));
    assert!(scoped.preparations.is_empty());

    let mut denied = NativeSession::default();
    denied
        .authorization(
            10_000,
            1,
            ResourcePermissions::Grants {
                joined_room: Some(RoomId::new("current").unwrap()),
                grants: GrantSet::default(),
            },
        )
        .unwrap();
    for (reference, room) in [(1, "current"), (2, "other")] {
        let mut request = spec();
        let Some(wire::resource_spec::Declaration::SourceLookup(lookup)) =
            request.declaration.as_mut()
        else {
            panic!("source lookup declaration")
        };
        lookup.source.as_mut().unwrap().room = room.into();
        input(&mut denied, vec![present(reference, reference, request)]).unwrap();
        assert!(matches!(
            &denied.rows[&Key::Owned(reference)].value,
            wire::ConfirmedState {
                rejection: Some(failure),
                value: Some(wire::confirmed_state::Value::Absent(_)),
                ..
            } if failure.code == wire::FailureCode::PermissionDenied as i32
        ));
    }
    assert!(denied.preparations.is_empty());
}

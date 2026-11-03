use super::*;

fn session() -> NativeSession {
    let mut session = NativeSession::default();
    session.authorization(10_000, 1).unwrap();
    session.set_cid(ChannelId::from_value(0));
    session
}

fn publication(label: &str, enabled: bool) -> wire::ResourceSpec {
    wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(enabled),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::Publication(
            wire::PublicationSpec {
                label: label.to_owned(),
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

fn present(reference: u64, revision: u64, spec: wire::ResourceSpec) -> wire::StateRecord {
    desired(
        reference,
        revision,
        wire::desired_state::Value::Present(Box::new(spec)),
    )
}

fn absent(reference: u64, revision: u64) -> wire::StateRecord {
    desired(
        reference,
        revision,
        wire::desired_state::Value::Absent(true),
    )
}

fn desired(reference: u64, revision: u64, value: wire::desired_state::Value) -> wire::StateRecord {
    wire::StateRecord {
        key: Some(Key::Owned(reference).wire()),
        state: Some(wire::state_record::State::Desired(wire::DesiredState {
            revision,
            value: Some(value),
        })),
    }
}

fn input(
    session: &mut NativeSession,
    records: Vec<wire::StateRecord>,
) -> Result<Vec<u64>, InputError> {
    let bytes = codec::encode(
        &wire::StateBatch {
            records,
            initial_complete: false,
        },
        validation::Direction::Client,
    )
    .unwrap();
    session.apply_input(&bytes)
}

fn source(reference: u64, label: &str) -> wire::Track {
    wire::Track {
        track_id: format!("source-{reference}"),
        participant_id: "owner".into(),
        label: label.into(),
        kind: Some(wire::track::Kind::Data(wire::DataTrack {
            delivery: wire::DataDelivery::Ordered as i32,
        })),
    }
}

fn live(work: &Preparation) -> wire::LiveResource {
    let Some(wire::resource_spec::Declaration::Publication(publication)) = &work.spec.declaration
    else {
        panic!("publication fixture")
    };
    wire::LiveResource {
        spec: Some(work.spec.clone()),
        participant: Some(wire::Participant {
            participant_id: "owner".into(),
            participant_external_id: "Owner".into(),
        }),
        track: Some(source(work.reference, &publication.label)),
        execution: if work.spec.settings.as_ref().unwrap().enabled == Some(true) {
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
    let value = live(&work);
    assert!(session.complete(work, Ok(value), 100).unwrap());
}

fn output(session: &mut NativeSession) -> Option<wire::StateBatch> {
    session.poll().unwrap().map(|output| {
        assert!(output.bytes.len() <= codec::MAX_FRAME_SIZE);
        codec::decode(&output.bytes, validation::Direction::Server).unwrap()
    })
}

fn drain(session: &mut NativeSession) -> Vec<wire::StateBatch> {
    let mut batches = Vec::new();
    for _ in 0..2048 {
        if !session.needs_poll() {
            return batches;
        }
        if let Some(batch) = output(session) {
            assert!(batch.records.len() <= codec::MAX_RECORDS);
            batches.push(batch);
            session.commit_sent();
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
fn superseded_preparation_cannot_publish_and_latest_target_is_serialized() {
    let mut session = session();
    input(
        &mut session,
        vec![present(1, 1, publication("topic", true))],
    )
    .unwrap();
    let old = session.take_preparation().unwrap().unwrap();
    let old_value = live(&old);
    input(
        &mut session,
        vec![present(1, 2, publication("topic", false))],
    )
    .unwrap();
    assert!(session.take_preparation().unwrap().is_none());
    assert!(!session.complete(old, Ok(old_value), 100).unwrap());
    let latest = session.take_preparation().unwrap().unwrap();
    assert_eq!(latest.revision, 2);
    let latest_value = live(&latest);
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
    assert!(!session.rows.contains_key(&Key::Track("source-1".into())));
}

#[test]
fn invalid_pending_declaration_terminates_creation_and_discards_old_work() {
    let mut session = session();
    input(
        &mut session,
        vec![present(1, 1, publication("topic", true))],
    )
    .unwrap();
    let old = session.take_preparation().unwrap().unwrap();
    let old_value = live(&old);
    input(
        &mut session,
        vec![present(1, 2, publication("relabel", true))],
    )
    .unwrap();
    assert_eq!(owned(&session, 1).evaluated_revision, 2);
    assert_eq!(
        owned(&session, 1).rejection.as_ref().unwrap().code,
        wire::FailureCode::InvalidConfiguration as i32
    );
    assert!(!session.complete(old, Ok(old_value), 100).unwrap());
    assert!(session.take_preparation().unwrap().is_none());
    drain(&mut session);
    assert!(!session.rows.contains_key(&Key::Owned(1)));
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
fn replacement_grant_fences_prepared_work_and_expired_grants_cannot_commit() {
    let mut session = session();
    input(
        &mut session,
        vec![present(1, 1, publication("topic", true))],
    )
    .unwrap();
    let old = session.take_preparation().unwrap().unwrap();
    let old_value = live(&old);
    let stale = Preparation {
        reference: old.reference,
        revision: old.revision,
        spec: old.spec.clone(),
        grant_revision: old.grant_revision,
    };
    let stale_value = live(&stale);
    session.authorization(9000, 2).unwrap();
    session.authorization(20000, 1).unwrap();
    assert_eq!(session.grant_revision, 2);
    assert!(!session.complete(old, Ok(old_value), 100).unwrap());
    let next = session.take_preparation().unwrap().unwrap();
    let next_value = live(&next);
    assert!(!session.complete(next, Ok(next_value), 9000).unwrap());
    assert_eq!(owned(&session, 1).evaluated_revision, 0);
    assert!(owned(&session, 1).rejection.is_none());
    session.authorization(20000, 3).unwrap();
    let current = session.take_preparation().unwrap().unwrap();
    assert!(!session.complete(stale, Ok(stale_value), 9000).unwrap());
    assert!(session.preparation_current(&current, 9000).unwrap());
    let value = live(&current);
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
        admit(&mut session, reference, publication("topic", true));
        drain(&mut session);
        input(&mut session, vec![absent(reference, reference + 1)]).unwrap();
        session.cutoff_complete(reference).unwrap();
        drain(&mut session);
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
    session.authorization(10000, 1).unwrap();
    for index in 0..512 {
        session
            .observe_participant(wire::Participant {
                participant_id: format!("initial-{index}"),
                participant_external_id: format!("Peer {index}"),
            })
            .unwrap();
    }
    session.set_cid(ChannelId::from_value(0));
    let mut represented = BTreeSet::new();
    let mut marked = false;
    for index in 0..128 {
        session
            .observe_participant(wire::Participant {
                participant_id: format!("late-{index}"),
                participant_external_id: "Late".into(),
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
                    && id.starts_with("initial-")
                {
                    represented.insert(id);
                }
            }
            if batch.initial_complete {
                assert_eq!(represented.len(), 512);
                marked = true;
            }
            session.commit_sent();
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
fn selected_output_is_frozen_and_commit_does_not_clear_newer_state() {
    let mut session = session();
    let first = session.poll().unwrap().unwrap().bytes.clone();
    session.authorization(9000, 2).unwrap();
    assert_eq!(session.poll().unwrap().unwrap().bytes, first);
    session.commit_sent();
    let next = output(&mut session).unwrap();
    assert!(!next.initial_complete);
    let Some(wire::state_record::State::Confirmed(value)) = &next.records[0].state else {
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
    session.unavailable(Key::Track("source-1".into())).unwrap();
    let first = output(&mut session).unwrap();
    assert_eq!(first.records.len(), 1);
    assert!(!first.initial_complete);
    session.commit_sent();
    assert!(matches!(
        session.poll(),
        Err(OutputError::InvalidObservation)
    ));
    session.observe_track(source(1, "literal topic")).unwrap();
    assert_eq!(session.unavailable, 0);
    drain(&mut session);
    assert!(matches!(
        session.observe_track(source(1, "relabel")),
        Err(OutputError::InvalidObservation)
    ));
    session
        .observe_participant(wire::Participant {
            participant_id: "peer".into(),
            participant_external_id: "Original".into(),
        })
        .unwrap();
    assert!(matches!(
        session.observe_participant(wire::Participant {
            participant_id: "peer".into(),
            participant_external_id: "Changed".into()
        }),
        Err(OutputError::InvalidObservation)
    ));
}

#[tokio::test(start_paused = true)]
async fn unrelated_grant_churn_cannot_hide_a_stalled_selected_frame() {
    let mut session = session();
    let bytes = session.poll().unwrap().unwrap().bytes.clone();
    for revision in 2..=30 {
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        session.authorization(10000, revision).unwrap();
        assert_eq!(session.poll().unwrap().unwrap().bytes, bytes);
    }
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert!(matches!(session.poll(), Err(OutputError::ProgressExpired)));
}

#[test]
fn preparation_cannot_replace_the_target_it_was_issued_for() {
    for change_declaration in [false, true] {
        let mut session = session();
        let original = publication("topic", true);
        input(&mut session, vec![present(1, 1, original.clone())]).unwrap();
        let mut work = session.take_preparation().unwrap().unwrap();
        assert!(session.preparation_current(&work, 100).unwrap());
        if change_declaration {
            let Some(wire::resource_spec::Declaration::Publication(declaration)) =
                &mut work.spec.declaration
            else {
                panic!("publication")
            };
            declaration.label = "different".into();
        } else {
            work.spec.settings.as_mut().unwrap().enabled = Some(false);
        }
        let fabricated = live(&work);
        assert!(!session.preparation_current(&work, 100).unwrap());
        assert!(!session.complete(work, Ok(fabricated), 100).unwrap());
        assert_eq!(owned(&session, 1).evaluated_revision, 0);
        let retry = session.take_preparation().unwrap().unwrap();
        assert_eq!(retry.spec, original);
        assert!(session.preparation_current(&retry, 100).unwrap());
    }
}

#[test]
fn data_discovery_projects_actual_source_metadata_without_a_payload_format_claim() {
    use crate::entity::{ConnectionId, ParticipantId, RoomExternalId, RoomId, TrackKind};
    use crate::track::{DataLane, Topic, Track, TrackMeta};
    let mut session = session();
    let connection = ConnectionId::new();
    let origin = ParticipantId::new();
    let room = RoomId::from_external(&RoomExternalId::new("discovery").unwrap());
    for (lane, expected) in [
        (DataLane::Latest, wire::DataDelivery::Latest),
        (DataLane::Lossy, wire::DataDelivery::Lossy),
        (DataLane::Ordered, wire::DataDelivery::Ordered),
    ] {
        let label = format!("literal label ✓ {lane:?}");
        let id = connection.derive_track_id(TrackKind::Data, &label);
        let source = Track::data(
            TrackMeta {
                room_id: room,
                shard_id: crate::id::ShardId::new(0),
                id,
                origin,
                connection_id: connection,
                label: Some(label.clone()),
            },
            Topic::for_test("application-topic"),
            lane,
            None,
        );
        session.observe_candidate(&source).unwrap();
        let Some(wire::confirmed_state::Value::Track(observed)) = &session
            .rows
            .get(&Key::Track(id.as_str()))
            .unwrap()
            .value
            .value
        else {
            panic!("discovered track")
        };
        assert_eq!(observed.track_id, id.as_str());
        assert_eq!(observed.participant_id, origin.as_str());
        assert_eq!(observed.label, label);
        assert_eq!(
            observed.kind,
            Some(wire::track::Kind::Data(wire::DataTrack {
                delivery: expected as i32
            }))
        );
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
    session.source_ended(1).unwrap();
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
        session.authorization(10000, 2),
        Err(OutputError::Capacity)
    ));
    assert_eq!(session.version, u64::MAX);
}

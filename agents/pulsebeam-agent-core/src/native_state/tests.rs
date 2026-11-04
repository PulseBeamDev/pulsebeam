use super::*;

mod execution;

fn participant() -> wire::Participant {
    wire::Participant {
        participant_id: "self".into(),
        participant_external_id: "alice".into(),
    }
}

fn resource() -> Resource {
    Resource::Publication {
        kind: wire::TrackKind::Data,
        label: "updates".into(),
    }
}

fn binding(stream: u32) -> wire::TrackBinding {
    wire::TrackBinding {
        transport: Some(wire::track_binding::Transport::Sctp(wire::SctpParameters {
            stream_id: Some(stream),
        })),
    }
}

fn spec(enabled: bool, stream: u32) -> wire::ResourceSpec {
    wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(enabled),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::Publication(
            wire::PublicationSpec {
                label: "updates".into(),
                kind: Some(wire::publication_spec::Kind::Data(wire::DataPublication {
                    delivery: wire::DataDelivery::Ordered as i32,
                    binding: Some(binding(stream)),
                })),
            },
        )),
    }
}

fn frame(records: Vec<wire::ServerStateRecord>, initial_complete: bool) -> Vec<u8> {
    codec::encode_server(&wire::ServerStateBatch {
        records,
        initial_complete,
    })
    .unwrap()
}

fn record(reference: u64, state: wire::ConfirmedState) -> wire::ServerStateRecord {
    wire::ServerStateRecord {
        key: Some(wire::ResourceKey {
            key: Some(wire::resource_key::Key::Owned(reference)),
        }),
        confirmed: Some(state),
    }
}

fn live(revision: u64, spec: wire::ResourceSpec) -> wire::ConfirmedState {
    let enabled = spec.settings.as_ref().unwrap().enabled.unwrap();
    wire::ConfirmedState {
        evaluated_revision: revision,
        rejection: None,
        value: Some(wire::confirmed_state::Value::Owned(Box::new(
            wire::LiveResource {
                spec: Some(spec),
                participant: Some(participant()),
                track: Some(wire::Track {
                    track_id: "source".into(),
                    participant_id: "self".into(),
                    label: "updates".into(),
                    kind: Some(wire::track::Kind::Data(wire::DataTrack {
                        delivery: wire::DataDelivery::Ordered as i32,
                    })),
                }),
                execution: if enabled {
                    wire::ExecutionStatus::Installed
                } else {
                    wire::ExecutionStatus::Disabled
                } as i32,
                execution_failure: None,
                binding: None,
                activity: None,
            },
        ))),
    }
}

fn absent(revision: u64) -> wire::ConfirmedState {
    wire::ConfirmedState {
        evaluated_revision: revision,
        rejection: None,
        value: Some(wire::confirmed_state::Value::Absent(wire::AbsentState {
            reason: wire::AbsenceReason::Removed as i32,
        })),
    }
}

fn session(
    revision: u64,
    expires: i64,
    closed: Option<wire::CloseReason>,
) -> wire::ServerStateRecord {
    wire::ServerStateRecord {
        key: Some(wire::ResourceKey {
            key: Some(wire::resource_key::Key::Session(true)),
        }),
        confirmed: Some(wire::ConfirmedState {
            evaluated_revision: 0,
            rejection: None,
            value: Some(wire::confirmed_state::Value::Session(wire::SessionState {
                authorization: Some(wire::Authorization {
                    expires_at_unix_seconds: expires,
                    revision,
                }),
                closed: closed.map(|reason| reason as i32),
                diagnostic: String::new(),
            })),
        }),
    }
}

fn next(client: &mut NativeState) -> Vec<(u64, wire::DesiredState)> {
    codec::decode_client(&client.next_frame().unwrap().unwrap())
        .unwrap()
        .records
        .into_iter()
        .map(|record| (record.owned, record.desired.unwrap()))
        .collect()
}

#[test]
fn coalescing_allocates_only_serialized_revisions_and_cancellation_retains_no_wire_lifetime() {
    let mut client = NativeState::new(1024 * 1024, participant()).unwrap();
    client.stage(resource(), Some(spec(true, 1))).unwrap();
    client.stage(resource(), Some(spec(false, 1))).unwrap();
    client.stage(resource(), None).unwrap();
    assert!(client.next_frame().unwrap().is_none());
    assert!(client.targets.is_empty());
    assert!(client.owned.is_empty());
    assert_eq!(client.retained_bytes, 0);
    client.stage(resource(), Some(spec(true, 1))).unwrap();
    let records = next(&mut client);
    assert_eq!(records.len(), 1);
    assert_eq!((records[0].0, records[0].1.revision), (1, 1));
    client.stage(resource(), Some(spec(false, 1))).unwrap();
    client.stage(resource(), Some(spec(true, 1))).unwrap();
    assert!(client.next_frame().unwrap().is_none());
    assert_eq!(client.revision, 1);
}

#[test]
fn pending_resource_coalesces_without_blocking_other_resources_or_retirement() {
    let mut client = NativeState::new(1024 * 1024, participant()).unwrap();
    client.stage(resource(), Some(spec(true, 1))).unwrap();
    assert_eq!(next(&mut client)[0].1.revision, 1);
    client.stage(resource(), Some(spec(false, 1))).unwrap();
    assert!(client.next_frame().unwrap().is_none());
    client.retry(&resource());
    assert!(client.next_frame().unwrap().is_none());

    let other = Resource::Publication {
        kind: wire::TrackKind::Data,
        label: "other".into(),
    };
    let mut other_spec = spec(true, 2);
    let Some(wire::resource_spec::Declaration::Publication(publication)) =
        &mut other_spec.declaration
    else {
        panic!("publication")
    };
    publication.label = "other".into();
    client.stage(other, Some(other_spec)).unwrap();
    let records = next(&mut client);
    assert_eq!((records[0].0, records[0].1.revision), (2, 2));

    client.stage(resource(), Some(spec(true, 1))).unwrap();
    client.stage(resource(), Some(spec(false, 1))).unwrap();
    assert!(client.next_frame().unwrap().is_none());
    client
        .apply(&frame(
            alloc::vec![record(1, live(1, spec(true, 1)))],
            false,
        ))
        .unwrap();
    let records = next(&mut client);
    assert_eq!((records[0].0, records[0].1.revision), (1, 3));
    let Some(wire::desired_state::Value::Present(latest)) = &records[0].1.value else {
        panic!("present")
    };
    assert_eq!(latest.settings.as_ref().unwrap().enabled, Some(false));
    client.stage(resource(), None).unwrap();
    let retirement = next(&mut client);
    assert_eq!((retirement[0].0, retirement[0].1.revision), (1, 4));
    assert_eq!(
        retirement[0].1.value,
        Some(wire::desired_state::Value::Absent(true))
    );
}

#[test]
fn ordered_session_and_latest_evaluation_gate_media_without_discarding_rejected_acceptance() {
    let mut client = NativeState::new(1024 * 1024, participant()).unwrap();
    client.stage(resource(), Some(spec(true, 1))).unwrap();
    next(&mut client);
    client
        .apply(&frame(
            alloc::vec![record(1, live(1, spec(true, 1)))],
            false,
        ))
        .unwrap();
    assert!(client.converged(&resource()));
    assert!(client.eligible(&resource()).is_none());
    assert!(client.tracks().is_empty());
    assert!(client.participants().is_empty());
    client
        .apply(&frame(alloc::vec![session(1, 2_000_000_000, None)], true))
        .unwrap();
    assert!(client.initial_complete());
    assert!(client.eligible(&resource()).is_some());
    client.stage(resource(), Some(spec(false, 1))).unwrap();
    assert!(client.eligible(&resource()).is_none());
    assert_eq!(next(&mut client)[0].1.revision, 2);
    client.stage(resource(), Some(spec(true, 1))).unwrap();
    assert!(client.next_frame().unwrap().is_none());
    client
        .apply(&frame(
            alloc::vec![record(1, live(1, spec(true, 1)))],
            false,
        ))
        .unwrap();
    assert!(client.next_frame().unwrap().is_none());
    assert!(client.eligible(&resource()).is_none());
    assert!(!client.converged(&resource()));
    let mut rejected_disable = live(2, spec(true, 1));
    rejected_disable.rejection = Some(wire::Failure {
        code: wire::FailureCode::InternalFailure as i32,
        diagnostic: String::new(),
    });
    client
        .apply(&frame(alloc::vec![record(1, rejected_disable)], false))
        .unwrap();
    assert_eq!(next(&mut client)[0].1.revision, 3);
    let mut rejected = live(3, spec(true, 1));
    rejected.rejection = Some(wire::Failure {
        code: wire::FailureCode::CapacityExceeded as i32,
        diagnostic: String::new(),
    });
    let changes = client
        .apply(&frame(alloc::vec![record(1, rejected.clone())], false))
        .unwrap();
    assert!(matches!(
        changes.as_slice(),
        [Change::Owned {
            new_evaluation: true,
            ..
        }]
    ));
    let changes = client
        .apply(&frame(alloc::vec![record(1, rejected)], false))
        .unwrap();
    assert!(matches!(
        changes.as_slice(),
        [Change::Owned {
            new_evaluation: false,
            ..
        }]
    ));
    assert!(client.eligible(&resource()).is_some());
    assert!(!client.converged(&resource()));
    client.stage(resource(), Some(spec(true, 1))).unwrap();
    assert!(client.next_frame().unwrap().is_none());
    client.retry(&resource());
    let retry = next(&mut client);
    assert_eq!((retry[0].0, retry[0].1.revision), (1, 4));
    assert!(client.confirmed(&resource()).unwrap().rejection.is_some());
    assert!(client.eligible(&resource()).is_none());
}

#[test]
fn replacement_waits_for_terminal_cutoff_and_late_completion_cannot_reopen_a_retired_reference() {
    let mut client = NativeState::new(1024 * 1024, participant()).unwrap();
    client.stage(resource(), Some(spec(true, 1))).unwrap();
    next(&mut client);
    client.stage(resource(), Some(spec(true, 2))).unwrap();
    let retirement = next(&mut client);
    assert_eq!((retirement[0].0, retirement[0].1.revision), (1, 2));
    assert_eq!(
        retirement[0].1.value,
        Some(wire::desired_state::Value::Absent(true))
    );
    assert!(client.next_frame().unwrap().is_none());
    client
        .apply(&frame(
            alloc::vec![record(1, live(1, spec(true, 1)))],
            false,
        ))
        .unwrap();
    assert!(client.eligible(&resource()).is_none());
    client
        .apply(&frame(alloc::vec![record(1, absent(2))], false))
        .unwrap();
    assert!(client.source_references.is_empty());
    assert!(client.participant_references.is_empty());
    let replacement = next(&mut client);
    assert_eq!((replacement[0].0, replacement[0].1.revision), (3, 3));
    assert!(
        client
            .apply(&frame(
                alloc::vec![record(1, live(1, spec(true, 1)))],
                false
            ))
            .is_err()
    );
    assert!(client.eligible(&resource()).is_none());
}

#[test]
fn terminal_session_ignores_later_records_and_completion_and_renewal_can_narrow_expiry() {
    let mut client = NativeState::new(1024 * 1024, participant()).unwrap();
    client
        .apply(&frame(alloc::vec![session(1, 2_000_000_000, None)], false))
        .unwrap();
    client
        .apply(&frame(alloc::vec![session(2, 1_900_000_000, None)], false))
        .unwrap();
    assert_eq!(
        client
            .session()
            .unwrap()
            .authorization
            .as_ref()
            .unwrap()
            .revision,
        2
    );
    client
        .apply(&frame(
            alloc::vec![
                session(
                    2,
                    1_900_000_000,
                    Some(wire::CloseReason::AuthorizationRevoked)
                ),
                session(3, 2_000_000_000, None)
            ],
            true,
        ))
        .unwrap();
    assert!(client.terminal());
    assert!(!client.initial_complete());
    assert!(client.next_frame().unwrap().is_none());
    assert!(client.stage(resource(), Some(spec(true, 1))).is_err());
    assert_eq!(
        client
            .session()
            .unwrap()
            .authorization
            .as_ref()
            .unwrap()
            .revision,
        2
    );
}

#[test]
fn scoped_memory_failure_preserves_existing_work_and_refuses_unbounded_staging() {
    let mut client = NativeState::new(4096, participant()).unwrap();
    client.stage(resource(), Some(spec(true, 1))).unwrap();
    next(&mut client);
    let before = client.retained_bytes;
    let other = Resource::Publication {
        kind: wire::TrackKind::Data,
        label: "other".into(),
    };
    let mut other_spec = spec(true, 2);
    let Some(wire::resource_spec::Declaration::Publication(publication)) =
        other_spec.declaration.as_mut()
    else {
        panic!("publication")
    };
    publication.label = "other".into();
    client.memory_limit = before;
    assert!(client.stage(other, Some(other_spec)).is_err());
    assert_eq!(client.retained_bytes, before);
    assert_eq!(client.targets.len(), 1);
    client.stage(resource(), None).unwrap();
    let retirement = next(&mut client);
    client
        .apply(&frame(
            alloc::vec![record(1, absent(retirement[0].1.revision))],
            false,
        ))
        .unwrap();
    assert_eq!(client.retained_bytes, 0);
    assert!(client.targets.is_empty());
    assert!(client.owned.is_empty());
}

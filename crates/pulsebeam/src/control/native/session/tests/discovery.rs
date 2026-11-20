use super::*;
use crate::control::native::{ExecutionCompletion, ResolvedSource};
use crate::entity::{ConnectionId, ParticipantId, RoomId, TrackKind};
use crate::participant::policy::ReceiverExecution;
use crate::track::{DataLane, TrackMeta};
use pulsebeam_core::auth::grants::{Action, Grant, GrantSet, TrackKind as GrantKind};

fn scope(
    actions: Vec<Action>,
    kind: Option<GrantKind>,
    label: Option<&str>,
) -> ResourcePermissions {
    ResourcePermissions::Grants {
        joined_room: Some(RoomId::new("room").unwrap()),
        grants: GrantSet::new(vec![
            Grant::new(
                vec!["room".into()],
                actions,
                Some("publisher".into()),
                kind,
                label.map(str::to_owned),
            )
            .unwrap(),
        ])
        .unwrap(),
    }
}

fn participant(id: ParticipantId, _external: &ParticipantId) -> wire::Participant {
    wire::Participant {
        participant_id: id.as_str().to_owned(),
        room_id: "room".into(),
    }
}

fn track(connection: ConnectionId, owner: ParticipantId, label: &str) -> Track {
    Track::data_publication(
        TrackMeta {
            room: crate::entity::ProjectRoomKey::fixture(RoomId::new("room").unwrap()),
            id: connection.fixture_publication_at(
                TrackKind::Data,
                label,
                std::num::NonZeroU64::new(1).unwrap(),
            ),
            origin: owner,
            connection_id: connection,
            track_id: Some(crate::entity::TrackId::new(&label).unwrap()),
        },
        DataLane::Ordered,
    )
}

fn settle(model: &mut NativeSession, tracks: &[Track], publisher: &ParticipantId) {
    for _ in 0..16 {
        if !model.revocation_pending() {
            return;
        }
        assert!(
            model
                .service_revocations(SELECT_WORK, |id| tracks
                    .iter()
                    .find(|track| track.id() == id)
                    .map(|track| ResolvedSource { track, publisher }))
                .unwrap()
                .is_empty()
        );
    }
    panic!("discovery reconciliation did not settle")
}

#[test]
fn discovery_filters_do_not_expose_or_fund_hidden_sources_and_context_is_reference_counted() {
    let mut model = session();
    let publisher = ParticipantId::new("publisher").unwrap();
    let other = ParticipantId::new("other").unwrap();
    let owner = publisher;
    let myself = crate::entity::fixture_participant_id();
    let connection = ConnectionId::new();
    model.observe_self(participant(myself, &other)).unwrap();
    assert!(
        model
            .authorization(
                10_000,
                2,
                scope(
                    vec![Action::Discover],
                    Some(GrantKind::Data),
                    Some("allowed")
                )
            )
            .unwrap()
    );
    model
        .observe_participant(participant(owner, &publisher))
        .unwrap();
    assert!(
        !model
            .rows
            .contains_key(&Key::Participant(("room".into(), owner.as_str().into())))
    );
    let allowed = track(connection, owner, "allowed");
    let mut incomplete = track(
        connection,
        crate::entity::fixture_participant_id(),
        "preparing",
    );
    incomplete.meta_mut().track_id = None;
    let hidden = track(connection, owner, "hidden");
    let before = model.retained_bytes;
    for source in [
        ResolvedSource {
            track: &incomplete,
            publisher: &other,
        },
        ResolvedSource {
            track: &hidden,
            publisher: &publisher,
        },
    ] {
        let id = source.track.id();
        model.observe_candidate(source).unwrap();
        model.remove_track(id.as_str()).unwrap();
        assert!(!model.rows.contains_key(&Key::Publication(id.as_str())));
    }
    assert_eq!(model.retained_bytes, before);
    assert_eq!(model.unavailable, 0);
    for _ in 0..2 {
        model
            .observe_candidate(ResolvedSource {
                track: &allowed,
                publisher: &publisher,
            })
            .unwrap();
    }
    assert_eq!(
        model.rows[&Key::Participant(("room".into(), owner.as_str().into()))].discovered_tracks,
        1
    );
    settle(&mut model, std::slice::from_ref(&allowed), &publisher);
    let batches = drain(&mut model);
    assert!(batches.iter().any(|batch| batch.records.iter().any(|record| matches!(record.key.as_ref().and_then(|key| key.key.as_ref()), Some(wire::resource_key::Key::Publication(id)) if *id == allowed.id().as_str()))));
    model.remove_track(allowed.id().as_str()).unwrap();
    assert!(matches!(
        &model.rows[&Key::Participant(("room".into(), owner.as_str().into()))]
            .value
            .value,
        Some(wire::confirmed_state::Value::Absent(_))
    ));
    assert!(matches!(
        &model.rows[&Key::Participant(("room".into(), myself.as_str().into()))]
            .value
            .value,
        Some(wire::confirmed_state::Value::Participant(_))
    ));
    drain(&mut model);
    assert!(
        !model
            .rows
            .contains_key(&Key::Participant(("room".into(), owner.as_str().into())))
    );
    model
        .authorization(10_000, 3, scope(vec![Action::Discover], None, None))
        .unwrap();
    model
        .observe_participant(participant(owner, &publisher))
        .unwrap();
    settle(&mut model, &[], &publisher);
    assert!(matches!(
        &model.rows[&Key::Participant(("room".into(), owner.as_str().into()))]
            .value
            .value,
        Some(wire::confirmed_state::Value::Participant(_))
    ));
    assert_eq!(
        model.rows[&Key::Participant(("room".into(), owner.as_str().into()))].discovered_tracks,
        0
    );
    let mut preparing = track(connection, owner, "authorized-preparing");
    preparing.meta_mut().track_id = None;
    let before = model.retained_bytes;
    model
        .observe_candidate(ResolvedSource {
            track: &preparing,
            publisher: &publisher,
        })
        .unwrap();
    assert!(model.retained_bytes > before);
    assert_eq!(model.unavailable, 1);
    let pending = output(&mut model).unwrap();
    assert!(pending.records.iter().all(|record| matches!(
        record.key.as_ref().and_then(|key| key.key.as_ref()),
        Some(wire::resource_key::Key::Session(true))
    )));
    commit_sent(&mut model);
    assert!(matches!(model.poll(), Err(OutputError::InvalidObservation)));
    preparing.meta_mut().track_id =
        Some(crate::entity::TrackId::new("authorized-preparing").unwrap());
    model
        .observe_candidate(ResolvedSource {
            track: &preparing,
            publisher: &publisher,
        })
        .unwrap();
    assert_eq!(model.unavailable, 0);
    let ready = drain(&mut model);
    assert!(ready.iter().flat_map(|batch| &batch.records).any(|record| record.confirmed.as_ref().is_some_and(|value| matches!(&value.value, Some(wire::confirmed_state::Value::Track(track)) if track.track_id == "authorized-preparing"))));
    let mut never_exposed = track(connection, owner, "never-exposed");
    never_exposed.meta_mut().track_id = None;
    let hidden_id = never_exposed.id();
    model
        .observe_candidate(ResolvedSource {
            track: &never_exposed,
            publisher: &publisher,
        })
        .unwrap();
    assert_eq!(model.unavailable, 1);
    model
        .authorization(
            10_000,
            4,
            scope(
                vec![Action::Discover],
                Some(GrantKind::Data),
                Some("allowed"),
            ),
        )
        .unwrap();
    settle(&mut model, &[preparing, never_exposed], &publisher);
    assert_eq!(model.unavailable, 0);
    assert!(
        !model
            .rows
            .contains_key(&Key::Publication(hidden_id.as_str()))
    );
    let withdrawn = drain(&mut model);
    assert!(withdrawn.iter().flat_map(|batch| &batch.records).all(|record| !matches!(record.key.as_ref().and_then(|key| key.key.as_ref()), Some(wire::resource_key::Key::Publication(id)) if *id == hidden_id.as_str())));
}

#[test]
fn scope_reconciliation_withholds_stale_catalog_rows_and_preserves_independent_receivers() {
    let mut model = session();
    let publisher = ParticipantId::new("publisher").unwrap();
    settle(&mut model, &[], &publisher);
    let owner = publisher;
    let connection = ConnectionId::new();
    let mut tracks = vec![track(connection, owner, "allowed")];
    for index in 0..SELECT_WORK + 4 {
        tracks.push(track(connection, owner, &format!("hidden-{index}")));
    }
    for track in &tracks {
        model
            .observe_candidate(ResolvedSource {
                track,
                publisher: &publisher,
            })
            .unwrap();
    }
    let target = tracks[0].id();
    let spec = wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::Subscription(
            wire::SubscriptionSpec {
                publication_id: target.as_str(),
                kind: wire::TrackKind::Data as i32,
                source_room: "room".into(),
                binding: Some(wire::TrackBinding {
                    transport: Some(wire::track_binding::Transport::Sctp(wire::SctpParameters {
                        stream_id: Some(1),
                    })),
                }),
            },
        )),
    };
    input(&mut model, vec![present(1, 1, spec)]).unwrap();
    let work = model.take_preparation().unwrap().unwrap();
    model
        .prepare_execution(&work, 100, None, |_| {
            Some(ResolvedSource {
                track: &tracks[0],
                publisher: &publisher,
            })
        })
        .unwrap()
        .unwrap();
    model
        .observe_completion(
            ExecutionCompletion {
                work,
                result: Ok(ResourceExecution::Receiver(ReceiverExecution {
                    source: target,
                    enabled: true,
                    ready: false,
                    failed: false,
                })),
                released: false,
                unix_now: 100,
            },
            &publisher,
        )
        .unwrap();
    drain(&mut model);
    model
        .authorization(
            10_000,
            2,
            scope(
                vec![Action::Subscribe],
                Some(GrantKind::Data),
                Some("allowed"),
            ),
        )
        .unwrap();
    let update = output(&mut model).unwrap();
    assert!(update.records.iter().all(|record| matches!(
        record.key.as_ref().and_then(|key| key.key.as_ref()),
        Some(wire::resource_key::Key::Session(true))
    )));
    commit_sent(&mut model);
    assert!(
        model
            .service_revocations(0, |id| tracks.iter().find(|track| track.id() == id).map(
                |track| ResolvedSource {
                    track,
                    publisher: &publisher
                }
            ))
            .unwrap()
            .is_empty()
    );
    let remaining = model
        .rows
        .values()
        .filter(|row| {
            matches!(
                row.value.value,
                Some(wire::confirmed_state::Value::Track(_))
            )
        })
        .count();
    assert!(remaining > 0 && remaining < tracks.len());
    assert!(model.discovery_reconciling);
    assert!(model.poll().unwrap().is_none());
    settle(&mut model, &tracks, &publisher);
    let withdrawn = drain(&mut model);
    assert!(
        withdrawn
            .iter()
            .flat_map(|batch| &batch.records)
            .filter_map(|record| record.confirmed.as_ref())
            .all(|value| !matches!(
                value.value,
                Some(
                    wire::confirmed_state::Value::Participant(_)
                        | wire::confirmed_state::Value::Track(_)
                )
            ))
    );
    assert!(!model.rows[&Key::Owned(1)].owned.as_ref().unwrap().retired);
    assert!(matches!(
        model.rows[&Key::Owned(1)].value.value,
        Some(wire::confirmed_state::Value::Owned(_))
    ));
    assert!(
        model
            .rows
            .keys()
            .all(|key| !matches!(key, Key::Participant(_) | Key::Publication(_)))
    );
    model
        .authorization(
            10_000,
            3,
            scope(
                vec![Action::Discover, Action::Subscribe],
                Some(GrantKind::Data),
                Some("allowed"),
            ),
        )
        .unwrap();
    settle(&mut model, &tracks, &publisher);
    assert!(
        model
            .rows
            .keys()
            .all(|key| !matches!(key, Key::Participant(_) | Key::Publication(_)))
    );
    for track in &tracks {
        model
            .observe_candidate(ResolvedSource {
                track,
                publisher: &publisher,
            })
            .unwrap();
    }
    assert!(model.rows.contains_key(&Key::Publication(target.as_str())));
    assert_eq!(
        model.rows[&Key::Participant(("room".into(), owner.as_str().into()))].discovered_tracks,
        1
    );
    assert_eq!(
        model
            .rows
            .keys()
            .filter(|key| matches!(key, Key::Publication(_)))
            .count(),
        1
    );
    assert!(!model.rows[&Key::Owned(1)].owned.as_ref().unwrap().retired);
}

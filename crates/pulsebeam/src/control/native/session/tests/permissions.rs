use super::*;
use crate::control::native::{PreparedResource, ReceiverPreparation, ResolvedSource};
use crate::entity::{ConnectionId, ParticipantId, RoomId, TrackKind};
use crate::participant::policy::ResourceObservation;
use crate::track::{DataLane, TrackMeta};
use pulsebeam_core::auth::grants::{Action, Grant, GrantSet, TrackKind as GrantKind};

fn permissions(
    action: Action,
    participant: Option<&str>,
    label: Option<&str>,
) -> ResourcePermissions {
    ResourcePermissions::Grants {
        joined_room: Some(RoomId::new("room").unwrap()),
        grants: GrantSet::new(vec![
            Grant::new(
                vec!["room".into()],
                vec![action],
                participant.map(str::to_owned),
                Some(GrantKind::Data),
                label.map(str::to_owned),
            )
            .unwrap(),
        ])
        .unwrap(),
    }
}

fn settle_sweep(model: &mut NativeSession) -> Vec<ExecutionCommand> {
    let mut commands = Vec::new();
    for _ in 0..32 {
        if !model.revocation_pending() {
            return commands;
        }
        commands.extend(model.service_revocations(SELECT_WORK, |_| None).unwrap());
    }
    panic!("bounded permission sweep did not settle")
}

#[test]
fn reduced_permissions_fence_pending_and_installed_resources_before_absence() {
    let mut model = session();
    assert!(settle_sweep(&mut model).is_empty());
    admit(&mut model, 1, publication("keep", true));
    admit(&mut model, 2, publication("drop", true));
    input(
        &mut model,
        vec![present(3, 3, publication("pending", true))],
    )
    .unwrap();
    let stale = model.take_preparation().unwrap().unwrap();
    let stale_value = live(&model, &stale);
    model
        .authorization(10_000, 2, permissions(Action::Publish, None, Some("keep")))
        .unwrap();
    assert!(model.preparation_spec(&stale).is_none());
    assert!(model.service_revocations(0, |_| None).unwrap().is_empty());
    assert!(!model.rows[&Key::Owned(2)].owned.as_ref().unwrap().retired);
    assert!(!model.complete(stale, Ok(stale_value.clone()), 100).unwrap());
    let resumed = model.take_preparation().unwrap().unwrap();
    assert_eq!(resumed.reference, 3);
    let denied = model
        .prepare_execution(&resumed, 100, None, |_| {
            panic!("denied publication must not resolve a source")
        })
        .unwrap()
        .unwrap();
    let mut commands = vec![denied];
    commands.extend(settle_sweep(&mut model));
    let mut scopes: Vec<_> = commands
        .into_iter()
        .map(|command| match command.into_action() {
            ExecutionAction::Retire(scope) => scope,
            _ => panic!("retirement fence required"),
        })
        .collect();
    scopes.sort_by_key(|scope| scope.binding);
    assert_eq!(
        scopes.iter().map(|scope| scope.binding).collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert!(
        scopes
            .iter()
            .all(|scope| scope.authorization == 2 && scope.sequence > stale.scope().sequence)
    );
    assert!(!model.rows[&Key::Owned(1)].owned.as_ref().unwrap().retired);
    assert!(matches!(
        model.rows[&Key::Owned(2)].value.value,
        Some(wire::confirmed_state::Value::Owned(_))
    ));
    assert!(
        model
            .publications
            .contains_key(&(wire::TrackKind::Data as i32, "drop".into()))
    );
    assert!(!model.complete(stale, Ok(stale_value), 100).unwrap());
    for scope in scopes {
        model
            .observe_resource(ResourceObservation::StorageReleased {
                binding: scope.binding,
                sequence: scope.sequence,
            })
            .unwrap();
        assert!(
            model.rows[&Key::Owned(scope.binding)]
                .owned
                .as_ref()
                .unwrap()
                .cutoff_pending
        );
        model
            .observe_resource(ResourceObservation::Released {
                binding: scope.binding,
            })
            .unwrap();
        let value = &model.rows[&Key::Owned(scope.binding)].value;
        let reason = if scope.binding == 3 {
            wire::AbsenceReason::CreateRejected
        } else {
            wire::AbsenceReason::PermissionRevoked
        };
        assert!(
            matches!(&value.value, Some(wire::confirmed_state::Value::Absent(absent)) if absent.reason == reason as i32)
        );
        assert_eq!(
            value.rejection.as_ref().unwrap().code,
            wire::FailureCode::PermissionDenied as i32
        );
    }
    assert!(
        !model
            .publications
            .contains_key(&(wire::TrackKind::Data as i32, "drop".into()))
    );
    input(&mut model, vec![present(4, 4, publication("denied", true))]).unwrap();
    assert!(model.rows[&Key::Owned(4)].owned.is_none());
    assert_eq!(
        model.rows[&Key::Owned(4)]
            .value
            .rejection
            .as_ref()
            .unwrap()
            .code,
        wire::FailureCode::PermissionDenied as i32
    );
    let restored = permissions(Action::Publish, None, None);
    model.authorization(10_000, 3, restored.clone()).unwrap();
    model
        .authorization(10_000, 2, ResourcePermissions::RoomBound)
        .unwrap();
    assert!(model.permissions == restored);
    assert!(settle_sweep(&mut model).is_empty());
    assert!(model.rows[&Key::Owned(2)].owned.as_ref().unwrap().retired);
    admit(&mut model, 5, publication("drop", true));
}

#[test]
fn subscriptions_require_resolved_filters_but_no_discovery_projection() {
    let connection = ConnectionId::new();
    let id = connection.fixture_publication_at(
        TrackKind::Data,
        "allowed",
        std::num::NonZeroU64::new(1).unwrap(),
    );
    let spec = |id: crate::entity::PublicationKey| wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::Subscription(
            wire::SubscriptionSpec {
                publication_id: id.as_str(),
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
    for (external, label, allowed) in [
        ("publisher", "allowed", true),
        ("other", "allowed", false),
        ("publisher", "other", false),
    ] {
        let mut model = session();
        model
            .authorization(
                10_000,
                2,
                permissions(Action::Subscribe, Some("publisher"), Some("allowed")),
            )
            .unwrap();
        assert!(settle_sweep(&mut model).is_empty());
        let id = connection.fixture_publication_at(
            TrackKind::Data,
            label,
            std::num::NonZeroU64::new(1).unwrap(),
        );
        input(&mut model, vec![present(1, 1, spec(id))]).unwrap();
        let work = model.take_preparation().unwrap().unwrap();
        let track = Track::data_publication(
            TrackMeta {
                room: crate::entity::ProjectRoomKey::fixture(RoomId::new("room").unwrap()),
                shard_id: crate::id::ShardId::new(0),
                id,
                origin: ParticipantId::new(external).unwrap(),
                connection_id: connection,
                track_id: Some(crate::entity::TrackId::new(&label).unwrap()),
            },
            DataLane::Ordered,
        );
        let publisher = ParticipantId::new(external).unwrap();
        let command = model
            .prepare_execution(&work, 100, None, |_| {
                Some(ResolvedSource {
                    track: &track,
                    publisher: &publisher,
                })
            })
            .unwrap()
            .unwrap();
        if allowed {
            assert!(matches!(
                command.into_action(),
                ExecutionAction::Prepare {
                    resource: PreparedResource::Receiver(ReceiverPreparation::Ready(_)),
                    ..
                }
            ));
            assert!(
                model.rows[&Key::Owned(1)]
                    .owned
                    .as_ref()
                    .unwrap()
                    .receiver_source
                    .is_some()
            );
        } else {
            assert!(matches!(command.into_action(), ExecutionAction::Retire(_)));
            assert!(
                model.rows[&Key::Owned(1)]
                    .owned
                    .as_ref()
                    .unwrap()
                    .receiver_source
                    .is_none()
            );
            assert!(
                model.rows[&Key::Owned(1)]
                    .owned
                    .as_ref()
                    .unwrap()
                    .cutoff_pending
            );
            model
                .observe_resource(ResourceObservation::Released { binding: 1 })
                .unwrap();
            let value = &model.rows[&Key::Owned(1)].value;
            assert_eq!(
                value.rejection.as_ref().unwrap().code,
                wire::FailureCode::PermissionDenied as i32
            );
            assert!(
                matches!(&value.value, Some(wire::confirmed_state::Value::Absent(absent)) if absent.reason == wire::AbsenceReason::CreateRejected as i32)
            );
        }
        assert!(
            model
                .rows
                .keys()
                .all(|key| !matches!(key, Key::Participant(_) | Key::Publication(_)))
        );
    }
    let mut model = session();
    model
        .authorization(
            10_000,
            2,
            permissions(Action::Discover, Some("publisher"), Some("allowed")),
        )
        .unwrap();
    input(&mut model, vec![present(1, 1, spec(id))]).unwrap();
    assert!(model.take_preparation().unwrap().is_none());
    assert_eq!(
        model.rows[&Key::Owned(1)]
            .value
            .rejection
            .as_ref()
            .unwrap()
            .code,
        wire::FailureCode::PermissionDenied as i32
    );
    assert!(model.source_waits.is_empty());
}

#[test]
fn permission_sweeps_are_bounded_and_new_revisions_do_not_starve_the_tail() {
    let mut model = session();
    assert!(settle_sweep(&mut model).is_empty());
    for index in 0..SELECT_WORK * 3 {
        let mut track = source(u64::try_from(index).unwrap(), &format!("visible-{index}"));
        track.participant_id = format!("owner-{}", index / SELECT_WORK);
        model
            .observe_track(track, &ParticipantId::new("Owner").unwrap())
            .unwrap();
    }
    admit(&mut model, 1, publication("tail", true));
    let mut retired = false;
    for revision in 2..10 {
        let scope = if revision % 2 == 0 {
            ResourcePermissions::Denied
        } else {
            permissions(Action::Discover, None, None)
        };
        model.authorization(10_000, revision, scope).unwrap();
        model
            .observe_track(
                source(1000 + revision, "new"),
                &ParticipantId::new("Owner").unwrap(),
            )
            .unwrap();
        let commands = model.service_revocations(1, |_| None).unwrap();
        assert!(commands.len() <= 1);
        if !commands.is_empty() {
            retired = true;
            break;
        }
    }
    assert!(retired);
    assert!(
        model.rows[&Key::Owned(1)]
            .owned
            .as_ref()
            .unwrap()
            .cutoff_pending
    );
    assert!(matches!(
        model.rows[&Key::Owned(1)].value.value,
        Some(wire::confirmed_state::Value::Owned(_))
    ));
}

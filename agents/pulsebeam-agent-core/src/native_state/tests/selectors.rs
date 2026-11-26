use super::*;
use wire::{SelectorMatchStatus, confirmed_state::Value, resource_spec::Declaration};

fn selector(room: &str) -> wire::ResourceSpec {
    wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(Declaration::SubscriptionSelector(
            wire::SubscriptionSelectorSpec {
                room: Some(room.into()),
                ..Default::default()
            },
        )),
    }
}

fn snapshot(
    revision: u64,
    spec: wire::ResourceSpec,
    status: SelectorMatchStatus,
    membership_revision: u64,
) -> wire::ConfirmedState {
    let matches = if status == SelectorMatchStatus::Complete {
        alloc::vec![wire::LogicalSourceAddress {
            room: "stage".into(),
            participant_id: "publisher".into(),
            kind: wire::TrackKind::Video as i32,
            track_id: "camera".into(),
        }]
    } else {
        Vec::new()
    };
    wire::ConfirmedState {
        evaluated_revision: revision,
        rejection: None,
        value: Some(Value::SubscriptionSelector(Box::new(
            wire::SubscriptionSelectorState {
                spec: Some(spec),
                status: status as i32,
                membership: (status != SelectorMatchStatus::Pending).then_some(
                    wire::SelectorMembershipSpan {
                        revision: membership_revision,
                        offset: 0,
                        total: matches.len() as u32,
                    },
                ),
                matches,
            },
        ))),
    }
}

#[test]
fn renewed_grant_reopens_revoked_selector_on_a_fresh_reference() {
    let mut client = NativeState::new(64 * 1024, participant()).unwrap();
    client
        .apply(&frame(alloc::vec![session(1, 2_000_000_000, None)], true))
        .unwrap();
    let resource = Resource::SubscriptionSelector(17);
    let spec = selector("stage");
    client.stage(resource.clone(), Some(spec.clone())).unwrap();
    let (first, desired) = next(&mut client).pop().unwrap();
    client
        .apply(&frame(
            alloc::vec![record(
                first,
                snapshot(
                    desired.revision,
                    spec.clone(),
                    SelectorMatchStatus::Complete,
                    1,
                )
            )],
            false,
        ))
        .unwrap();

    let revoked = wire::ConfirmedState {
        evaluated_revision: desired.revision,
        rejection: None,
        value: Some(Value::Absent(wire::AbsentState {
            reason: wire::AbsenceReason::PermissionRevoked as i32,
        })),
    };
    client
        .apply(&frame(alloc::vec![record(first, revoked)], false))
        .unwrap();
    assert!(client.selector_snapshot(17).is_none());
    assert!(client.next_frame().unwrap().is_none());
    client.stage(resource, Some(spec.clone())).unwrap();
    assert!(
        client.next_frame().unwrap().is_none(),
        "unchanged intent does not retry a denial"
    );

    client.retry_revoked_selectors();
    let (fresh, declared) = next(&mut client).pop().unwrap();
    assert_ne!(fresh, first);
    assert!(declared.revision > desired.revision);
    client
        .apply(&frame(
            alloc::vec![record(
                fresh,
                snapshot(declared.revision, spec, SelectorMatchStatus::Complete, 2)
            )],
            false,
        ))
        .unwrap();
    assert_eq!(client.selector_snapshot(17).unwrap().matches.len(), 1);
}

#[test]
fn selector_snapshot_obeys_owned_revision_and_withdrawal_without_discovery() {
    let mut client = NativeState::new(64 * 1024, participant()).unwrap();
    client
        .apply(&frame(alloc::vec![session(1, 2_000_000_000, None)], true))
        .unwrap();
    let resource = Resource::SubscriptionSelector(17);
    let spec = selector("stage");
    client.stage(resource.clone(), Some(spec.clone())).unwrap();
    assert!(client.selector_snapshot(17).is_none());
    assert_eq!(next(&mut client)[0].0, 1);
    let update = |spec, status, version| {
        frame(
            alloc::vec![record(1, snapshot(1, spec, status, version))],
            false,
        )
    };
    assert!(matches!(
        client.apply(&update(selector("other"), SelectorMatchStatus::Pending, 0)),
        Err(SignalingError::Invalid("accepted declaration"))
    ));
    for (index, status) in [
        SelectorMatchStatus::Pending,
        SelectorMatchStatus::Complete,
        SelectorMatchStatus::Overflow,
        SelectorMatchStatus::Complete,
    ]
    .into_iter()
    .enumerate()
    {
        assert!(
            matches!(client.apply(&update(spec.clone(), status, index as u64)).unwrap().as_slice(), [Change::Owned { resource: observed, retired_binding: None, .. }] if observed == &resource)
        );
        let snapshot = client.selector_snapshot(17).unwrap();
        assert_eq!(snapshot.status(), status);
        assert_eq!(
            snapshot.matches.len(),
            usize::from(status == SelectorMatchStatus::Complete)
        );
        assert!(client.participants().is_empty());
        assert!(client.tracks().is_empty());
    }
    client.stage(resource, None).unwrap();
    assert!(
        client.selector_snapshot(17).is_none(),
        "withdraw exposure before terminal acknowledgement"
    );
}

#[test]
fn selector_replacement_commits_a_hundred_matches_at_one_cut_and_restarts_after_invalid_base() {
    let mut client = NativeState::new(128 * 1024, participant()).unwrap();
    client
        .apply(&frame(alloc::vec![session(1, 2_000_000_000, None)], true))
        .unwrap();
    let owner = 17;
    let resource = Resource::SubscriptionSelector(owner);
    let spec = selector("stage");
    client.stage(resource, Some(spec.clone())).unwrap();
    let (reference, declared) = next(&mut client).pop().unwrap();
    let chunk = |version, offset: usize, count: usize, total| {
        let mut state = snapshot(
            declared.revision,
            spec.clone(),
            SelectorMatchStatus::Complete,
            version,
        );
        let Some(Value::SubscriptionSelector(selector)) = state.value.as_mut() else {
            unreachable!()
        };
        selector.matches = (offset..offset + count)
            .map(|index| wire::LogicalSourceAddress {
                room: "stage".into(),
                participant_id: "publisher".into(),
                kind: wire::TrackKind::Video as i32,
                track_id: alloc::format!("camera-{index:03}"),
            })
            .collect();
        selector.membership = Some(wire::SelectorMembershipSpan {
            revision: version,
            offset: offset as u32,
            total,
        });
        frame(alloc::vec![record(reference, state)], false)
    };
    assert!(client.apply(&chunk(1, 0, 1, 1)).unwrap().len() == 1);
    assert!(client.apply(&chunk(2, 0, 16, 100)).unwrap().is_empty());
    assert!(client.apply(&chunk(2, 32, 16, 100)).unwrap().is_empty());
    assert!(client.apply(&chunk(2, 16, 16, 100)).unwrap().is_empty());
    assert_eq!(client.selector_snapshot(owner).unwrap().matches.len(), 1);
    for offset in (0..100).step_by(16) {
        if offset != 0 {
            assert!(client.apply(&chunk(2, offset, 1, 100)).unwrap().is_empty());
        }
        let changes = client
            .apply(&chunk(3, offset, 16.min(100 - offset), 100))
            .unwrap();
        if offset < 96 {
            assert!(changes.is_empty());
            assert_eq!(client.selector_snapshot(owner).unwrap().matches.len(), 1);
        } else {
            assert_eq!(changes.len(), 1);
            assert_eq!(client.selector_snapshot(owner).unwrap().matches.len(), 100);
        }
    }
    assert!(client.apply(&chunk(4, 0, 16, 100)).unwrap().is_empty());
    client
        .apply(&frame(
            alloc::vec![record(
                reference,
                wire::ConfirmedState {
                    evaluated_revision: declared.revision,
                    rejection: None,
                    value: Some(Value::Absent(wire::AbsentState {
                        reason: wire::AbsenceReason::PermissionRevoked as i32
                    })),
                }
            )],
            false,
        ))
        .unwrap();
    assert!(client.selector_snapshot(owner).is_none());
    assert!(client.apply(&chunk(4, 16, 16, 100)).is_err());
}

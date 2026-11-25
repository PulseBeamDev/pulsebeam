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
                matches,
            },
        ))),
    }
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
    let update = |spec, status| frame(alloc::vec![record(1, snapshot(1, spec, status))], false);
    assert!(matches!(
        client.apply(&update(selector("other"), SelectorMatchStatus::Pending)),
        Err(SignalingError::Invalid("accepted declaration"))
    ));
    for status in [
        SelectorMatchStatus::Pending,
        SelectorMatchStatus::Complete,
        SelectorMatchStatus::Overflow,
        SelectorMatchStatus::Complete,
    ] {
        assert!(
            matches!(client.apply(&update(spec.clone(), status)).unwrap().as_slice(), [Change::Owned { resource: observed, retired_binding: None, .. }] if observed == &resource)
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

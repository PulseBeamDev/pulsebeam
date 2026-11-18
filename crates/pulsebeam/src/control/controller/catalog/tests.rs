use super::*;

fn viewer_scope(endpoint: &str) -> Scope {
    Scope {
        project: *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID,
        endpoint: ParticipantExternalId::new(endpoint).unwrap(),
        room: "room".into(),
        limit: 100,
        ordering_version: ORDERING_VERSION,
    }
}

#[test]
fn continuations_are_scoped_expiring_and_bounded_without_granting_authority() {
    let mut cursors = Cursors::default();
    let mut entropy = pulsebeam_runtime::rand::seeded_rng(31);
    let now = Instant::now();
    let scope = viewer_scope("viewer");
    let first = cursors
        .issue(scope.clone(), "canonical-last".into(), now, &mut entropy)
        .unwrap();
    assert_eq!(
        cursors.resolve(&scope, &first, now).unwrap(),
        "canonical-last"
    );
    for changed in [
        Scope {
            ordering_version: 2,
            ..scope.clone()
        },
        Scope {
            room: "other".into(),
            ..scope.clone()
        },
        Scope {
            limit: 99,
            ..scope.clone()
        },
        Scope {
            endpoint: ParticipantExternalId::new("other").unwrap(),
            ..scope.clone()
        },
        Scope {
            project: ProjectId::from_bytes([42; 16]),
            ..scope.clone()
        },
    ] {
        assert_eq!(
            cursors.resolve(&changed, &first, now),
            Err(wire::FailureCode::InvalidConfiguration)
        );
    }
    assert_eq!(
        cursors.resolve(&scope, &[0; 31], now),
        Err(wire::FailureCode::InvalidConfiguration)
    );
    let later = now.checked_add(Duration::from_secs(61)).unwrap();
    assert_eq!(
        cursors.resolve(&scope, &first, later),
        Err(wire::FailureCode::InvalidConfiguration)
    );
    assert!(cursors.values.is_empty());
    let oldest = cursors
        .issue(scope.clone(), "oldest".into(), now, &mut entropy)
        .unwrap();
    for index in 0..ENDPOINT_CURSORS {
        cursors
            .issue(scope.clone(), index.to_string(), now, &mut entropy)
            .unwrap();
    }
    assert_eq!(cursors.values.len(), ENDPOINT_CURSORS);
    assert_eq!(
        cursors.resolve(&scope, &oldest, now),
        Err(wire::FailureCode::InvalidConfiguration)
    );
    for index in 0..SERVER_CURSORS {
        cursors
            .issue(
                viewer_scope(&format!("viewer-{index}")),
                "last".into(),
                now,
                &mut entropy,
            )
            .unwrap();
    }
    assert_eq!(cursors.values.len(), SERVER_CURSORS);
    assert_eq!(cursors.expiry.len(), SERVER_CURSORS);
    assert_eq!(
        cursors.endpoints.values().map(BTreeSet::len).sum::<usize>(),
        SERVER_CURSORS
    );
}

use super::*;

fn viewer_scope(endpoint: &str) -> Scope {
    Scope {
        project: *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID,
        endpoint: ParticipantId::new(endpoint).unwrap(),
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
    let cut = CatalogCut {
        connection: ConnectionId::new(),
        authorization: 1,
        coverage: responders::Cut {
            local: owner::Revision {
                presence: 2,
                publications: 3,
            },
            peers: Box::new([]),
        },
    };
    let first = cursors
        .issue(
            scope.clone(),
            cut.clone(),
            "canonical-last".into(),
            now,
            &mut entropy,
        )
        .unwrap();
    assert_eq!(
        cursors.resolve(&scope, &cut, &first, now).unwrap(),
        "canonical-last"
    );
    let mut presence_cut = cut.clone();
    presence_cut.coverage.local.presence = 3;
    let mut publication_cut = cut.clone();
    publication_cut.coverage.local.publications = 4;
    let mut responder_cut = cut.clone();
    responder_cut.coverage.peers = Box::new([(
        (
            pulsebeam_core::identity::NodeId::new(),
            std::num::NonZeroU64::MIN,
        ),
        None,
    )]);
    for stale in [
        CatalogCut {
            connection: ConnectionId::new(),
            ..cut.clone()
        },
        CatalogCut {
            authorization: 2,
            ..cut.clone()
        },
        presence_cut,
        publication_cut,
        responder_cut,
    ] {
        assert_eq!(
            cursors.resolve(&scope, &stale, &first, now),
            Err(wire::FailureCode::ResourceGone)
        );
    }
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
            endpoint: ParticipantId::new("other").unwrap(),
            ..scope.clone()
        },
        Scope {
            project: ProjectId::from_bytes([42; 16]),
            ..scope.clone()
        },
    ] {
        assert_eq!(
            cursors.resolve(&changed, &cut, &first, now),
            Err(wire::FailureCode::InvalidConfiguration)
        );
    }
    assert_eq!(
        cursors.resolve(&scope, &cut, &[0; 31], now),
        Err(wire::FailureCode::InvalidConfiguration)
    );
    let later = now.checked_add(Duration::from_secs(61)).unwrap();
    assert_eq!(
        cursors.resolve(&scope, &cut, &first, later),
        Err(wire::FailureCode::InvalidConfiguration)
    );
    assert!(cursors.values.is_empty());
    let oldest = cursors
        .issue(
            scope.clone(),
            cut.clone(),
            "oldest".into(),
            now,
            &mut entropy,
        )
        .unwrap();
    for index in 0..ENDPOINT_CURSORS {
        cursors
            .issue(
                scope.clone(),
                cut.clone(),
                index.to_string(),
                now,
                &mut entropy,
            )
            .unwrap();
    }
    assert_eq!(cursors.values.len(), ENDPOINT_CURSORS);
    assert_eq!(
        cursors.resolve(&scope, &cut, &oldest, now),
        Err(wire::FailureCode::InvalidConfiguration)
    );
    for index in 0..SERVER_CURSORS {
        cursors
            .issue(
                viewer_scope(&format!("viewer-{index}")),
                cut.clone(),
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

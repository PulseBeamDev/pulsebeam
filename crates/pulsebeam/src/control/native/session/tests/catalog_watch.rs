use super::*;

fn page(session: &mut NativeSession, id: u64, revision: u64) {
    session
        .replace_catalog(
            id,
            revision,
            vec![wire::CatalogPageRecord {
                watch_id: id,
                room: "room".into(),
                revision,
                complete: true,
                ..Default::default()
            }],
        )
        .unwrap();
}

fn watched_session() -> NativeSession {
    let mut session = session();
    drain(&mut session);
    for id in 1..=2 {
        session
            .catalog
            .input(wire::CatalogClientRecord {
                command: Some(wire::catalog_client_record::Command::Open(
                    wire::CatalogPageRequest {
                        watch_id: id,
                        room: "room".into(),
                        participant_limit: 100,
                        cursor: Vec::new(),
                    },
                )),
            })
            .unwrap();
        page(&mut session, id, 1);
    }
    session
}

#[test]
fn unsent_replacement_preserves_other_watch_fragments_and_resource_ack_accounting() {
    let mut session = watched_session();
    session
        .authorization(20_000, 2, ResourcePermissions::RoomBound)
        .unwrap();
    for id in 1..=2 {
        page(&mut session, id, 2);
    }
    let selected = output(&mut session).unwrap();
    assert_eq!(selected.catalog_records.len(), 2);
    assert!(!selected.records.is_empty());
    let old_sequence = session.pending.as_ref().unwrap().sequence;
    page(&mut session, 1, 3);
    assert!(session.pending.is_none());
    let replacement = output(&mut session).unwrap();
    assert!(session.pending.as_ref().unwrap().sequence > old_sequence);
    let mut revisions = replacement
        .catalog_records
        .iter()
        .map(|record| {
            let Some(wire::catalog_server_record::Response::Page(page)) = &record.response else {
                panic!("page expected")
            };
            (page.watch_id, page.revision)
        })
        .collect::<Vec<_>>();
    revisions.sort();
    assert_eq!(revisions, vec![(1, 3), (2, 2)]);
    assert_eq!(replacement.records, selected.records);
    commit_sent(&mut session);
    assert!(
        drain(&mut session)
            .iter()
            .all(|batch| batch.catalog_records.is_empty())
    );
    assert!(!session.needs_poll());
}

#[test]
fn authorization_invalidates_serialized_unsent_pages_before_failure_exposure() {
    let mut session = watched_session();
    assert_eq!(output(&mut session).unwrap().catalog_records.len(), 2);
    session
        .authorization(20_000, 2, ResourcePermissions::Denied)
        .unwrap();
    assert!(session.pending.is_none());
    for id in 1..=2 {
        session
            .fail_catalog(id, wire::FailureCode::PermissionDenied)
            .unwrap();
    }
    let batches = drain(&mut session);
    let mut failures = 0;
    for record in batches.into_iter().flat_map(|batch| batch.catalog_records) {
        let Some(wire::catalog_server_record::Response::Error(error)) = record.response else {
            panic!("revocation exposed serialized old-authority metadata")
        };
        assert_eq!(
            error.failure.unwrap().code,
            wire::FailureCode::PermissionDenied as i32
        );
        failures += 1;
    }
    assert_eq!(failures, 2);
}

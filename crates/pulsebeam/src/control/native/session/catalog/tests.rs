use super::*;

fn open(id: u64, limit: u32) -> wire::CatalogClientRecord {
    wire::CatalogClientRecord {
        command: Some(wire::catalog_client_record::Command::Open(
            wire::CatalogPageRequest {
                watch_id: id,
                room: "room".into(),
                participant_limit: limit,
                cursor: Vec::new(),
            },
        )),
    }
}

#[test]
fn watch_admission_is_bounded_scoped_and_terminal_without_closed_id_history() {
    let mut session = CatalogSession::default();
    session.input(open(1, 0)).unwrap();
    session.input(open(1, 100)).unwrap();
    assert_eq!(session.watches.len(), 1);
    for id in 2..=8 {
        session.input(open(id, 100)).unwrap();
    }
    session.input(open(9, 100)).unwrap();
    assert_eq!(session.watches.len(), MAX_WATCHES);
    let Some(wire::catalog_server_record::Response::Error(error)) =
        session.errors.pop_front().unwrap().response
    else {
        panic!("watch limit must report an error")
    };
    assert_eq!(
        error.failure.unwrap().code,
        wire::FailureCode::CapacityExceeded as i32
    );
    session
        .input(wire::CatalogClientRecord {
            command: Some(wire::catalog_client_record::Command::Close(
                wire::CatalogWatchClose { watch_id: 1 },
            )),
        })
        .unwrap();
    session.input(open(1, 100)).unwrap();
    assert!(!session.contains(1));
    assert_eq!(session.watches.len(), 7);
    session.input(open(10, 100)).unwrap();
    assert!(session.contains(10));
    session.close();
    assert!(session.watches.is_empty());
    assert!(session.errors.is_empty());
}

#[test]
fn slow_catalog_output_coalesces_revisions_and_shares_the_raw_frame_budget() {
    let mut session = CatalogSession::default();
    session.input(open(1, 100)).unwrap();
    for revision in 1..=50 {
        let page = wire::CatalogPageRecord {
            watch_id: 1,
            room: "room".into(),
            revision,
            complete: true,
            ..Default::default()
        };
        session.replace(1, revision, vec![page]).unwrap();
    }
    assert_eq!(session.watches[&1].output.len(), 1);
    let mut batch = wire::ServerStateBatch::default();
    session.append(&mut batch);
    let Some(wire::catalog_server_record::Response::Page(page)) =
        &batch.catalog_records[0].response
    else {
        panic!("page expected")
    };
    assert_eq!(page.revision, 50);
    assert!(!session.pending());
    assert!(codec::encode_server(&batch).unwrap().len() <= codec::MAX_FRAME_SIZE);
    session
        .replace(
            1,
            51,
            vec![wire::CatalogPageRecord {
                watch_id: 1,
                room: "room".into(),
                revision: 51,
                complete: true,
                ..Default::default()
            }],
        )
        .unwrap();
    let mut full = wire::ServerStateBatch {
        records: vec![wire::ServerStateRecord::default(); codec::MAX_RECORDS],
        ..Default::default()
    };
    session.append(&mut full);
    assert!(full.catalog_records.is_empty());
    assert!(session.pending());
    session.invalidate();
    assert!(!session.pending());
    assert_eq!(session.watches[&1].bytes, 0);
}

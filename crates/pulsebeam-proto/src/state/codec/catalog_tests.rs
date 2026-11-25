#![allow(clippy::expect_used, clippy::indexing_slicing)]

use alloc::{string::String, vec};

use super::*;
use crate::state::*;

fn client_open(id: u64) -> CatalogClientRecord {
    CatalogClientRecord {
        command: Some(catalog_client_record::Command::Open(CatalogPageRequest {
            watch_id: id,
            room: String::from("room"),
            participant_limit: 0,
            cursor: vec![],
        })),
    }
}

fn resource(id: u64) -> ClientStateRecord {
    ClientStateRecord {
        owned: id,
        desired: Some(DesiredState {
            revision: id.saturating_add(1),
            value: Some(desired_state::Value::Absent(true)),
        }),
        execution: None,
    }
}

fn page(id: u64) -> CatalogPageRecord {
    CatalogPageRecord {
        watch_id: id,
        room: String::from("room"),
        revision: 1,
        complete: true,
        partial: false,
        has_more: false,
        next_cursor: vec![],
        cursor_expires_at_unix_seconds: 0,
        fragments: vec![],
    }
}

fn server_page(id: u64) -> CatalogServerRecord {
    CatalogServerRecord {
        response: Some(catalog_server_record::Response::Page(page(id))),
    }
}

#[test]
fn directional_catalog_open_close_page_and_typed_error_shapes_roundtrip() {
    let request = ClientStateBatch {
        catalog_records: vec![client_open(7)],
        ..Default::default()
    };
    let bytes = encode_client(&request).expect("open encodes");
    assert_eq!(decode_client(&bytes), Ok(request));

    let close = ClientStateBatch {
        catalog_records: vec![CatalogClientRecord {
            command: Some(catalog_client_record::Command::Close(CatalogWatchClose {
                watch_id: 7,
            })),
        }],
        ..Default::default()
    };
    let bytes = encode_client(&close).expect("close encodes");
    assert_eq!(decode_client(&bytes), Ok(close));

    let page = ServerStateBatch {
        initial_complete: true,
        catalog_records: vec![server_page(7)],
        ..Default::default()
    };
    let bytes = encode_server(&page).expect("page encodes");
    assert_eq!(decode_server(&bytes), Ok(page));

    let error = ServerStateBatch {
        initial_complete: true,
        catalog_records: vec![CatalogServerRecord {
            response: Some(catalog_server_record::Response::Error(CatalogWatchError {
                watch_id: 7,
                failure: Some(Failure {
                    code: FailureCode::PermissionDenied as i32,
                    diagnostic: String::new(),
                }),
            })),
        }],
        ..Default::default()
    };
    let bytes = encode_server(&error).expect("error encodes");
    assert_eq!(decode_server(&bytes), Ok(error));
}

#[test]
fn mixed_resource_and_catalog_records_share_one_count_ceiling() {
    let mut client = ClientStateBatch {
        records: (1..=8).map(resource).collect(),
        catalog_records: (9..=16).map(client_open).collect(),
    };
    let bytes = encode_client(&client).expect("mixed client frame");
    assert_eq!(decode_client(&bytes), Ok(client.clone()));
    client.catalog_records.push(client_open(17));
    assert_eq!(
        encode_client(&client),
        Err(EncodeError::Validation(ValidationError::Batch))
    );
    assert_eq!(
        decode_client(&client.encode_to_vec()),
        Err(DecodeError::Structure)
    );

    let server = ServerStateBatch {
        initial_complete: false,
        records: (1..=8)
            .map(|id| ServerStateRecord {
                key: Some(ResourceKey {
                    key: Some(resource_key::Key::Owned(id)),
                }),
                confirmed: Some(ConfirmedState {
                    evaluated_revision: id,
                    rejection: None,
                    value: Some(confirmed_state::Value::Absent(AbsentState {
                        reason: AbsenceReason::Removed as i32,
                    })),
                }),
            })
            .collect(),
        catalog_records: (9..=16).map(server_page).collect(),
    };
    let bytes = encode_server(&server).expect("mixed server frame");
    assert_eq!(decode_server(&bytes), Ok(server.clone()));
    let too_many = ServerStateBatch {
        catalog_records: (9..=17).map(server_page).collect(),
        ..server
    };
    assert_eq!(
        encode_server(&too_many),
        Err(EncodeError::Validation(ValidationError::Batch))
    );
    assert_eq!(
        decode_server(&too_many.encode_to_vec()),
        Err(DecodeError::Structure)
    );
}

#[test]
fn catalog_commands_and_responses_require_valid_directional_shapes() {
    for record in [
        CatalogClientRecord { command: None },
        CatalogClientRecord {
            command: Some(catalog_client_record::Command::Close(CatalogWatchClose {
                watch_id: 0,
            })),
        },
        CatalogClientRecord {
            command: Some(catalog_client_record::Command::Open(CatalogPageRequest {
                watch_id: 1,
                room: String::from("room"),
                participant_limit: 501,
                cursor: vec![],
            })),
        },
    ] {
        let batch = ClientStateBatch {
            catalog_records: vec![record],
            ..Default::default()
        };
        assert!(matches!(
            encode_client(&batch),
            Err(EncodeError::Validation(ValidationError::Catalog))
        ));
    }

    for record in [
        CatalogServerRecord { response: None },
        CatalogServerRecord {
            response: Some(catalog_server_record::Response::Error(CatalogWatchError {
                watch_id: 0,
                failure: Some(Failure {
                    code: FailureCode::PermissionDenied as i32,
                    diagnostic: String::new(),
                }),
            })),
        },
        CatalogServerRecord {
            response: Some(catalog_server_record::Response::Error(CatalogWatchError {
                watch_id: 1,
                failure: None,
            })),
        },
    ] {
        let batch = ServerStateBatch {
            catalog_records: vec![record],
            ..Default::default()
        };
        assert!(matches!(
            encode_server(&batch),
            Err(EncodeError::Validation(ValidationError::Catalog))
        ));
    }
}

#[test]
fn a_single_catalog_record_must_fit_real_batch_overhead() {
    let page_for_len = |target| {
        let mut page = page(7);
        page.fragments.push(CatalogParticipantFragment {
            watch_id: 7,
            revision: 1,
            participant_index: 0,
            fragment_index: 0,
            fragment_count: 1,
            participant_id: String::from("p"),
            entry_chunk: vec![],
        });
        for size in 0..=MAX_FRAME_SIZE {
            page.fragments[0].entry_chunk.resize(size, 0);
            if page.encoded_len() == target {
                return CatalogServerRecord {
                    response: Some(catalog_server_record::Response::Page(page)),
                };
            }
            page.fragments[0].entry_chunk.clear();
        }
        panic!("page length is representable")
    };

    for target in [4088, 4089] {
        let batch = ServerStateBatch {
            initial_complete: true,
            catalog_records: vec![page_for_len(target)],
            ..Default::default()
        };
        if target == 4088 {
            let bytes = encode_server(&batch).expect("exact 4096-byte frame");
            assert_eq!(bytes.len(), MAX_FRAME_SIZE);
        } else {
            assert!(matches!(encode_server(&batch), Err(EncodeError::TooLarge)));
        }
    }

    let page_only = ServerStateBatch {
        initial_complete: true,
        catalog_records: vec![page_for_len(4080)],
        ..Default::default()
    };
    assert_eq!(
        encode_server(&page_only).expect("catalog alone fits").len(),
        4088
    );
    let mixed = ServerStateBatch {
        initial_complete: true,
        records: vec![ServerStateRecord {
            key: Some(ResourceKey {
                key: Some(resource_key::Key::Owned(1)),
            }),
            confirmed: Some(ConfirmedState {
                evaluated_revision: 1,
                value: Some(confirmed_state::Value::Absent(AbsentState {
                    reason: AbsenceReason::Removed as i32,
                })),
                ..Default::default()
            }),
        }],
        catalog_records: vec![page_for_len(4080)],
    };
    assert_eq!(encode_server(&mixed), Err(EncodeError::TooLarge));
}

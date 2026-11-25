use super::*;
use crate::{
    IdGenerator, TrackResources,
    native_state::{NativeState, Resource},
    observer::WatchStateObserver,
};
use pulsebeam_proto::{
    catalog::{CatalogPageMetadata, fragment_entry},
    state::codec,
};

fn native() -> NativeState {
    NativeState::new(
        64 * 1024 * 1024,
        wire::Participant {
            participant_id: "self".into(),

            room_id: "room".into(),
        },
    )
    .unwrap()
}

fn open(catalogs: &mut Catalogs, resources: &mut TrackResources, room: &str) -> TrackOwner {
    let owner = resources.new_owner().unwrap();
    let state = catalogs
        .open(owner, room.into(), WatchConfig::default())
        .unwrap();
    assert_eq!(state.status, WatchStatus::Loading);
    assert!(state.page.is_none());
    owner
}

fn records(
    id: u64,
    revision: u64,
    participant: &str,
    tracks: usize,
) -> Vec<wire::CatalogPageRecord> {
    let entry = wire::CatalogParticipantEntry {
        participant: Some(wire::Participant {
            participant_id: participant.into(),

            room_id: "room".into(),
        }),
        tracks: (0..tracks)
            .map(|index| wire::Track {
                publication_id: format!("pub_00000000001R0100000000{index:04}0"),
                participant_id: participant.into(),
                room_id: "room".into(),
                track_id: format!("track-{index:030}"),
                kind: Some(wire::track::Kind::Data(wire::DataTrack {
                    capture_clock: None,
                    delivery: wire::DataDelivery::Latest as i32,
                    record_dd: false,
                })),
            })
            .collect(),
    };
    let metadata = CatalogPageMetadata {
        watch_id: id,
        room: "room".into(),
        revision,
        complete: true,
        has_more: false,
        next_cursor: Vec::new(),
        cursor_expires_at_unix_seconds: 0,
    };
    let mut records = fragment_entry(&metadata, 0, &entry).unwrap();
    for record in &mut records {
        record.complete = false;
    }
    records.last_mut().unwrap().complete = true;
    records
}

fn page(record: wire::CatalogPageRecord) -> wire::CatalogServerRecord {
    wire::CatalogServerRecord {
        response: Some(wire::catalog_server_record::Response::Page(record)),
    }
}

#[test]
fn agent_emits_raw_open_observes_page_and_closes_without_receive_demand() {
    use crate::tests::{RunReconciliation, native_connection::connected};
    use crate::{DataChannelEffect, DataChannelEvent, Effect, HostEvent};
    let (mut agent, generation, channel) = connected(crate::TopicRegistrations::default());
    let owner = agent
        .catalog_watch("room".into(), WatchConfig::default())
        .unwrap();
    let Some(Effect::DataChannel(DataChannelEffect::Send {
        payload, operation, ..
    })) = agent.next_effect_after_turn()
    else {
        panic!("catalog open was not signaled")
    };
    let batch = codec::decode_client(&payload).unwrap();
    assert!(batch.records.is_empty());
    let Some(wire::catalog_client_record::Command::Open(request)) =
        &batch.catalog_records[0].command
    else {
        panic!("open expected")
    };
    let id = request.watch_id;
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Sent {
            generation,
            channel,
            operation,
        }))
        .unwrap();
    let response = wire::ServerStateBatch {
        catalog_records: records(id, 1, "presence-only", 0)
            .into_iter()
            .map(page)
            .collect(),
        ..Default::default()
    };
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            binary: true,
            payload: codec::encode_server(&response).unwrap(),
        }))
        .unwrap();
    let state = agent.catalog_watch_state(owner).unwrap();
    assert_eq!(state.status, WatchStatus::Live);
    assert!(
        state.page.as_ref().unwrap().participants()[0]
            .publications
            .is_empty()
    );
    let closed = agent.close_catalog_watch(owner).unwrap();
    assert_eq!(closed.status, WatchStatus::Closed);
    assert!(closed.page.is_none());
    let Some(Effect::DataChannel(DataChannelEffect::Send { payload, .. })) =
        agent.next_effect_after_turn()
    else {
        panic!("catalog close was not signaled")
    };
    let batch = codec::decode_client(&payload).unwrap();
    assert!(batch.records.is_empty());
    assert!(
        matches!(&batch.catalog_records[0].command, Some(wire::catalog_client_record::Command::Close(close)) if close.watch_id == id)
    );
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            binary: true,
            payload: codec::encode_server(&response).unwrap(),
        }))
        .unwrap();
    assert!(agent.catalog_watch_state(owner).is_none());
}

#[test]
fn failed_watch_projection_survives_until_terminal_notification_transfer_without_slot_history() {
    use crate::tests::{RunReconciliation, native_connection::connected};
    use crate::{DataChannelEffect, DataChannelEvent, Effect, HostEvent};
    let (mut agent, generation, channel) = connected(crate::TopicRegistrations::default());
    let owner = agent
        .catalog_watch("room".into(), WatchConfig::default())
        .unwrap();
    let Some(Effect::DataChannel(DataChannelEffect::Send { payload, .. })) =
        agent.next_effect_after_turn()
    else {
        panic!("catalog open expected")
    };
    let batch = codec::decode_client(&payload).unwrap();
    let Some(wire::catalog_client_record::Command::Open(request)) =
        &batch.catalog_records[0].command
    else {
        panic!("open expected")
    };
    let response = wire::ServerStateBatch {
        catalog_records: vec![wire::CatalogServerRecord {
            response: Some(wire::catalog_server_record::Response::Error(
                wire::CatalogWatchError {
                    watch_id: request.watch_id,
                    failure: Some(wire::Failure {
                        code: wire::FailureCode::PermissionDenied as i32,
                        diagnostic: "private authority".into(),
                    }),
                },
            )),
        }],
        ..Default::default()
    };
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            binary: true,
            payload: codec::encode_server(&response).unwrap(),
        }))
        .unwrap();
    let failed = agent.catalog_watch_state(owner).unwrap();
    assert_eq!(failed.status, WatchStatus::Failed);
    assert_eq!(
        failed.error.as_ref().unwrap().code,
        ErrorCode::PermissionDenied
    );
    assert!(
        !failed
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("private authority")
    );
    assert!(failed.page.is_none());
    let mut transferred = false;
    while let Some(notification) = agent.next_notification() {
        if let crate::Notification::CatalogWatchTerminated {
            owner: ended,
            state,
        } = notification
        {
            assert_eq!(ended, owner.id());
            assert_eq!(state, failed);
            transferred = true;
        }
    }
    assert!(transferred);
    assert!(agent.catalog_watch_state(owner).is_none());
    for _ in 0..8 {
        agent
            .catalog_watch("room".into(), WatchConfig::default())
            .unwrap();
    }
}

#[test]
fn pre_admission_watch_scope_capacity_and_close_have_no_closed_owner_history() {
    let mut catalogs = Catalogs::default();
    let mut resources = TrackResources::default();
    let owners = (0..8)
        .map(|_| open(&mut catalogs, &mut resources, "room"))
        .collect::<Vec<_>>();
    let ninth = resources.new_owner().unwrap();
    assert_eq!(
        catalogs
            .open(ninth, "remote".into(), WatchConfig::default())
            .unwrap_err()
            .code,
        ErrorCode::CapacityExceeded
    );
    assert!(catalogs.pending.is_empty());
    assert!(catalogs.state(owners[0]).unwrap().page.is_none());
    let final_state = catalogs.close(owners[0]).unwrap();
    assert_eq!(final_state.status, WatchStatus::Closed);
    assert!(final_state.page.is_none());
    assert!(catalogs.close(owners[0]).is_none());
    catalogs
        .open(ninth, "remote".into(), WatchConfig::default())
        .unwrap();
    assert_eq!(catalogs.watches.len(), 8);
    assert_eq!(catalogs.state(ninth).unwrap().room, "remote");
}

#[test]
fn atomic_replacements_ignore_stale_fragments_and_share_the_published_page() {
    let mut catalogs = Catalogs::default();
    let mut resources = TrackResources::default();
    let owner = open(&mut catalogs, &mut resources, "room");
    let mut ids = IdGenerator::new();
    catalogs.reconcile(ids.generation(), &mut native()).unwrap();
    let id = catalogs.watches[&owner].wire.unwrap();
    for record in records(id, 1, "a", 0) {
        catalogs.apply(page(record));
    }
    let published = catalogs.state(owner).unwrap();
    assert_eq!(published.status, WatchStatus::Live);
    assert!(!published.page.as_ref().unwrap().has_more());
    let replacement = records(
        id,
        2,
        "b",
        pulsebeam_proto::catalog::MAX_TRACKS_PER_PARTICIPANT,
    );
    assert!(replacement.len() > 1);
    assert!(catalogs.apply(page(replacement[0].clone())).is_none());
    assert!(core::ptr::eq(
        published.page.as_deref().unwrap(),
        catalogs.state(owner).unwrap().page.as_deref().unwrap()
    ));
    for record in records(id, 3, "c", 0) {
        catalogs.apply(page(record));
    }
    for record in replacement.into_iter().skip(1) {
        assert!(catalogs.apply(page(record)).is_none());
    }
    let current = catalogs.state(owner).unwrap();
    assert_eq!(current.page.as_ref().unwrap().participants()[0].id, "c");
    assert_eq!(published.page.as_ref().unwrap().participants()[0].id, "a");
    let observer = WatchStateObserver::new(current.clone());
    let mut observers = Vec::new();
    for _ in 0..crate::observer::MAX_OBSERVERS {
        observers.push(observer.updates().unwrap());
    }
    assert!(observer.updates().is_err());
    assert!(core::ptr::eq(
        current.page.as_deref().unwrap(),
        observer.state().page.as_deref().unwrap()
    ));
    observer.close(current).unwrap();
    assert!(observer.state().page.is_none());
    catalogs.disconnect();
    assert_eq!(catalogs.state(owner).unwrap().status, WatchStatus::Loading);
    assert!(catalogs.state(owner).unwrap().page.is_none());
}

#[test]
fn raw_catalog_commands_share_resource_budgets_and_reconnect_reissues_pinned_scope() {
    let mut catalogs = Catalogs::default();
    let mut resources = TrackResources::default();
    let mut native = native();
    let mut ids = IdGenerator::new();
    let generation = ids.generation();
    let owners = (0..8)
        .map(|_| open(&mut catalogs, &mut resources, "room"))
        .collect::<Vec<_>>();
    catalogs.reconcile(generation, &mut native).unwrap();
    for index in 0..12 {
        let label = format!("publication-{index}");
        native
            .stage(
                Resource::Publication {
                    kind: wire::TrackKind::Data,
                    track_id: label.clone(),
                },
                Some(wire::ResourceSpec {
                    settings: Some(wire::ResourceSettings {
                        enabled: Some(true),
                        receive: None,
                    }),
                    declaration: Some(wire::resource_spec::Declaration::Publication(
                        wire::PublicationSpec {
                            track_id: label,
                            kind: Some(wire::publication_spec::Kind::Data(wire::DataPublication {
                                capture_clock: None,
                                delivery: wire::DataDelivery::Latest as i32,
                                binding: Some(wire::TrackBinding {
                                    transport: Some(wire::track_binding::Transport::Sctp(
                                        wire::SctpParameters {
                                            stream_id: Some(index + 1),
                                        },
                                    )),
                                }),
                            })),
                        },
                    )),
                }),
            )
            .unwrap();
    }
    let bytes = native.next_frame().unwrap().unwrap();
    let batch = codec::decode_client(&bytes).unwrap();
    assert_eq!(batch.catalog_records.len(), 8);
    assert_eq!(batch.records.len(), 8);
    assert!(bytes.len() <= codec::MAX_FRAME_SIZE);
    let old_id = catalogs.watches[&owners[0]].wire.unwrap();
    catalogs.close(owners[0]).unwrap();
    let next = open(&mut catalogs, &mut resources, "room");
    catalogs.reconcile(generation, &mut native).unwrap();
    assert!(catalogs.watches[&next].wire.unwrap() > old_id);
    let failure = wire::CatalogServerRecord {
        response: Some(wire::catalog_server_record::Response::Error(
            wire::CatalogWatchError {
                watch_id: catalogs.watches[&next].wire.unwrap(),
                failure: Some(wire::Failure {
                    code: wire::FailureCode::PermissionDenied as i32,
                    diagnostic: "private detail".into(),
                }),
            },
        )),
    };
    let (_, failed) = catalogs.apply(failure).unwrap();
    assert_eq!(failed.status, WatchStatus::Failed);
    assert!(failed.page.is_none());
    assert_eq!(failed.error.unwrap().code, ErrorCode::PermissionDenied);
    assert!(catalogs.state(next).is_none());
    catalogs.disconnect();
    let mut replacement = self::native();
    catalogs
        .reconcile(ids.generation(), &mut replacement)
        .unwrap();
    let batch = codec::decode_client(&replacement.next_frame().unwrap().unwrap()).unwrap();
    assert!(batch.records.is_empty());
    assert_eq!(batch.catalog_records.len(), 7);
    assert!(batch.catalog_records.into_iter().all(|record| matches!(record.command, Some(wire::catalog_client_record::Command::Open(request)) if request.room == "room")));
}

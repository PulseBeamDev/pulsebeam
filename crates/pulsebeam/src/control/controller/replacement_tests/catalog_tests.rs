use super::*;
use crate::control::native::{
    ExecutionAction, ExecutionCompletion, PreparedResource, ReceiverPreparation, ResourceExecution,
    ResourcePermissions, handoff,
};
use crate::participant::policy::ReceiverExecution;
use crate::participant::signaling::SignalingProgress;
use pulsebeam_proto::{
    catalog::{CatalogAssembler, CatalogPageSnapshot},
    state::{self as wire, codec},
};

fn service_catalog(actor: &mut ControllerActor) {
    let mut catalog = std::mem::take(&mut actor.catalog);
    catalog.service(actor);
    actor.catalog = catalog;
}

fn receive_page(
    actor: &mut ControllerActor,
    endpoint: &mut handoff::EndpointHandoff,
    id: u64,
    minimum: u64,
) -> CatalogPageSnapshot {
    let mut assembler = CatalogAssembler::new(id, "catalog-source".into(), minimum).unwrap();
    for _ in 0..128 {
        actor.service_native_connections();
        service_catalog(actor);
        if let Some(output) = endpoint.output() {
            let batch = codec::decode_server(&output.bytes).unwrap();
            assert!(output.bytes.len() <= codec::MAX_FRAME_SIZE);
            assert!(
                batch
                    .records
                    .len()
                    .saturating_add(batch.catalog_records.len())
                    <= codec::MAX_RECORDS
            );
            for record in batch.catalog_records {
                match record.response.unwrap() {
                    wire::catalog_server_record::Response::Page(page) if page.watch_id == id => {
                        assembler.push_record(page).unwrap();
                    }
                    wire::catalog_server_record::Response::Error(error) => {
                        panic!("unexpected catalog failure: {error:?}")
                    }
                    _ => {}
                }
            }
            assert!(endpoint.progress(SignalingProgress {
                sequence: output.sequence,
                cid: output.cid,
                at: pulsebeam_runtime::time::Instant::now()
            }));
            if let Some(page) = assembler.finish_page().unwrap() {
                return page;
            }
        }
    }
    panic!("bounded catalog work did not converge")
}

fn request(endpoint: &handoff::EndpointHandoff, id: u64, cursor: Vec<u8>) {
    let batch = wire::ClientStateBatch {
        catalog_records: vec![wire::CatalogClientRecord {
            command: Some(wire::catalog_client_record::Command::Open(
                wire::CatalogPageRequest {
                    watch_id: id,
                    room: "catalog-source".into(),
                    participant_limit: 70,
                    cursor,
                },
            )),
        }],
        ..Default::default()
    };
    assert!(endpoint.input(codec::encode_client(&batch).unwrap()));
}

fn source_permissions(allow: &[&str]) -> ResourcePermissions {
    ResourcePermissions::Grants {
        room: RoomExternalId::new("viewer-room").unwrap(),
        grants: serde_json::from_value(serde_json::json!([
            {"rooms":["viewer-room"],"allow":["join"]},
            {"rooms":["catalog-source"],"allow":allow}
        ]))
        .unwrap(),
    }
}

fn authorize(
    actor: &mut ControllerActor,
    participant: ParticipantId,
    connection: ConnectionId,
    lease: &AuthorizationLease,
) {
    actor
        .core
        .registry
        .update_authorization(&participant, connection, lease.clone());
    assert!(
        actor
            .native_connections
            .get_mut(&connection)
            .unwrap()
            .handler
            .model
            .authorization(i64::MAX, lease.revision, lease.permissions().clone())
            .unwrap()
    );
}

fn install_receiver(
    actor: &mut ControllerActor,
    endpoint: &mut handoff::EndpointHandoff,
    track: &crate::track::Track,
) {
    let request = wire::ClientStateBatch {
        records: vec![wire::ClientStateRecord {
            owned: 1,
            execution: None,
            desired: Some(wire::DesiredState {
                revision: 1,
                value: Some(wire::desired_state::Value::Present(Box::new(
                    wire::ResourceSpec {
                        settings: Some(wire::ResourceSettings {
                            enabled: Some(true),
                            receive: None,
                        }),
                        declaration: Some(wire::resource_spec::Declaration::Subscription(
                            wire::SubscriptionSpec {
                                track_id: track.id().as_str(),
                                kind: wire::TrackKind::Data as i32,
                                source_room: "catalog-source".into(),
                                binding: Some(wire::TrackBinding {
                                    transport: Some(wire::track_binding::Transport::Sctp(
                                        wire::SctpParameters { stream_id: Some(1) },
                                    )),
                                }),
                            },
                        )),
                    },
                ))),
            }),
        }],
        ..Default::default()
    };
    assert!(endpoint.input(codec::encode_client(&request).unwrap()));
    actor.service_native_connections();
    let ExecutionAction::Prepare {
        work,
        resource: PreparedResource::Receiver(ReceiverPreparation::Ready(plan)),
    } = endpoint.command().unwrap().into_action()
    else {
        panic!("authoritative receiver preparation");
    };
    assert_eq!(plan.source.id(), track.id());
    let output = endpoint.output().unwrap();
    assert!(endpoint.progress(SignalingProgress {
        sequence: output.sequence,
        cid: output.cid,
        at: pulsebeam_runtime::time::Instant::now(),
    }));
    assert!(
        endpoint.fact(handoff::Fact::Completion(ExecutionCompletion {
            work,
            result: Ok(ResourceExecution::Receiver(ReceiverExecution {
                source: track.id(),
                enabled: true,
                ready: true,
                failed: false,
            })),
            released: false,
            unix_now: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        }))
    );
    actor.service_native_connections();
    let output = endpoint.output().unwrap();
    let batch = codec::decode_server(&output.bytes).unwrap();
    assert!(
        batch
            .records
            .iter()
            .any(|record| record.confirmed.as_ref().is_some_and(
                |confirmed| matches!(&confirmed.value,
            Some(wire::confirmed_state::Value::Owned(live))
                if live.execution == wire::ExecutionStatus::Installed as i32
                    && live.track == crate::control::native::track_descriptor(track))
            ))
    );
    assert!(endpoint.progress(SignalingProgress {
        sequence: output.sequence,
        cid: output.cid,
        at: pulsebeam_runtime::time::Instant::now(),
    }));
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn live_catalog_uses_bounded_ordered_presence_pages_and_independent_watches() {
    let (mut actor, _receivers) = actor_with_receivers(1);
    // Match the legacy project used by the Native unit-test source-room resolver.
    let project = crate::entity::ProjectId::from_bytes([
        0x01, 0x8f, 0x4f, 0x7c, 0x20, 0x00, 0x70, 0x00, 0x80, 0x00, 0, 0, 0, 0, 0, 1,
    ]);
    let source_room = RoomId::derive(&project, &RoomExternalId::new("catalog-source").unwrap());
    let viewer_room = RoomId::derive(&project, &RoomExternalId::new("viewer-room").unwrap());
    let mut ordered = Vec::new();
    for index in 1..=80u8 {
        let external =
            crate::entity::ParticipantExternalId::new(&format!("presence-{index}")).unwrap();
        let participant = ParticipantId::derive(&source_room, &external);
        let transport = actor
            .core
            .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
        actor
            .core
            .registry
            .commit_candidate(
                participant,
                source_room,
                ShardId::new(0),
                usize::from(index).saturating_sub(1),
                transport,
                connection_id(index),
                SignalingProtocol::State,
                Some(external),
            )
            .unwrap();
        ordered.push(participant.as_str());
    }
    ordered.sort();
    let publisher = ordered.last().unwrap().parse::<ParticipantId>().unwrap();
    let publisher_connection = actor
        .core
        .registry
        .get_participant(&publisher)
        .unwrap()
        .connection_id;
    actor.core.registry.mark_materialized(&publisher);
    let mut publisher_lease = authorization_lease(
        u64::MAX,
        UNIX_EPOCH,
        pulsebeam_runtime::time::Instant::now(),
        "catalog-source",
        source_room,
        publisher,
    );
    publisher_lease.authority.project_id = project;
    actor
        .core
        .registry
        .update_authorization(&publisher, publisher_connection, publisher_lease);
    let track = crate::track::Track::data_publication(
        crate::track::TrackMeta {
            room_id: source_room,
            shard_id: ShardId::new(0),
            id: publisher_connection.derive_publication_track_id(
                crate::entity::TrackKind::Data,
                "events",
                std::num::NonZeroU64::new(1).unwrap(),
            ),
            origin: publisher,
            connection_id: publisher_connection,
            label: Some("events".into()),
        },
        crate::track::DataLane::Ordered,
    );
    actor.handle_shard_event((ShardId::new(0), ShardEvent::TrackPublished(track.clone())));
    let external = crate::entity::ParticipantExternalId::new("alice").unwrap();
    let participant = ParticipantId::derive(&viewer_room, &external);
    let connection = connection_id(200);
    let transport = actor
        .core
        .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
    actor
        .core
        .registry
        .commit_candidate(
            participant,
            viewer_room,
            ShardId::new(0),
            0,
            transport,
            connection,
            SignalingProtocol::State,
            Some(external.clone()),
        )
        .unwrap();
    let mut lease = authorization_lease(
        u64::MAX,
        UNIX_EPOCH,
        pulsebeam_runtime::time::Instant::now(),
        "viewer-room",
        viewer_room,
        participant,
    );
    lease.authority.project_id = project;
    lease.permissions = source_permissions(&["discover", "subscribe"]);
    actor
        .core
        .registry
        .update_authorization(&participant, connection, lease.clone());
    let (controller, mut endpoint) = handoff::pair(connection, actor.native_ready_tx.clone());
    let mut native =
        StateProtocolHandler::new(participant, viewer_room, external, None, controller).unwrap();
    native
        .model
        .authorization(i64::MAX, 1, lease.permissions().clone())
        .unwrap();
    actor.native_connections.insert(
        connection,
        NativeService {
            handler: native,
            deadline: None,
        },
    );
    assert!(endpoint.opened(handoff::Opened {
        channel: crate::media::ChannelId::from_value(0),
        at: pulsebeam_runtime::time::Instant::now()
    }));
    actor.core.registry.mark_materialized(&participant);
    install_receiver(&mut actor, &mut endpoint, &track);
    request(&endpoint, 1, Vec::new());
    actor.service_native_connections();
    service_catalog(&mut actor);
    assert!(
        !actor.native_connections[&connection]
            .handler
            .model
            .catalog
            .pending(),
        "one turn must inspect no more than 64 candidates"
    );
    let first = receive_page(&mut actor, &mut endpoint, 1, 1);
    assert_eq!(
        first
            .entries
            .iter()
            .map(|entry| entry.participant.as_ref().unwrap().participant_id.clone())
            .collect::<Vec<_>>(),
        ordered[..70]
    );
    assert!(first.entries.iter().all(|entry| entry.tracks.is_empty()));
    assert!(first.has_more);
    assert_eq!(first.next_cursor.len(), 32);
    request(&endpoint, 2, first.next_cursor.clone());
    let second = receive_page(&mut actor, &mut endpoint, 2, 1);
    assert_eq!(second.entries.len(), 10);
    assert!(second.entries.iter().any(|entry| {
        entry
            .tracks
            .iter()
            .any(|visible| visible.track_id == track.id().as_str())
    }));
    assert!(!second.has_more);
    assert!(second.next_cursor.is_empty());
    let removed = first.entries[0]
        .participant
        .as_ref()
        .unwrap()
        .participant_id
        .parse::<ParticipantId>()
        .unwrap();
    let incarnation = actor
        .core
        .registry
        .get_participant(&removed)
        .unwrap()
        .connection_id;
    actor
        .core
        .registry
        .remove_incarnation(&removed, incarnation)
        .unwrap();
    let changed = receive_page(
        &mut actor,
        &mut endpoint,
        1,
        first.revision.saturating_add(1),
    );
    assert_eq!(changed.entries.len(), 70);
    assert!(
        changed
            .entries
            .iter()
            .all(|entry| entry.participant.as_ref().unwrap().participant_id != removed.as_str())
    );
    let close = wire::ClientStateBatch {
        catalog_records: vec![wire::CatalogClientRecord {
            command: Some(wire::catalog_client_record::Command::Close(
                wire::CatalogWatchClose { watch_id: 1 },
            )),
        }],
        ..Default::default()
    };
    assert!(endpoint.input(codec::encode_client(&close).unwrap()));
    actor.service_native_connections();
    assert!(
        !actor.native_connections[&connection]
            .handler
            .model
            .catalog
            .contains(1)
    );
    assert!(
        actor.native_connections[&connection]
            .handler
            .model
            .catalog
            .contains(2)
    );
    lease.revision = 2;
    lease.permissions = source_permissions(&["subscribe"]);
    authorize(&mut actor, participant, connection, &lease);
    let mut denied = false;
    for _ in 0..32 {
        actor.service_native_connections();
        service_catalog(&mut actor);
        if let Some(output) = endpoint.output() {
            let batch = codec::decode_server(&output.bytes).unwrap();
            assert!(batch.records.iter().all(|record| !matches!(
                record.key.as_ref().and_then(|key| key.key.as_ref()),
                Some(wire::resource_key::Key::Owned(1))
            )));
            for record in batch.catalog_records {
                let Some(wire::catalog_server_record::Response::Error(error)) = record.response
                else {
                    panic!("revocation exposed queued catalog metadata");
                };
                assert_eq!(error.watch_id, 2);
                assert_eq!(
                    error.failure.unwrap().code,
                    wire::FailureCode::PermissionDenied as i32
                );
                denied = true;
            }
            assert!(endpoint.progress(SignalingProgress {
                sequence: output.sequence,
                cid: output.cid,
                at: pulsebeam_runtime::time::Instant::now()
            }));
        }
        if denied {
            break;
        }
    }
    assert!(denied, "revocation did not terminate its watch");
    assert!(
        endpoint.command().is_none(),
        "Discover loss must not retire the receiver"
    );
    assert!(
        !actor.native_connections[&connection]
            .handler
            .model
            .catalog
            .contains(2)
    );

    lease.revision = 3;
    lease.permissions = source_permissions(&["discover", "subscribe"]);
    authorize(&mut actor, participant, connection, &lease);
    request(&endpoint, 3, changed.next_cursor);
    let restored = receive_page(&mut actor, &mut endpoint, 3, 1);
    assert!(restored.entries.iter().any(|entry| {
        entry
            .tracks
            .iter()
            .any(|visible| visible.track_id == track.id().as_str())
    }));
    assert!(
        endpoint.command().is_none(),
        "restoring Discover must not recreate the receiver"
    );

    lease.revision = 4;
    lease.permissions = source_permissions(&["discover"]);
    authorize(&mut actor, participant, connection, &lease);
    actor.service_native_connections();
    let ExecutionAction::Retire(scope) = endpoint.command().unwrap().into_action() else {
        panic!("Subscribe loss must retire the receiver");
    };
    assert_eq!(scope.binding, 1);
    assert_eq!(scope.authorization, 4);
    let output = endpoint.output().unwrap();
    let batch = codec::decode_server(&output.bytes).unwrap();
    assert!(batch.records.iter().all(|record| !matches!(
        record.key.as_ref().and_then(|key| key.key.as_ref()),
        Some(wire::resource_key::Key::Owned(1))
    )));
    assert!(endpoint.progress(SignalingProgress {
        sequence: output.sequence,
        cid: output.cid,
        at: pulsebeam_runtime::time::Instant::now(),
    }));
    assert!(endpoint.fact(handoff::Fact::Resource(
        crate::participant::policy::ResourceObservation::Released { binding: 1 }
    )));
    actor.service_native_connections();
    let output = endpoint.output().unwrap();
    let batch = codec::decode_server(&output.bytes).unwrap();
    assert!(batch.records.iter().any(|record| {
        record.key.as_ref().and_then(|key| key.key.as_ref())
            == Some(&wire::resource_key::Key::Owned(1))
            && record.confirmed.as_ref().is_some_and(|confirmed| {
                matches!(&confirmed.value,
            Some(wire::confirmed_state::Value::Absent(absent))
                if absent.reason == wire::AbsenceReason::PermissionRevoked as i32)
            })
    }));
    assert!(endpoint.progress(SignalingProgress {
        sequence: output.sequence,
        cid: output.cid,
        at: pulsebeam_runtime::time::Instant::now(),
    }));
    assert!(
        actor.native_connections[&connection]
            .handler
            .model
            .catalog
            .contains(3)
    );
    let removed = restored.entries[0]
        .participant
        .as_ref()
        .unwrap()
        .participant_id
        .parse::<ParticipantId>()
        .unwrap();
    assert_ne!(removed, publisher);
    let incarnation = actor
        .core
        .registry
        .get_participant(&removed)
        .unwrap()
        .connection_id;
    actor
        .core
        .registry
        .remove_incarnation(&removed, incarnation)
        .unwrap();
    let updated = receive_page(
        &mut actor,
        &mut endpoint,
        3,
        restored.revision.saturating_add(1),
    );
    assert_eq!(updated.entries.len(), restored.entries.len() - 1);
    assert!(
        updated
            .entries
            .iter()
            .all(|entry| entry.participant.as_ref().unwrap().participant_id != removed.as_str())
    );
    assert!(updated.entries.iter().any(|entry| {
        entry
            .tracks
            .iter()
            .any(|visible| visible.track_id == track.id().as_str())
    }));
    assert!(endpoint.command().is_none());
}

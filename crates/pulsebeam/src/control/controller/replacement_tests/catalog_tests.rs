use super::*;
use crate::control::controller::catalog::neighbor;
use crate::control::native::{
    ExecutionAction, ExecutionCompletion, PreparedResource, ReceiverPreparation, ResourceExecution,
    ResourcePermissions, handoff,
};
use crate::participant::policy::ReceiverExecution;
use crate::participant::signaling::{SignalingOutput, SignalingProgress};
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
    receive_page_with_reset(actor, endpoint, id, minimum, None)
}

fn receive_page_with_reset(
    actor: &mut ControllerActor,
    endpoint: &mut handoff::EndpointHandoff,
    id: u64,
    minimum: u64,
    reset: Option<u64>,
) -> CatalogPageSnapshot {
    receive_page_driven(actor, endpoint, id, minimum, reset, service_catalog)
}

fn receive_page_driven(
    actor: &mut ControllerActor,
    endpoint: &mut handoff::EndpointHandoff,
    id: u64,
    minimum: u64,
    reset: Option<u64>,
    drive: impl Fn(&mut ControllerActor),
) -> CatalogPageSnapshot {
    let mut reader = PageReader::new(id, minimum, reset);
    for _ in 0..128 {
        actor.service_native_connections();
        drive(actor);
        if let Some(page) = reader.poll(endpoint) {
            return page;
        }
    }
    panic!("bounded catalog work did not converge")
}

struct PageReader {
    id: u64,
    revision: u64,
    assembler: Option<CatalogAssembler>,
    reset: Option<u64>,
}

impl PageReader {
    fn new(id: u64, minimum: u64, reset: Option<u64>) -> Self {
        Self {
            id,
            revision: minimum.saturating_sub(1),
            assembler: None,
            reset,
        }
    }

    fn poll(&mut self, endpoint: &mut handoff::EndpointHandoff) -> Option<CatalogPageSnapshot> {
        let output = endpoint.output()?;
        self.consume(endpoint, output)
    }

    fn drain(
        &mut self,
        actor: &mut ControllerActor,
        endpoint: &mut handoff::EndpointHandoff,
    ) -> Option<CatalogPageSnapshot> {
        for _ in 0..128 {
            actor.service_native_connections();
            let output = endpoint.output()?;
            if let Some(page) = self.consume(endpoint, output) {
                return Some(page);
            }
        }
        panic!("Native catalog output exceeded the fixture drain budget")
    }

    fn consume(
        &mut self,
        endpoint: &mut handoff::EndpointHandoff,
        output: SignalingOutput,
    ) -> Option<CatalogPageSnapshot> {
        let batch = codec::decode_server(&output.bytes).unwrap();
        assert!(output.bytes.len() <= codec::MAX_FRAME_SIZE);
        assert!(
            batch
                .records
                .len()
                .saturating_add(batch.catalog_records.len())
                <= codec::MAX_RECORDS
        );
        let mut latest = None;
        for record in batch.catalog_records {
            match record.response.unwrap() {
                wire::catalog_server_record::Response::Page(page) if page.watch_id == self.id => {
                    if page.revision > self.revision {
                        self.revision = page.revision;
                        self.assembler = Some(
                            CatalogAssembler::new(self.id, "catalog-source".into(), self.revision)
                                .unwrap(),
                        );
                    }
                    if page.revision != self.revision {
                        continue;
                    }
                    if let Some(assembler) = &mut self.assembler {
                        assembler.push_record(page).unwrap();
                        if let Some(page) = assembler.finish_page().unwrap() {
                            latest = Some(page);
                            self.assembler = None;
                        }
                    }
                }
                wire::catalog_server_record::Response::Error(error) => {
                    assert_eq!(
                        self.reset.take(),
                        Some(error.watch_id),
                        "unexpected catalog failure: {error:?}"
                    );
                    assert_eq!(
                        error.failure.unwrap().code(),
                        wire::FailureCode::ResourceGone
                    );
                }
                _ => {}
            }
        }
        assert!(endpoint.progress(SignalingProgress {
            sequence: output.sequence,
            cid: output.cid,
            at: pulsebeam_runtime::time::Instant::now()
        }));
        if latest.is_some() {
            assert!(
                self.reset.is_none(),
                "stale continuation was not invalidated"
            );
        }
        latest
    }
}
fn request(endpoint: &handoff::EndpointHandoff, id: u64, cursor: Vec<u8>) {
    request_limit(endpoint, id, cursor, 70);
}

fn request_limit(
    endpoint: &handoff::EndpointHandoff,
    id: u64,
    cursor: Vec<u8>,
    participant_limit: u32,
) {
    let batch = wire::ClientStateBatch {
        catalog_records: vec![wire::CatalogClientRecord {
            command: Some(wire::catalog_client_record::Command::Open(
                wire::CatalogPageRequest {
                    watch_id: id,
                    room: "catalog-source".into(),
                    participant_limit,
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
        joined_room: Some(RoomId::new("viewer-room").unwrap()),
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
                    crate::control::native::fixture::audio_receiver(track.id(), "catalog-source"),
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
    if let Some(output) = endpoint.output() {
        assert!(endpoint.progress(SignalingProgress {
            sequence: output.sequence,
            cid: output.cid,
            at: pulsebeam_runtime::time::Instant::now(),
        }));
    }
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

fn register_viewer(
    actor: &mut ControllerActor,
    project: crate::entity::ProjectId,
) -> (
    ParticipantId,
    ConnectionId,
    AuthorizationLease,
    handoff::EndpointHandoff,
) {
    let viewer_room = RoomId::new("viewer-room").unwrap();
    let participant = ParticipantId::new("alice").unwrap();
    let connection = connection_id(200);
    let transport = actor
        .core
        .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
    actor
        .core
        .registry
        .commit_candidate(
            participant,
            ProjectRoomKey {
                project,
                room: viewer_room,
            },
            ShardId::new(0),
            0,
            transport,
            connection,
            SignalingProtocol::State,
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
    let mut native = StateProtocolHandler::new(
        participant,
        Some(viewer_room),
        Some(crate::control::rtp::capabilities()),
        controller,
    )
    .unwrap();
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
    (participant, connection, lease, endpoint)
}

fn publish_catalog_audio(
    actor: &mut ControllerActor,
    room_key: ProjectRoomKey,
) -> crate::track::Track {
    publish_catalog_audio_at(actor, room_key, "publisher", 0, connection_id(80))
}

fn publish_catalog_audio_at(
    actor: &mut ControllerActor,
    room_key: ProjectRoomKey,
    name: &str,
    slot: usize,
    connection: ConnectionId,
) -> crate::track::Track {
    let ProjectRoomKey { project, room } = room_key;
    let publisher = ParticipantId::new(name).unwrap();
    let transport = actor
        .core
        .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
    actor
        .core
        .registry
        .commit_candidate(
            publisher,
            room_key,
            ShardId::new(0),
            slot,
            transport,
            connection,
            SignalingProtocol::State,
        )
        .unwrap();
    let mut lease = authorization_lease(
        u64::MAX,
        UNIX_EPOCH,
        pulsebeam_runtime::time::Instant::now(),
        "catalog-source",
        room,
        publisher,
    );
    lease.authority.project_id = project;
    actor
        .core
        .registry
        .update_authorization(&publisher, connection, lease);
    let track = crate::control::native::fixture::audio_track(crate::track::TrackMeta {
        room: room_key,
        id: connection.fixture_publication_at(
            crate::entity::TrackKind::Audio,
            "events",
            std::num::NonZeroU64::new(1).unwrap(),
        ),
        origin: publisher,
        connection_id: connection,
        track_id: Some(crate::entity::TrackId::new("events").unwrap()),
    });
    actor.handle_shard_event((ShardId::new(0), ShardEvent::TrackPublished(track.clone())));
    track
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn empty_catalog_recovers_a_lost_first_publisher_wake() {
    let (mut actor, _receivers) = actor_with_receivers(1);
    let project = *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID;
    let room = RoomId::new("catalog-source").unwrap();
    let room_key = ProjectRoomKey { project, room };
    let (_, _, _, mut endpoint) = register_viewer(&mut actor, project);
    request(&endpoint, 1, Vec::new());
    let empty = receive_page(&mut actor, &mut endpoint, 1, 1);
    assert!(empty.entries.is_empty() && !empty.partial && !empty.has_more);
    service_catalog(&mut actor);
    assert!(!actor.catalog.has_work());

    let track = publish_catalog_audio(&mut actor, room_key);
    actor.catalog.skip_change_wake(
        actor.core.registry.catalog_version(),
        actor.lifecycle.catalog_version(),
    );
    service_catalog(&mut actor);
    assert!(!actor.catalog.has_work());
    pulsebeam_runtime::time::sleep(Duration::from_secs(5)).await;
    let recovered = receive_page(&mut actor, &mut endpoint, 1, empty.revision + 1);
    assert!(!recovered.partial);
    assert!(
        matches!(recovered.entries.as_slice(), [entry] if entry.tracks.iter().any(|visible| visible.publication_id == track.id().as_str()))
    );
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn neighbor_get_requires_a_live_interest_and_reads_a_filtered_owner_cut() {
    use crate::cluster::{
        REGIONAL_FRAME_BYTES,
        declarations::{Declaration, Exchange, Kind, Role},
    };
    use crate::control::controller::catalog::{neighbor, owner};
    use pulsebeam_core::auth::grants::{Action, Grant, GrantSet, RoomDiscover, TrackKind};
    use pulsebeam_link::{LinkTime, StreamControl};
    use pulsebeam_runtime::time::Instant;
    use std::{
        collections::BTreeSet,
        num::{NonZeroU64, NonZeroUsize},
    };

    let (mut actor, _receivers) = actor_with_receivers(1);
    let room = ProjectRoomKey {
        project: *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID,
        room: RoomId::new("catalog-source").unwrap(),
    };
    let visible = publish_catalog_audio(&mut actor, room);
    let grant = Grant::new(
        vec!["*".to_owned()],
        vec![Action::Discover],
        Some("publisher".to_owned()),
        Some(TrackKind::Audio),
        Some("events".to_owned()),
    )
    .unwrap();
    let grants = GrantSet::new(vec![grant]).unwrap();
    let discover = grants.room_discover(room.room.as_str()).unwrap();
    for invalid in [
        r#"{"room":"*","filters":[{}]}"#,
        r#"{"room":"catalog-source","filters":[{"label":null}]}"#,
        r#"{"room":"catalog-source","filters":[{"allow":["publish"]}]}"#,
    ] {
        assert!(serde_json::from_str::<RoomDiscover>(invalid).is_err());
    }
    let wanted = Declaration {
        project: room.project,
        room: room.room,
        kind: Kind::Queryable,
    };
    let now = Instant::now();
    let ttl = LinkTime::from_nanos(u64::MAX);
    let limit = NonZeroUsize::new(REGIONAL_FRAME_BYTES).unwrap();
    let (reader_lane, mut reader_stream) = StreamControl::new_authenticated(limit);
    let (owner_lane, mut owner_stream) = StreamControl::new_authenticated(limit);
    let mut reader = Exchange::new(Role::Edge, Role::Router);
    let mut declarations = Exchange::new(Role::Router, Role::Edge);
    reader.step(&BTreeSet::from([wanted]), |_| false, &reader_lane, ttl, now);
    let (subscription, _) = reader_stream.directions().0.recv().await.unwrap();
    assert!(declarations.receive(&subscription, now));
    declarations.step(&BTreeSet::new(), |key| key == wanted, &owner_lane, ttl, now);
    let (finalized, _) = owner_stream.directions().0.recv().await.unwrap();
    assert!(reader.receive(&finalized, now));
    let cut = reader.ready(wanted, now).filter(|cut| cut.present).unwrap();
    let send_get = |actor: &ControllerActor, interest, after: Option<&str>, revision, at| {
        let wire = neighbor::Get {
            project: room.project,
            discover: discover.clone(),
            interest,
            after: after.map(|id| ParticipantId::new(id).unwrap()),
            revision,
        }
        .encode(NonZeroU64::MIN)
        .unwrap();
        let Some(neighbor::MessageOnStream::Get(id, decoded)) = neighbor::decode(&wire) else {
            panic!("bad GET")
        };
        let wire = decoded.serve(id, actor, &declarations, at)?;
        let Some(neighbor::MessageOnStream::Reply(reply_id, decoded)) = neighbor::decode(&wire)
        else {
            panic!("bad reply")
        };
        assert_eq!(reply_id, id);
        Some(decoded)
    };
    assert!(send_get(&actor, NonZeroU64::MAX, None, None, now).is_none());
    assert!(send_get(&actor, cut.interest, None, None, cut.fresh_until).is_none());
    let get = |actor: &ControllerActor, after, revision| {
        send_get(actor, cut.interest, after, revision, now).unwrap()
    };
    let hidden = crate::control::native::fixture::audio_track(crate::track::TrackMeta {
        id: visible.meta().connection_id.fixture_publication_at(
            crate::entity::TrackKind::Audio,
            "hidden",
            NonZeroU64::MIN,
        ),
        track_id: Some(crate::entity::TrackId::new("hidden").unwrap()),
        ..visible.meta().clone()
    });
    actor.handle_shard_event((ShardId::new(0), ShardEvent::TrackPublished(hidden)));
    let owner::NeighborWindow::Row {
        revision,
        candidate,
    } = get(&actor, None, None).unwrap()
    else {
        panic!("authorized remote row was absent");
    };
    let entry = candidate.entry.unwrap();
    assert_eq!(entry.tracks.len(), 1);
    assert_eq!(entry.tracks[0].publication_id, visible.id().as_str());
    assert!(matches!(
        get(&actor, Some(&candidate.canonical), Some(revision)),
        Ok(owner::NeighborWindow::Exhausted { .. })
    ));
    let stale = owner::Revision {
        presence: revision.presence.saturating_add(1),
        ..revision
    };
    assert!(matches!(
        get(&actor, None, Some(stale)),
        Err(pulsebeam_proto::state::FailureCode::ResourceGone)
    ));
}

async fn cross(
    from: &mut pulsebeam_link::StreamControl,
    to: &mut pulsebeam_link::StreamControl,
    hold_reply: bool,
) -> Option<Box<[u8]>> {
    let mut held = None;
    while let Some(Some((frame, _))) =
        futures_lite::future::poll_once(from.directions().0.recv()).await
    {
        if hold_reply
            && matches!(
                neighbor::decode(&frame),
                Some(neighbor::MessageOnStream::Reply(..))
            )
        {
            assert!(held.replace(frame).is_none());
        } else {
            to.directions().1.send(frame).await.unwrap();
        }
    }
    held
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn regional_get_merges_filtered_native_pages_and_fences_a_replaced_lease() {
    use crate::cluster::{REGIONAL_FRAME_BYTES, declarations::Role};
    use pulsebeam_core::identity::NodeId;
    use pulsebeam_link::StreamControl;
    use pulsebeam_runtime::time::{Instant, sleep};
    use std::num::{NonZeroU64, NonZeroUsize};

    let (mut viewer, _viewer_receivers) = actor_with_receivers(1);
    let (mut owner, _owner_receivers) = actor_with_receivers(1);
    let project = *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID;
    let room = ProjectRoomKey {
        project,
        room: RoomId::new("catalog-source").unwrap(),
    };
    let (participant, connection, mut lease, mut endpoint) = register_viewer(&mut viewer, project);
    lease.revision = 2;
    lease.permissions = ResourcePermissions::Grants {
        joined_room: Some(RoomId::new("viewer-room").unwrap()),
        grants: serde_json::from_value(serde_json::json!([
            {"rooms":["viewer-room"],"allow":["join"]},
            {"rooms":["catalog-source"],"allow":["discover"],"kind":"audio","label":"events"}
        ]))
        .unwrap(),
    };
    authorize(&mut viewer, participant, connection, &lease);
    let a = publish_catalog_audio_at(&mut owner, room, "a", 0, connection_id(80));
    let b = publish_catalog_audio_at(&mut viewer, room, "b", 0, connection_id(81));
    let c = publish_catalog_audio_at(&mut owner, room, "c", 1, connection_id(82));
    let hidden = crate::control::native::fixture::audio_track(crate::track::TrackMeta {
        id: a.meta().connection_id.fixture_publication_at(
            crate::entity::TrackKind::Audio,
            "private",
            NonZeroU64::MIN,
        ),
        track_id: Some(crate::entity::TrackId::new("private").unwrap()),
        ..a.meta().clone()
    });
    owner.handle_shard_event((ShardId::new(0), ShardEvent::TrackPublished(hidden)));
    let limit = NonZeroUsize::new(REGIONAL_FRAME_BYTES).unwrap();
    let (viewer_lane, mut viewer_stream) = StreamControl::new_authenticated(limit);
    let (owner_lane, mut owner_stream) = StreamControl::new_authenticated(limit);
    viewer.install_catalog_fixture(
        NodeId::new(),
        (Role::Edge, Role::Router),
        viewer_lane,
        Instant::now(),
    );
    owner.install_catalog_fixture(
        NodeId::new(),
        (Role::Router, Role::Edge),
        owner_lane,
        Instant::now(),
    );
    let cadence = Duration::from_millis(150);
    request_limit(&endpoint, 1, vec![], 2);
    let partial = receive_page(&mut viewer, &mut endpoint, 1, 1);
    assert!(partial.partial && partial.next_cursor.is_empty());
    assert_eq!(partial.entries[0].tracks[0].publication_id, b.id().as_str());

    let mut held = None;
    for _ in 0..12 {
        viewer.drive_provisioned_peers(Instant::now());
        cross(&mut viewer_stream, &mut owner_stream, false).await;
        owner.drive_provisioned_peers(Instant::now());
        if let Some(reply) = cross(&mut owner_stream, &mut viewer_stream, true).await {
            held = Some(reply);
            break;
        }
        service_catalog(&mut viewer);
        sleep(cadence).await;
    }
    let held = held.expect("owner must see the real GET");
    lease.revision = 3;
    viewer
        .core
        .registry
        .update_authorization(&participant, connection, lease.clone());
    let changed = viewer
        .native_connections
        .get_mut(&connection)
        .unwrap()
        .handler
        .model
        .authorization(i64::MAX, lease.revision, lease.permissions().clone())
        .unwrap();
    assert!(!changed, "renewal preserves the Discover filter");
    viewer_stream.directions().1.send(held).await.unwrap();
    sleep(cadence).await;
    viewer.drive_provisioned_peers(Instant::now());
    let replaced = receive_page(&mut viewer, &mut endpoint, 1, partial.revision + 1);
    assert!(replaced.partial && replaced.next_cursor.is_empty());
    assert_eq!(
        replaced.entries[0].tracks[0].publication_id,
        b.id().as_str()
    );

    for _ in 0..14 {
        sleep(cadence).await;
        viewer.drive_provisioned_peers(Instant::now());
        cross(&mut viewer_stream, &mut owner_stream, false).await;
        owner.drive_provisioned_peers(Instant::now());
        cross(&mut owner_stream, &mut viewer_stream, false).await;
        viewer.service_native_connections();
        service_catalog(&mut viewer);
    }
    let page = receive_page(&mut viewer, &mut endpoint, 1, replaced.revision + 1);
    assert!(!page.partial && !page.next_cursor.is_empty());
    let accepted: Vec<_> = page
        .entries
        .iter()
        .map(|row| {
            assert_eq!(row.tracks.len(), 1);
            row.tracks[0].publication_id.as_str()
        })
        .collect();
    assert_eq!(accepted, [a.id().as_str(), b.id().as_str()]);
    request_limit(&endpoint, 2, page.next_cursor, 2);
    for _ in 0..10 {
        viewer.service_native_connections();
        service_catalog(&mut viewer);
        sleep(cadence).await;
        viewer.drive_provisioned_peers(Instant::now());
        cross(&mut viewer_stream, &mut owner_stream, false).await;
        owner.drive_provisioned_peers(Instant::now());
        cross(&mut owner_stream, &mut viewer_stream, false).await;
    }
    let continued = receive_page(&mut viewer, &mut endpoint, 2, 1);
    assert!(!continued.partial && continued.next_cursor.is_empty());
    assert_eq!(continued.entries.len(), 1);
    assert_eq!(
        continued.entries[0].tracks[0].publication_id,
        c.id().as_str()
    );
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn regional_merge_revalidates_a_later_head_across_window_expiry() {
    use crate::cluster::{REGIONAL_FRAME_BYTES, declarations::Role};
    use pulsebeam_core::identity::NodeId;
    use pulsebeam_link::StreamControl;
    use pulsebeam_runtime::time::{Instant, sleep};
    use std::num::NonZeroUsize;

    fn connect(
        viewer: &mut ControllerActor,
        owner: &mut ControllerActor,
    ) -> (StreamControl, StreamControl) {
        let limit = NonZeroUsize::new(REGIONAL_FRAME_BYTES).unwrap();
        let (viewer_lane, viewer_stream) = StreamControl::new_authenticated(limit);
        let (owner_lane, owner_stream) = StreamControl::new_authenticated(limit);
        let now = Instant::now();
        viewer.install_catalog_fixture(NodeId::new(), (Role::Edge, Role::Router), viewer_lane, now);
        owner.install_catalog_fixture(NodeId::new(), (Role::Router, Role::Edge), owner_lane, now);
        (viewer_stream, owner_stream)
    }

    let (mut viewer, _viewer_receivers) = actor_with_receivers(1);
    let (mut earlier, _earlier_receivers) = actor_with_receivers(1);
    let (mut later, _later_receivers) = actor_with_receivers(1);
    let project = *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID;
    let room = ProjectRoomKey {
        project,
        room: RoomId::new("catalog-source").unwrap(),
    };
    let (_, _, _, mut endpoint) = register_viewer(&mut viewer, project);
    for index in 0..101 {
        publish_catalog_audio_at(
            &mut earlier,
            room,
            &format!("a{index:03}"),
            usize::from(index),
            connection_id(index + 1),
        );
    }
    publish_catalog_audio_at(&mut later, room, "z", 0, connection_id(150));
    let (mut viewer_to_earlier, mut earlier_stream) = connect(&mut viewer, &mut earlier);
    let (mut viewer_to_later, mut later_stream) = connect(&mut viewer, &mut later);
    request_limit(&endpoint, 1, vec![], 100);
    let partial = receive_page(&mut viewer, &mut endpoint, 1, 1);
    assert!(partial.partial && partial.next_cursor.is_empty());
    let cadence = Duration::from_millis(150);
    let started = Instant::now();
    let mut reader = PageReader::new(1, partial.revision + 1, None);
    let mut first = None;
    let mut continuation = None;
    for _ in 0..240 {
        sleep(cadence).await;
        let now = Instant::now();
        viewer.drive_provisioned_peers(now);
        cross(&mut viewer_to_earlier, &mut earlier_stream, false).await;
        cross(&mut viewer_to_later, &mut later_stream, false).await;
        earlier.drive_provisioned_peers(now);
        later.drive_provisioned_peers(now);
        cross(&mut earlier_stream, &mut viewer_to_earlier, false).await;
        cross(&mut later_stream, &mut viewer_to_later, false).await;
        viewer.service_native_connections();
        service_catalog(&mut viewer);
        let Some(page) = reader.drain(&mut viewer, &mut endpoint) else {
            continue;
        };
        if page.partial {
            assert!(first.is_none() && page.entries.is_empty() && page.next_cursor.is_empty());
        } else if first.is_none() {
            assert!(now.duration_since(started) > Duration::from_secs(10));
            assert!(!page.next_cursor.is_empty());
            request_limit(&endpoint, 2, page.next_cursor.clone(), 100);
            first = Some(page);
            reader = PageReader::new(2, 1, None);
        } else {
            continuation = Some(page);
            break;
        }
    }
    let page = first.expect("healthy responders must complete the large page");
    let participants: Vec<_> = page
        .entries
        .iter()
        .map(|entry| entry.participant.as_ref().unwrap().participant_id.clone())
        .collect();
    assert_eq!(
        participants,
        (0..100).map(|i| format!("a{i:03}")).collect::<Vec<_>>()
    );
    let continued = continuation.expect("the same owner cut must accept a continuation");
    assert!(continued.next_cursor.is_empty());
    assert_eq!(
        continued
            .entries
            .iter()
            .map(|entry| entry.participant.as_ref().unwrap().participant_id.as_str())
            .collect::<Vec<_>>(),
        ["a100", "z"]
    );
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn unavailable_configured_catalog_owner_is_not_covered_by_a_gateway() {
    use crate::cluster::{
        RegionTree,
        declarations::Role,
        mesh::{LinkCost, Member, Port},
    };
    use pulsebeam_core::identity::NodeId;
    use std::num::NonZeroU64;

    let (mut actor, _receivers) = actor_with_receivers(1);
    let member = |node| Member {
        node,
        generation: NonZeroU64::MIN,
    };
    let (local, gateway, owner) = (NodeId::new(), NodeId::new(), NodeId::new());
    let tree: RegionTree = serde_json::from_value(serde_json::json!({
        "regions": [
            {"name": "parent", "members": [
                {"node": gateway, "generation": 1, "role": "gateway"},
                {"node": owner, "generation": 1, "role": "router", "catalog": true}
            ]},
            {"name": "child", "parent": "parent", "members": [
                {"node": local, "generation": 1, "role": "gateway"}
            ]}
        ],
        "borders": [{"child": local, "parent": gateway}]
    }))
    .unwrap();
    actor.set_region_tree(
        member(local),
        tree,
        pulsebeam_runtime::time::Instant::now(),
        None,
    );
    let (_sender, sessions) = pulsebeam_runtime::sync::mpsc::channel(1);
    actor
        .add_regional_border(
            member(gateway),
            Port {
                sessions,
                roles: (Role::Gateway, Role::Gateway),
                #[cfg(not(feature = "sim"))]
                cost: LinkCost::Fixed(std::num::NonZeroU16::MIN),
                #[cfg(feature = "sim")]
                cost: LinkCost::Sim(crate::cluster::SimLinkCost::new(std::num::NonZeroU16::MIN)),
            },
        )
        .unwrap();
    assert!(actor.receipt_trust.is_none());
    assert!(actor.provisioned_peers.is_empty());

    let project = *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID;
    let (_, _, _, mut endpoint) = register_viewer(&mut actor, project);
    request(&endpoint, 1, Vec::new());
    let page = receive_page_driven(&mut actor, &mut endpoint, 1, 1, None, |actor| {
        let mut catalog = std::mem::take(&mut actor.catalog);
        catalog.service(actor);
        actor.catalog = catalog;
    });
    assert!(page.partial && page.entries.is_empty() && page.next_cursor.is_empty());
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn sibling_catalog_requeries_through_parent_and_fails_over_gateways() {
    use crate::cluster::{
        REGIONAL_FRAME_BYTES, RegionTree,
        declarations::Role,
        mesh::{LinkCost, Member, Port},
    };
    use pulsebeam_core::identity::NodeId;
    use pulsebeam_link::StreamControl;
    use pulsebeam_runtime::time::{Instant, sleep};
    use std::num::{NonZeroU64, NonZeroUsize};

    let [west, east, first, second] = std::array::from_fn(|_| NodeId::new());
    let member = |node| Member {
        node,
        generation: NonZeroU64::MIN,
    };
    let gateway = |node, catalog| serde_json::json!({"node": node, "generation": 1, "role": "gateway", "catalog": catalog});
    let tree: RegionTree = serde_json::from_value(serde_json::json!({
        "regions": [
            {"name": "parent", "members": [gateway(first, false), gateway(second, false)]},
            {"name": "west", "parent": "parent", "members": [gateway(west, false)]},
            {"name": "east", "parent": "parent", "members": [gateway(east, true)]}
        ],
        "borders": [
            {"child": west, "parent": first}, {"child": east, "parent": first},
            {"child": west, "parent": second}, {"child": east, "parent": second}
        ]
    }))
    .unwrap();
    let epoch = Instant::now();
    let make = |node| {
        let (mut actor, receivers) = actor_with_receivers(1);
        actor.set_region_tree(member(node), tree.clone(), epoch, None);
        (actor, receivers)
    };
    let (mut viewer, _viewer_rx) = make(west);
    let (mut owner, _owner_rx) = make(east);
    let (mut parent, _parent_rx) = make(first);
    let (mut standby, _standby_rx) = make(second);
    let link = |a: (&mut ControllerActor, NodeId), b: (&mut ControllerActor, NodeId)| {
        let attach = |(actor, _): (&mut ControllerActor, NodeId), peer| {
            let (lane, stream) =
                StreamControl::new_authenticated(NonZeroUsize::new(REGIONAL_FRAME_BYTES).unwrap());
            let (sender, sessions) = pulsebeam_runtime::sync::mpsc::channel(1);
            assert!(sender.try_send(lane).is_ok());
            actor
                .add_regional_border(
                    member(peer),
                    Port {
                        sessions,
                        roles: (Role::Gateway, Role::Gateway),
                        #[cfg(not(feature = "sim"))]
                        cost: LinkCost::Fixed(std::num::NonZeroU16::MIN),
                        #[cfg(feature = "sim")]
                        cost: LinkCost::Sim(crate::cluster::SimLinkCost::new(
                            std::num::NonZeroU16::MIN,
                        )),
                    },
                )
                .unwrap();
            stream
        };
        let (a_node, b_node) = (a.1, b.1);
        (attach(a, b_node), attach(b, a_node))
    };
    let project = *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID;
    let room = ProjectRoomKey {
        project,
        room: RoomId::new("catalog-source").unwrap(),
    };
    let (participant, connection, mut lease, mut endpoint) = register_viewer(&mut viewer, project);
    lease.revision = 2;
    lease.permissions = ResourcePermissions::Grants {
        joined_room: Some(RoomId::new("viewer-room").unwrap()),
        grants: serde_json::from_value(serde_json::json!([
            {"rooms":["viewer-room"],"allow":["join"]},
            {"rooms":["catalog-source"],"allow":["discover"]}
        ]))
        .unwrap(),
    };
    authorize(&mut viewer, participant, connection, &lease);
    request(&endpoint, 1, Vec::new());
    let partial = receive_page(&mut viewer, &mut endpoint, 1, 1);
    assert!(partial.partial && partial.next_cursor.is_empty());

    async fn pump(
        actors: [&mut ControllerActor; 3],
        routes: &mut [(StreamControl, StreamControl); 2],
    ) {
        for actor in actors {
            actor.drive_regional_routing(Instant::now());
        }
        for (left, right) in routes {
            cross(left, right, false).await;
            cross(right, left, false).await;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let mut routes = [
        link((&mut viewer, west), (&mut parent, first)),
        link((&mut parent, first), (&mut owner, east)),
    ];
    let mut pages = PageReader::new(1, partial.revision + 1, None);
    let mut converge = |viewer: &mut ControllerActor,
                        endpoint: &mut handoff::EndpointHandoff,
                        pages: &mut PageReader,
                        want_partial,
                        want_rows| {
        service_catalog(viewer);
        pages
            .drain(viewer, endpoint)
            .filter(|page| page.partial == want_partial && page.entries.len() == want_rows)
    };
    let mut eventual = None;
    for _ in 0..70 {
        pump([&mut viewer, &mut parent, &mut owner], &mut routes).await;
        eventual = converge(&mut viewer, &mut endpoint, &mut pages, false, 0);
        if eventual.is_some() {
            break;
        }
    }
    assert!(
        eventual.is_some(),
        "an explicit empty owner read must complete the cut"
    );
    let track = publish_catalog_audio(&mut owner, room);
    for _ in 0..100 {
        pump([&mut viewer, &mut parent, &mut owner], &mut routes).await;
        eventual = converge(&mut viewer, &mut endpoint, &mut pages, false, 1);
        if eventual.is_some() {
            break;
        }
    }
    assert_eq!(
        eventual.unwrap().entries[0].tracks[0].publication_id,
        track.id().as_str()
    );

    drop(routes);
    sleep(Duration::from_secs(6)).await;
    let mut fallback = converge(&mut viewer, &mut endpoint, &mut pages, true, 0);
    for _ in 0..5 {
        if fallback.is_some() {
            break;
        }
        fallback = converge(&mut viewer, &mut endpoint, &mut pages, true, 0);
    }
    assert!(fallback.is_some_and(|page| page.next_cursor.is_empty()));
    let mut alternate = [
        link((&mut viewer, west), (&mut standby, second)),
        link((&mut standby, second), (&mut owner, east)),
    ];
    let mut recovered = None;
    for _ in 0..100 {
        pump([&mut viewer, &mut standby, &mut owner], &mut alternate).await;
        recovered = converge(&mut viewer, &mut endpoint, &mut pages, false, 1);
        if recovered.is_some() {
            break;
        }
    }
    assert_eq!(
        recovered.unwrap().entries[0].tracks[0].publication_id,
        track.id().as_str()
    );
    assert!(
        viewer.receipt_trust.is_none()
            && parent.receipt_trust.is_none()
            && standby.receipt_trust.is_none()
    );
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn watch_reconciles_delayed_scoped_readiness_and_expired_coverage() {
    use crate::control::controller::catalog::{CatalogChange, CurrentFuture, owner, responders};
    use pulsebeam_core::identity::NodeId;
    use pulsebeam_runtime::time::Instant;
    use std::num::NonZeroU64;

    let (mut actor, _receivers) = actor_with_receivers(1);
    let (mut owner_actor, _owner_receivers) = actor_with_receivers(1);
    let project = *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID;
    let (_, connection, lease, mut endpoint) = register_viewer(&mut actor, project);
    let read_scope = responders::ReadScope {
        room: ProjectRoomKey {
            project,
            room: RoomId::new("catalog-source").unwrap(),
        },
        viewer: connection,
        authorization: lease.revision,
    };
    let track = publish_catalog_audio(&mut owner_actor, read_scope.room);
    let remote =
        owner::RoomQuery::authorized(&owner_actor, read_scope, lease.permissions()).unwrap();
    let peer = (NodeId::new(), NonZeroU64::MIN);
    use responders::Observation::Ready;
    let drive = |actor: &mut ControllerActor, observations: &[responders::Observation<'_>]| {
        let mut catalog = std::mem::take(&mut actor.catalog);
        catalog.service_with_coverage(actor, &[peer], observations);
        actor.catalog = catalog;
    };

    request(&endpoint, 1, Vec::new());
    let partial = receive_page_driven(&mut actor, &mut endpoint, 1, 1, None, |actor| {
        drive(actor, &[]);
    });
    assert!(partial.partial && partial.entries.is_empty() && partial.next_cursor.is_empty());
    let interest = actor.catalog.current_future_interests().next().unwrap();
    assert!(
        interest
            == (CurrentFuture {
                watch_id: 1,
                scope: read_scope
            })
    );
    assert_eq!(actor.catalog.current_future_interests().count(), 1);
    drive(&mut actor, &[]);
    assert!(!actor.catalog.has_work());

    let interval = Duration::from_secs(5);
    let reply = responders::Reply {
        peer,
        declaration: responders::DeclarationId {
            stream: NonZeroU64::MIN,
            interest: NonZeroU64::MIN,
            changed_at: NonZeroU64::MIN,
        },
        fresh_until: Instant::now().checked_add(interval * 2).unwrap(),
        view: &remote,
    };
    // No change notification: the periodic reconciliation must find readiness.
    pulsebeam_runtime::time::sleep(interval).await;
    let complete = receive_page_driven(
        &mut actor,
        &mut endpoint,
        1,
        partial.revision + 1,
        None,
        |actor| drive(actor, &[Ready(reply)]),
    );
    assert!(!complete.partial);
    assert!(
        matches!(complete.entries.as_slice(), [entry] if entry.tracks.iter().any(|visible| visible.publication_id == track.id().as_str()))
    );

    pulsebeam_runtime::time::sleep(interval).await;
    let expired = receive_page_driven(
        &mut actor,
        &mut endpoint,
        1,
        complete.revision + 1,
        None,
        |actor| drive(actor, &[Ready(reply)]),
    );
    assert!(expired.partial && expired.entries.is_empty() && expired.next_cursor.is_empty());

    drive(&mut actor, &[Ready(reply)]);
    assert!(!actor.catalog.has_work());
    let stale_interest = CurrentFuture {
        scope: responders::ReadScope {
            authorization: read_scope.authorization + 1,
            ..read_scope
        },
        ..interest
    };
    assert!(!actor.catalog.notify(stale_interest, CatalogChange::Gap));
    assert!(!actor.catalog.has_work());
    assert!(actor.catalog.notify(interest, CatalogChange::Gap));
    assert!(actor.catalog.notify(interest, CatalogChange::Changed));
    assert!(actor.catalog.has_work());
    let renewed = responders::Reply {
        declaration: responders::DeclarationId {
            changed_at: NonZeroU64::new(2).unwrap(),
            ..reply.declaration
        },
        fresh_until: Instant::now().checked_add(interval).unwrap(),
        ..reply
    };
    drive(&mut actor, &[Ready(renewed)]);
    assert!(!actor.catalog.notify(interest, CatalogChange::Changed));
    assert_eq!(actor.catalog.current_future_interests().count(), 0);
    assert!(
        !actor.native_connections[&connection]
            .handler
            .model
            .catalog
            .contains(1)
    );
    request(&endpoint, 2, Vec::new());
    let recovered = receive_page_driven(&mut actor, &mut endpoint, 2, 1, Some(1), |actor| {
        drive(actor, &[Ready(renewed)]);
    });
    assert!(!recovered.partial);
    assert_eq!(recovered.entries, complete.entries);
}

#[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
async fn live_catalog_uses_bounded_ordered_presence_pages_and_independent_watches() {
    let (mut actor, _receivers) = actor_with_receivers(1);
    // Match the legacy project used by the Native unit-test source-room resolver.
    let project = crate::entity::ProjectId::from_bytes([
        0x01, 0x8f, 0x4f, 0x7c, 0x20, 0x00, 0x70, 0x00, 0x80, 0x00, 0, 0, 0, 0, 0, 1,
    ]);
    let source_room = RoomId::new("catalog-source").unwrap();
    let mut ordered = Vec::new();
    for index in 1..=80u8 {
        let external = crate::entity::ParticipantId::new(&format!("presence-{index}")).unwrap();
        let participant = external;
        let transport = actor
            .core
            .reserve_transport(ShardId::new(0), pulsebeam_runtime::time::Instant::now());
        actor
            .core
            .registry
            .commit_candidate(
                participant,
                ProjectRoomKey {
                    project,
                    room: source_room,
                },
                ShardId::new(0),
                usize::from(index).saturating_sub(1),
                transport,
                connection_id(index),
                SignalingProtocol::State,
            )
            .unwrap();
        ordered.push(participant.as_str().to_owned());
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
    let track = crate::control::native::fixture::audio_track(crate::track::TrackMeta {
        room: crate::entity::ProjectRoomKey {
            project,
            room: source_room,
        },
        id: publisher_connection.fixture_publication_at(
            crate::entity::TrackKind::Audio,
            "events",
            std::num::NonZeroU64::new(1).unwrap(),
        ),
        origin: publisher,
        connection_id: publisher_connection,
        track_id: Some(crate::entity::TrackId::new("events").unwrap()),
    });
    let (participant, connection, mut lease, mut endpoint) = register_viewer(&mut actor, project);
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
    actor.handle_shard_event((ShardId::new(0), ShardEvent::TrackPublished(track.clone())));
    let first = receive_page(&mut actor, &mut endpoint, 1, 2);
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
            .any(|visible| visible.publication_id == track.id().as_str())
    }));
    assert!(!second.has_more);
    assert!(second.next_cursor.is_empty());
    install_receiver(&mut actor, &mut endpoint, &track);
    pulsebeam_runtime::time::sleep(Duration::from_secs(5)).await;
    let reconciled = receive_page(&mut actor, &mut endpoint, 1, first.revision + 1);
    assert_eq!(reconciled.entries, first.entries);
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
    let changed = receive_page_with_reset(
        &mut actor,
        &mut endpoint,
        1,
        reconciled.revision + 1,
        Some(2),
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
    request(&endpoint, 3, changed.next_cursor.clone());
    receive_page(&mut actor, &mut endpoint, 3, 1);
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
                assert_eq!(error.watch_id, 3);
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
            .contains(3)
    );

    lease.revision = 3;
    lease.permissions = source_permissions(&["discover", "subscribe"]);
    authorize(&mut actor, participant, connection, &lease);
    request(&endpoint, 4, Vec::new());
    let first_restored = receive_page(&mut actor, &mut endpoint, 4, 1);
    request(&endpoint, 5, first_restored.next_cursor.clone());
    let restored = receive_page(&mut actor, &mut endpoint, 5, 1);
    assert!(restored.entries.iter().any(|entry| {
        entry
            .tracks
            .iter()
            .any(|visible| visible.publication_id == track.id().as_str())
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
    pulsebeam_runtime::time::sleep(Duration::from_secs(5)).await;
    let updated = receive_page_with_reset(
        &mut actor,
        &mut endpoint,
        4,
        first_restored.revision + 1,
        Some(5),
    );
    assert_eq!(updated.entries, first_restored.entries);
    assert!(endpoint.command().is_none());
}

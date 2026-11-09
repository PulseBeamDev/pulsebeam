#![allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "simulation fixture helpers fail immediately on invalid setup or missing progress"
)]

use super::*;
use crate::clock::WallAnchor;
use crate::link::{HostHealth, host::Host};
use crate::participant::{RoutedTrackPacket, TrackPacket};
use crate::route::{MediaDelivery, RouteId};
use crate::shard::core::ShardCore;
use crate::shard_update::{ShardUpdateOp, TrackRouteAction, TrackRuntime};
use crate::track::DataLane;
use pulsebeam_link::{
    LinkError, LinkRx, LinkTime, NetworkClass, NetworkId, NetworkPeer, PeerRole, SecretCredential,
    SendErrorKind,
};
use pulsebeam_runtime::sync::CancellationToken;
use pulsebeam_runtime::time::Duration;

async fn health(links: &Links) -> HostHealth {
    for _ in 0..100 {
        let (reply, result) = oneshot::channel();
        links.inspect(reply);
        match result.await.unwrap() {
            Ok(health) => return health,
            Err(LinkError::Capacity) => {
                pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
            }
            result => panic!("unexpected host health: {result:?}"),
        }
    }
    panic!("host command queue failed to drain");
}

async fn received(links: &Links, peer: NetworkId, count: u64) -> pulsebeam_link::io::Health {
    for _ in 0..1000 {
        let snapshot = health(links).await;
        let peer = snapshot
            .peers
            .iter()
            .find(|entry| entry.peer == peer)
            .unwrap();
        assert!(peer.failure.is_none());
        if peer.transport.received >= count {
            return peer.transport;
        }
        pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("authenticated packet did not arrive");
}

async fn side(
    origin: Instant,
    count: usize,
    shutdown: CancellationToken,
) -> (
    Links,
    Vec<LinkRx<ShardFrame>>,
    pulsebeam_runtime::task::JoinHandle<std::io::Result<()>>,
) {
    let mut control = LinkControl::new(move || {
        LinkTime::from_nanos(u64::try_from(origin.elapsed().as_nanos()).unwrap())
    })
    .unwrap();
    let mut ports = Vec::new();
    let mut receivers = Vec::new();
    for _ in 0..count {
        let (port, receiver) = control.create_port().unwrap();
        ports.push(port);
        receivers.push(receiver);
    }
    let (host, sender) = Host::bind("127.0.0.1:0".parse().unwrap(), origin)
        .await
        .unwrap();
    let mut links = Links::new(control, ports, origin);
    links.set_host(sender);
    (
        links,
        receivers,
        pulsebeam_runtime::task::spawn_local(host.run(shutdown)),
    )
}

fn payload(packet: &ShardFrame) -> &[u8] {
    let ShardFrame::Media { payload, .. } = packet else {
        panic!("expected media frame");
    };
    let TrackPacket::Data { bytes, .. } = &payload.packet else {
        panic!("expected data publication");
    };
    bytes
}

#[test]
fn udp_host_fanout_saturation_and_cutoffs_preserve_delivery_boundaries() {
    pulsebeam_runtime::testing::run_local(
        pulsebeam_runtime::testing::test_host_ip("192.168.250.104"),
        async {
            pulsebeam_runtime::time::timeout(Duration::from_secs(6), async {
                let shutdown = CancellationToken::new();
                let _cleanup = shutdown.clone().drop_guard();
                let origin = Instant::now();
                let (mut a, _arx, a_task) = side(origin, 0, shutdown.child_token()).await;
                let (mut b, mut receivers, b_task) = side(origin, 3, shutdown.child_token()).await;
                let aa = health(&a).await.local_addr;
                let ba = health(&b).await.local_addr;
                let ca = SecretCredential::generate().unwrap();
                let cb = SecretCredential::generate().unwrap();
                let aid = a
                    .open_peer(NetworkPeer {
                        endpoint: ba,
                        credential: ca.clone(),
                        expected_peer: cb.fingerprint(),
                        role: PeerRole::Initiator,
                        class: NetworkClass::Lan,
                    })
                    .unwrap();
                let bid = b
                    .open_peer(NetworkPeer {
                        endpoint: aa,
                        credential: cb,
                        expected_peer: ca.fingerprint(),
                        role: PeerRole::Responder,
                        class: NetworkClass::Wan,
                    })
                    .unwrap();
                let mut connected = false;
                for _ in 0..4000 {
                    let ah = health(&a).await;
                    let bh = health(&b).await;
                    if ah
                        .peers
                        .iter()
                        .any(|peer| peer.peer == aid && peer.transport.ready)
                        && bh
                            .peers
                            .iter()
                            .any(|peer| peer.peer == bid && peer.transport.ready)
                    {
                        connected = true;
                        break;
                    }
                    assert!(ah.peers.iter().all(|peer| peer.failure.is_none()));
                    assert!(bh.peers.iter().all(|peer| peer.failure.is_none()));
                    pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
                }
                assert!(connected);
                let track = crate::entity::ConnectionId::from_bytes([8; 16])
                    .derive_track_id(crate::entity::TrackKind::Data, "fanout");
                let mut destinations = Vec::new();
                let mut shards = Vec::new();
                for index in 0..2 {
                    let shard = ShardId::new(index);
                    let address = NodeRouteAddress::new(RouteId::new(shard, 7), 3);
                    let (mut writer, updates) = crate::shard_update::new_shard_update(shard);
                    let mut core = ShardCore::new(
                        shard,
                        1,
                        3,
                        WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, origin),
                        updates,
                    );
                    writer.stage(
                        1,
                        ShardUpdateOp::InsertTrackRuntime {
                            track_id: track,
                            runtime: TrackRuntime::default(),
                        },
                    );
                    writer.stage(
                        1,
                        ShardUpdateOp::InstallRoute {
                            address,
                            action: TrackRouteAction::Forward { track_id: track },
                        },
                    );
                    b.install_after(
                        address,
                        TrackRouteAction::Forward { track_id: track },
                        writer.stage_cutoff(1),
                    );
                    assert_eq!(writer.publish(), Some(1));
                    assert_eq!(core.apply_updates(64), 1);
                    destinations.push(address);
                    shards.push((writer, core));
                }
                let published = b.destinations(track);
                assert_eq!(
                    published
                        .iter()
                        .map(|destination| destination.address)
                        .collect::<Vec<_>>(),
                    destinations
                );
                assert!(
                    published
                        .iter()
                        .all(|destination| destination.kind == crate::link::ReceiveKind::Forward)
                );
                let receive = b.bind_receive(bid, destinations.clone()).unwrap();
                let source = ShardId::new(0);
                let mut plan = TrackPlanUpdate {
                    track_id: track,
                    plan: Some(TrackPlan::default()),
                };
                assert!(a.prepare_plan(source, &mut plan).is_empty());
                let (binding, mut plan) = a.bind_send(source, track, aid, receive.address).unwrap();
                assert!(a.prepare_plan(source, &mut plan).is_empty());
                let tx = plan.plan.as_ref().unwrap().compiled_network[0].tx.clone();
                let (mut source_writer, updates) = crate::shard_update::new_shard_update(source);
                let mut source_core = ShardCore::new(
                    source,
                    1,
                    1,
                    WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, origin),
                    updates,
                );
                source_writer.stage(
                    1,
                    ShardUpdateOp::InsertTrackRuntime {
                        track_id: track,
                        runtime: TrackRuntime::default(),
                    },
                );
                source_writer.stage_plans(1, vec![plan]);
                assert_eq!(source_writer.publish(), Some(1));
                assert_eq!(source_core.apply_updates(64), 1);
                let frame = |bytes: &'static [u8]| ShardFrame::Media {
                    delivery: MediaDelivery {
                        address: destinations[0],
                        playout_ntp32: 7,
                    },
                    payload: RoutedTrackPacket {
                        track_id: track,
                        packet: TrackPacket::Data {
                            lane: DataLane::Lossy,
                            bytes: bytes::Bytes::from_static(bytes),
                        },
                    },
                };
                tx.try_send(frame(b"first"), LinkTime::from_nanos(u64::MAX))
                    .unwrap();
                assert_eq!(received(&b, bid, 1).await.received, 1);
                let mut first = Vec::new();
                let mut second = Vec::new();
                assert_eq!(receivers[0].try_recv_batch(&mut first, 8), 1);
                assert_eq!(receivers[1].try_recv_batch(&mut second, 8), 1);
                assert_eq!(first[0].tag, crate::link::delivery_tag(destinations[0]));
                assert_eq!(second[0].tag, crate::link::delivery_tag(destinations[1]));
                assert_eq!(payload(&first[0].packet), b"first");
                assert_eq!(
                    payload(&first[0].packet).as_ptr(),
                    payload(&second[0].packet).as_ptr()
                );
                assert_eq!(first[0].received_at, second[0].received_at);
                assert!(first[0].received_at.wall.is_some());
                let mut bystander = Vec::new();
                assert_eq!(receivers[2].try_recv_batch(&mut bystander, 8), 0);

                let (_, local) = b
                    .control
                    .bind_send(Destination::Local {
                        port: b.ports[0],
                        tag: crate::link::delivery_tag(destinations[0]),
                    })
                    .unwrap();
                let mut full = false;
                for _ in 0..4096 {
                    match local.try_send(frame(b"local"), LinkTime::from_nanos(u64::MAX)) {
                        Ok(()) => {}
                        Err(error) if error.kind == SendErrorKind::Full => {
                            full = true;
                            break;
                        }
                        result => panic!("unexpected local enqueue: {result:?}"),
                    }
                }
                assert!(full);
                tx.try_send(frame(b"queued"), LinkTime::from_nanos(u64::MAX))
                    .unwrap();
                assert_eq!(received(&b, bid, 2).await.local_full, 1);
                let (writer, core) = &mut shards[1];
                writer.stage(
                    2,
                    ShardUpdateOp::RetireRoute {
                        address: destinations[1],
                    },
                );
                b.retire_after(destinations[1], writer.stage_cutoff(2));
                assert_eq!(writer.publish(), Some(2));
                assert_eq!(core.apply_updates(64), 1);
                b.reap();
                second.clear();
                assert_eq!(receivers[1].try_recv_batch(&mut second, 8), 0);
                assert_eq!(receivers[1].drop_counts().stale, 1);
                tx.try_send(frame(b"retired"), LinkTime::from_nanos(u64::MAX))
                    .unwrap();
                assert_eq!(received(&b, bid, 3).await.stale, 1);
                assert_eq!(receivers[2].try_recv_batch(&mut bystander, 8), 0);
                let mut acknowledged = false;
                for _ in 0..100 {
                    let snapshot = health(&a).await;
                    let peer = snapshot.peers.iter().find(|peer| peer.peer == aid).unwrap();
                    assert_eq!(peer.transport.sent, 3);
                    if peer.transport.bytes_in_flight == 0 {
                        acknowledged = true;
                        break;
                    }
                    pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
                }
                assert!(
                    acknowledged,
                    "local saturation or retirement withheld transport feedback"
                );

                let (_, mut clear) = a.remove_send(binding).unwrap();
                assert!(a.prepare_plan(source, &mut clear).is_empty());
                source_writer.stage_plans(2, vec![clear]);
                a.retire_bindings_after(vec![binding], source_writer.stage_cutoff(2));
                tx.try_send(frame(b"fenced"), LinkTime::from_nanos(u64::MAX))
                    .unwrap();
                assert_eq!(source_writer.publish(), Some(2));
                assert_eq!(source_core.apply_updates(64), 1);
                a.reap();
                assert_eq!(
                    tx.try_send(frame(b"stale"), LinkTime::from_nanos(u64::MAX))
                        .unwrap_err()
                        .kind,
                    SendErrorKind::Stale
                );
                let mut fenced = false;
                for _ in 0..100 {
                    let snapshot = health(&a).await;
                    let peer = snapshot.peers.iter().find(|peer| peer.peer == aid).unwrap();
                    assert_eq!(peer.transport.sent, 3);
                    if peer.transport.stale == 1 {
                        fenced = true;
                        break;
                    }
                    pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
                }
                assert!(
                    fenced,
                    "queued source work must be fenced before UDP submission"
                );
                assert_eq!(received(&b, bid, 3).await.received, 3);
                shutdown.cancel();
                a_task.await.unwrap().unwrap();
                b_task.await.unwrap().unwrap();
            })
            .await
            .expect("bounded virtual-time UDP fanout and retirement scenario");
        },
    );
}

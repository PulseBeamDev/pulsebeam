use std::num::NonZeroU64;

use super::*;
use crate::control::{
    controller::{
        ownership::wire,
        peer_ownership::{
            fabric_data, provisioned::RepairLookups, relay, transfer_fixture_snapshot,
        },
    },
    link::{FabricRelayPermit, LiveReceiveSpec, RouteScope},
};
use crate::entity::{ConnectionId, ProjectId, ProjectRoomKey, RoomId, TrackKind};
use crate::route::fabric::{LiveRelayChange, RelayRetirement, local::Emission};
use crate::shard::core::LiveNetworkFixture;
use pulsebeam_core::fabric::{
    Extension, ExtensionKind, FabricPacket, Header, MAJOR, Profile, REGISTRY_REVISION,
};
use pulsebeam_link::{Destination, FabricIngress, FabricPath, NodeRouteKey};

fn peer(
    endpoint: std::net::SocketAddr,
    credential: &SecretCredential,
    remote: &SecretCredential,
    role: PeerRole,
) -> pulsebeam_link::FabricPeer {
    NetworkPeer {
        endpoint,
        expected_peer: remote.fingerprint(),
        credential: credential.clone(),
        role,
        class: NetworkClass::Lan,
    }
    .with_fabric_path(FabricPath {
        udp_payload_limit: 1200,
        additional_overhead: 0,
    })
}

fn assert_relay_repair_rejects_stale_control(
    pending: &mut RepairLookups,
    lookup: fabric_data::RepairFrame,
    ready: fabric_data::RepairFrame,
    now: Instant,
) {
    use fabric_data::RepairAction;
    let RepairAction::Ready { sequence, label } = ready.action else {
        panic!("expected repair Ready");
    };
    let cancel = fabric_data::RepairFrame {
        action: RepairAction::Cancel,
        ..lookup
    };
    let mut reordered = RepairLookups::default();
    assert_eq!(reordered.cancel(cancel, now), None);
    reordered.lookup(lookup, now, || panic!("cancelled lookup cannot be staged"));
    let reject_handoff = |_, _| panic!("stale control cannot hand off repair");
    reordered.ready(ready, now, reject_handoff);
    let stale = fabric_data::RepairFrame {
        request: crate::route::fabric::local::RepairRequest {
            id: NonZeroU64::MAX,
            ..lookup.request
        },
        ..ready
    };
    pending.ready(stale, now, reject_handoff);
    pending.cancel(
        fabric_data::RepairFrame {
            action: RepairAction::Cancel,
            ..stale
        },
        now,
    );
    let wrong_sequence = fabric_data::RepairFrame {
        action: RepairAction::Ready {
            sequence: sequence.wrapping_add(1),
            label,
        },
        ..ready
    };
    pending.ready(wrong_sequence, now, reject_handoff);
}

fn install_repair_child(
    links: &mut Links,
    live: crate::link::ReceiveRoute,
    parent: crate::route::fabric::LiveNetworkChild,
    shard: &mut LiveNetworkFixture,
) -> (pulsebeam_link::BindingCutoff, NodeRouteKey) {
    use crate::route::fabric::{
        local::{RepairFragment, RepairRequest},
        network_repair::Interest,
    };
    use std::num::NonZeroU64;
    let expires = Instant::now() + Duration::from_secs(1);
    let receiver = shard.receiver_connection();
    let spec = |id| Interest {
        parent,
        receiver,
        branch: NonZeroU64::MIN,
        request: RepairRequest {
            fragment: RepairFragment {
                unit_seq: 3,
                index: 1,
            },
            id,
        },
        expires,
    };
    let mut wrong_parent = spec(NonZeroU64::MIN);
    wrong_parent.parent.incarnation = NonZeroU64::MAX;
    assert_eq!(
        links
            .stage_located_repair_receive(wrong_parent, 1, |_, _| panic!(
                "wrong source incarnation"
            ))
            .unwrap_err(),
        BindError::Closed,
    );

    let mut no_lease = links
        .stage_located_repair_receive(spec(NonZeroU64::MIN), 1, |child, lifetime| {
            shard.stage_repair_child(child, lifetime)
        })
        .unwrap();
    shard.apply();
    links.reap();
    assert_eq!(no_lease.try_recv(), Ok(Err(BindError::Closed)));

    let wrong_id = NonZeroU64::new(2).unwrap();
    let (wrong_tx, wrong_rx) = oneshot::channel();
    let mut wrong_cutoff = None;
    let mut refused = links
        .stage_located_repair_receive(spec(wrong_id), 1, |mut child, lifetime| {
            wrong_cutoff = Some(lifetime);
            child.demand.sequence = 2;
            wrong_tx.send(Some(child)).unwrap();
            wrong_rx
        })
        .unwrap();
    links.reap();
    assert_eq!(refused.try_recv(), Ok(Err(BindError::Closed)));
    assert!(!wrong_cutoff.unwrap().is_live());
    assert_eq!(
        links
            .stage_located_repair_receive(spec(NonZeroU64::MIN), 1, |_, _| panic!(
                "replayed demand"
            ))
            .unwrap_err(),
        BindError::Closed
    );

    let id = NonZeroU64::new(3).unwrap();
    let (ready_tx, ready_rx) = oneshot::channel();
    let mut staged = None;
    let mut ready = links
        .stage_located_repair_receive(spec(id), 1, |child, lifetime| {
            staged = Some((child, lifetime));
            ready_rx
        })
        .unwrap();
    let (child, cutoff) = staged.unwrap();
    assert_eq!(child.parent.key, live.address);
    assert_ne!(child.route.ingress.key, live.address);
    links.reap();
    assert_eq!(ready.try_recv(), Err(oneshot::error::TryRecvError::Empty));
    links.cancel_repair_receive(spec(wrong_id));
    assert!(cutoff.is_live());
    ready_tx.send(Some(child)).unwrap();
    links.reap();
    let accepted = ready.try_recv().unwrap().unwrap();
    assert_eq!(accepted.address, child.route.ingress.key);
    assert!(links.repair_receive_live(accepted, Instant::now()));
    links.cancel_repair_receive(spec(wrong_id));
    assert!(links.repair_receive_live(accepted, Instant::now()));

    let cancelled = spec(NonZeroU64::new(4).unwrap());
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let mut staged_cutoff = None;
    let mut cancelled_receipt = links
        .stage_located_repair_receive(cancelled, 1, |_, lifetime| {
            staged_cutoff = Some(lifetime);
            cancel_rx
        })
        .unwrap();
    links.cancel_repair_receive(cancelled);
    assert!(!staged_cutoff.unwrap().is_live());
    assert!(cancel_tx.send(None).is_err());
    assert_eq!(
        cancelled_receipt.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    );
    assert!(links.repair_receive_live(accepted, Instant::now()));
    assert!(
        links
            .fabric_offers(child.route.ingress.association)
            .into_iter()
            .all(|offer| {
                let decoded = relay::fabric::decode(Box::new([offer])).unwrap();
                decoded.iter().all(|route| route.key != accepted.address)
            })
    );
    (cutoff, accepted.address)
}

#[test]
fn fabric_host_waits_for_all_children_and_fences_the_association_route() {
    exercise_live_route(Profile::Opus, TrackKind::Audio, 48_000, false, &[0x55]);
}

#[test]
fn pt99_preserves_exact_originals_over_shared_routes_and_two_protected_hops() {
    exercise_live_route(
        Profile::Data,
        TrackKind::Data,
        3,
        true,
        &crate::participant::data_fabric::R4,
    );
}

fn exercise_live_route(
    profile: Profile,
    kind: TrackKind,
    timestamp: u32,
    marker: bool,
    payload: &'static [u8],
) {
    pulsebeam_runtime::testing::run_local(
        pulsebeam_runtime::testing::test_host_ip("192.168.250.106"),
        async move {
            pulsebeam_runtime::time::timeout(Duration::from_secs(6), async {
                let shutdown = CancellationToken::new();
                let _cleanup = shutdown.clone().drop_guard();
                let origin = Instant::now();
                let (mut a, _, a_task) = side(origin, 0, shutdown.child_token()).await;
                let (mut b, mut receivers, b_task) = side(origin, 2, shutdown.child_token()).await;
                let (mut c, mut leaf_receivers, c_task) =
                    side(origin, 1, shutdown.child_token()).await;
                let [left_rx, right_rx] = receivers.as_mut_slice() else {
                    panic!("two shared-route receivers")
                };
                let [leaf_rx] = leaf_receivers.as_mut_slice() else {
                    panic!("one leaf receiver")
                };
                let (aa, ba, ca) = (
                    health(&a).await.local_addr,
                    health(&b).await.local_addr,
                    health(&c).await.local_addr,
                );
                let ka = SecretCredential::generate().unwrap();
                let kb = SecretCredential::generate().unwrap();
                let kc = SecretCredential::generate().unwrap();
                let aid = a
                    .open_peer(peer(ba, &ka, &kb, PeerRole::Initiator))
                    .unwrap();
                let bid = b
                    .open_peer(peer(aa, &kb, &ka, PeerRole::Responder))
                    .unwrap();
                let _ = health(&b).await;
                let b_node = NodeId::new();
                let c_node = NodeId::new();
                let (cid, mut c_control) = c
                    .open_control_peer(peer(ba, &kc, &kb, PeerRole::Initiator), b_node)
                    .unwrap();
                let (other, mut b_control) = b
                    .open_control_peer(peer(ca, &kb, &kc, PeerRole::Responder), c_node)
                    .unwrap();
                loop {
                    if health(&b)
                        .await
                        .peers
                        .iter()
                        .filter(|peer| peer.transport.ready)
                        .count()
                        == 2
                        && health(&a)
                            .await
                            .peers
                            .iter()
                            .any(|peer| peer.transport.ready)
                        && health(&c)
                            .await
                            .peers
                            .iter()
                            .any(|peer| peer.transport.ready)
                    {
                        break;
                    }
                    pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
                }

                let publication =
                    ConnectionId::from_bytes([9; 16]).fixture_publication(kind, "fabric");
                let room = ProjectRoomKey {
                    project: ProjectId::from_bytes([2; 16]),
                    room: RoomId::new("fabric-room").unwrap(),
                };
                let addresses = [0, 1]
                    .map(|index| NodeRouteAddress::new(RouteId::new(ShardId::new(index), 7), 1));
                let [left_address, right_address] = addresses;
                let mut shards = addresses.map(|address| {
                    let mut shard = LiveNetworkFixture::new(address.shard(), publication, origin);
                    let receipt = shard.install_route(address);
                    b.install_after(
                        address,
                        TrackRouteAction::Forward {
                            publication_id: publication,
                        },
                        Some(RouteScope {
                            room,
                            ingress: Some(bid),
                        }),
                        receipt,
                    );
                    shard
                });
                let opus = profile == Profile::Opus;
                let kinds: &[_] = if opus {
                    &[ExtensionKind::AudioLevel]
                } else {
                    &[]
                };
                let emission = Emission::new(profile, kinds).unwrap();
                let spec = |peer, addresses| LiveReceiveSpec {
                    peer,
                    room,
                    publication,
                    incarnation: std::num::NonZeroU64::MIN,
                    sequence_anchor: 0,
                    major: MAJOR,
                    revision: if opus { REGISTRY_REVISION } else { 0 },
                    emission,
                    addresses,
                };
                if opus {
                    assert!(matches!(
                        b.stage_live_receive(
                            LiveReceiveSpec {
                                revision: 0,
                                ..spec(bid, addresses.to_vec())
                            },
                            |_, _| panic!("revision 0 must not stage audio-level children"),
                        ),
                        Err(BindError::UnsupportedProfile)
                    ));
                }
                if profile == Profile::Data {
                    let mut pending = None;
                    let mut cancelled = b
                        .stage_live_receive(spec(bid, vec![left_address]), |child, cutoff| {
                            let (tx, rx) = oneshot::channel();
                            pending = Some((tx, child, cutoff));
                            rx
                        })
                        .unwrap();
                    let (tx, child, cutoff) = pending.unwrap();
                    assert!(cutoff.is_live());
                    b.unbind_receive(cancelled.binding);
                    assert!(!cutoff.is_live());
                    assert!(tx.send(Some(child)).is_err());
                    assert_eq!(
                        cancelled.receipt.try_recv(),
                        Err(oneshot::error::TryRecvError::Closed)
                    );
                    assert!(b.fabric_offers(bid).is_empty());
                }
                let mut children = Vec::new();
                let mut result = b
                    .stage_live_receive(spec(bid, addresses.to_vec()), |child, lifetime| {
                        children.push(child);
                        shards
                            .get_mut(child.destination.shard().index())
                            .unwrap()
                            .stage_child(child, lifetime)
                    })
                    .unwrap()
                    .receipt;
                let &[left, right] = children.as_slice() else {
                    panic!("exactly two acknowledged children")
                };
                let [left_shard, right_shard] = &mut shards;
                assert_eq!(left.destination, left_address);
                assert_eq!(right.destination, right_address);
                assert_eq!(left.ingress, right.ingress);
                left_shard.apply();
                left_shard.select_branch(left_address);
                b.reap();
                assert!(matches!(
                    result.try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                ));

                let key = left.ingress.key;
                let (_, tx) = a
                    .control
                    .bind_send(Destination::Network {
                        peer: aid,
                        address: key,
                    })
                    .unwrap();
                let audio_level = [Extension {
                    kind: ExtensionKind::AudioLevel,
                    value: &[0x9e],
                }];
                let extensions = if opus { &audio_level[..] } else { &[] };
                let packet = FabricPacket::new(
                    Header {
                        profile,
                        marker,
                        sequence: 1,
                        timestamp,
                        ingress_ntp32: 0,
                    },
                    extensions,
                    Box::<[u8]>::from(payload),
                )
                .unwrap();
                let image = || ShardFrame::FabricImage(packet.clone());
                let send = |tx: &pulsebeam_link::LinkTx<ShardFrame>| {
                    tx.try_send(image(), LinkTime::from_nanos(u64::MAX))
                        .unwrap();
                };
                send(&tx);
                assert_eq!(received(&b, bid, 1).await.stale, 1);
                for rx in [&mut *left_rx, &mut *right_rx] {
                    assert_eq!(rx.try_recv_batch(&mut Vec::new(), 1), 0);
                }
                right_shard.apply();
                right_shard.select_branch(right_address);
                b.reap();
                let input = result.try_recv().unwrap().unwrap();
                assert_eq!(input.address, key);
                let repair_cutoff = (profile == Profile::Data)
                    .then(|| install_repair_child(&mut b, input, left, left_shard));

                let (_, wrong_association) = c
                    .control
                    .bind_send(Destination::Network {
                        peer: cid,
                        address: key,
                    })
                    .unwrap();
                send(&wrong_association);
                assert_eq!(received(&b, other, 1).await.stale, 1);
                let (_, wrong_key) = a
                    .control
                    .bind_send(Destination::Network {
                        peer: aid,
                        address: NodeRouteKey::from_bytes([3, 0, 0, 0]),
                    })
                    .unwrap();
                send(&wrong_key);
                assert_eq!(received(&b, bid, 2).await.stale, 2);
                for rx in [&mut *left_rx, &mut *right_rx] {
                    assert_eq!(rx.try_recv_batch(&mut Vec::new(), 1), 0);
                }

                let leaf_address = NodeRouteAddress::new(RouteId::new(ShardId::new(0), 9), 1);
                let mut leaf = LiveNetworkFixture::new(leaf_address.shard(), publication, origin);
                let spectator = (profile == Profile::Data).then(|| leaf.add_spectator());
                c.install_after(
                    leaf_address,
                    TrackRouteAction::Forward {
                        publication_id: publication,
                    },
                    Some(RouteScope {
                        room,
                        ingress: Some(cid),
                    }),
                    leaf.install_route(leaf_address),
                );
                let mut leaf_parent = None;
                let mut leaf_route = c
                    .stage_live_receive(spec(cid, vec![leaf_address]), |child, lifetime| {
                        leaf_parent = Some(child);
                        leaf.stage_child(child, lifetime)
                    })
                    .unwrap()
                    .receipt;
                assert!(c.fabric_offers(cid).is_empty());
                leaf.apply();
                leaf.select_branch(leaf_address);
                c.reap();
                let leaf_route = leaf_route.try_recv().unwrap().unwrap();
                let advertised = crate::control::controller::peer_ownership::relay::fabric::decode(
                    c.fabric_offers(cid).into_boxed_slice(),
                )
                .unwrap();
                let &[ack] = advertised.as_ref() else {
                    panic!("one leaf route acknowledgment")
                };
                assert_eq!(ack.key, leaf_route.address);
                assert_eq!(ack.emission, emission);
                if profile == Profile::Data {
                    use crate::control::controller::peer_ownership::fabric_data::{
                        Action, Frame, Scope,
                    };
                    use crate::route::fabric::local::DataExpectation;
                    let expectation = Frame {
                        scope: Scope {
                            key: ack.key,
                            source: ack.incarnation,
                        },
                        unit: DataExpectation {
                            unit_seq: 3,
                            fragment_count: 2,
                        },
                        action: Action::Expect { lease_ms: 500 },
                    };
                    b_control
                        .try_send(
                            expectation.encode().unwrap(),
                            LinkTime::from_nanos(u64::MAX),
                        )
                        .unwrap();
                    let protected = c_control.recv().await.unwrap();
                    let decoded = Frame::decode(&protected).unwrap();
                    assert_eq!(decoded, expectation);
                    let now = Instant::now();
                    assert!(c.peer_data_expectations(other, decoded, now).is_none());
                    let notices = c.peer_data_expectations(cid, decoded, now).unwrap();
                    let &[notice] = notices.as_slice() else {
                        panic!("one installed leaf")
                    };
                    leaf.seed_repairable_expectation(notice, now);
                    let mut replay = leaf.stage_data_expectation(notice);
                    leaf.apply();
                    assert_eq!(replay.try_recv(), Ok(true));
                    leaf.exercise_repair_interest(notice, Instant::now());
                }
                let now = Instant::now();
                let expires = now.checked_add(Duration::from_millis(90)).unwrap();
                assert!(
                    b.compile_fabric_relay(input.binding, left_address, other, ack, now, now)
                        .is_err()
                );
                assert!(
                    b.compile_fabric_relay(input.binding, left_address, bid, ack, expires, now)
                        .is_err()
                );
                assert!(
                    b.compile_fabric_relay(
                        input.binding,
                        left_address,
                        other,
                        crate::control::controller::peer_ownership::relay::fabric::LiveRouteAck {
                            incarnation: std::num::NonZeroU64::new(2).unwrap(),
                            ..ack
                        },
                        expires,
                        now
                    )
                    .is_err()
                );
                assert!(
                    b.reconcile_fabric_relays(other, &[ack], expires, now)
                        .is_empty()
                );
                let permit = FabricRelayPermit {
                    upstream: input.binding,
                    destination: left_address,
                    peer: other,
                    expected: ack,
                };
                b.permit_fabric_relay(permit).unwrap();
                if opus {
                    let missing_audio_level =
                        crate::control::controller::peer_ownership::relay::fabric::LiveRouteAck {
                            emission: Emission::new(Profile::Opus, &[]).unwrap(),
                            ..ack
                        };
                    assert!(
                        b.reconcile_fabric_relays(other, &[missing_audio_level], expires, now)
                            .is_empty()
                    );
                }
                assert_eq!(
                    b.permit_fabric_relay(FabricRelayPermit {
                        destination: right_address,
                        ..permit
                    }),
                    Err(BindError::Closed)
                );
                let mut updates = b.reconcile_fabric_relays(other, &[ack], expires, now);
                assert_eq!(updates.len(), 1);
                let (relay_id, LiveRelayChange::Install(relay)) = updates.pop().unwrap() else {
                    panic!("expected one exact relay installation")
                };
                assert_eq!(relay.input, left);
                assert_eq!(relay.next.peer, other);
                assert_eq!(relay.next.key, ack.key);
                assert!(
                    b.reconcile_fabric_relays(other, &[ack], expires, now)
                        .is_empty()
                );

                assert!(left_shard.update_relay(LiveRelayChange::Install(relay.clone())));
                let relayed_notice = (profile == Profile::Data).then(|| {
                    let notice = crate::route::fabric::NetworkDataExpectation {
                        parent: left,
                        unit: crate::route::fabric::local::DataExpectation {
                            unit_seq: 3,
                            fragment_count: 2,
                        },
                        expires: Instant::now() + Duration::from_millis(500),
                    };
                    let mut pending = left_shard.stage_data_expectation(notice);
                    left_shard.apply();
                    assert_eq!(pending.try_recv(), Ok(true));
                    assert!(!left_shard.observe_relay_data_expectation(
                        Instant::now(),
                        &relay,
                        notice.unit
                    ));
                    notice
                });
                let converging = NodeRouteAddress::new(RouteId::new(ShardId::new(0), 10), 1);
                b.install_after(
                    converging,
                    TrackRouteAction::Forward {
                        publication_id: publication,
                    },
                    Some(RouteScope {
                        room,
                        ingress: Some(bid),
                    }),
                    left_shard.install_route(converging),
                );
                let mut converging_route = b
                    .stage_live_receive(spec(bid, vec![converging]), |child, lifetime| {
                        left_shard.stage_child(child, lifetime)
                    })
                    .unwrap()
                    .receipt;
                left_shard.apply();
                b.reap();
                let converging_route = converging_route.try_recv().unwrap().unwrap();
                b.permit_fabric_relay(FabricRelayPermit {
                    upstream: converging_route.binding,
                    destination: converging,
                    ..permit
                })
                .unwrap();
                let mut convergence =
                    b.reconcile_fabric_relays(other, &[ack], expires, Instant::now());
                assert_eq!(convergence.len(), 1);
                let (second_relay, install) = convergence.pop().unwrap();
                assert!(matches!(install, LiveRelayChange::Install(_)));
                assert!(left_shard.update_relay(install));
                let mut repair_proof = None;
                if let Some(notice) = relayed_notice {
                    use crate::route::fabric::relay::RelayExpectationStep;
                    let mut replay = left_shard.stage_data_expectation(notice);
                    left_shard.apply();
                    assert_eq!(replay.try_recv(), Ok(true));
                    let now = Instant::now();
                    let Some(RelayExpectationStep::Send(sent)) =
                        left_shard.tick_relay_data_expectation(now)
                    else {
                        panic!("one exact live relay expectation");
                    };
                    assert_eq!(sent.route.input, left);
                    assert_eq!(sent.route.next, relay.next);
                    assert_eq!(sent.route.tx, relay.tx);
                    assert_eq!(sent.unit, notice.unit);
                    assert_eq!(sent.expires, expires);
                    assert!(left_shard.tick_relay_data_expectation(now).is_none());
                    assert!(!left_shard.observe_relay_data_expectation(
                        now,
                        &relay,
                        crate::route::fabric::local::DataExpectation {
                            fragment_count: 1,
                            ..notice.unit
                        }
                    ));
                    use crate::control::controller::peer_ownership::fabric_data::{
                        Action, Frame, RepairAction, RepairFrame, Scope,
                    };
                    use crate::route::fabric::local::{
                        PropagationOutcome, RepairFragment, RepairRequest,
                    };
                    let observed = Frame {
                        scope: Scope {
                            key: relay.next.key,
                            source: relay.input.incarnation,
                        },
                        unit: notice.unit,
                        action: Action::Observed(PropagationOutcome::Confirmed),
                    };
                    let lookup = RepairFrame {
                        scope: observed.scope,
                        branch: std::num::NonZeroU64::MIN,
                        request: RepairRequest {
                            fragment: RepairFragment {
                                unit_seq: notice.unit.unit_seq,
                                index: 0,
                            },
                            id: std::num::NonZeroU64::new(5).unwrap(),
                        },
                        action: RepairAction::Lookup,
                    };
                    assert!(
                        b.peer_repair_lookups(other, lookup, now)
                            .iter()
                            .all(|lookup| left_shard.relay_repair_deadline(lookup, now).is_none())
                    );
                    c_control
                        .try_send(observed.encode().unwrap(), LinkTime::from_nanos(u64::MAX))
                        .unwrap();
                    let protected = b_control.recv().await.unwrap();
                    let decoded = Frame::decode(&protected).unwrap();
                    assert_eq!(decoded, observed);
                    assert!(
                        b.peer_data_observations(bid, decoded, Instant::now())
                            .is_empty()
                    );
                    let observed = b.peer_data_observations(other, decoded, Instant::now());
                    assert!(observed.iter().any(|outcome| outcome.route.tx == relay.tx));
                    for outcome in observed {
                        left_shard.stage_relay_data_observation(outcome);
                        left_shard.apply();
                    }
                    assert!(!left_shard.observe_relay_data_expectation(
                        Instant::now(),
                        &relay,
                        notice.unit
                    ));
                    assert!(left_shard.next_relay_data_expectation().is_none());
                    let miss = RepairFrame {
                        request: RepairRequest {
                            id: std::num::NonZeroU64::new(4).unwrap(),
                            ..lookup.request
                        },
                        ..lookup
                    };
                    let missed = leaf.request_repair(miss.request, Instant::now());
                    c_control
                        .try_send(miss.encode().unwrap(), LinkTime::from_nanos(u64::MAX))
                        .unwrap();
                    let received_miss =
                        RepairFrame::decode(&b_control.recv().await.unwrap()).unwrap();
                    assert_eq!(received_miss, miss);
                    let now = Instant::now();
                    let mut repairs = RepairLookups::default();
                    repairs.lookup(received_miss, now, || {
                        let lookups = b.peer_repair_lookups(other, received_miss, now);
                        assert_eq!(lookups.len(), 2);
                        left_shard.stage_relay_repair_lookups(lookups)
                    });
                    repairs.flush(now, |_| {
                        panic!("lookup cannot reply before the owning shard")
                    });
                    left_shard.apply();
                    repairs.flush(now, |response| {
                        b_control
                            .try_send(response.encode().unwrap(), LinkTime::from_nanos(u64::MAX))
                            .is_ok()
                    });
                    let unavailable = RepairFrame {
                        action: RepairAction::Unavailable,
                        ..miss
                    };
                    assert_eq!(
                        RepairFrame::decode(&c_control.recv().await.unwrap()),
                        Some(unavailable)
                    );
                    assert!(leaf.settle_unavailable_repair(missed, Instant::now()));

                    let interest = leaf.request_repair(lookup.request, Instant::now());
                    assert!(!leaf.settle_unavailable_repair(missed, Instant::now()));
                    assert_eq!(interest.parent, leaf_parent.unwrap());
                    assert_eq!(interest.branch, lookup.branch);
                    let now = Instant::now();
                    assert!(b.peer_repair_lookups(bid, lookup, now).is_empty());
                    let lookups = b.peer_repair_lookups(other, lookup, now);
                    let mut eligible = lookups
                        .iter()
                        .filter(|request| left_shard.relay_repair_deadline(request, now).is_some());
                    let Some(exact) = eligible.next() else {
                        panic!("the acknowledged route must be eligible");
                    };
                    assert!(eligible.next().is_none());
                    assert_eq!(exact.branch, lookup.branch);
                    assert_eq!(exact.route.tx, relay.tx);
                    assert_eq!(left_shard.relay_repair_deadline(exact, expires), None);
                    repair_proof =
                        Some((lookup, exact.clone(), interest, repairs, miss, unavailable));
                }
                let (_, converging_tx) = a
                    .control
                    .bind_fabric_send(aid, converging_route.address)
                    .unwrap();

                let single_hop = packet.arrival_view(1).unwrap().into_owned();
                let publisher_send = if profile == Profile::Data {
                    use crate::control::link::PublisherSendPermit;
                    use crate::route::fabric::{DataPublisher, PublisherSendChange};
                    let downstream =
                        relay::fabric::decode(b.fabric_offers(bid).into_boxed_slice()).unwrap();
                    let ack = *downstream
                        .iter()
                        .find(|ack| ack.key == input.address)
                        .unwrap();
                    let now = Instant::now();
                    let expires = now + Duration::from_secs(1);
                    let permit = PublisherSendPermit {
                        publisher: DataPublisher {
                            publication,
                            connection: ConnectionId::new(),
                            incarnation: left.incarnation,
                        }
                        .into(),
                        shard: ShardId::new(0),
                        ack,
                    };
                    let wrong = PublisherSendPermit {
                        publisher: crate::shard_update::PublisherAnchor {
                            incarnation: std::num::NonZeroU64::MAX,
                            ..permit.publisher
                        },
                        ..permit
                    };
                    assert!(
                        a.reconcile_publisher_sends(aid, &[wrong], expires, now)
                            .is_empty()
                    );
                    let mut changes = a.reconcile_publisher_sends(aid, &[permit], expires, now);
                    let Some((binding, PublisherSendChange::Install { route, lifetime })) =
                        changes.pop()
                    else {
                        panic!("an exact acknowledgment must install the publisher hop");
                    };
                    assert!(changes.is_empty());
                    let send = route.tx.clone();
                    let (runtime, source) = LiveNetworkFixture::send_as_data_publisher(
                        route, lifetime, single_hop, now,
                    );
                    Some((runtime, source, send, binding, permit, expires))
                } else {
                    tx.try_send(
                        ShardFrame::FabricImage(single_hop),
                        LinkTime::from_nanos(u64::MAX),
                    )
                    .unwrap();
                    None
                };
                received(&b, bid, 3).await;
                for ((rx, address), shard) in [&mut *left_rx, &mut *right_rx]
                    .into_iter()
                    .zip(addresses)
                    .zip([&mut *left_shard, &mut *right_shard])
                {
                    let mut batch = Vec::new();
                    assert_eq!(rx.try_recv_batch(&mut batch, 1), 1);
                    let frame = batch.pop().unwrap();
                    assert_eq!(
                        frame.fabric,
                        Some(FabricIngress {
                            association: bid,
                            key,
                            purpose: pulsebeam_link::FabricPurpose::Live,
                        })
                    );
                    assert_eq!(frame.tag, crate::link::delivery_tag(address));
                    let ShardFrame::FabricImage(image) = &frame.packet else {
                        panic!("expected Fabric image")
                    };
                    assert!(image.same_content(&packet));
                    assert_eq!(image.remaining_hops(), 0);
                    assert_eq!(shard.offer(frame, Instant::now()).enqueued, 1);
                }
                let publisher_repair_datagrams =
                    if let Some((mut runtime, source, send, binding, permit, expires)) =
                        publisher_send
                    {
                        use crate::control::controller::peer_ownership::fabric_data::{
                            Action, Frame, RepairAction, RepairFrame, Scope,
                        };
                        use crate::route::fabric::local::{
                            DataExpectation, PropagationOutcome, RepairFragment, RepairRequest,
                        };
                        use std::num::NonZeroU64;
                        let now = Instant::now();
                        let scope = Scope {
                            key: input.address,
                            source: permit.publisher.incarnation,
                        };
                        let unit = DataExpectation {
                            unit_seq: 3,
                            fragment_count: 2,
                        };
                        let observed = Frame {
                            scope,
                            unit,
                            action: Action::Observed(PropagationOutcome::Confirmed),
                        };
                        let request = RepairFrame {
                            scope,
                            branch: NonZeroU64::MIN,
                            request: RepairRequest {
                                fragment: RepairFragment {
                                    unit_seq: unit.unit_seq,
                                    index: 1,
                                },
                                id: NonZeroU64::new(3).unwrap(),
                            },
                            action: RepairAction::Lookup,
                        };
                        assert!(
                            a.peer_publisher_repair_lookups(other, request, now)
                                .next()
                                .is_none()
                        );
                        let lookup = a
                            .peer_publisher_repair_lookups(aid, request, now)
                            .next()
                            .unwrap();
                        let observation = a
                            .peer_publisher_data_observations(aid, observed, now)
                            .pop()
                            .unwrap();
                        let located = LiveNetworkFixture::locate_as_data_publisher(
                            &mut runtime,
                            source,
                            &lookup,
                            observation,
                            now,
                        );
                        assert_eq!(located.sequence, 1);
                        let label = repair_cutoff.as_ref().unwrap().1;
                        let (_, handoff) = a.bind_peer_repair_send(located, label, now).unwrap();
                        let repair_tx = handoff.tx.clone();
                        LiveNetworkFixture::send_publisher_repair(&runtime, source, handoff, now);
                        received(&b, bid, 4).await;
                        let mut batch = Vec::new();
                        assert_eq!(left_rx.try_recv_batch(&mut batch, 1), 1);
                        let repaired = batch.pop().unwrap();
                        assert_eq!(
                            repaired.fabric,
                            Some(FabricIngress {
                                association: bid,
                                key: label,
                                purpose: pulsebeam_link::FabricPurpose::RepairOnly
                            })
                        );
                        let ShardFrame::FabricImage(repaired) = repaired.packet else {
                            panic!("expected publisher repair");
                        };
                        assert!(repaired.same_content(&packet));
                        assert_eq!(right_rx.try_recv_batch(&mut batch, 1), 0);
                        assert!(runtime.retire_fabric_source(source));
                        assert!(!repair_tx.is_live());
                        assert!(!send.is_live());
                        let now = Instant::now();
                        let mut changes = a.reconcile_publisher_sends(aid, &[permit], expires, now);
                        let Some((retired, crate::route::fabric::PublisherSendChange::Retire(_))) =
                            changes.pop()
                        else {
                            panic!("source cutoff must retire the acknowledged send");
                        };
                        assert_eq!(retired, binding);
                        assert!(changes.is_empty());
                        assert!(
                            a.reconcile_publisher_sends(aid, &[permit], expires, now)
                                .is_empty()
                        );
                        assert!(a.fabric_retired_labels(aid).is_empty());
                        1
                    } else {
                        0
                    };
                let leaf_control_packets = if profile == Profile::Data { 2_u64 } else { 0 };
                assert_eq!(
                    health(&c).await.peers.first().unwrap().transport.received,
                    leaf_control_packets
                );
                send(&converging_tx);
                received(&b, bid, 4 + publisher_repair_datagrams).await;
                let mut relay_batch = Vec::new();
                assert_eq!(left_rx.try_recv_batch(&mut relay_batch, 1), 1);
                let report = left_shard.offer(relay_batch.pop().unwrap(), Instant::now());
                assert_eq!((report.enqueued, report.duplicates), (1, 1));
                received(&c, cid, leaf_control_packets.saturating_add(1)).await;
                assert_eq!(leaf_rx.try_recv_batch(&mut relay_batch, 1), 1);
                let forwarded = relay_batch.pop().unwrap();
                let ShardFrame::FabricImage(relayed) = &forwarded.packet else {
                    panic!("expected relayed Fabric image")
                };
                assert_eq!(relayed.remaining_hops(), 5);
                assert!(relayed.same_content(&packet));
                assert_eq!(
                    leaf.offer(forwarded, Instant::now()).enqueued,
                    1 + usize::from(spectator.is_some())
                );
                assert_eq!(leaf.take(Instant::now()), Some(1));
                if let Some(spectator) = spectator {
                    assert_eq!(
                        leaf.take_spectator(leaf_address, spectator, Instant::now()),
                        Some(1)
                    );
                }

                send(&tx);
                received(&b, bid, 5 + publisher_repair_datagrams).await;
                assert_eq!(left_rx.try_recv_batch(&mut relay_batch, 1), 1);
                assert_eq!(
                    left_shard
                        .offer(relay_batch.pop().unwrap(), Instant::now())
                        .duplicates,
                    2
                );
                assert_eq!(right_rx.try_recv_batch(&mut relay_batch, 1), 1);
                let _ = right_shard.offer(relay_batch.pop().unwrap(), Instant::now());
                assert_eq!(leaf_rx.try_recv_batch(&mut relay_batch, 1), 0);
                let repair_send =
                    if let Some((request, lookup, interest, mut repairs, miss, unavailable)) =
                        repair_proof
                    {
                        use crate::control::controller::peer_ownership::fabric_data::{
                            RepairAction, RepairFrame,
                        };
                        let original = FabricPacket::new(
                            Header {
                                sequence: 0,
                                marker: false,
                                ..packet.header()
                            },
                            &[],
                            crate::participant::data_fabric::r3_record().into_boxed_slice(),
                        )
                        .unwrap();
                        tx.try_send(
                            ShardFrame::FabricImage(original.clone()),
                            LinkTime::from_nanos(u64::MAX),
                        )
                        .unwrap();
                        received(&b, bid, 6 + publisher_repair_datagrams).await;
                        assert_eq!(right_rx.try_recv_batch(&mut relay_batch, 1), 1);
                        let sibling = relay_batch.pop().unwrap();
                        assert_eq!(sibling.fabric, Some(right.ingress));
                        assert_eq!(sibling.tag, crate::link::delivery_tag(right_address));
                        assert_eq!(left_rx.try_recv_batch(&mut relay_batch, 1), 1);
                        let received_original = relay_batch.pop().unwrap();
                        assert_eq!(received_original.fabric, Some(left.ingress));
                        let ShardFrame::FabricImage(first_hop) = &received_original.packet else {
                            panic!("expected protected original");
                        };
                        assert!(first_hop.same_content(&original));
                        assert_eq!(first_hop.remaining_hops(), original.remaining_hops() - 1);
                        let now = Instant::now();
                        let _ = left_shard.offer(received_original, now);
                        assert!(leaf_rx.readable().await);
                        assert_eq!(leaf_rx.try_recv_batch(&mut relay_batch, 1), 1);
                        assert_eq!(
                            relay_batch.pop().unwrap().fabric,
                            Some(leaf_parent.unwrap().ingress)
                        );

                        c_control
                            .try_send(request.encode().unwrap(), LinkTime::from_nanos(u64::MAX))
                            .unwrap();
                        let received_lookup =
                            RepairFrame::decode(&b_control.recv().await.unwrap()).unwrap();
                        assert_eq!(received_lookup, request);
                        let located = left_shard.locate_relay_repair(&lookup, now).unwrap();
                        assert_eq!(located.sequence, 0);
                        repairs.lookup(miss, now, || panic!("replay cannot reopen the old demand"));
                        let mut replay = None;
                        repairs.flush(now, |response| {
                            assert!(replay.replace(response).is_none());
                            true
                        });
                        assert_eq!(replay, Some(unavailable));
                        assert_eq!(
                            b.bind_peer_repair_send(located, ack.key, now).unwrap_err(),
                            BindError::Closed
                        );
                        repairs.lookup(received_lookup, now, || {
                            left_shard.stage_relay_repair_lookups(b.peer_repair_lookups(
                                other,
                                received_lookup,
                                now,
                            ))
                        });
                        let early = RepairFrame {
                            action: RepairAction::Ready {
                                sequence: 0,
                                label: ack.key,
                            },
                            ..received_lookup
                        };
                        repairs.ready(early, now, |_, _| {
                            panic!("Ready before Located cannot hand off")
                        });
                        left_shard.apply();
                        repairs.flush(now, |response| {
                            b_control
                                .try_send(
                                    response.encode().unwrap(),
                                    LinkTime::from_nanos(u64::MAX),
                                )
                                .is_ok()
                        });
                        let echoed = RepairFrame::decode(&c_control.recv().await.unwrap()).unwrap();
                        assert_eq!(
                            echoed,
                            RepairFrame {
                                action: RepairAction::Located { sequence: 0 },
                                ..received_lookup
                            }
                        );
                        let RepairAction::Located { sequence } = echoed.action else {
                            panic!("expected a location");
                        };
                        let mut pending = None;
                        let mut receipt = c
                            .stage_located_repair_receive(interest, sequence, |child, cutoff| {
                                let (tx, rx) = oneshot::channel();
                                pending = Some((tx, child, cutoff));
                                rx
                            })
                            .unwrap();
                        c.reap();
                        assert_eq!(receipt.try_recv(), Err(oneshot::error::TryRecvError::Empty));
                        let (ack, child, cutoff) = pending.unwrap();
                        let installed = leaf.install_repair_for_branch(child, cutoff);
                        assert_eq!(installed, Some(child));
                        ack.send(installed).unwrap();
                        c.reap();
                        let repair_route = receipt.try_recv().unwrap().unwrap();
                        let ready = RepairFrame {
                            action: RepairAction::Ready {
                                sequence,
                                label: repair_route.address,
                            },
                            ..request
                        };
                        c_control
                            .try_send(ready.encode().unwrap(), LinkTime::from_nanos(u64::MAX))
                            .unwrap();
                        let received_ready =
                            RepairFrame::decode(&b_control.recv().await.unwrap()).unwrap();
                        assert_eq!(received_ready, ready);
                        let now = Instant::now();
                        assert_relay_repair_rejects_stale_control(
                            &mut repairs,
                            received_lookup,
                            received_ready,
                            now,
                        );
                        let mut tx = None;
                        repairs.ready(received_ready, now, |located, label| {
                            let (binding, handoff) =
                                b.bind_peer_repair_send(located, label, now).unwrap();
                            tx = Some(handoff.tx.clone());
                            left_shard.stage_relay_repair_handoff(handoff);
                            Some(binding)
                        });
                        repairs.ready(received_ready, now, |_, _| {
                            panic!("duplicate Ready cannot hand off again")
                        });
                        let tx = tx.unwrap();
                        left_shard.apply();
                        assert!(leaf_rx.readable().await);
                        assert_eq!(leaf_rx.try_recv_batch(&mut relay_batch, 1), 1);
                        let frame = relay_batch.pop().unwrap();
                        assert_eq!(
                            frame.fabric,
                            Some(FabricIngress {
                                association: cid,
                                key: repair_route.address,
                                purpose: pulsebeam_link::FabricPurpose::RepairOnly
                            })
                        );
                        let ShardFrame::FabricImage(repaired) = &frame.packet else {
                            panic!("expected canonical repair image");
                        };
                        assert!(repaired.same_content(&original));
                        assert_eq!(repaired.remaining_hops(), original.remaining_hops() - 2);
                        leaf.admit_repair_for_branch(frame, request.request, Instant::now());
                        assert_eq!(
                            leaf.take_spectator(leaf_address, spectator.unwrap(), Instant::now()),
                            None
                        );
                        assert_eq!(leaf_rx.try_recv_batch(&mut relay_batch, 1), 0);
                        Some((
                            tx,
                            original,
                            received_lookup,
                            interest,
                            repair_route.address,
                        ))
                    } else {
                        None
                    };
                assert!(!left_shard.update_relay(LiveRelayChange::Retire(
                    relay.clone(),
                    RelayRetirement::Revoked
                )));
                let pending_retirements = (profile == Profile::Data).then(|| {
                    use crate::route::fabric::{
                        NetworkDataExpectation, local::DataExpectation, relay::LiveRelays,
                    };
                    std::array::from_fn::<_, 3, _>(|_| {
                        let mut pending = LiveRelays::default();
                        let now = Instant::now();
                        assert!(pending.install(now, relay.clone()));
                        assert!(pending.seed_expectation(
                            now,
                            NetworkDataExpectation {
                                parent: relay.input,
                                unit: DataExpectation {
                                    unit_seq: 9,
                                    fragment_count: 1
                                },
                                expires: relay.expires_at,
                            }
                        ));
                        pending
                    })
                });
                let issued = LiveNextHop {
                    peer: cid,
                    key: ack.key,
                };
                let label = ack.key.to_bytes();
                c.accept_fabric_retirements(cid, &[label]);
                assert!(c.fabric_issued.contains_key(&issued));
                c.retire_after(leaf_address, |_| {
                    panic!("Fabric receive cutoff is immediate")
                });
                leaf.retire_route(leaf_address);
                c.accept_fabric_retirements(other, &[label]);
                assert!(c.fabric_issued.contains_key(&issued));
                let snapshot = |offers, confirmed| {
                    wire::encode(
                        std::num::NonZeroU64::MIN,
                        wire::fixture_checkpoint(),
                        &Box::default(),
                        wire::RouteImage::Fabric {
                            offers,
                            retired: confirmed,
                            data_anchors: wire::DataAnchors::new(),
                            opus_anchors: None,
                        },
                    )
                    .unwrap()
                };
                let withdrawal = transfer_fixture_snapshot(
                    &mut c_control,
                    &mut b_control,
                    snapshot(c.fabric_offers(cid), Vec::new()),
                )
                .await;
                assert!(withdrawal.fabric_routes.is_empty());
                let now = Instant::now();
                let expires = now.checked_add(Duration::from_secs(1)).unwrap();
                let mut withdrawals =
                    b.reconcile_fabric_relays(other, &withdrawal.fabric_routes, expires, now);
                assert_eq!(withdrawals.len(), 2);
                assert!(repair_send.as_ref().is_none_or(|(tx, ..)| !tx.is_live()));
                if let Some([mut bare_cutoff, mut disconnected, mut revoked]) = pending_retirements
                {
                    use crate::route::fabric::{
                        local::PropagationOutcome, relay::RelayExpectationStep,
                    };
                    assert!(now < relay.expires_at);
                    assert!(bare_cutoff.tick_expectation(now).is_none());
                    assert!(matches!(
                        bare_cutoff.tick_expectation(relay.expires_at),
                        Some(RelayExpectationStep::Settled(PropagationOutcome::Unknown))
                    ));
                    disconnected.retire(&relay, RelayRetirement::Unconfirmed, now);
                    assert!(matches!(
                        disconnected.tick_expectation(now),
                        Some(RelayExpectationStep::Settled(PropagationOutcome::Unknown))
                    ));
                    revoked.retire(&relay, RelayRetirement::Revoked, now);
                    assert!(matches!(
                        revoked.tick_expectation(now),
                        Some(RelayExpectationStep::Settled(PropagationOutcome::Failed))
                    ));
                }
                for (withdrawn, retired) in withdrawals.drain(..) {
                    assert!(withdrawn == relay_id || withdrawn == second_relay);
                    assert!(matches!(
                        retired,
                        LiveRelayChange::Retire(_, RelayRetirement::Revoked)
                    ));
                    let receipt = left_shard.stage_relay_update(retired);
                    b.record_fabric_retirement(withdrawn, Some(receipt));
                    assert!(b.fabric_retired_labels(other).is_empty());
                    left_shard.apply();
                }
                let confirmed = b.fabric_retired_labels(other);
                assert_eq!(confirmed, vec![label]);
                assert!(b.fabric_retired_labels(bid).is_empty());
                let confirmation = transfer_fixture_snapshot(
                    &mut b_control,
                    &mut c_control,
                    snapshot(b.fabric_offers(other), confirmed),
                )
                .await;
                c.accept_fabric_retirements(cid, &confirmation.fabric_retired);
                assert!(!c.fabric_issued.contains_key(&issued));
                c.accept_fabric_retirements(cid, &confirmation.fabric_retired);
                assert!(!c.fabric_issued.contains_key(&issued));
                assert!(!b.revoke_fabric_relay(permit));
                assert_eq!(b.permit_fabric_relay(permit), Err(BindError::Closed));
                assert!(
                    b.reconcile_fabric_relays(other, &[ack], expires, Instant::now())
                        .is_empty()
                );
                assert!(!left_shard.update_relay(LiveRelayChange::Install(relay.clone())));

                let alias = NodeRouteAddress::new(RouteId::new(ShardId::new(0), 8), 1);
                let receipt = left_shard.install_route(alias);
                b.install_after(
                    alias,
                    TrackRouteAction::Forward {
                        publication_id: publication,
                    },
                    Some(RouteScope {
                        room,
                        ingress: Some(other),
                    }),
                    receipt,
                );
                let mut alternate = b
                    .stage_live_receive(spec(other, vec![alias]), |child, lifetime| {
                        left_shard.stage_child(child, lifetime)
                    })
                    .unwrap()
                    .receipt;
                left_shard.apply();
                b.reap();
                let alternate = alternate.try_recv().unwrap().unwrap();
                let (_, alternate_tx) = c
                    .control
                    .bind_send(Destination::Network {
                        peer: cid,
                        address: alternate.address,
                    })
                    .unwrap();
                send(&alternate_tx);
                assert!(left_rx.readable().await);
                let mut batch = Vec::new();
                assert_eq!(left_rx.try_recv_batch(&mut batch, 1), 1);
                assert_eq!(
                    left_shard
                        .offer(batch.pop().unwrap(), Instant::now())
                        .duplicates,
                    1
                );

                let first_hop_repair_originals = u64::from(repair_send.is_some());
                send(&tx);
                received(
                    &b,
                    bid,
                    6 + publisher_repair_datagrams + first_hop_repair_originals,
                )
                .await;
                b.retire_after(left_address, |_| {
                    panic!("the Fabric binding is fenced immediately")
                });
                for rx in [&mut *left_rx, &mut *right_rx] {
                    assert_eq!(rx.try_recv_batch(&mut Vec::new(), 1), 0);
                }
                left_shard.retire_route(left_address);
                assert!(repair_cutoff.is_none_or(|(cutoff, _)| !cutoff.is_live()));
                assert!(!relay.tx.is_live());
                let now = Instant::now();
                assert!(!relay.tx.try_send(
                    now,
                    now + Duration::from_secs(1),
                    u64::from(packet.header().sequence),
                    packet.clone()
                ));
                assert_eq!(left_shard.take(Instant::now()), None);
                assert_eq!(right_shard.take(Instant::now()), None);
                send(&tx);
                assert_eq!(
                    received(
                        &b,
                        bid,
                        7 + publisher_repair_datagrams + first_hop_repair_originals
                    )
                    .await
                    .stale,
                    3
                );

                let (next_timestamp, next_payload) = if profile == Profile::Data {
                    let mut payload = crate::participant::data_fabric::R4;
                    *payload.get_mut(11).unwrap() = 4; // Last byte of the golden record's unit sequence.
                    (4, Box::<[u8]>::from(payload))
                } else {
                    (timestamp, Box::<[u8]>::from(payload))
                };
                let next = FabricPacket::new(
                    Header {
                        sequence: 2,
                        timestamp: next_timestamp,
                        ..packet.header()
                    },
                    &[],
                    next_payload,
                )
                .unwrap();
                alternate_tx
                    .try_send(
                        ShardFrame::FabricImage(next),
                        LinkTime::from_nanos(u64::MAX),
                    )
                    .unwrap();
                assert!(left_rx.readable().await);
                assert_eq!(left_rx.try_recv_batch(&mut batch, 1), 1);
                assert_eq!(
                    left_shard
                        .offer(batch.pop().unwrap(), Instant::now())
                        .enqueued,
                    1
                );
                assert_eq!(left_shard.take(Instant::now()), Some(2));

                let candidate = |slot: usize| FabricRelayPermit {
                    upstream: converging_route.binding,
                    destination: converging,
                    expected: relay::fabric::LiveRouteAck {
                        key: NodeRouteKey::from_bytes(
                            (0xff00_0000 | u32::try_from(slot).unwrap()).to_be_bytes(),
                        ),
                        ..ack
                    },
                    ..permit
                };
                // The retired key is still reserved on this association.
                for slot in 1..relay::LIMIT {
                    assert_eq!(b.permit_fabric_relay(candidate(slot)), Ok(()));
                }
                assert_eq!(
                    b.permit_fabric_relay(candidate(relay::LIMIT)),
                    Err(BindError::Capacity)
                );
                assert_eq!(b.permit_fabric_relay(candidate(1)), Ok(()));

                if let Some((old_send, original, lookup, interest, old_label)) = repair_send {
                    use crate::control::controller::peer_ownership::fabric_data::RepairFrame;
                    b.close_peer(other);
                    c.close_peer(cid);
                    assert!(b_control.is_closed());
                    assert!(c_control.is_closed());
                    assert!(!old_send.is_live());
                    let now = Instant::now();
                    assert!(!old_send.try_send(
                        now,
                        now + Duration::from_secs(1),
                        u64::from(original.header().sequence),
                        original.clone()
                    ));
                    let (next_c, next_c_lane) = c
                        .open_control_peer(peer(ba, &kc, &kb, PeerRole::Initiator), b_node)
                        .unwrap();
                    let (next_b, mut next_b_lane) = b
                        .open_control_peer(peer(ca, &kb, &kc, PeerRole::Responder), c_node)
                        .unwrap();
                    assert_ne!(next_b, other);
                    assert_ne!(next_c, cid);
                    while !(next_c_lane.is_ready() && next_b_lane.is_ready()) {
                        pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
                    }

                    let fresh = NodeRouteAddress::new(RouteId::new(ShardId::new(0), 10), 1);
                    c.install_after(
                        fresh,
                        TrackRouteAction::Forward {
                            publication_id: publication,
                        },
                        Some(RouteScope {
                            room,
                            ingress: Some(next_c),
                        }),
                        leaf.install_route(fresh),
                    );
                    let mut receipt = c
                        .stage_live_receive(spec(next_c, vec![fresh]), |child, cutoff| {
                            leaf.stage_child(child, cutoff)
                        })
                        .unwrap()
                        .receipt;
                    leaf.apply();
                    c.reap();
                    let live = receipt.try_recv().unwrap().unwrap();
                    assert_ne!(live.address, old_label);
                    leaf.select_branch(fresh);
                    assert!(Instant::now() < interest.expires);
                    assert!(!c.allows_repair_interest(interest, Instant::now()));
                    assert_eq!(
                        c.stage_located_repair_receive(interest, 0, |_, _| panic!(
                            "old demand must not stage on replacement"
                        ))
                        .unwrap_err(),
                        BindError::Closed
                    );
                    next_c_lane
                        .try_send(lookup.encode().unwrap(), LinkTime::from_nanos(u64::MAX))
                        .unwrap();
                    let delayed = RepairFrame::decode(&next_b_lane.recv().await.unwrap()).unwrap();
                    assert_eq!(delayed, lookup);
                    assert!(
                        b.peer_repair_lookups(next_b, delayed, Instant::now())
                            .is_empty()
                    );
                    let (_, stale) = b.control.bind_fabric_send(next_b, old_label).unwrap();
                    stale
                        .try_send(
                            ShardFrame::FabricImage(original),
                            LinkTime::from_nanos(u64::MAX),
                        )
                        .unwrap();
                    assert_eq!(received(&c, next_c, 1).await.stale, 1);
                    assert_eq!(leaf_rx.try_recv_batch(&mut Vec::new(), 1), 0);
                    let (_, live_tx) = b.control.bind_fabric_send(next_b, live.address).unwrap();
                    send(&live_tx);
                    assert!(leaf_rx.readable().await);
                    let mut fresh_batch = Vec::new();
                    assert_eq!(leaf_rx.try_recv_batch(&mut fresh_batch, 1), 1);
                    let received = fresh_batch.pop().unwrap();
                    assert_eq!(received.tag, crate::link::delivery_tag(fresh));
                    assert_eq!(
                        received.fabric.map(|ingress| ingress.association),
                        Some(next_c)
                    );
                }
                shutdown.cancel();
                a_task.await.unwrap().unwrap();
                b_task.await.unwrap().unwrap();
                c_task.await.unwrap().unwrap();
            })
            .await
            .unwrap();
        },
    );
}

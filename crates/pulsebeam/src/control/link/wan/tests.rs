use std::time::Duration;

use pulsebeam_core::fabric::{FabricPacket, Header, Profile};
use pulsebeam_link::{LinkTime, NetworkClass, NetworkPeer, PeerRole, SecretCredential};

use crate::{
    control::controller::peer_ownership::fabric_data::{self, RepairFrame, WanRepairReady},
    entity::{ConnectionId, ProjectId, RoomId, TrackKind},
    route::{
        RouteId,
        fabric::{
            NetworkPacket,
            local::{DataExpectation, RepairFragment, RepairRequest},
        },
    },
    shard_update::PublisherAnchor,
};

use super::*;

#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn border_media_network_cannot_back_an_ownership_session() {
    let origin = Instant::now();
    let control = LinkControl::new(|| LinkTime::from_nanos(0)).unwrap();
    let mut links = Links::new(control, Vec::new(), origin);
    let (_host, provisioner) = wan_host::Service::bind("[::1]:0".parse().unwrap(), origin)
        .await
        .unwrap();
    let network = links.reserve_wan_peer().unwrap();
    let remote = SecretCredential::generate().unwrap();
    let peer = NetworkPeer {
        endpoint: "[::1]:9".parse().unwrap(),
        expected_peer: remote.fingerprint(),
        credential: SecretCredential::generate().unwrap(),
        role: PeerRole::Initiator,
        class: NetworkClass::Wan,
    };
    links
        .attach_wan_host(
            provisioner
                .open_media(peer, network, NonZeroU64::MIN)
                .unwrap(),
            Carriage::Border,
        )
        .unwrap();
    assert!(links.is_wan_peer(network));
    assert!(!links.accepts_ownership(network));
    assert!(links.wan_offers(network).is_empty());
}

#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn wan_host_receives_only_controller_scoped_commits_and_fences_on_peer_close() {
    let origin = Instant::now();
    let mut control = LinkControl::new(move || {
        LinkTime::from_nanos(u64::try_from(origin.elapsed().as_nanos()).unwrap())
    })
    .unwrap();
    let (port, mut mailbox) = control.create_port().unwrap();
    let mut links = Links::new(control, vec![port], origin);
    let address = "127.0.0.1:0".parse().unwrap();
    let (sender_host, sender) = wan_host::Service::bind(address, origin).await.unwrap();
    let (receiver_host, provisioner) = wan_host::Service::bind(address, origin).await.unwrap();
    let left = SecretCredential::generate().unwrap();
    let right = SecretCredential::generate().unwrap();
    let outbound = NetworkPeer {
        endpoint: receiver_host.local_addr(),
        expected_peer: right.fingerprint(),
        credential: left.clone(),
        role: PeerRole::Initiator,
        class: NetworkClass::Wan,
    };
    let inbound = NetworkPeer {
        endpoint: sender_host.local_addr(),
        expected_peer: left.fingerprint(),
        credential: right,
        role: PeerRole::Responder,
        class: NetworkClass::Wan,
    };
    let initiate = |links: &mut Links| {
        let left = links.reserve_wan_peer().unwrap();
        let right = links.reserve_wan_peer().unwrap();
        let (sender_commands, sender_lane) = sender
            .open(outbound.clone(), left, NonZeroU64::MIN, origin)
            .unwrap();
        let (commands, receiver_lane) = provisioner
            .open(inbound.clone(), right, NonZeroU64::MIN, origin)
            .unwrap();
        links
            .attach_wan_host(sender_commands, Carriage::Ownership)
            .unwrap();
        links
            .attach_wan_host(commands, Carriage::Ownership)
            .unwrap();
        (left, sender_lane, right, receiver_lane)
    };
    let (sender_peer, mut sender_lane, peer, mut receiver_lane) = initiate(&mut links);
    let shutdown = pulsebeam_runtime::sync::CancellationToken::new();
    let sender_task = pulsebeam_runtime::task::spawn(sender_host.run(shutdown.child_token()));
    let receiver_task = pulsebeam_runtime::task::spawn(receiver_host.run(shutdown.child_token()));
    let timeout = Duration::from_secs(2);
    pulsebeam_runtime::time::timeout(timeout, async {
        while !sender_lane.is_ready() || !receiver_lane.is_ready() {
            pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let frame = Box::<[u8]>::from([1, 0x0b, 12]);
    sender_lane
        .try_send(
            frame.clone(),
            pulsebeam_link::LinkTime::from_nanos(u64::MAX),
        )
        .unwrap();
    assert_eq!(
        pulsebeam_runtime::time::timeout(timeout, receiver_lane.recv())
            .await
            .unwrap(),
        Some(frame)
    );
    let now = Instant::now();
    let room = ProjectRoomKey {
        project: ProjectId::from_bytes([1; 16]),
        room: RoomId::new("wan").unwrap(),
    };
    let receiver = ConnectionId::from_bytes([7; 16]);
    let publication = receiver.fixture_publication(TrackKind::Data, "wan");
    let address = NodeRouteAddress::new(RouteId::new(ShardId::new(0), 1), 1);
    let (committed, receipt) = oneshot::channel();
    links.install_after(
        address,
        TrackRouteAction::Forward {
            publication_id: publication,
        },
        Some(RouteScope {
            room,
            ingress: Some(peer),
        }),
        receipt,
    );
    let start = (1 << 34) | 0x18000;
    let spec = || WanLiveReceiveSpec {
        live: LiveReceiveSpec {
            peer,
            room,
            publication,
            incarnation: NonZeroU64::MIN,
            sequence_anchor: 0,
            major: fabric::MAJOR,
            revision: fabric::REGISTRY_REVISION,
            emission: Emission::DATA,
            addresses: vec![address],
        },
        canonical_ingress: peer,
        start,
        forms: 0,
        offer_cut: None,
        regional: None,
        routed: false,
        expires: now + Duration::from_secs(2),
        timing: None,
    };
    assert!(matches!(
        links.stage_wan_live_receive(now, spec(), |_, _| panic!("uncommitted route")),
        Err(BindError::Closed)
    ));
    committed.send(()).unwrap();
    let mut wrong_ingress = spec();
    wrong_ingress.canonical_ingress = sender_peer;
    assert!(matches!(
        links.stage_wan_live_receive(now, wrong_ingress, |_, _| panic!("wrong ingress")),
        Err(BindError::Closed)
    ));
    let mut gate = None;
    let live_window = links
        .stage_wan_live_receive(now, spec(), |child, cutoff| {
            let (installed, receipt) = oneshot::channel();
            gate = Some((installed, child, cutoff));
            receipt
        })
        .unwrap();
    pulsebeam_runtime::task::yield_now().await;
    assert!(links.wan_offers(peer).is_empty());
    let (installed, child, live_cutoff) = gate.unwrap();
    let expectation = fabric_data::Frame {
        scope: fabric_data::Scope {
            key: child.ingress.key,
            source: child.incarnation,
        },
        unit: DataExpectation {
            unit_seq: 10,
            fragment_count: 2,
        },
        action: fabric_data::Action::Expect { lease_ms: 1_000 },
    };
    assert!(
        links
            .wan_data_expectations(peer, expectation, now)
            .is_none()
    );
    installed.send(Some(child)).unwrap();
    let offers = pulsebeam_runtime::time::timeout(timeout, async {
        loop {
            let offers = links.wan_offers(peer);
            if !offers.is_empty() {
                break offers;
            }
            pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        offers.as_ref(),
        &[WanLiveRouteAck {
            room,
            publication,
            incarnation: child.incarnation,
            sequence_anchor: 0,
            offer_cut: None,
            window: live_window
        }]
    );
    assert_eq!(child.window, Some(live_window));
    assert_eq!(live_window.start(), start);
    sender_lane
        .try_send(
            expectation.encode().unwrap(),
            LinkTime::from_nanos(u64::MAX),
        )
        .unwrap();
    let wire = pulsebeam_runtime::time::timeout(timeout, receiver_lane.recv())
        .await
        .unwrap()
        .unwrap();
    let expect = fabric_data::Frame::decode(&wire).unwrap();
    assert_eq!(expect, expectation);
    let clipped = now + Duration::from_millis(1_500);
    let notices = links.wan_data_expectations(peer, expect, clipped).unwrap();
    assert!(
        matches!(notices.as_slice(), [notice] if notice.parent == child && notice.unit == expect.unit && notice.expires == now + Duration::from_secs(2))
    );
    assert!(
        links
            .wan_data_expectations(sender_peer, expect, now)
            .is_none()
    );
    let wrong = fabric_data::Frame {
        scope: fabric_data::Scope {
            source: NonZeroU64::MAX,
            ..expect.scope
        },
        ..expect
    };
    assert!(links.wan_data_expectations(peer, wrong, now).is_none());
    let source = WanSourceWindow {
        room,
        publisher: PublisherAnchor {
            publication,
            connection: receiver,
            incarnation: child.incarnation,
            emission: Emission::DATA,
        },
        sequence_anchor: 0,
        window_start: start,
        timing: None,
        ladder: None,
        origin: None,
        offer_cut: None,
    };
    let ack = offers[0];
    let deadline = now + Duration::from_secs(2);
    assert!(matches!(
        links.stage_wan_publisher_transport(
            sender_peer,
            source,
            WanLiveRouteAck {
                sequence_anchor: 1 << 32,
                ..ack
            },
            deadline,
            now
        ),
        Err(BindError::Closed)
    ));
    assert!(matches!(
        links.stage_wan_publisher_transport(sender_peer, source, ack, now, now),
        Err(BindError::Closed)
    ));
    let (send_cutoff, mut pending) = links
        .stage_wan_publisher_transport(sender_peer, source, ack, deadline, now)
        .unwrap();
    let tx = pulsebeam_runtime::time::timeout(timeout, async {
        loop {
            if let Poll::Ready(tx) = pending.poll() {
                break tx.unwrap();
            }
            pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(!tx.renew(now, deadline));
    let packet = FabricPacket::new(
        Header {
            profile: Profile::Data,
            marker: true,
            sequence: 0x18000,
            timestamp: 0,
        },
        &[],
        vec![0; 64].into_boxed_slice(),
    )
    .unwrap();
    let sent_at = Instant::now();
    assert!(tx.try_send(sent_at, deadline, start, packet.clone()));
    assert!(
        pulsebeam_runtime::time::timeout(timeout, mailbox.readable())
            .await
            .unwrap()
    );
    let mut frames = Vec::new();
    assert_eq!(mailbox.try_recv_batch(&mut frames, 1), 1);
    assert!(
        matches!(frames.pop().unwrap().packet, ShardFrame::FabricWanImage { ingress, image: NetworkPacket { window, position, packet: received, .. } }
        if ingress == child.ingress && window == live_window && position == start && received.same_content(&packet))
    );

    let interest = Interest {
        parent: child,
        receiver,
        branch: NonZeroU64::MIN,
        request: RepairRequest {
            id: NonZeroU64::MIN,
            fragment: RepairFragment {
                unit_seq: 10,
                index: 0,
            },
        },
        expires: now + Duration::from_secs(1),
    };
    let position = start + 0xffff;
    let mut repair_gate = None;
    let repair = links
        .stage_wan_repair_receive(now, interest, position, |child, cutoff| {
            let (installed, receipt) = oneshot::channel();
            repair_gate = Some((installed, child, cutoff));
            receipt
        })
        .unwrap();
    assert_eq!(
        links.wan_repair_receive_state(interest, repair, now),
        Poll::Pending
    );
    let (installed, expected, repair_cutoff) = repair_gate.unwrap();
    assert_eq!(repair, expected);
    installed.send(Some(repair)).unwrap();
    let repair_window = pulsebeam_runtime::time::timeout(timeout, async {
        loop {
            links.reap();
            if let Poll::Ready(window) = links.wan_repair_receive_state(interest, repair, now) {
                break window.unwrap();
            }
            pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(repair.window, Some(repair_window));
    let ready = WanRepairReady::from_install(child, repair, repair_window).unwrap();
    assert!(WanRepairReady::from_install(child, repair, live_window).is_none());
    receiver_lane
        .try_send(ready.encode().unwrap(), LinkTime::from_nanos(u64::MAX))
        .unwrap();
    let bytes = pulsebeam_runtime::time::timeout(timeout, sender_lane.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        WanRepairReady::decode(&bytes, live_window.emission()),
        Some(ready)
    );
    assert!(RepairFrame::decode(&bytes).is_none());
    let parent = wan_host::ParentSend {
        cutoff: send_cutoff.clone(),
        window: live_window,
    };
    let mismatched = WanRepairReady {
        window: Window::new(
            Profile::Data,
            repair_window.start(),
            1,
            repair_window.ssrc(),
            0,
        )
        .unwrap(),
        ..ready
    };
    assert!(matches!(
        links.stage_wan_repair_transport(
            sender_peer,
            parent.clone(),
            mismatched,
            interest.expires,
            now
        ),
        Err(BindError::Closed)
    ));
    let (repair_send_cutoff, mut flight) = links
        .stage_wan_repair_transport(sender_peer, parent, ready, interest.expires, now)
        .unwrap();
    let repair_tx = pulsebeam_runtime::time::timeout(timeout, async {
        loop {
            if let Poll::Ready(tx) = flight.poll() {
                break tx.unwrap();
            }
            pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let repaired = FabricPacket::new(
        Header {
            sequence: repair.demand.wire_sequence(),
            ..packet.header()
        },
        &[],
        vec![0; 64].into_boxed_slice(),
    )
    .unwrap();
    assert!(repair_tx.try_send(Instant::now(), interest.expires, position, repaired.clone()));
    assert!(
        pulsebeam_runtime::time::timeout(timeout, mailbox.readable())
            .await
            .unwrap()
    );
    assert_eq!(mailbox.try_recv_batch(&mut frames, 1), 1);
    assert!(
        matches!(frames.pop().unwrap().packet, ShardFrame::FabricWanImage { ingress, image: NetworkPacket { window, position: full, packet, .. } }
        if ingress == repair.route.ingress && window == repair_window && full == position && packet.same_content(&repaired))
    );
    assert_ne!(live_window.ssrc(), repair_window.ssrc());
    assert!(repair_window.contains(position));
    assert!(live_cutoff.is_live() && repair_cutoff.is_live());

    let relay_window = Window::new(
        live_window.profile(),
        start,
        live_window.forms(),
        NonZeroU32::new(90).unwrap(),
        live_window.emission(),
    )
    .unwrap();
    let relay_ack = WanLiveRouteAck {
        window: relay_window,
        ..ack
    };
    let now = Instant::now();
    assert!(matches!(
        links.stage_wan_relay_transport(peer, child, relay_ack, deadline, false, now),
        Err(BindError::Closed)
    ));
    assert!(matches!(
        links.stage_wan_relay_transport(
            sender_peer,
            child,
            WanLiveRouteAck {
                sequence_anchor: 1,
                ..relay_ack
            },
            deadline,
            false,
            now
        ),
        Err(BindError::Closed)
    ));
    let (_, mut relay_pending) = links
        .stage_wan_relay_transport(sender_peer, child, relay_ack, deadline, false, now)
        .unwrap();
    let relay_tx = pulsebeam_runtime::time::timeout(timeout, async {
        loop {
            if let Poll::Ready(tx) = relay_pending.poll() {
                break tx.unwrap();
            }
            pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(relay_tx.is_live());
    let retired_label = [live_window.ssrc().get().to_be_bytes()];
    assert!(!links.accept_wan_retirements(sender_peer, &retired_label));
    assert!(relay_tx.is_live());
    let renewed = deadline + Duration::from_secs(3);
    links.retain_wan_live_receives(peer, now, renewed, |current, _, _, _| current == ack);
    assert_eq!(
        links.wan[&peer].receives[&live_window.ssrc()].expires(),
        renewed
    );
    assert!(live_cutoff.is_live());
    links.retain_wan_live_receives(peer, now, renewed, |current, ingress, children, _| {
        assert_eq!(current, ack);
        assert_eq!(ingress, peer);
        assert_eq!(children, [child]);
        false
    });
    assert!(!live_cutoff.is_live());
    assert!(!relay_tx.is_live());
    let mut replacement = spec();
    replacement.live.publication = receiver.fixture_publication(TrackKind::Data, "replacement");
    replacement.live.incarnation = NonZeroU64::new(2).unwrap();
    replacement.live.addresses = vec![NodeRouteAddress::new(RouteId::new(ShardId::new(0), 2), 1)];
    let replacement_source = WanSourceWindow {
        publisher: PublisherAnchor {
            publication: replacement.live.publication,
            incarnation: replacement.live.incarnation,
            ..source.publisher
        },
        ..source
    };
    let (committed, receipt) = oneshot::channel();
    links.install_after(
        replacement.live.addresses[0],
        TrackRouteAction::Forward {
            publication_id: replacement_source.publisher.publication,
        },
        Some(RouteScope {
            room,
            ingress: Some(peer),
        }),
        receipt,
    );
    committed.send(()).unwrap();
    let replacement_window = links
        .stage_wan_live_receive(now, replacement, |child, _| {
            let (installed, receipt) = oneshot::channel();
            installed.send(Some(child)).unwrap();
            receipt
        })
        .unwrap();
    assert_ne!(replacement_window.ssrc(), live_window.ssrc());
    let replacement_ack = pulsebeam_runtime::time::timeout(timeout, async {
        loop {
            if let [ack] = links.wan_offers(peer).as_ref() {
                break *ack;
            }
            pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(replacement_ack.window, replacement_window);
    assert!(!links.accept_wan_retirements(peer, &retired_label));
    let (_, mut flight) = links
        .stage_wan_publisher_transport(
            sender_peer,
            replacement_source,
            replacement_ack,
            deadline,
            now,
        )
        .unwrap();
    let replacement_tx = pulsebeam_runtime::time::timeout(timeout, async {
        loop {
            if let Poll::Ready(tx) = flight.poll() {
                break tx.unwrap();
            }
            pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(sender_lane.is_ready() && receiver_lane.is_ready());
    let old_packet = FabricPacket::new(
        Header {
            sequence: packet.header().sequence + 1,
            ..packet.header()
        },
        &[],
        vec![0; 64].into_boxed_slice(),
    )
    .unwrap();
    assert!(tx.try_send(Instant::now(), deadline, start + 1, old_packet));
    assert!(replacement_tx.try_send(Instant::now(), deadline, start, packet.clone()));
    assert!(
        pulsebeam_runtime::time::timeout(timeout, mailbox.readable())
            .await
            .unwrap()
    );
    assert_eq!(mailbox.try_recv_batch(&mut frames, 2), 1);
    assert!(
        matches!(frames.pop().unwrap().packet, ShardFrame::FabricWanImage { image: NetworkPacket { window, position, .. }, .. }
        if window == replacement_window && position == start)
    );
    links.close_peer(peer);
    assert!(!relay_tx.try_send(Instant::now(), deadline, start, packet));
    assert!(links.wan_offers(peer).is_empty());
    assert!(links.wan_data_expectations(peer, expect, now).is_none());
    assert_eq!(
        links.wan_repair_receive_state(interest, repair, now),
        Poll::Ready(None)
    );
    assert!(!live_cutoff.is_live() && !repair_cutoff.is_live());
    links.close_peer(sender_peer);
    assert!(!send_cutoff.is_live() && !repair_send_cutoff.is_live());
    pulsebeam_runtime::time::timeout(timeout, async {
        while !sender_lane.is_closed() || !receiver_lane.is_closed() {
            pulsebeam_runtime::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (_sender_commands, sender_lane, replacement, receiver_lane) = initiate(&mut links);
    assert_ne!(replacement, peer);
    pulsebeam_runtime::time::timeout(timeout, async {
        while !sender_lane.is_ready() || !receiver_lane.is_ready() {
            pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(!live_cutoff.is_live() && !repair_cutoff.is_live());
    shutdown.cancel();
    sender_task.await.unwrap().unwrap();
    receiver_task.await.unwrap().unwrap();
}

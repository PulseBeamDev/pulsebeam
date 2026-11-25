use std::num::NonZeroU64;

use pulsebeam_core::fabric::{FabricPacket, Header, Profile};
use pulsebeam_link::{Destination as LinkDestination, LinkControl, NodeRouteKey};
use pulsebeam_runtime::{sync::oneshot, time::Duration};

use super::*;
use crate::{
    entity::{ConnectionId, TrackKind},
    id::ShardId,
    route::{
        NodeRouteAddress, RouteId,
        fabric::{
            NetworkPacket,
            local::{Emission, RepairFragment, RepairRequest},
            network_repair::{Demand, RepairRoute},
        },
    },
};

#[cfg(feature = "transport-conformance")]
#[derive(Clone, Copy)]
enum StockSctpLeg {
    Ingress,
    Egress,
}

#[cfg(feature = "transport-conformance")]
fn cross_stock_sctp(leg: StockSctpLeg, bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    use crate::stock_rtc::{FixtureDataEvent, PeerFixture};
    use pulsebeam_rtc::DataMessage;

    let mut fixture = PeerFixture::connected_bounded_udp_datachannels(1_216);
    let mut channel = None;
    let mut native_open = false;
    while channel.is_none() || !native_open {
        match fixture.next_data_event() {
            FixtureDataEvent::ConnectionOpened(id) => channel = Some(id),
            FixtureDataEvent::PeerOpened => native_open = true,
            _ => {}
        }
    }
    assert_eq!(
        fixture.native_sctp_dtls_parameters(),
        (Some(0xfefc), Some(0x1302))
    );
    let channel = channel.ok_or_else(|| anyhow::anyhow!("missing stock SCTP channel"))?;
    match leg {
        StockSctpLeg::Ingress => {
            fixture.peer_send(true, bytes);
            loop {
                if let FixtureDataEvent::ConnectionMessage {
                    channel: received,
                    message,
                } = fixture.next_data_event()
                {
                    assert_eq!(received, channel);
                    let DataMessage::Binary(bytes) = message else {
                        anyhow::bail!("expected a binary DATA v1 record")
                    };
                    return Ok(bytes.to_vec());
                }
            }
        }
        StockSctpLeg::Egress => {
            fixture
                .connection
                .send_data(
                    fixture.at(),
                    channel,
                    DataMessage::Binary(bytes::Bytes::copy_from_slice(bytes)),
                )
                .map_err(|error| anyhow::anyhow!("send stock SCTP record: {error:?}"))?;
            loop {
                if let FixtureDataEvent::PeerMessage { binary, payload } = fixture.next_data_event()
                {
                    assert!(binary);
                    return Ok(payload);
                }
            }
        }
    }
}

async fn ready_pair() -> anyhow::Result<(WanHost, WanHost, LinkControl<ShardFrame>)> {
    ready_pair_with_windows(2).await
}

async fn ready_pair_with_windows(
    active_windows: usize,
) -> anyhow::Result<(WanHost, WanHost, LinkControl<ShardFrame>)> {
    let origin = Instant::now();
    let mut control = LinkControl::new(move || {
        LinkTime::from_nanos(
            u64::try_from(Instant::now().saturating_duration_since(origin).as_nanos())
                .unwrap_or(u64::MAX),
        )
    })
    .map_err(|error| anyhow::anyhow!("create link controller: {error:?}"))?;
    let reserve_error = |error| anyhow::anyhow!("reserve network ID: {error:?}");
    let left_id = control.reserve_network_id().map_err(reserve_error)?;
    let right_id = control.reserve_network_id().map_err(reserve_error)?;
    let identity_error = |error| anyhow::anyhow!("generate WAN identity: {error:?}");
    let left_secret = wan::LocalIdentity::generate().map_err(identity_error)?;
    let right_secret = wan::LocalIdentity::generate().map_err(identity_error)?;
    let bind_error = |error| anyhow::anyhow!("bind WAN test socket: {error:?}");
    let loopback = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 0));
    let left_socket = bind_socket(loopback).await.map_err(bind_error)?;
    let right_socket = bind_socket(loopback).await.map_err(bind_error)?;
    let left_addr = left_socket.local_addr();
    let right_addr = right_socket.local_addr();
    fn config(
        id: NetworkId,
        role: wan::Role,
        local: &wan::LocalIdentity,
        peer: wan::Fingerprint,
        address: SocketAddr,
        active_windows: usize,
    ) -> anyhow::Result<wan::Config<'_>> {
        Ok(wan::Config {
            binding: Binding {
                epoch: NonZeroU64::new(id.epoch())
                    .ok_or_else(|| anyhow::anyhow!("zero WAN association epoch"))?,
                generation: NonZeroU64::MIN,
                peer: address,
            },
            role,
            local,
            peer,
            bootstrap_mtu: 1200,
            limits: wan::Limits {
                active_windows,
                epoch_windows: 4,
            },
        })
    }
    let host_error = |error| anyhow::anyhow!("WAN host setup: {error:?}");
    let (mut left, left_lane) = WanHost::new(
        left_socket,
        left_id,
        config(
            left_id,
            wan::Role::Client,
            &left_secret,
            right_secret.fingerprint(),
            right_addr,
            active_windows,
        )?,
        origin,
    )
    .map_err(host_error)?;
    let (mut right, right_lane) = WanHost::new(
        right_socket,
        right_id,
        config(
            right_id,
            wan::Role::Server,
            &right_secret,
            left_secret.fingerprint(),
            left_addr,
            active_windows,
        )?,
        origin,
    )
    .map_err(host_error)?;
    for _ in 0..128 {
        let now = Instant::now();
        left.poll(now).map_err(host_error)?;
        right.poll(now).map_err(host_error)?;
        if left_lane.is_ready() && right_lane.is_ready() {
            return Ok((left, right, control));
        }
        pulsebeam_runtime::select! {
            ready = left.socket.readable() => ready?,
            ready = right.socket.readable() => ready?,
            () = pulsebeam_runtime::time::sleep(Duration::from_millis(1)) => {},
        }
    }
    anyhow::bail!("WAN socket pair did not qualify")
}

pub(crate) async fn live_send_fixture(
    window: Window,
    authority: NonZeroU64,
    expires: Instant,
) -> anyhow::Result<(WanHost, WanHost, LinkControl<ShardFrame>, NetworkId, WanTx)> {
    let (mut sender, receiver, mut control) = ready_pair().await?;
    let peer = sender.id;
    let (commands, inbox, media) = command_channel(sender.id);
    sender.commands = Some(inbox);
    sender.egress.inbox = Some(media);
    let cutoff = control
        .reserve_wan_send(peer)
        .map_err(|error| anyhow::anyhow!("reserve fixture WAN send: {error:?}"))?;
    let mut pending = commands
        .install_send(
            cutoff,
            window,
            window,
            WindowSpec {
                authority,
                direction: Direction::Send,
                start: window.start(),
                ssrc: window.ssrc(),
                purpose: Purpose::Live,
                expires: expires.into_std(),
            },
            false,
            None,
        )
        .map_err(|error| anyhow::anyhow!("install fixture WAN send: {error:?}"))?;
    sender
        .poll(Instant::now())
        .map_err(|error| anyhow::anyhow!("poll fixture WAN host: {error:?}"))?;
    let std::task::Poll::Ready(Some(tx)) = pending.poll() else {
        anyhow::bail!("fixture requires an active WAN send");
    };
    Ok((sender, receiver, control, peer, tx))
}

async fn submit_paced(
    host: &mut WanHost,
    now: &mut Instant,
    mut prepared: wan::Prepared,
) -> Submission {
    loop {
        match host.submit(*now, prepared) {
            Submission::Deferred {
                prepared: deferred,
                deadline: Some(deadline),
            } => {
                pulsebeam_runtime::time::sleep_until(Instant::from_std(deadline)).await;
                *now = Instant::now();
                prepared = deferred;
            }
            result => return result,
        }
    }
}

#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn protected_boundary_datagrams_reach_only_exactly_acknowledged_shard_windows() {
    let (mut left, mut right, mut control) = ready_pair().await.unwrap();
    let ssrc = NonZeroU32::new(0x0506_0708).unwrap();
    let start = (1 << 32) | 0x18000;
    let window = Window::new(Profile::H264, start, 0, ssrc, 0).unwrap();
    let address = NodeRouteAddress::new(RouteId::new(ShardId::new(0), 1), 1);
    let (port, mut rx) = control.create_port().unwrap();
    let (_, tx) = control
        .bind_send(LinkDestination::Local {
            port,
            tag: delivery_tag(address),
        })
        .unwrap();
    let cutoff = tx.local_cutoff().unwrap();
    let child = LiveNetworkChild {
        ingress: pulsebeam_link::FabricIngress {
            association: right.id,
            key: NodeRouteKey::from_bytes(ssrc.get().to_be_bytes()),
            purpose: pulsebeam_link::FabricPurpose::Live,
        },
        destination: address,
        publication: ConnectionId::from_bytes([7; 16]).fixture_publication(TrackKind::Video, "wan"),
        incarnation: NonZeroU64::MIN,
        major: 0,
        revision: 0,
        sequence_anchor: 0,
        emission: Emission::new(Profile::H264, &[]).unwrap(),
        window: Some(window),
    };
    let mut now = Instant::now();
    let expires = now.checked_add(Duration::from_secs(2)).unwrap();
    let (installed, receipt) = oneshot::channel();
    let (report, mut active) = oneshot::channel();
    let activity = ReceiveActivity::default();
    right.install(
        now,
        ReceiveCommand::Live {
            expires,
            deadline: None,
            timing: None,
            activity: Some(activity.clone()),
            children: vec![ChildInstall {
                child,
                tx: tx.clone(),
                receipt,
            }],
            report,
        },
    );
    let spec = WindowSpec {
        authority: child.incarnation,
        direction: Direction::Send,
        start,
        ssrc,
        purpose: Purpose::Live,
        expires: expires.into_std(),
    };
    let sender = left.stage_send(now, window, spec).unwrap();
    let packet = |sequence, len| {
        FabricPacket::new(
            Header {
                profile: Profile::H264,
                marker: true,
                sequence,
                timestamp: 9000,
            },
            &[],
            vec![0x65; len].into_boxed_slice(),
        )
    };
    assert!(
        left.prepare(now, sender, start, &packet(0x18000, 1187).unwrap())
            .is_err()
    );
    left.activate_send(now, sender, window, spec).unwrap();
    let early = left
        .prepare(now, sender, start, &packet(0x18000, 1187).unwrap())
        .unwrap();
    assert!(matches!(
        submit_paced(&mut left, &mut now, early).await,
        Submission::Accepted { bytes: 1215 }
    ));
    pulsebeam_runtime::time::timeout(Duration::from_secs(1), right.socket.readable())
        .await
        .unwrap()
        .unwrap();
    right.poll(now).unwrap();
    assert_eq!(rx.try_recv_batch(&mut Vec::new(), 2), 0);
    assert!(!activity.observed());
    assert!(matches!(
        active.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    installed.send(Some(child)).unwrap();
    right.poll(now).unwrap();
    assert_eq!(active.try_recv().unwrap().unwrap(), window);
    for (delta, sequence, payload, sealed_len) in
        [(1, 0x18001, 1203, 1231), (2, 0x18002, 1204, 1232)]
    {
        let position = start | delta;
        let original = packet(sequence, payload).unwrap();
        let prepared = left.prepare(now, sender, position, &original).unwrap();
        assert!(
            matches!(submit_paced(&mut left, &mut now, prepared).await, Submission::Accepted { bytes } if bytes == sealed_len)
        );
        pulsebeam_runtime::time::timeout(Duration::from_secs(1), right.socket.readable())
            .await
            .unwrap()
            .unwrap();
        right.poll(now).unwrap();
        let mut frames = Vec::new();
        assert_eq!(rx.try_recv_batch(&mut frames, 2), 1);
        let frame = frames.pop().unwrap();
        assert_eq!(frame.tag, delivery_tag(address));
        assert!(
            matches!(frame.packet, ShardFrame::FabricWanImage { ingress, image: NetworkPacket { window: bound, position: received, packet, .. } }
            if ingress == child.ingress && bound == window && received == position
                && packet.same_content(&original)
                && packet.wan_received_bytes().is_some_and(|bytes| usize::from(bytes.get()) == sealed_len))
        );
    }
    assert!(activity.observed());
    let oversize = packet(0x18003, 1205).unwrap();
    assert!(left.prepare(now, sender, start + 3, &oversize).is_err());
    assert_eq!(
        right.socket.local_addr().ip(),
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
    );
    assert_eq!(wan::MAX_DATAGRAM_BYTES + 40 + 8, 1280);
    let position = start + 2;
    let repair_ssrc = NonZeroU32::new(0x0506_0710).unwrap();
    let repair_window = window.repair_window(position, repair_ssrc).unwrap();
    let repair = MediaRepairChild {
        parent: child,
        window: repair_window,
        position,
        expires,
    };
    let (_, tx) = control
        .bind_send(LinkDestination::Local {
            port,
            tag: delivery_tag(address),
        })
        .unwrap();
    let repair_cutoff = tx.local_cutoff().unwrap();
    let (installed, receipt) = oneshot::channel();
    let (report, mut active) = oneshot::channel();
    right.install(
        now,
        ReceiveCommand::MediaRepair {
            install: Box::new(ChildInstall {
                child: repair,
                tx,
                receipt,
            }),
            report,
        },
    );
    right.poll(now).unwrap();
    assert!(matches!(
        active.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    let repair_spec = WindowSpec {
        start: repair_window.start(),
        ssrc: repair_ssrc,
        purpose: Purpose::Repair { position },
        ..spec
    };
    let repair_send = left.stage_send(now, repair_window, repair_spec).unwrap();
    let original = packet(0x18002, 1188).unwrap();
    assert!(left.prepare(now, repair_send, position, &original).is_err());
    installed.send(Some(repair)).unwrap();
    right.poll(now).unwrap();
    assert_eq!(active.try_recv().unwrap().unwrap(), repair_window);
    left.activate_send(now, repair_send, repair_window, repair_spec)
        .unwrap();
    assert!(
        left.prepare(
            now,
            repair_send,
            position - 1,
            &packet(0x18001, 1188).unwrap()
        )
        .is_err()
    );
    let prepared = left.prepare(now, repair_send, position, &original).unwrap();
    assert!(matches!(
        submit_paced(&mut left, &mut now, prepared).await,
        Submission::Accepted { bytes: 1216 }
    ));
    pulsebeam_runtime::time::timeout(Duration::from_secs(1), right.socket.readable())
        .await
        .unwrap()
        .unwrap();
    right.poll(now).unwrap();
    let mut frames = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut frames, 2), 1);
    assert!(matches!(frames.pop().unwrap().packet,
        ShardFrame::FabricWanImage { ingress, image: NetworkPacket { window: bound, position: received, packet, .. } }
        if ingress == repair.ingress() && bound == repair_window && received == position && packet.same_content(&original)));
    left.driver.retire(repair_send);
    repair_cutoff.fence();
    right.poll(now).unwrap();

    let next_start = start + pulsebeam_core::fabric::wan::WINDOW_SPAN;
    let next_ssrc = NonZeroU32::new(0x0506_0709).unwrap();
    let next_window = Window::new(Profile::H264, next_start, 0, next_ssrc, 0).unwrap();
    let next_child = LiveNetworkChild {
        ingress: pulsebeam_link::FabricIngress {
            key: NodeRouteKey::from_bytes(next_ssrc.get().to_be_bytes()),
            ..child.ingress
        },
        window: Some(next_window),
        ..child
    };
    let (_, tx) = control
        .bind_send(LinkDestination::Local {
            port,
            tag: delivery_tag(address),
        })
        .unwrap();
    let next_cutoff = tx.local_cutoff().unwrap();
    let (installed, receipt) = oneshot::channel();
    let (report, mut active) = oneshot::channel();
    right.install(
        now,
        ReceiveCommand::Live {
            expires,
            deadline: None,
            timing: None,
            activity: None,
            children: vec![ChildInstall {
                child: next_child,
                tx,
                receipt,
            }],
            report,
        },
    );
    let next_spec = WindowSpec {
        start: next_start,
        ssrc: next_ssrc,
        ..spec
    };
    let next = left.stage_send(now, next_window, next_spec).unwrap();
    assert!(
        left.prepare(now, next, next_start, &packet(0x20000, 16).unwrap())
            .is_err()
    );
    installed.send(Some(next_child)).unwrap();
    right.poll(now).unwrap();
    assert_eq!(active.try_recv().unwrap().unwrap(), next_window);
    left.activate_send(now, next, next_window, next_spec)
        .unwrap();
    let positions = [
        (sender, window, next_start - 1, 0x1ffff),
        (next, next_window, next_start, 0x20000),
        (sender, window, next_start - 129, 0x1ff7f),
    ];
    for (route, _, position, sequence) in positions {
        let frame = left
            .prepare(now, route, position, &packet(sequence, 16).unwrap())
            .unwrap();
        assert!(matches!(
            submit_paced(&mut left, &mut now, frame).await,
            Submission::Accepted { .. }
        ));
    }
    let mut frames = Vec::new();
    pulsebeam_runtime::time::timeout(Duration::from_secs(1), async {
        while frames.len() < positions.len() {
            right.socket.readable().await.unwrap();
            right.poll(now).unwrap();
            rx.try_recv_batch(&mut frames, positions.len());
        }
    })
    .await
    .unwrap();
    for ((_, wanted, position, _), frame) in positions.into_iter().zip(frames) {
        assert!(
            matches!(frame.packet, ShardFrame::FabricWanImage { image: NetworkPacket { window, position: received, .. }, .. }
            if window == wanted && received == position)
        );
    }
    let queued_old = left
        .prepare(now, sender, start + 4, &packet(0x18004, 16).unwrap())
        .unwrap();
    assert!(matches!(
        submit_paced(&mut left, &mut now, queued_old).await,
        Submission::Accepted { .. }
    ));
    pulsebeam_runtime::time::timeout(Duration::from_secs(1), right.socket.readable())
        .await
        .unwrap()
        .unwrap();
    right.poll(now).unwrap();
    cutoff.fence();
    assert_eq!(rx.try_recv_batch(&mut Vec::new(), 2), 0);
    assert!(next_cutoff.is_live());
    drop(right);
    assert!(!next_cutoff.is_live());
}

#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn sparse_repair_requires_exact_receipt_and_live_parent() {
    use crate::participant::data_fabric::{
        admit_dd_fixture, maximum_dd_bytes, reconstruct_dd_fixture,
    };
    use pulsebeam_core::fabric::ExtensionKind;

    // Keep both the completed and queued repair windows active under one parent.
    let (mut left, mut right, mut control) = ready_pair_with_windows(3).await.unwrap();
    let now = Instant::now();
    let expires = now + Duration::from_secs(1);
    let start = (1 << 33) | 0x18000;
    let position = start | 0x7fff;
    let emission = Emission::new(Profile::Data, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let window = |ssrc| {
        Window::new(
            Profile::Data,
            start,
            2,
            NonZeroU32::new(ssrc).unwrap(),
            emission.wire_profile().1,
        )
        .unwrap()
    };
    let live = window(7);
    let repair = window(8);
    let key = |window: Window| NodeRouteKey::from_bytes(window.ssrc().get().to_be_bytes());
    let address = NodeRouteAddress::new(RouteId::new(ShardId::new(0), 1), 1);
    let receiver = ConnectionId::from_bytes([7; 16]);
    let parent = LiveNetworkChild {
        ingress: pulsebeam_link::FabricIngress {
            association: right.id,
            key: key(live),
            purpose: pulsebeam_link::FabricPurpose::Live,
        },
        destination: address,
        publication: receiver.fixture_publication(TrackKind::Data, "wan"),
        incarnation: NonZeroU64::MIN,
        major: pulsebeam_core::fabric::MAJOR,
        revision: 0,
        sequence_anchor: 0,
        emission,
        window: Some(live),
    };
    let child = RepairChild {
        parent: parent.ingress,
        route: RepairRoute {
            ingress: pulsebeam_link::FabricIngress {
                association: right.id,
                key: key(repair),
                purpose: pulsebeam_link::FabricPurpose::RepairOnly,
            },
            publication: parent.publication,
            source: parent.incarnation,
            branch: NonZeroU64::MIN,
            major: parent.major,
            revision: parent.revision,
            emission: parent.emission,
        },
        destination: address,
        receiver,
        demand: Demand {
            request: RepairRequest {
                id: NonZeroU64::MIN,
                fragment: RepairFragment {
                    unit_seq: 123,
                    index: 1,
                },
            },
            sequence: position,
            expires,
        },
        window: Some(repair),
    };
    let (port, mut rx) = control.create_port().unwrap();
    let route = || LinkDestination::Local {
        port,
        tag: delivery_tag(address),
    };
    let (_, live_tx) = control.bind_send(route()).unwrap();
    let (_, repair_tx) = control.bind_send(route()).unwrap();
    assert!(
        right
            .stage_repair_receive(now, parent, child, repair_tx.clone())
            .is_err()
    );
    let (installed, receipt) = oneshot::channel();
    let (report, mut active) = oneshot::channel();
    right.install(
        now,
        ReceiveCommand::Live {
            expires,
            deadline: None,
            timing: None,
            activity: None,
            children: vec![ChildInstall {
                child: parent,
                tx: live_tx.clone(),
                receipt,
            }],
            report,
        },
    );
    installed.send(Some(parent)).unwrap();
    right.poll(now).unwrap();
    assert_eq!(active.try_recv().unwrap().unwrap(), live);
    assert!(
        right
            .stage_repair_receive(now, parent, child, live_tx.clone())
            .is_err()
    );
    let stage = |right: &mut WanHost, now, child, tx| {
        let (installed, receipt) = oneshot::channel();
        let (report, active) = oneshot::channel();
        right.install(
            now,
            ReceiveCommand::Repair {
                parent,
                install: Box::new(ChildInstall { child, tx, receipt }),
                report,
            },
        );
        (installed, active)
    };
    let rejected_window = window(9);
    let mut rejected = child;
    rejected.route.ingress.key = key(rejected_window);
    rejected.window = Some(rejected_window);
    let (_, rejected_tx) = control.bind_send(route()).unwrap();
    let (installed, mut active) = stage(&mut right, now, rejected, rejected_tx.clone());
    installed.send(Some(child)).unwrap();
    right.poll(now).unwrap();
    assert!(matches!(active.try_recv().unwrap(), Err(Error::Binding)));
    assert!(!rejected_tx.is_live());

    let (installed, mut active) = stage(&mut right, now, child, repair_tx.clone());
    installed.send(Some(child)).unwrap();
    right.poll(now).unwrap();
    assert_eq!(active.try_recv().unwrap().unwrap(), repair);
    let mut wrong = child;
    wrong.demand.sequence ^= 1 << 32;

    let spec = WindowSpec {
        authority: parent.incarnation,
        direction: Direction::Send,
        start,
        ssrc: repair.ssrc(),
        purpose: Purpose::Repair { position },
        expires: expires.into_std(),
    };
    let (commands, inbox, media) = command_channel(left.id);
    left.commands = Some(inbox);
    left.egress.inbox = Some(media);
    let parent_cutoff = control.reserve_wan_send(left.id).unwrap();
    let mut parent_send = commands
        .install_send(
            parent_cutoff.clone(),
            live,
            live,
            WindowSpec {
                ssrc: live.ssrc(),
                purpose: Purpose::Live,
                ..spec
            },
            false,
            None,
        )
        .unwrap();
    left.poll(now).unwrap();
    assert!(matches!(
        parent_send.poll(),
        std::task::Poll::Ready(Some(_))
    ));
    let parent = ParentSend {
        cutoff: parent_cutoff.clone(),
        window: live,
    };
    let refused = control.reserve_wan_send(left.id).unwrap();
    let mut send = commands
        .install_send(
            refused.clone(),
            repair,
            live,
            spec,
            false,
            Some(SendParent::Repair(parent.clone())),
        )
        .unwrap();
    left.poll(now).unwrap();
    assert!(matches!(send.poll(), std::task::Poll::Ready(None)));
    assert!(!refused.is_live());
    let cutoff = control.reserve_wan_send(left.id).unwrap();
    let mut send = commands
        .install_send(
            cutoff,
            repair,
            repair,
            spec,
            false,
            Some(SendParent::Repair(parent.clone())),
        )
        .unwrap();
    left.poll(now).unwrap();
    let std::task::Poll::Ready(Some(sender)) = send.poll() else {
        panic!("exact WAN send receipt required")
    };
    let send_cutoff = sender.cutoff();
    let fragment = child.demand.request.fragment;
    let raw = maximum_dd_bytes(fragment);
    #[cfg(feature = "transport-conformance")]
    let ingress = cross_stock_sctp(StockSctpLeg::Ingress, &raw).unwrap();
    #[cfg(feature = "transport-conformance")]
    assert_eq!(ingress, raw);
    #[cfg(feature = "transport-conformance")]
    let ingress = ingress.as_slice();
    #[cfg(not(feature = "transport-conformance"))]
    let ingress = &raw;
    let original = admit_dd_fixture(ingress, u64::from(child.demand.wire_sequence()), now);
    assert!(!sender.try_send(now, expires, position ^ (1 << 32), original.clone()));
    assert!(!sender.try_send(now, expires, position - 1, original.clone()));
    let later = now + Duration::from_millis(1);
    assert!(sender.try_send(now, later, position, original.clone()));
    left.poll(later).unwrap();
    right.poll(later).unwrap();
    assert_eq!(rx.try_recv_batch(&mut Vec::new(), 2), 0);
    assert!(sender.try_send(later, expires, position, original.clone()));
    left.poll(later).unwrap();
    let now = later;
    pulsebeam_runtime::time::timeout(Duration::from_secs(1), right.socket.readable())
        .await
        .unwrap()
        .unwrap();
    right.poll(now).unwrap();
    let mut frames = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut frames, 2), 1);
    let ShardFrame::FabricWanImage { ingress, image } = frames.pop().unwrap().packet else {
        panic!("expected WAN repair")
    };
    assert_eq!(ingress, child.route.ingress);
    assert!(wrong.accept_image(image.clone()).is_none());
    let repaired = child.accept_image(image).unwrap();
    assert!(repaired.same_content(&original));
    let output = reconstruct_dd_fixture(&repaired, Some(fragment));
    assert_eq!(output, raw);
    #[cfg(feature = "transport-conformance")]
    assert_eq!(
        cross_stock_sctp(StockSctpLeg::Egress, &output).unwrap(),
        raw
    );

    let fresh = live
        .repair_window(position, NonZeroU32::new(10).unwrap())
        .unwrap();
    let mut queued_child = child;
    queued_child.route.ingress.key = key(fresh);
    queued_child.window = Some(fresh);
    let (_, queued_rx) = control.bind_send(route()).unwrap();
    let (installed, mut active) = stage(&mut right, now, queued_child, queued_rx);
    installed.send(Some(queued_child)).unwrap();
    right.poll(now).unwrap();
    assert_eq!(active.try_recv().unwrap().unwrap(), fresh);
    let queued_cutoff = control.reserve_wan_send(left.id).unwrap();
    let mut pending = commands
        .install_send(
            queued_cutoff.clone(),
            fresh,
            fresh,
            WindowSpec {
                ssrc: fresh.ssrc(),
                ..spec
            },
            false,
            Some(SendParent::Repair(parent)),
        )
        .unwrap();
    left.poll(now).unwrap();
    let std::task::Poll::Ready(Some(queued)) = pending.poll() else {
        panic!("fresh WAN repair send must be active before cutoff")
    };
    assert!(queued.try_send(now, expires, position, original.clone()));
    parent_cutoff.fence();
    assert!(!queued.try_send(now, expires, position, original));
    left.poll(now).unwrap();
    right.poll(now).unwrap();
    assert_eq!(rx.try_recv_batch(&mut frames, 2), 0);
    assert!(!send_cutoff.is_live());
    assert!(!queued_cutoff.is_live());

    let cutoff = repair_tx.local_cutoff().unwrap();
    live_tx.local_cutoff().unwrap().fence();
    right.poll(now).unwrap();
    assert!(!cutoff.is_live());
}

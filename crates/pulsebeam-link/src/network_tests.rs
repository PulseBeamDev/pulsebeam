#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "bounded deterministic fixtures with a single injected clock anchor"
)]

use super::*;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Debug)]
enum Packet {
    Wire(Arc<[u8]>),
    Local,
}
struct Codec;
impl WireCodec<Packet> for Codec {
    fn encoded_len(&self, packet: &Packet) -> Result<usize, CodecError> {
        match packet {
            Packet::Wire(bytes) => Ok(bytes.len()),
            Packet::Local => Err(CodecError::Unsupported),
        }
    }
    fn encode(&self, packet: &Packet, dst: &mut [u8]) -> Result<usize, CodecError> {
        let Packet::Wire(bytes) = packet else {
            return Err(CodecError::Unsupported);
        };
        dst.get_mut(..bytes.len())
            .ok_or(CodecError::TooLarge)?
            .copy_from_slice(bytes);
        Ok(bytes.len())
    }
    fn decode(&self, wire: &[u8]) -> Result<Packet, CodecError> {
        Ok(Packet::Wire(wire.into()))
    }
}

struct FabricImageCodec;

impl FabricCodec<Packet> for FabricImageCodec {
    fn encoded_len(&self, packet: &Packet) -> Result<usize, CodecError> {
        Codec.encoded_len(packet)
    }

    fn encode(
        &self,
        packet: &Packet,
        key: NodeRouteKey,
        dst: &mut [u8],
    ) -> Result<usize, CodecError> {
        let len = Codec.encode(packet, dst)?;
        dst.get_mut(..len)
            .and_then(|image| image.get_mut(8..12))
            .ok_or(CodecError::Malformed)?
            .copy_from_slice(&key.to_bytes());
        Ok(len)
    }

    fn encode_positioned(
        &self,
        packet: &Packet,
        key: NodeRouteKey,
        position: Option<u64>,
        dst: &mut [u8],
    ) -> Result<usize, CodecError> {
        let len = self.encode(packet, key, dst)?;
        if let Some(position) = position {
            dst[..8].copy_from_slice(&position.to_be_bytes());
        }
        Ok(len)
    }

    fn route_key(&self, image: &[u8]) -> Result<NodeRouteKey, CodecError> {
        let bytes = image
            .get(8..12)
            .ok_or(CodecError::Malformed)?
            .try_into()
            .map_err(|_| CodecError::Malformed)?;
        Ok(NodeRouteKey::from_bytes(bytes))
    }

    fn decode(&self, image: &[u8]) -> Result<Packet, CodecError> {
        Codec.decode(image)
    }
}

struct Pair {
    a: LinkControl<Packet>,
    b: LinkControl<Packet>,
    aid: NetworkId,
    bid: NetworkId,
    ad: io::NetworkDriver<Packet>,
    bd: io::NetworkDriver<Packet>,
    aa: SocketAddr,
    ba: SocketAddr,
    aout: VecDeque<Vec<u8>>,
    bout: VecDeque<Vec<u8>>,
    ticks: Arc<AtomicU64>,
    a_peer: PeerConfig,
    b_peer: PeerConfig,
    control: Option<[PeerControl; 2]>,
    a_epochs: [bool; 4],
    a_full_image_epochs: [bool; 4],
    reject_a: bool,
    short_a: bool,
    drop_b: bool,
    budget: usize,
}

impl Pair {
    fn new() -> Self {
        Self::with_key_limit(None)
    }

    fn with_key_limit(key_limit: Option<u64>) -> Self {
        Self::configured(key_limit, false, false)
    }

    fn with_backend(quic: bool) -> Self {
        Self::configured(None, quic, false)
    }

    fn configured(key_limit: Option<u64>, quic: bool, control: bool) -> Self {
        Self::configured_profile(key_limit, quic, control, None)
    }

    fn configured_profile(
        key_limit: Option<u64>,
        quic: bool,
        control: bool,
        fabric: Option<FabricPath>,
    ) -> Self {
        let ticks = Arc::new(AtomicU64::new(0));
        let clock = ticks.clone();
        let mut a = LinkControl::new(move || LinkTime(clock.load(Ordering::Relaxed))).unwrap();
        let clock = ticks.clone();
        let mut b = LinkControl::new(move || LinkTime(clock.load(Ordering::Relaxed))).unwrap();
        let aa = "127.0.0.1:4000".parse().unwrap();
        let ba = "127.0.0.1:4001".parse().unwrap();
        let (a_peer, b_peer): (PeerConfig, PeerConfig) = if quic {
            let ca = QuicCredential::generate().unwrap();
            let cb = QuicCredential::generate().unwrap();
            (
                QuicPeer {
                    endpoint: ba,
                    credential: ca.clone(),
                    expected_peer: cb.fingerprint(),
                    role: PeerRole::Initiator,
                    class: NetworkClass::Lan,
                    tuning: QuicTuning::default(),
                }
                .into(),
                QuicPeer {
                    endpoint: aa,
                    credential: cb,
                    expected_peer: ca.fingerprint(),
                    role: PeerRole::Responder,
                    class: NetworkClass::Wan,
                    tuning: QuicTuning::default(),
                }
                .into(),
            )
        } else {
            let ca = SecretCredential::generate().unwrap();
            let cb = SecretCredential::generate().unwrap();
            let profile = |peer| -> PeerConfig {
                match fabric {
                    Some(path) => NetworkPeer::with_fabric_path(peer, path).into(),
                    None => peer.into(),
                }
            };
            (
                profile(NetworkPeer {
                    endpoint: ba,
                    credential: ca.clone(),
                    expected_peer: cb.fingerprint(),
                    role: PeerRole::Initiator,
                    class: NetworkClass::Lan,
                }),
                profile(NetworkPeer {
                    endpoint: aa,
                    credential: cb,
                    expected_peer: ca.fingerprint(),
                    role: PeerRole::Responder,
                    class: NetworkClass::Wan,
                }),
            )
        };
        let (aid, bid, control) =
            if let (PeerConfig::Fabric(left), PeerConfig::Fabric(right)) = (&a_peer, &b_peer) {
                if control {
                    let (aid, left) = a
                        .open_fabric_network_with_control(left.clone(), FabricImageCodec)
                        .unwrap();
                    let (bid, right) = b
                        .open_fabric_network_with_control(right.clone(), FabricImageCodec)
                        .unwrap();
                    (aid, bid, Some([left, right]))
                } else {
                    (
                        a.open_fabric_network(left.clone(), FabricImageCodec)
                            .unwrap(),
                        b.open_fabric_network(right.clone(), FabricImageCodec)
                            .unwrap(),
                        None,
                    )
                }
            } else if control {
                let (aid, left) = a.open_network_with_control(a_peer.clone(), Codec).unwrap();
                let (bid, right) = b.open_network_with_control(b_peer.clone(), Codec).unwrap();
                (aid, bid, Some([left, right]))
            } else {
                (
                    a.open_network(a_peer.clone(), Codec).unwrap(),
                    b.open_network(b_peer.clone(), Codec).unwrap(),
                    None,
                )
            };
        let mut ad = a.take_network_driver(aid).unwrap();
        let mut bd = b.take_network_driver(bid).unwrap();
        if let Some(limit) = key_limit {
            ad.set_test_key_limit(limit);
            bd.set_test_key_limit(limit);
        }
        let instant = Instant::now();
        ad.start(LinkTime(0), instant).unwrap();
        bd.start(LinkTime(0), instant).unwrap();
        Self {
            a,
            b,
            aid,
            bid,
            ad,
            bd,
            aa,
            ba,
            aout: VecDeque::new(),
            bout: VecDeque::new(),
            ticks,
            a_peer,
            b_peer,
            control,
            a_epochs: [false; 4],
            a_full_image_epochs: [false; 4],
            reject_a: false,
            short_a: false,
            drop_b: false,
            budget: 256,
        }
    }

    fn reset(&mut self) {
        self.a.close_network(self.aid);
        self.b.close_network(self.bid);
        self.aout.clear();
        self.bout.clear();
        let open = |link: &mut LinkControl<Packet>, peer: &PeerConfig| {
            match peer {
                PeerConfig::Fabric(peer) => {
                    link.open_fabric_network(peer.clone(), FabricImageCodec)
                }
                _ => link.open_network(peer.clone(), Codec),
            }
            .unwrap()
        };
        self.aid = open(&mut self.a, &self.a_peer);
        self.bid = open(&mut self.b, &self.b_peer);
        self.ad = self.a.take_network_driver(self.aid).unwrap();
        self.bd = self.b.take_network_driver(self.bid).unwrap();
        let now = LinkTime(self.ticks.load(Ordering::Relaxed));
        let instant = Instant::now();
        self.ad.start(now, instant).unwrap();
        self.bd.start(now, instant).unwrap();
    }

    fn poll_a(&mut self, now: LinkTime) -> Result<(), io::Error> {
        self.ad.poll(now, self.budget, |datagram| {
            assert_eq!(datagram.destination, self.ba);
            assert!(datagram.payload.len() <= wire::UDP_BUDGET);
            if self.reject_a {
                return io::SendOutcome::Rejected;
            }
            if self.short_a {
                return io::SendOutcome::Accepted {
                    bytes: datagram.payload.len().saturating_sub(1),
                };
            }
            // DTLS 1.3 unified headers expose the low epoch bits. Observation
            // only: Link transport accounting must never use record identity.
            if let Some(header) = datagram.payload.first()
                && header & 0xe0 == 0x20
            {
                let epoch = usize::from(header & 3);
                self.a_epochs[epoch] = true;
                if datagram.payload.len() == 1180 {
                    self.a_full_image_epochs[epoch] = true;
                }
            }
            self.aout.push_back(datagram.payload.to_vec());
            io::SendOutcome::Accepted {
                bytes: datagram.payload.len(),
            }
        })?;
        Ok(())
    }

    fn poll_b(&mut self, now: LinkTime) -> Result<(), io::Error> {
        self.bd.poll(now, self.budget, |datagram| {
            assert_eq!(datagram.destination, self.aa);
            assert!(datagram.payload.len() <= wire::UDP_BUDGET);
            if !self.drop_b {
                self.bout.push_back(datagram.payload.to_vec());
            }
            io::SendOutcome::Accepted {
                bytes: datagram.payload.len(),
            }
        })?;
        Ok(())
    }

    fn step(&mut self) {
        self.try_step().unwrap();
    }

    fn try_step(&mut self) -> Result<(), io::Error> {
        let now = LinkTime(self.ticks.fetch_add(1_000_000, Ordering::Relaxed) + 1_000_000);
        self.poll_a(now)?;
        self.poll_b(now)?;
        for _ in 0..256 {
            let mut progress = false;
            if let Some(bytes) = self.aout.pop_front() {
                let observed = ReceiveTime {
                    monotonic: now,
                    wall: Some(SystemTime::UNIX_EPOCH),
                };
                match self.bd.receive(self.aa, &bytes, observed) {
                    Ok(()) => {
                        progress = true;
                        self.poll_b(now)?;
                    }
                    Err(io::Error::Busy) => self.aout.push_front(bytes),
                    Err(error) => return Err(error),
                }
            }
            if let Some(bytes) = self.bout.pop_front() {
                let observed = ReceiveTime {
                    monotonic: now,
                    wall: Some(SystemTime::UNIX_EPOCH),
                };
                match self.ad.receive(self.ba, &bytes, observed) {
                    Ok(()) => {
                        progress = true;
                        self.poll_a(now)?;
                    }
                    Err(io::Error::Busy) => self.bout.push_front(bytes),
                    Err(error) => return Err(error),
                }
            }
            if !progress {
                break;
            }
        }
        Ok(())
    }

    fn connect(&mut self) {
        for _ in 0..5000 {
            self.step();
            if self.ad.health().ready && self.bd.health().ready {
                return;
            }
        }
        panic!("virtual-time handshake failed to become ready");
    }

    fn route(&mut self, ports: &[(PortId, DeliveryTag)]) -> (BindingId, LinkTx<Packet>) {
        let (_, key) = self.b.bind_receive(self.bid, ports).unwrap();
        self.a
            .bind_send(Destination::Network {
                peer: self.aid,
                address: key,
            })
            .unwrap()
    }
}

#[test]
fn negotiated_fabric_link_preserves_full_images_through_rekey_and_path_shrink() {
    let path = FabricPath {
        udp_payload_limit: 1200,
        additional_overhead: 20,
    };
    let mut pair = Pair::configured_profile(Some(8), false, false, Some(path));
    let (port, mut rx) = pair.b.create_port().unwrap();
    assert_eq!(
        pair.b
            .stage_fabric_receive(pair.bid, 3, &[(port, DeliveryTag(91))], |_| true),
        Err(BindError::UnsupportedProfile)
    );
    assert_eq!(
        pair.a
            .bind_fabric_send(pair.aid, NodeRouteKey::from_bytes([1, 3, 0, 0]))
            .unwrap_err(),
        BindError::UnsupportedProfile
    );
    pair.connect();
    assert_eq!(
        pair.a
            .bind_fabric_send(pair.aid, NodeRouteKey::from_bytes([0; 4]))
            .unwrap_err(),
        BindError::Closed
    );
    let (binding, key) = pair
        .b
        .stage_fabric_receive(pair.bid, 3, &[(port, DeliveryTag(91))], |_| true)
        .unwrap();
    pair.b.activate_fabric_receive(binding).unwrap();
    let (_, tx) = pair.a.bind_fabric_send(pair.aid, key).unwrap();
    let mut image = vec![0x65; wire::FABRIC_IMAGE];
    image[..24].copy_from_slice(&[
        0x90, 0xe0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0xbe, 0xde, 0, 2, 0x16, 0, 0, 0, 1, 0, 0, 0xc0,
    ]);
    for seq in 1_u16..=20 {
        image[2..4].copy_from_slice(&seq.to_be_bytes());
        let positioned = seq == 1;
        let position = (1_u64 << 32) | u64::from(seq);
        if positioned {
            tx.try_send_fabric(
                Packet::Wire(image.clone().into()),
                position,
                LinkTime(u64::MAX),
            )
            .unwrap();
        } else {
            tx.try_send(Packet::Wire(image.clone().into()), LinkTime(u64::MAX))
                .unwrap();
        }
        for _ in 0..20 {
            pair.step();
        }
        let mut out = Vec::new();
        assert_eq!(rx.try_recv_batch(&mut out, 2), 1);
        assert_eq!(out[0].tag, DeliveryTag(91));
        assert_eq!(
            out[0].fabric,
            Some(FabricIngress {
                association: pair.bid,
                key,
                purpose: FabricPurpose::Live,
            })
        );
        let Packet::Wire(ref actual) = out[0].packet else {
            panic!("expected the protected Fabric image")
        };
        let mut expected = image.clone();
        expected[8..12].copy_from_slice(&key.to_bytes());
        if positioned {
            expected[..8].copy_from_slice(&position.to_be_bytes());
        }
        assert_eq!(actual.as_ref(), expected);
    }
    assert!(
        pair.a_full_image_epochs
            .into_iter()
            .filter(|seen| *seen)
            .count()
            > 1
    );
    assert_eq!(pair.ad.health().sent, 20);
    assert_eq!(pair.ad.health().bytes_in_flight, 0);

    tx.try_send(Packet::Wire(image.clone().into()), LinkTime(u64::MAX))
        .unwrap();
    pair.ad
        .set_fabric_path(FabricPath {
            udp_payload_limit: 1199,
            ..path
        })
        .unwrap();
    assert_eq!(
        tx.try_send(Packet::Wire(image.into()), LinkTime(u64::MAX))
            .unwrap_err()
            .kind,
        SendErrorKind::NotReady
    );
    pair.step();
    assert_eq!(pair.ad.health().sent, 20);
    assert_eq!(pair.ad.health().send_failed, 1);
    assert_eq!(rx.try_recv_batch(&mut Vec::new(), 1), 0);
}

#[test]
fn fabric_route_cutoff_fences_all_queued_members_and_reuse_changes_generation() {
    let path = FabricPath {
        udp_payload_limit: 1200,
        additional_overhead: 0,
    };
    let mut pair = Pair::configured_profile(None, false, false, Some(path));
    pair.connect();
    let (left, mut left_rx) = pair.b.create_port().unwrap();
    let (right, mut right_rx) = pair.b.create_port().unwrap();
    let (original, key) = pair
        .b
        .stage_fabric_receive(
            pair.bid,
            9,
            &[(left, DeliveryTag(1)), (right, DeliveryTag(2))],
            |packet| matches!(packet, Packet::Wire(image) if image.len() == 24),
        )
        .unwrap();
    assert_eq!(key.to_bytes(), [1, 9, 0, 0]);
    let (_, tx) = pair
        .a
        .bind_send(Destination::Network {
            peer: pair.aid,
            address: key,
        })
        .unwrap();
    let image = || Packet::Wire(vec![0; 24].into());
    tx.try_send(image(), LinkTime(u64::MAX)).unwrap();
    for _ in 0..30 {
        pair.step();
    }
    assert_eq!(pair.bd.health().received, 1);
    assert_eq!(pair.bd.health().stale, 1);
    assert_eq!(left_rx.try_recv_batch(&mut Vec::new(), 1), 0);
    assert_eq!(right_rx.try_recv_batch(&mut Vec::new(), 1), 0);
    pair.b.activate_fabric_receive(original).unwrap();

    tx.try_send(Packet::Wire(vec![0; 25].into()), LinkTime(u64::MAX))
        .unwrap();
    for _ in 0..30 {
        pair.step();
    }
    assert_eq!(pair.bd.health().received, 2);
    assert_eq!(pair.bd.health().invalid, 1);
    assert_eq!(left_rx.try_recv_batch(&mut Vec::new(), 1), 0);
    assert_eq!(right_rx.try_recv_batch(&mut Vec::new(), 1), 0);

    tx.try_send(image(), LinkTime(u64::MAX)).unwrap();
    for _ in 0..30 {
        pair.step();
    }
    assert_eq!(pair.bd.health().received, 3);
    pair.b.receive_port_cutoff(original, left).unwrap().fence();
    assert_eq!(
        pair.b.activate_fabric_receive(original),
        Err(BindError::Closed)
    );
    assert_eq!(left_rx.try_recv_batch(&mut Vec::new(), 1), 0);
    assert_eq!(right_rx.try_recv_batch(&mut Vec::new(), 1), 0);
    pair.b.unbind(original);

    let (_, other) = pair
        .b
        .stage_fabric_receive(pair.bid, 9, &[(right, DeliveryTag(3))], |_| true)
        .unwrap();
    assert_eq!(other.to_bytes(), [1, 9, 0, 1]);
    pair.ticks.fetch_add(2_000_000_000, Ordering::Relaxed);
    let (successor, replacement) = pair
        .b
        .stage_fabric_receive(pair.bid, 9, &[(right, DeliveryTag(4))], |_| true)
        .unwrap();
    pair.b.activate_fabric_receive(successor).unwrap();
    assert_eq!(replacement.to_bytes(), [2, 9, 0, 0]);
    let (_, fresh) = pair
        .a
        .bind_send(Destination::Network {
            peer: pair.aid,
            address: replacement,
        })
        .unwrap();
    tx.try_send(image(), LinkTime(u64::MAX)).unwrap();
    fresh.try_send(image(), LinkTime(u64::MAX)).unwrap();
    for _ in 0..100 {
        pair.step();
    }
    let mut received = Vec::new();
    assert_eq!(right_rx.try_recv_batch(&mut received, 2), 1);
    assert_eq!(received[0].tag, DeliveryTag(4));
    assert!(pair.bd.health().stale > 0);
}

#[test]
fn one_authenticated_wire_copy_fans_out_and_feedback_ignores_local_overload() {
    for quic in [false, true] {
        one_authenticated_wire_copy_fans_out_and_feedback_ignores_local_overload_case(quic);
    }
}

fn one_authenticated_wire_copy_fans_out_and_feedback_ignores_local_overload_case(quic: bool) {
    let mut pair = Pair::with_backend(quic);
    assert_eq!(pair.ad.health().feedback_interval, Duration::from_millis(5));
    assert_eq!(
        pair.bd.health().feedback_interval,
        Duration::from_millis(10)
    );
    let (port1, mut rx1) = pair.b.create_port().unwrap();
    let (port2, mut rx2) = pair.b.create_port().unwrap();
    let (_, local) = pair
        .b
        .bind_send(Destination::Local {
            port: port1,
            tag: DeliveryTag(0),
        })
        .unwrap();
    let (_, tx) = pair.route(&[(port1, DeliveryTag(1)), (port2, DeliveryTag(2))]);
    assert_eq!(
        tx.try_send(Packet::Local, LinkTime(u64::MAX))
            .unwrap_err()
            .kind,
        SendErrorKind::NotReady
    );
    pair.connect();
    assert_eq!(
        tx.try_send(Packet::Local, LinkTime(u64::MAX))
            .unwrap_err()
            .kind,
        SendErrorKind::Unsupported
    );
    assert_eq!(
        tx.try_send(Packet::Wire(vec![0; 1200].into()), LinkTime(u64::MAX))
            .unwrap_err()
            .kind,
        SendErrorKind::TooLarge
    );
    for _ in 0..1024 {
        local.try_send(Packet::Local, LinkTime(u64::MAX)).unwrap();
    }
    tx.try_send(Packet::Wire(Arc::from([1u8, 2, 3])), LinkTime(u64::MAX))
        .unwrap();
    for _ in 0..100 {
        pair.step();
    }
    assert_eq!(pair.ad.health().sent, 1);
    assert_eq!(pair.bd.health().received, 1);
    assert_eq!(pair.bd.health().local_full, 1);
    assert_eq!(pair.ad.health().bytes_in_flight, 0);
    let mut batch = Vec::new();
    assert_eq!(rx2.try_recv_batch(&mut batch, 10), 1);
    assert_eq!(batch[0].tag, DeliveryTag(2));
    assert_eq!(batch[0].received_at.wall, Some(SystemTime::UNIX_EPOCH));
    assert!(matches!(&batch[0].packet, Packet::Wire(bytes) if bytes.as_ref() == [1, 2, 3]));
    batch.clear();
    assert_eq!(rx1.try_recv_batch(&mut batch, 1024), 1024);
}

#[test]
fn one_unit_poll_budgets_preserve_outbound_and_feedback_progress() {
    for quic in [false, true] {
        one_unit_poll_budgets_preserve_outbound_and_feedback_progress_case(quic);
    }
}

fn one_unit_poll_budgets_preserve_outbound_and_feedback_progress_case(quic: bool) {
    let mut pair = Pair::with_backend(quic);
    let (port, mut rx) = pair.b.create_port().unwrap();
    let (_, tx) = pair.route(&[(port, DeliveryTag(1))]);
    pair.budget = 1;
    pair.connect();
    tx.try_send(Packet::Wire(Arc::from([42u8])), LinkTime(u64::MAX))
        .unwrap();
    for _ in 0..100 {
        pair.step();
    }
    let mut batch = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut batch, 1), 1);
    assert!(matches!(&batch[0].packet, Packet::Wire(bytes) if bytes.as_ref() == [42]));
    assert_eq!(pair.ad.health().sent, 1);
    assert_eq!(pair.ad.health().bytes_in_flight, 0);
}

#[test]
fn rejected_udp_attempt_is_not_committed_and_stale_queued_work_is_fenced() {
    let mut pair = Pair::new();
    let (port, mut rx) = pair.b.create_port().unwrap();
    let (binding, tx) = pair.route(&[(port, DeliveryTag(1))]);
    pair.connect();
    tx.try_send(Packet::Wire(Arc::from([1u8])), LinkTime(u64::MAX))
        .unwrap();
    pair.a.unbind(binding);
    for _ in 0..10 {
        pair.step();
    }
    assert_eq!(pair.ad.health().sent, 0);
    assert_eq!(pair.ad.health().stale, 1);
    let (_, fresh) = pair.route(&[(port, DeliveryTag(2))]);
    pair.reject_a = true;
    fresh
        .try_send(Packet::Wire(Arc::from([2u8])), LinkTime(u64::MAX))
        .unwrap();
    for _ in 0..10 {
        pair.step();
    }
    assert!(pair.ad.health().send_failed > 0);
    assert_eq!(pair.ad.health().sent, 0);
    assert_eq!(pair.ad.health().bytes_in_flight, 0);
    pair.reject_a = false;
    pair.short_a = true;
    let rejected = pair.ad.health().send_failed;
    fresh
        .try_send(Packet::Wire(Arc::from([4u8])), LinkTime(u64::MAX))
        .unwrap();
    for _ in 0..10 {
        pair.step();
    }
    assert!(pair.ad.health().send_failed > rejected);
    assert_eq!(pair.ad.health().sent, 0);
    assert_eq!(pair.ad.health().bytes_in_flight, 0);
    pair.short_a = false;
    fresh
        .try_send(Packet::Wire(Arc::from([3u8])), LinkTime(u64::MAX))
        .unwrap();
    for _ in 0..100 {
        pair.step();
    }
    let mut batch = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut batch, 10), 1);
    assert!(matches!(&batch[0].packet, Packet::Wire(bytes) if bytes.as_ref() == [3]));
    assert_eq!(pair.ad.health().sent, 1);
    assert_eq!(pair.ad.health().bytes_in_flight, 0);
}

#[test]
fn receive_retirement_fences_queued_payloads_without_withholding_transport_feedback() {
    for quic in [false, true] {
        receive_retirement_fences_queued_payloads_without_withholding_transport_feedback_case(quic);
    }
}

fn receive_retirement_fences_queued_payloads_without_withholding_transport_feedback_case(
    quic: bool,
) {
    let mut pair = Pair::with_backend(quic);
    let (port, mut rx) = pair.b.create_port().unwrap();
    let (binding, key) = pair
        .b
        .bind_receive(pair.bid, &[(port, DeliveryTag(1))])
        .unwrap();
    let (_, old) = pair
        .a
        .bind_send(Destination::Network {
            peer: pair.aid,
            address: key,
        })
        .unwrap();
    pair.connect();
    old.try_send(Packet::Wire(Arc::from([1u8])), LinkTime(u64::MAX))
        .unwrap();
    for _ in 0..20 {
        pair.step();
    }
    assert_eq!(pair.bd.health().received, 1);
    let late_cutoff = pair.b.receive_port_cutoff(binding, port).unwrap();
    pair.b.unbind(binding);
    old.try_send(Packet::Wire(Arc::from([2u8])), LinkTime(u64::MAX))
        .unwrap();
    let (_, replacement_key) = pair
        .b
        .bind_receive(pair.bid, &[(port, DeliveryTag(2))])
        .unwrap();
    assert_ne!(key, replacement_key);
    late_cutoff.fence();
    let (_, replacement) = pair
        .a
        .bind_send(Destination::Network {
            peer: pair.aid,
            address: replacement_key,
        })
        .unwrap();
    replacement
        .try_send(Packet::Wire(Arc::from([3u8])), LinkTime(u64::MAX))
        .unwrap();
    for _ in 0..100 {
        pair.step();
    }
    let mut batch = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut batch, 10), 1);
    assert_eq!(rx.drop_counts().stale, 1);
    assert_eq!(batch[0].tag, DeliveryTag(2));
    assert!(matches!(&batch[0].packet, Packet::Wire(bytes) if bytes.as_ref() == [3]));
    assert_eq!(pair.bd.health().received, 3);
    assert_eq!(pair.bd.health().stale, 1);
    assert_eq!(pair.bd.take_unknown_label(), Some(key));
    assert_eq!(pair.bd.take_unknown_label(), None);
    assert_eq!(pair.ad.health().sent, 3);
    assert_eq!(pair.ad.health().bytes_in_flight, 0);
}

#[test]
fn provisioned_control_shares_an_authenticated_link_without_entering_media_routes() {
    for profile in [
        None,
        Some(FabricPath {
            udp_payload_limit: 1200,
            additional_overhead: 0,
        }),
    ] {
        control_lane_case(profile);
    }
}

fn control_lane_case(profile: Option<FabricPath>) {
    let mut pair = Pair::configured_profile(None, false, true, profile);
    let [left, mut right] = pair.control.take().unwrap();
    assert_eq!(
        left.try_send(Box::new([1]), LinkTime(u64::MAX)),
        Err(SendErrorKind::NotReady)
    );
    let (port, mut rx) = pair.b.create_port().unwrap();
    assert!(
        pair.a
            .bind_send(Destination::Network {
                peer: pair.aid,
                address: NodeRouteKey::from_bytes([0; 4]),
            })
            .is_err()
    );
    pair.connect();
    assert!(left.is_ready() && right.is_ready());
    let (media, packet) = if profile.is_some() {
        let (binding, key) = pair
            .b
            .stage_fabric_receive(pair.bid, 3, &[(port, DeliveryTag(7))], |_| true)
            .unwrap();
        pair.b.activate_fabric_receive(binding).unwrap();
        let mut packet = [0; 24];
        packet[23] = 3;
        (
            pair.a.bind_fabric_send(pair.aid, key).unwrap().1,
            Packet::Wire(Arc::from(packet)),
        )
    } else {
        (
            pair.route(&[(port, DeliveryTag(7))]).1,
            Packet::Wire(Arc::from([3])),
        )
    };
    assert_eq!(
        left.try_send(
            vec![0; CONTROL_FRAME_BYTES + 1].into_boxed_slice(),
            LinkTime(u64::MAX)
        ),
        Err(SendErrorKind::TooLarge)
    );
    left.try_send(Box::new([2]), LinkTime(u64::MAX)).unwrap();
    media.try_send(packet, LinkTime(u64::MAX)).unwrap();
    for _ in 0..20 {
        pair.step();
    }
    let message = futures_lite::future::block_on(futures_lite::future::poll_once(right.recv()));
    assert_eq!(message.flatten().as_deref(), Some([2].as_slice()));
    let mut batch = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut batch, 10), 1);
    assert_eq!(batch[0].tag, DeliveryTag(7));
    assert!(matches!(&batch[0].packet, Packet::Wire(bytes) if bytes.last() == Some(&3)));
    assert!(right.try_recv().is_none());
    pair.b.close_network(pair.bid);
    assert!(right.is_closed());
    assert!(futures_lite::future::block_on(right.recv()).is_none());
}

#[test]
fn automatic_key_updates_preserve_link_identity_and_flight_accounting() {
    let mut pair = Pair::with_key_limit(Some(10));
    let (port, mut rx) = pair.b.create_port().unwrap();
    let (_, tx) = pair.route(&[(port, DeliveryTag(1))]);
    pair.connect();
    for value in 0u8..32 {
        tx.try_send(Packet::Wire(Arc::from([value])), LinkTime(u64::MAX))
            .unwrap();
        for _ in 0..20 {
            pair.step();
        }
    }
    assert!(
        pair.a_epochs[0],
        "application epoch must advance past its initial epoch 3"
    );
    assert!(pair.ad.health().ready && pair.bd.health().ready);
    assert_eq!(pair.ad.health().sent, 32);
    assert_eq!(pair.bd.health().received, 32);
    assert_eq!(pair.ad.health().bytes_in_flight, 0);
    let mut batch = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut batch, 64), 32);
    for (index, received) in batch.iter().enumerate() {
        assert!(
            matches!(&received.packet, Packet::Wire(bytes) if bytes.as_ref() == [u8::try_from(index).unwrap()])
        );
    }
}

#[test]
fn association_replacement_reuses_pinned_identities_but_not_bindings_or_queued_packets() {
    for quic in [false, true] {
        association_replacement_reuses_pinned_identities_but_not_bindings_or_queued_packets_case(
            quic,
        );
    }
}

fn association_replacement_reuses_pinned_identities_but_not_bindings_or_queued_packets_case(
    quic: bool,
) {
    let mut pair = Pair::with_backend(quic);
    let (port, mut rx) = pair.b.create_port().unwrap();
    assert_eq!(
        pair.b
            .stage_fabric_receive(pair.bid, 3, &[(port, DeliveryTag(1))], |_| true),
        Err(BindError::UnsupportedProfile)
    );
    let (_, key) = pair
        .b
        .bind_receive(pair.bid, &[(port, DeliveryTag(1))])
        .unwrap();
    let (_, old) = pair
        .a
        .bind_send(Destination::Network {
            peer: pair.aid,
            address: key,
        })
        .unwrap();
    pair.connect();
    assert_eq!(
        pair.a
            .bind_fabric_send(pair.aid, NodeRouteKey::from_bytes([1, 3, 0, 0]))
            .unwrap_err(),
        BindError::UnsupportedProfile
    );
    old.try_send(Packet::Wire(Arc::from([1u8])), LinkTime(u64::MAX))
        .unwrap();
    for _ in 0..20 {
        pair.step();
    }
    assert_eq!(pair.bd.health().received, 1);
    old.try_send(Packet::Wire(Arc::from([2u8])), LinkTime(u64::MAX))
        .unwrap();
    let previous = (pair.aid, pair.bid);
    pair.reset();
    assert_ne!(previous, (pair.aid, pair.bid));
    assert_eq!(
        old.try_send(Packet::Wire(Arc::from([9u8])), LinkTime(u64::MAX))
            .unwrap_err()
            .kind,
        SendErrorKind::Stale
    );
    let (_, replacement_key) = pair
        .b
        .bind_receive(pair.bid, &[(port, DeliveryTag(2))])
        .unwrap();
    assert_eq!(key, replacement_key);
    let (_, replacement) = pair
        .a
        .bind_send(Destination::Network {
            peer: pair.aid,
            address: replacement_key,
        })
        .unwrap();
    pair.connect();
    replacement
        .try_send(Packet::Wire(Arc::from([3u8])), LinkTime(u64::MAX))
        .unwrap();
    for _ in 0..100 {
        pair.step();
    }
    let mut batch = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut batch, 10), 1);
    assert_eq!(rx.drop_counts().stale, 1);
    assert_eq!(batch[0].tag, DeliveryTag(2));
    assert!(matches!(&batch[0].packet, Packet::Wire(bytes) if bytes.as_ref() == [3]));
    assert_eq!(pair.ad.health().sent, 1);
    assert_eq!(pair.bd.health().received, 1);
    assert_eq!(pair.ad.health().bytes_in_flight, 0);
}

#[test]
fn replacement_wan_peer_pin_rejects_retired_quic_credential() -> Result<(), &'static str> {
    let mut pair = Pair::with_backend(true);
    pair.connect();

    let replacement = QuicCredential::generate().unwrap();
    let PeerConfig::Quic(responder) = &mut pair.b_peer else {
        return Err("expected QUIC responder");
    };
    responder.expected_peer = replacement.fingerprint();
    pair.reset();
    let failure = (0..5000).find_map(|_| {
        let result = pair.try_step();
        assert!(
            !pair.bd.health().ready,
            "retired credential passed the new pin"
        );
        result.err()
    });
    assert!(
        matches!(failure, Some(io::Error::Security)),
        "expected pin rejection, got {failure:?}"
    );

    let PeerConfig::Quic(initiator) = &mut pair.a_peer else {
        return Err("expected QUIC initiator");
    };
    initiator.credential = replacement;
    pair.reset();
    pair.connect();
    let (port, mut rx) = pair.b.create_port().unwrap();
    let (_, tx) = pair.route(&[(port, DeliveryTag(1))]);
    tx.try_send(Packet::Wire(Arc::from([1u8])), LinkTime(u64::MAX))
        .unwrap();
    for _ in 0..20 {
        pair.step();
    }
    let mut batch = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut batch, 2), 1);
    assert!(matches!(&batch[0].packet, Packet::Wire(bytes) if bytes.as_ref() == [1]));
    Ok(())
}

#[test]
fn fabric_restart_reuses_route_label_but_never_admits_old_association_work() {
    let path = FabricPath {
        udp_payload_limit: 1200,
        additional_overhead: 0,
    };
    let mut pair = Pair::configured_profile(None, false, false, Some(path));
    let (port, mut rx) = pair.b.create_port().unwrap();
    let install = |pair: &mut Pair, tag| {
        pair.connect();
        let (binding, key) = pair
            .b
            .stage_fabric_receive(pair.bid, 3, &[(port, tag)], |_| true)
            .unwrap();
        pair.b.activate_fabric_receive(binding).unwrap();
        let (_, tx) = pair
            .a
            .bind_send(Destination::Network {
                peer: pair.aid,
                address: key,
            })
            .unwrap();
        (key, tx)
    };
    let image = || Packet::Wire(Arc::from([0; 24]));
    let (label, old) = install(&mut pair, DeliveryTag(1));
    let previous = pair.bid;
    old.try_send(image(), LinkTime(u64::MAX)).unwrap();
    for _ in 0..20 {
        pair.step();
    }
    assert_eq!(pair.bd.health().received, 1);
    old.try_send(image(), LinkTime(u64::MAX)).unwrap();
    pair.reset();
    let (label_after_restart, fresh) = install(&mut pair, DeliveryTag(2));
    assert_ne!(previous, pair.bid);
    assert_eq!(label, label_after_restart);
    assert_eq!(
        old.try_send(image(), LinkTime(u64::MAX)).unwrap_err().kind,
        SendErrorKind::Stale
    );
    fresh.try_send(image(), LinkTime(u64::MAX)).unwrap();
    for _ in 0..20 {
        pair.step();
    }
    let mut delivered = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut delivered, 2), 1);
    assert_eq!(rx.drop_counts().stale, 1);
    assert_eq!(delivered[0].tag, DeliveryTag(2));
    assert_eq!(
        delivered[0].fabric,
        Some(FabricIngress {
            association: pair.bid,
            key: label,
            purpose: FabricPurpose::Live,
        })
    );
    assert_eq!(pair.bd.health().received, 1);
}

#[test]
fn protected_repair_label_has_one_destination_and_cannot_compile_as_live() {
    let path = FabricPath {
        udp_payload_limit: 1200,
        additional_overhead: 0,
    };
    let mut pair = Pair::configured_profile(None, false, false, Some(path));
    pair.connect();
    let (live_port, mut live) = pair.b.create_port().unwrap();
    let (repair_port, mut repair) = pair.b.create_port().unwrap();
    let (live_binding, _) = pair
        .b
        .stage_fabric_receive(pair.bid, 3, &[(live_port, DeliveryTag(1))], |_| true)
        .unwrap();
    let (repair_binding, key) = pair
        .b
        .stage_fabric_repair_receive(
            pair.bid,
            3,
            (repair_port, DeliveryTag(2)),
            |packet| matches!(packet, Packet::Wire(bytes) if bytes.get(1) == Some(&0xe3)),
        )
        .unwrap();
    pair.b.activate_fabric_receive(live_binding).unwrap();
    pair.b.activate_fabric_receive(repair_binding).unwrap();
    assert_eq!(
        pair.b
            .bind_fabric_relay(repair_binding, pair.bid, key)
            .unwrap_err(),
        BindError::UnsupportedProfile
    );
    assert_eq!(
        pair.b
            .bind_fabric_repair_relay(live_binding, pair.bid, key)
            .unwrap_err(),
        BindError::UnsupportedProfile
    );
    assert!(
        pair.b
            .bind_fabric_repair_relay(repair_binding, pair.bid, key)
            .is_ok()
    );

    let (_, tx) = pair.a.bind_fabric_send(pair.aid, key).unwrap();
    let mut image = vec![0; 64];
    image[0] = 0x80;
    image[1] = 0xe3;
    tx.try_send(Packet::Wire(image.into()), LinkTime(u64::MAX))
        .unwrap();
    for _ in 0..20 {
        pair.step();
    }
    let mut out = Vec::new();
    assert_eq!(live.try_recv_batch(&mut out, 2), 0);
    assert_eq!(repair.try_recv_batch(&mut out, 2), 1);
    assert_eq!(out[0].tag, DeliveryTag(2));
    assert_eq!(
        out[0].fabric,
        Some(FabricIngress {
            association: pair.bid,
            key,
            purpose: FabricPurpose::RepairOnly
        })
    );
}

#[test]
fn relay_cutoff_cancels_protected_output_at_either_binding() {
    for cut_input in [true, false] {
        let path = FabricPath {
            udp_payload_limit: 1200,
            additional_overhead: 0,
        };
        let mut pair = Pair::configured_profile(None, false, false, Some(path));
        pair.connect();
        let (input_port, _input) = pair.a.create_port().unwrap();
        let (output_port, mut output) = pair.b.create_port().unwrap();
        let (input, _) = pair
            .a
            .stage_fabric_receive(pair.aid, 0, &[(input_port, DeliveryTag(1))], |_| true)
            .unwrap();
        let (output_binding, key) = pair
            .b
            .stage_fabric_receive(pair.bid, 1, &[(output_port, DeliveryTag(2))], |_| true)
            .unwrap();
        pair.b.activate_fabric_receive(output_binding).unwrap();
        assert_eq!(
            pair.a.bind_fabric_relay(input, pair.aid, key).unwrap_err(),
            BindError::Closed
        );
        pair.a.activate_fabric_receive(input).unwrap();
        let (relay, tx) = pair.a.bind_fabric_relay(input, pair.aid, key).unwrap();
        let image = || Packet::Wire(Arc::from([0; 24]));
        tx.try_send(image(), LinkTime(u64::MAX)).unwrap();
        for _ in 0..40 {
            pair.step();
        }
        assert_eq!(output.try_recv_batch(&mut Vec::new(), 1), 1);
        assert_eq!(pair.ad.health().sent, 1);

        tx.try_send(image(), LinkTime(u64::MAX)).unwrap();
        assert!(pair.ad.stage_test_outbound().unwrap());
        assert_eq!(pair.ad.health().sent, 1);
        if cut_input {
            pair.a
                .receive_port_cutoff(input, input_port)
                .unwrap()
                .fence();
        } else {
            pair.a.unbind(relay);
        }
        assert_eq!(
            tx.try_send(image(), LinkTime(u64::MAX)).unwrap_err().kind,
            SendErrorKind::Stale
        );
        for _ in 0..40 {
            pair.step();
        }
        assert_eq!(output.try_recv_batch(&mut Vec::new(), 1), 0);
        assert_eq!(pair.ad.health().sent, 1);
        assert_eq!(pair.ad.health().send_failed, 1);
    }
}

#[test]
fn attempt_wrap_with_reordered_ciphertext_keeps_extended_feedback_identity() {
    for quic in [false, true] {
        attempt_wrap_with_reordered_ciphertext_keeps_extended_feedback_identity_case(quic);
    }
}

fn attempt_wrap_with_reordered_ciphertext_keeps_extended_feedback_identity_case(quic: bool) {
    let mut pair = Pair::with_backend(quic);
    let (port, mut rx) = pair.b.create_port().unwrap();
    let (_, tx) = pair.route(&[(port, DeliveryTag(1))]);
    pair.connect();
    for _ in 0..20 {
        pair.step();
    }
    let start = u64::from(u32::MAX) - 1;
    pair.ad.seed_test_sequence(start, None);
    pair.bd.seed_test_sequence(0, Some(start - 1));
    for value in [1u8, 2, 3] {
        tx.try_send(Packet::Wire(Arc::from([value])), LinkTime(u64::MAX))
            .unwrap();
    }
    for _ in 0..30 {
        let now = LinkTime(pair.ticks.fetch_add(1_000_000, Ordering::Relaxed) + 1_000_000);
        pair.poll_a(now).unwrap();
    }
    assert_eq!(pair.ad.health().sent, 3);
    assert!(
        pair.aout.len() >= 3,
        "QUIC may additionally emit control packets"
    );
    pair.aout.make_contiguous().reverse();
    for _ in 0..100 {
        pair.step();
    }
    let mut batch = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut batch, 10), 3);
    assert!(matches!(&batch[0].packet, Packet::Wire(bytes) if bytes.as_ref() == [3]));
    assert!(matches!(&batch[2].packet, Packet::Wire(bytes) if bytes.as_ref() == [1]));
    assert_eq!(pair.bd.health().received, 3);
    assert_eq!(pair.ad.health().sent, 3);
    assert_eq!(pair.ad.health().bytes_in_flight, 0);
}

#[test]
fn queued_expiry_and_lost_feedback_leave_no_unbounded_in_flight_state() {
    for quic in [false, true] {
        queued_expiry_and_lost_feedback_leave_no_unbounded_in_flight_state_case(quic);
    }
}

fn queued_expiry_and_lost_feedback_leave_no_unbounded_in_flight_state_case(quic: bool) {
    let mut pair = Pair::with_backend(quic);
    let (port, mut rx) = pair.b.create_port().unwrap();
    let (_, tx) = pair.route(&[(port, DeliveryTag(1))]);
    pair.connect();
    let deadline = LinkTime(pair.ticks.load(Ordering::Relaxed));
    tx.try_send(Packet::Wire(Arc::from([1u8])), deadline)
        .unwrap();
    pair.step();
    assert_eq!(pair.ad.health().expired, 1);
    assert_eq!(pair.ad.health().sent, 0);
    pair.drop_b = true;
    tx.try_send(Packet::Wire(Arc::from([2u8])), LinkTime(u64::MAX))
        .unwrap();
    for _ in 0..100 {
        pair.step();
    }
    assert_eq!(pair.ad.health().sent, 1);
    assert!(pair.ad.health().bytes_in_flight > 0);
    let mut batch = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut batch, 10), 1);
    if quic {
        let now = LinkTime(
            pair.ticks
                .load(Ordering::Relaxed)
                .saturating_add(2_500_000_000),
        );
        assert_eq!(
            pair.ad.poll(now, 256, |_| io::SendOutcome::Rejected),
            Err(io::Error::FeedbackUnavailable)
        );
        assert!(!pair.ad.health().ready);
        assert_eq!(pair.ad.health().bytes_in_flight, 0);
        assert!(matches!(
            tx.try_send(Packet::Wire(Arc::from([3u8])), LinkTime(u64::MAX)),
            Err(SendError {
                kind: SendErrorKind::Closed,
                ..
            })
        ));
        return;
    }
    for _ in 0..2500 {
        pair.step();
    }
    assert_eq!(pair.ad.health().bytes_in_flight, 0);
    pair.drop_b = false;
    tx.try_send(Packet::Wire(Arc::from([3u8])), LinkTime(u64::MAX))
        .unwrap();
    for _ in 0..100 {
        pair.step();
    }
    batch.clear();
    assert_eq!(rx.try_recv_batch(&mut batch, 10), 1);
    assert!(matches!(&batch[0].packet, Packet::Wire(bytes) if bytes.as_ref() == [3]));
    assert_eq!(pair.ad.health().bytes_in_flight, 0);
}

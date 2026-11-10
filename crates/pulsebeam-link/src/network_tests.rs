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
    a_epochs: [bool; 4],
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
        Self::configured(key_limit, false)
    }

    fn with_backend(quic: bool) -> Self {
        Self::configured(None, quic)
    }

    fn configured(key_limit: Option<u64>, quic: bool) -> Self {
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
            (
                NetworkPeer {
                    endpoint: ba,
                    credential: ca.clone(),
                    expected_peer: cb.fingerprint(),
                    role: PeerRole::Initiator,
                    class: NetworkClass::Lan,
                }
                .into(),
                NetworkPeer {
                    endpoint: aa,
                    credential: cb,
                    expected_peer: ca.fingerprint(),
                    role: PeerRole::Responder,
                    class: NetworkClass::Wan,
                }
                .into(),
            )
        };
        let aid = a.open_network(a_peer.clone(), Codec).unwrap();
        let bid = b.open_network(b_peer.clone(), Codec).unwrap();
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
            a_epochs: [false; 4],
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
        self.aid = self.a.open_network(self.a_peer.clone(), Codec).unwrap();
        self.bid = self.b.open_network(self.b_peer.clone(), Codec).unwrap();
        self.ad = self.a.take_network_driver(self.aid).unwrap();
        self.bd = self.b.take_network_driver(self.bid).unwrap();
        let now = LinkTime(self.ticks.load(Ordering::Relaxed));
        let instant = Instant::now();
        self.ad.start(now, instant).unwrap();
        self.bd.start(now, instant).unwrap();
    }

    fn poll_a(&mut self, now: LinkTime) {
        self.ad
            .poll(now, self.budget, |datagram| {
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
                    self.a_epochs[usize::from(header & 3)] = true;
                }
                self.aout.push_back(datagram.payload.to_vec());
                io::SendOutcome::Accepted {
                    bytes: datagram.payload.len(),
                }
            })
            .unwrap();
    }

    fn poll_b(&mut self, now: LinkTime) {
        self.bd
            .poll(now, self.budget, |datagram| {
                assert_eq!(datagram.destination, self.aa);
                assert!(datagram.payload.len() <= wire::UDP_BUDGET);
                if !self.drop_b {
                    self.bout.push_back(datagram.payload.to_vec());
                }
                io::SendOutcome::Accepted {
                    bytes: datagram.payload.len(),
                }
            })
            .unwrap();
    }

    fn step(&mut self) {
        let now = LinkTime(self.ticks.fetch_add(1_000_000, Ordering::Relaxed) + 1_000_000);
        self.poll_a(now);
        self.poll_b(now);
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
                        self.poll_b(now);
                    }
                    Err(io::Error::Busy) => self.aout.push_front(bytes),
                    other => panic!("receive B: {other:?}"),
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
                        self.poll_a(now);
                    }
                    Err(io::Error::Busy) => self.bout.push_front(bytes),
                    other => panic!("receive A: {other:?}"),
                }
            }
            if !progress {
                break;
            }
        }
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
    pair.b.unbind(binding);
    old.try_send(Packet::Wire(Arc::from([2u8])), LinkTime(u64::MAX))
        .unwrap();
    let (_, replacement_key) = pair
        .b
        .bind_receive(pair.bid, &[(port, DeliveryTag(2))])
        .unwrap();
    assert_ne!(key, replacement_key);
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
    assert_eq!(pair.ad.health().sent, 3);
    assert_eq!(pair.ad.health().bytes_in_flight, 0);
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
        pair.poll_a(now);
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

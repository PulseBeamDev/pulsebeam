#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::arithmetic_side_effects,
    reason = "fixture owns both Noq peers, their synthetic network and a single clock origin"
)]

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::BytesMut;
use noq_proto::{ConnectionError, DatagramEvent, FourTuple, PathId};
use rustls::pki_types::{PrivatePkcs8KeyDer, UnixTime};

use super::*;

#[derive(Debug)]
struct Clock;

impl TimeProvider for Clock {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(Duration::from_secs(
            1_800_000_000,
        )))
    }
}

impl noq_proto::TimeSource for Clock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }
}

struct Peer {
    address: SocketAddr,
    endpoint: Endpoint,
    connection: Option<(ConnectionHandle, Connection)>,
    rejected: Option<ConnectionError>,
    out: Vec<u8>,
    scheduler: PacketScheduler,
    control_runnable: bool,
    cid_bytes: usize,
    routed_shard: Option<u16>,
    last_short_dcid: Vec<u8>,
    largest_delivered: usize,
}

impl Peer {
    fn turn(&mut self, now: Instant, other: &mut Self) {
        let Some((handle, connection)) = &mut self.connection else {
            return;
        };
        service_timeout(connection, now);
        for _ in 0..16 {
            if !forward_endpoint_event(&mut self.endpoint, *handle, connection) {
                break;
            }
        }
        for _ in 0..16 {
            let Some(tx) =
                self.scheduler
                    .poll(connection, now, &mut self.out, self.control_runnable)
            else {
                break;
            };
            assert!(tx.segment_size.is_none());
            assert!(
                tx.size <= usize::from(UDP_PAYLOAD_BYTES),
                "oversized UDP transmit: {}",
                tx.size
            );
            assert!(tx.src_ip.is_none_or(|ip| ip == self.address.ip()));
            if tx.destination != other.address {
                continue;
            }
            self.largest_delivered = self.largest_delivered.max(tx.size);
            if let Some(shard) = other.routed_shard {
                assert_eq!(
                    routing::destination(&self.out[..tx.size], usize::from(shard) + 1),
                    Some(shard)
                );
            }
            assert!(self.out.first().is_some_and(|flags| flags & 0x40 != 0));
            if self.out.first().is_some_and(|flags| flags & 0x80 == 0) {
                self.last_short_dcid.clear();
                self.last_short_dcid
                    .extend_from_slice(&self.out[1..=other.cid_bytes]);
            }
            let tuple = FourTuple::new(self.address, Some(other.address.ip()));
            other.out.clear();
            let bytes = BytesMut::from(&self.out[..tx.size]);
            match other
                .endpoint
                .handle(now, tuple, tx.ecn, bytes, &mut other.out)
            {
                Some(DatagramEvent::ConnectionEvent(id, event)) => {
                    let (handle, conn) = other.connection.as_mut().unwrap();
                    assert_eq!(*handle, id);
                    conn.handle_event(event);
                }
                Some(DatagramEvent::NewConnection(incoming)) => {
                    assert!(other.connection.is_none());
                    match other.endpoint.accept(incoming, now, &mut other.out, None) {
                        Ok(accepted) => other.connection = Some(accepted),
                        Err(error) => {
                            other.rejected = Some(error.cause);
                            if let Some(response) = error.response {
                                assert_eq!(response.destination, self.address);
                                let tuple = FourTuple::new(other.address, Some(self.address.ip()));
                                let bytes = BytesMut::from(&other.out[..response.size]);
                                self.out.clear();
                                if let Some(DatagramEvent::ConnectionEvent(id, event)) = self
                                    .endpoint
                                    .handle(now, tuple, response.ecn, bytes, &mut self.out)
                                {
                                    assert_eq!(*handle, id);
                                    connection.handle_event(event);
                                }
                            }
                        }
                    }
                }
                Some(DatagramEvent::Response(_)) | None => {}
            }
        }
    }

    fn offer_frame(&mut self, now: Instant, sender: &mut framing::Sender) -> framing::SendProgress {
        sender
            .offer(
                &mut self.scheduler,
                &mut self.connection.as_mut().unwrap().1,
                now,
            )
            .unwrap()
    }

    fn conn(&mut self) -> &mut Connection {
        &mut self.connection.as_mut().unwrap().1
    }
}

fn credentials(trusted: bool) -> (RootCertStore, ServerConfig) {
    let cert = rcgen::generate_simple_self_signed(vec!["edge.example".into()]).unwrap();
    let mut roots = RootCertStore::empty();
    if trusted {
        roots.add(cert.cert.der().clone()).unwrap();
    }
    let server = server_config(
        vec![cert.cert.der().clone()],
        PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        Arc::new(Clock),
        Arc::new(Clock),
        NonZeroUsize::MIN,
    )
    .unwrap();
    (roots, server)
}

struct Pair {
    now: Instant,
    client: Peer,
    server: Peer,
}

impl Pair {
    fn new(name: &str, trusted: bool) -> Self {
        Self::profile(name, trusted, ALPN, true)
    }

    fn profile(name: &str, trusted: bool, alpn: &[u8], datagrams: bool) -> Self {
        Self::with_cid(
            name,
            trusted,
            alpn,
            CONNECTION_ID_BYTES,
            datagrams.then_some(DATAGRAM_QUEUE_BYTES),
            None,
        )
    }

    fn with_cid(
        name: &str,
        trusted: bool,
        alpn: &[u8],
        cid_bytes: usize,
        receive_buffer: Option<usize>,
        routed_shard: Option<u16>,
    ) -> Self {
        let (roots, server_config) = credentials(trusted);
        let mut client_config = client_config_for_alpn(roots, Arc::new(Clock), alpn).unwrap();
        let mut transport = limits();
        transport.datagram_receive_buffer_size(receive_buffer);
        client_config.transport_config(Arc::new(transport));
        let now = Instant::now();
        let server_endpoint = if let Some(shard) = routed_shard {
            assert_eq!(cid_bytes, routing::ROUTED_CID_BYTES);
            routing::set_initial_destination(&mut client_config, routing::mint(shard));
            endpoint_on_shard(server_config, shard)
        } else {
            endpoint_with_cid_bytes(Some(server_config), cid_bytes)
        }
        .unwrap();
        let server = Peer {
            address: "[2001:db8:1::1]:8443".parse().unwrap(),
            endpoint: server_endpoint,
            connection: None,
            rejected: None,
            out: Vec::new(),
            scheduler: PacketScheduler::default(),
            control_runnable: false,
            cid_bytes,
            routed_shard,
            last_short_dcid: Vec::new(),
            largest_delivered: 0,
        };
        let mut client_endpoint = endpoint_with_cid_bytes(None, cid_bytes).unwrap();
        let connection = client_endpoint
            .connect(now, client_config, server.address, name)
            .unwrap();
        let client = Peer {
            address: "[2001:db8:2::1]:9000".parse().unwrap(),
            endpoint: client_endpoint,
            connection: Some(connection),
            rejected: None,
            out: Vec::new(),
            scheduler: PacketScheduler::default(),
            control_runnable: false,
            cid_bytes,
            routed_shard: None,
            last_short_dcid: Vec::new(),
            largest_delivered: 0,
        };
        Self {
            now,
            client,
            server,
        }
    }

    fn advance(&mut self, millis: usize) {
        for _ in 0..millis {
            self.client.turn(self.now, &mut self.server);
            self.server.turn(self.now, &mut self.client);
            self.now += Duration::from_millis(1);
        }
    }
}

const TEST_BINDING: std::num::NonZeroU16 = std::num::NonZeroU16::MIN;

fn framing_pair() -> (framing::Sender, framing::Receiver) {
    let version = crate::records::PacketFraming::negotiate(1).unwrap();
    (
        framing::Sender::new(version, TEST_BINDING),
        framing::Receiver::new(version, TEST_BINDING),
    )
}

fn canonical_fixture(profile: pulsebeam_core::fabric::Profile, len: usize) -> Box<[u8]> {
    use pulsebeam_core::fabric::{
        Extension, ExtensionKind, FabricPacket, Header, Profile,
        packet::{Agreement, Offer},
    };
    use pulsebeam_proto::data_record::{Capture, Clock, Record};
    let desc = [0xdb; 96];
    let act = [0x24; 16];
    let extensions = [
        Extension {
            kind: ExtensionKind::DependencyDescriptor,
            value: &desc,
        },
        Extension {
            kind: ExtensionKind::AbsoluteCaptureTime,
            value: &act,
        },
    ];
    let mut payload = vec![7; len - 12 - 116];
    if profile == Profile::Data {
        let fragment = vec![7; payload.len() - 34];
        let clock = Clock {
            id: "source",
            epoch: 1,
        };
        let capture = Capture {
            clock,
            nanos: -3,
            uncertainty_ns: Some(17),
        };
        let record = Record::new(
            pulsebeam_proto::data_record::Header {
                unit_seq: u64::MAX,
                index: 0,
                count: 1,
                unit_len: fragment.len().try_into().unwrap(),
                capture: Some(capture),
            },
            &fragment,
            Some(clock),
        )
        .unwrap();
        assert_eq!(record.write(&mut payload).unwrap(), payload.len());
    }
    let header = Header {
        profile,
        marker: true,
        sequence: u32::MAX,
        timestamp: 0xcafe_affe,
    };
    let packet = FabricPacket::new(header, &extensions, &payload).unwrap();
    let offer = Offer::current(profile, 0x000c);
    let mut bytes = vec![0; len];
    assert_eq!(
        Agreement::negotiate(profile, offer, offer)
            .unwrap()
            .write(&packet, &mut bytes)
            .unwrap(),
        len
    );
    bytes.into_boxed_slice()
}

impl Pair {
    fn exchange_frame(
        &mut self,
        forward: bool,
        sender: &mut framing::Sender,
        receiver: &mut framing::Receiver,
    ) -> (usize, Option<Box<[u8]>>, usize) {
        let tx = if forward {
            &mut self.client
        } else {
            &mut self.server
        };
        tx.largest_delivered = 0;
        let mut progress = tx.offer_frame(self.now, sender);
        if matches!(progress, framing::SendProgress::Deferred(_)) {
            self.advance(1);
            let tx = if forward {
                &mut self.client
            } else {
                &mut self.server
            };
            progress = tx.offer_frame(self.now, sender);
        }
        assert!(matches!(
            progress,
            framing::SendProgress::Partial | framing::SendProgress::Admitted
        ));
        self.advance(10);
        let (tx, rx) = if forward {
            (&mut self.client, &mut self.server)
        } else {
            (&mut self.server, &mut self.client)
        };
        let wire = rx
            .conn()
            .datagrams()
            .recv()
            .expect("offered DATAGRAM was emitted by Noq");
        let bytes = receiver
            .receive(self.now, &wire)
            .unwrap()
            .map(|v| Box::from(v.as_ref()));
        assert_eq!(progress == framing::SendProgress::Admitted, bytes.is_some());
        (wire.len(), bytes, tx.largest_delivered)
    }
}

#[test]
fn floor_budget_is_identical_through_rebind_and_cid_rotation() {
    use pulsebeam_core::fabric::Profile;
    let mut pair = Pair::with_cid(
        "edge.example",
        true,
        ALPN,
        MAX_V1_CID_BYTES,
        Some(DATAGRAM_QUEUE_BYTES),
        Some(257),
    );
    assert_eq!(data_readiness(pair.client.conn()), Readiness::Handshaking);
    pair.advance(100);
    assert!(!pair.client.conn().has_0rtt());
    assert!(!pair.server.conn().is_multipath_negotiated());
    assert_eq!(APPLICATION_DATAGRAM_BYTES, 1150);

    let before = pair.client.last_short_dcid.clone();
    assert_eq!(before.len(), 20);
    for stage in 0..2 {
        for forward in [true, false] {
            let connection = if forward {
                pair.client.conn()
            } else {
                pair.server.conn()
            };
            assert_eq!(connection.current_mtu(), 1200);
            assert_eq!(connection.datagrams().max_size(), Some(1150));
            assert_eq!(data_readiness(connection), Readiness::Ready);
            let (mut sender, mut receiver) = framing_pair();
            for (profile, len) in [
                (Profile::Data, 1147),
                (Profile::Data, 1148),
                (Profile::Data, 1186),
                (Profile::Opus, 1147),
                (Profile::Opus, 1148),
                (Profile::H264, 1147),
                (Profile::H264, 1148),
                (Profile::H264, 1280),
                (Profile::Av1, 1147),
                (Profile::Av1, 1148),
            ] {
                let bytes = canonical_fixture(profile, len);
                sender.start(pair.now, bytes.clone()).unwrap();
                let (envelope, packet, udp) =
                    pair.exchange_frame(forward, &mut sender, &mut receiver);
                assert_eq!(envelope, 1150);
                assert!((1188..=1200).contains(&udp), "UDP segment: {udp}");
                if len == 1147 {
                    assert_eq!(packet, Some(bytes));
                } else {
                    assert_eq!(packet, None);
                    let (envelope, packet, udp) =
                        pair.exchange_frame(forward, &mut sender, &mut receiver);
                    assert_eq!(envelope, len - 1139 + 11);
                    assert!(udp > envelope && udp <= 1200);
                    assert_eq!(packet, Some(bytes));
                }
            }
        }
        if stage == 0 {
            pair.client.address = "[2001:db8:3::1]:9002".parse().unwrap();
            let now = pair.now;
            pair.client.conn().handle_network_change(None, now);
        }
    }

    assert_eq!(pair.client.last_short_dcid.len(), 20);
    assert_ne!(
        pair.client.last_short_dcid, before,
        "migration must exercise a rotated CID"
    );
    assert_eq!(
        pair.server
            .conn()
            .network_path(PathId::ZERO)
            .unwrap()
            .remote(),
        pair.client.address
    );
}

#[test]
fn peer_datagram_capacity_is_an_admission_check_not_a_sizing_input() {
    let mut pair = Pair::new("edge.example", true);
    pair.advance(100);
    assert_eq!(pair.server.conn().datagrams().max_size(), Some(1162));
    let (mut sender, mut receiver) = framing_pair();
    let payload: Box<[u8]> = vec![0x7b; 1148].into();
    sender.start(pair.now, payload.clone()).unwrap();
    assert_eq!(
        pair.exchange_frame(false, &mut sender, &mut receiver).0,
        1150
    );
    let (envelope, received, _) = pair.exchange_frame(false, &mut sender, &mut receiver);
    assert_eq!(envelope, 20);
    assert_eq!(received, Some(payload));

    // Noq advertises its receive-buffer limit and accounts for its frame bound.
    let mut pair = Pair::with_cid(
        "edge.example",
        true,
        ALPN,
        CONNECTION_ID_BYTES,
        Some(1158),
        None,
    );
    pair.advance(100);
    assert_eq!(pair.server.conn().datagrams().max_size(), Some(1149));
    assert_eq!(
        data_readiness(pair.server.conn()),
        Readiness::InsufficientDatagramBudget
    );
    let (mut sender, _) = framing_pair();
    sender.start(pair.now, Box::from([7])).unwrap();
    let (_, conn) = pair.server.connection.as_mut().unwrap();
    assert_eq!(
        sender.offer(&mut pair.server.scheduler, conn, pair.now),
        Err(framing::SendError::Size)
    );
}

#[test]
fn tls_rejects_wrong_identity_untrusted_issuer_and_wrong_alpn() {
    for (name, trusted, alpn) in [
        ("other.example", true, ALPN),
        ("edge.example", false, ALPN),
        ("edge.example", true, b"not-pulsebeam".as_slice()),
    ] {
        let mut pair = Pair::profile(name, trusted, alpn, true);
        pair.advance(100);
        assert_eq!(data_readiness(pair.client.conn()), Readiness::Closed);
        if alpn != ALPN {
            assert!(pair.server.connection.is_none());
            assert!(matches!(pair.server.rejected,
                Some(ConnectionError::TransportError(error)) if error.crypto.as_deref().and_then(|err| err.downcast_ref::<rustls::Error>()) == Some(&rustls::Error::NoApplicationProtocol)
            ));
        }
    }
}

#[test]
fn no_data_admission_without_datagram_negotiation() {
    let mut pair = Pair::profile("edge.example", true, ALPN, false);
    pair.advance(100);
    assert_eq!(
        data_readiness(pair.server.conn()),
        Readiness::InsufficientDatagramBudget
    );
}

#[test]
fn control_progresses_between_full_payloads_without_displacing_admitted_data() {
    let mut pair = Pair::new("edge.example", true);
    pair.advance(100);
    use crate::records::DatagramKind;
    use std::num::NonZeroU16;

    let payload = [1; APPLICATION_DATAGRAM_BYTES - crate::records::DATAGRAM_HEADER];
    let offer = |peer: &mut Peer| {
        peer.scheduler.offer_bound(
            &mut peer.connection.as_mut().unwrap().1,
            Datagram {
                kind: DatagramKind::Rtcp,
                binding: NonZeroU16::MIN,
                payload: &payload,
            },
        )
    };
    offer(&mut pair.client).unwrap();
    assert_eq!(offer(&mut pair.client), Err(DatagramAdmission::Full));

    let id = pair
        .client
        .conn()
        .streams()
        .open(noq_proto::Dir::Bi)
        .unwrap();
    let control = [2; 256];
    assert_eq!(
        pair.client.conn().send_stream(id).write(&control).unwrap(),
        control.len()
    );
    pair.client.control_runnable = true;
    pair.advance(100);

    read_datagram(pair.server.conn(), |record| {
        assert_eq!(record.kind, DatagramKind::Rtcp);
        assert_eq!(record.binding, NonZeroU16::MIN);
        assert_eq!(record.payload, payload);
    })
    .unwrap()
    .unwrap();
    assert_eq!(
        pair.server.conn().streams().accept(noq_proto::Dir::Bi),
        Some(id)
    );
    let mut receive = pair.server.conn().recv_stream(id);
    let mut chunks = receive.read(true).unwrap();
    assert_eq!(
        chunks.next(control.len()).unwrap().unwrap().bytes.as_ref(),
        control
    );
    let _ = chunks.finalize();
    pair.client.control_runnable = false;
    offer(&mut pair.client).unwrap();
}

#[test]
fn listener_emits_exact_admit_once_and_fences_stalled_controller() {
    use super::{control::ControlStream, listener::Listener};
    use crate::{
        frame::{Kind, Phase},
        records::Admit,
    };
    use std::num::NonZeroU64;

    let (roots, server_config) = credentials(true);
    let mut listener = Listener::new(server_config).unwrap();
    let edge: SocketAddr = "127.0.0.1:4443".parse().unwrap();
    let agent: SocketAddr = "127.0.0.1:7000".parse().unwrap();
    let mut ep = endpoint(None).unwrap();
    let start = Instant::now();
    let (id, mut conn) = ep
        .connect(
            start,
            client_config(roots, Arc::new(Clock)).unwrap(),
            edge,
            "edge.example",
        )
        .unwrap();
    let fabric = crate::data_bindings::FabricOffer::decode([1, 1, 0, 0, 0, 12]);
    let body = Admit {
        fabric,
        attempt: NonZeroU64::MIN,
        room: Some("stage"),
        owner: "proof",
        token: "a.b.c",
        successor: None,
    }
    .encode()
    .unwrap();
    let mut sent = false;
    let mut admitted = None;
    let mut client_out = Vec::new();
    let mut server_out = Vec::new();
    for ms in 0..5_100 {
        let now = start + Duration::from_millis(ms);
        service_timeout(&mut conn, now);
        for _ in 0..16 {
            if !forward_endpoint_event(&mut ep, id, &mut conn) {
                break;
            }
        }
        if !sent && data_readiness(&mut conn) == Readiness::Ready {
            let mut control = ControlStream::open(&mut conn).unwrap().unwrap();
            control.offer(Kind::Admit, &body, Phase::Bootstrap).unwrap();
            assert!(matches!(
                control.write(&mut conn, now).unwrap(),
                super::control::Write::Admitted { .. }
            ));
            sent = true;
        }
        if let Some(tx) = poll_packet(&mut conn, now, &mut client_out) {
            assert_eq!(tx.destination, edge);
            assert!(
                listener
                    .receive(
                        now,
                        FourTuple::new(agent, Some(edge.ip())),
                        tx.ecn,
                        &client_out[..tx.size],
                        &mut server_out
                    )
                    .is_none()
            );
        }
        listener.begin_turn();
        if let Some((_, Some(tx))) = listener.poll_one(now, &mut server_out, |handle, peer| {
            if !peer.connection.is_closed()
                && let Some(attempt) = peer.take_admit(now).unwrap()
            {
                assert_eq!(attempt, body);
                assert_eq!(peer.peer_fabric(), Some(fabric));
                assert!(admitted.replace(handle).is_none());
            }
        }) {
            client_out.clear();
            if let Some(DatagramEvent::ConnectionEvent(handle, event)) = ep.handle(
                now,
                FourTuple::new(edge, Some(agent.ip())),
                tx.ecn,
                BytesMut::from(&server_out[..tx.size]),
                &mut client_out,
            ) {
                assert_eq!(handle, id);
                conn.handle_event(event);
            }
        }
    }
    assert!(sent);
    let handle = admitted.unwrap();
    assert!(conn.is_closed());
    assert!(!listener.admitted(handle));
}

#[cfg(feature = "executor")]
#[test]
fn executor_admits_and_renews_a_core_session() {
    use crate::{
        executor::Association,
        frame::{Kind, Phase},
        records::{Accepted, Admit, Lease, Renew, RenewResult, encode_admit_result},
    };
    use pulsebeam_agent_core::{
        Agent, AgentCommand, AgentConfig, ConnectionState, Effect, HostEvent, HttpEffect,
        HttpEvent, HttpResponse, QuicEffect,
    };

    let mut core = Agent::new_quic(AgentConfig {
        endpoint: "https://api.example".into(),
        token: "token".into(),
        retry: Default::default(),
        log_level: Default::default(),
    })
    .unwrap();
    core.command(AgentCommand::Connect).unwrap();
    let Some(Effect::Http(HttpEffect::Request { operation, .. })) = core.next_effect() else {
        panic!("placement request")
    };
    core.handle(HostEvent::Http(HttpEvent::Response { operation, response: HttpResponse {
        status: 200, headers: vec![], body: br#"{"profile":"pulsebeam-rt-v1","authority":{"host":"edge.example","port":4443},"owner":"owner","initial_dcid":"AAAAAAAAAAAAAAAAAAAAAAAAAAA","valid_for_ms":5000}"#.to_vec(),
    } })).unwrap();
    let Some(Effect::Quic(QuicEffect::Admit(admit))) = core.next_effect() else {
        panic!("admission effect")
    };
    let (roots, server) = credentials(true);
    let mut listener = listener::Listener::new(server).unwrap();
    let edge: SocketAddr = "127.0.0.1:4443".parse().unwrap();
    let local: SocketAddr = "127.0.0.1:7000".parse().unwrap();
    let start = Instant::now();
    let mut carrier = Association::connect(
        start,
        start,
        client_config(roots, Arc::new(Clock)).unwrap(),
        edge,
        admit,
    )
    .unwrap();
    let mut client_out = Vec::new();
    let mut server_out = Vec::new();
    use pulsebeam_proto::state;
    use std::num::NonZeroU64;

    let mut redemption_count = 0;
    let mut renewals = 0;
    let mut grant_to_send = None;
    let mut renewal_started = false;
    for step in 0..1_000 {
        let now = start + Duration::from_millis(step);
        if let Some(crate::executor::Event::Core(event)) = carrier.poll_event(now, |_| None) {
            if matches!(event, pulsebeam_agent_core::QuicEvent::Accepted { .. }) {
                assert_eq!(core.snapshot().connection, ConnectionState::Joining);
            }
            core.handle_at(HostEvent::Quic(event), now.duration_since(start))
                .unwrap();
        }
        if !renewal_started && core.snapshot().authorization_expires_at == Some(10_000) {
            core.command_at(
                AgentCommand::RenewAuthorization("next-token".into()),
                now.duration_since(start),
            )
            .unwrap();
            renewal_started = true;
        }
        while let Some(effect) = core.next_effect() {
            match effect {
                Effect::Quic(effect) => carrier.submit(effect, now).unwrap(),
                Effect::Timer(pulsebeam_agent_core::TimerEffect::Schedule { timer, after })
                    if after.is_zero() =>
                {
                    core.handle_at(
                        HostEvent::Timer(pulsebeam_agent_core::TimerEvent::Fired { timer }),
                        now.duration_since(start),
                    )
                    .unwrap();
                }
                Effect::Timer(pulsebeam_agent_core::TimerEffect::Cancel { .. }) => {}
                _ => panic!("unexpected effect in QUIC fixture: {effect:?}"),
            }
        }
        if core.snapshot().accepted_credential_revision == Some(1) {
            break;
        }
        if let (Some(tx), _) = carrier.poll_packet(now, &mut client_out) {
            assert_eq!(tx.destination, edge);
            assert!(
                listener
                    .receive(
                        now,
                        FourTuple::new(local, Some(edge.ip())),
                        tx.ecn,
                        &client_out[..tx.size],
                        &mut server_out
                    )
                    .is_none()
            );
        }
        listener.begin_turn();
        if let Some((_, Some(tx))) = listener.poll_one(now, &mut server_out, |_, peer| {
            if let Some(body) = peer.take_admit(now).unwrap() {
                redemption_count += 1;
                let admit = Admit::decode(&body).unwrap();
                assert_eq!(admit.attempt.get(), carrier.generation().get());
                assert_eq!((admit.owner, admit.token), ("owner", "token"));
                let accepted = encode_admit_result(Ok(Accepted {
                    fabric: crate::data_bindings::FabricOffer::CURRENT,
                    connection: [0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1],
                    participant: "alice",
                    room: None,
                    capabilities: &[],
                }))
                .unwrap();
                let control = peer.control.as_mut().unwrap();
                control
                    .offer(Kind::AdmitResult, &accepted, Phase::Bootstrap)
                    .unwrap();
                assert!(matches!(
                    control.write(&mut peer.connection, now).unwrap(),
                    control::Write::Admitted { .. }
                ));
                assert!(peer.finish_admission());
                grant_to_send = Some(1);
                return;
            }
            if redemption_count == 0 {
                return;
            }
            let Some(control) = peer.control.as_mut() else {
                return;
            };
            let frame = if let Some(revision) = grant_to_send.take() {
                let expires_at_unix_seconds = if revision == 1 { 10_000 } else { 20_000 };
                let batch = state::ServerStateBatch {
                    records: vec![state::ServerStateRecord {
                        key: Some(state::ResourceKey {
                            key: Some(state::resource_key::Key::Session(true)),
                        }),
                        confirmed: Some(state::ConfirmedState {
                            value: Some(state::confirmed_state::Value::Session(
                                state::SessionState {
                                    authorization: Some(state::Authorization {
                                        revision,
                                        expires_at_unix_seconds,
                                    }),
                                    ..Default::default()
                                },
                            )),
                            ..Default::default()
                        }),
                    }],
                    ..Default::default()
                };
                Some((
                    Kind::ServerState,
                    state::codec::encode_quic_server(&batch).unwrap(),
                ))
            } else if let control::Read::Frame(frame) = control
                .read(&mut peer.connection, Phase::Admitted, now)
                .unwrap()
            {
                assert_eq!(frame.kind, Kind::Renew);
                let request = Renew::decode(&frame.body).unwrap();
                assert_eq!(request.token, "next-token");
                renewals += 1;
                grant_to_send = Some(2);
                Some((
                    Kind::RenewResult,
                    RenewResult {
                        request: request.revision,
                        accepted: Some(Lease {
                            revision: NonZeroU64::new(2).unwrap(),
                            expires_unix_seconds: NonZeroU64::new(20_000).unwrap(),
                        }),
                    }
                    .encode(),
                ))
            } else {
                None
            };
            if let Some((kind, body)) = frame {
                control.offer(kind, &body, Phase::Admitted).unwrap();
                assert!(matches!(
                    control.write(&mut peer.connection, now).unwrap(),
                    control::Write::Admitted { .. }
                ));
            }
        }) {
            assert_eq!(tx.destination, local);
            carrier.receive(
                now,
                pulsebeam_udp::ReceivedDatagram {
                    source: edge,
                    destination: local,
                    payload: &server_out[..tx.size],
                    received_at: pulsebeam_udp::ReceiveTime {
                        monotonic: now.into(),
                        wall: noq_proto::TimeSource::now(&Clock),
                    },
                    ecn: None,
                },
            );
        }
    }
    assert_eq!(redemption_count, 1);
    assert_eq!(renewals, 1);
    assert_eq!(core.snapshot().accepted_credential_revision, Some(1));
    assert_eq!(core.snapshot().authorization_expires_at, Some(20_000));
    assert_eq!(core.snapshot().connection, ConnectionState::Connected);
    assert_eq!(core.snapshot().participant_id.as_deref(), Some("alice"));
    assert_eq!(
        core.snapshot().connection_id.as_ref().unwrap().as_str(),
        "conn_00000000001R010000000000004"
    );
}

#[test]
fn control_frame_reports_only_full_write_and_keeps_original_partial_deadline() {
    use super::control::{ControlStream, Error, Read, Write};
    use crate::frame::{Kind, Phase};

    let mut pair = Pair::new("edge.example", true);
    pair.advance(100);
    let mut client = ControlStream::open(pair.client.conn()).unwrap().unwrap();
    let body = [0x61; 24];
    client.offer(Kind::Admit, &body, Phase::Bootstrap).unwrap();
    assert_eq!(
        client.offer(Kind::Admit, &body, Phase::Bootstrap),
        Err(Error::Backpressure)
    );
    pair.client.conn().set_send_window(6);
    assert_eq!(
        client.write(pair.client.conn(), pair.now).unwrap(),
        Write::Pending
    );
    pair.advance(100);
    let mut server = ControlStream::accept(pair.server.conn()).unwrap().unwrap();
    assert!(matches!(
        server.read(pair.server.conn(), Phase::Bootstrap, pair.now),
        Ok(Read::Pending)
    ));
    let deadline = server.deadline().unwrap();
    assert_eq!(deadline, pair.now + Duration::from_secs(5));
    let now = pair.now + Duration::from_millis(1);
    assert!(matches!(
        server.read(pair.server.conn(), Phase::Bootstrap, now),
        Ok(Read::Pending)
    ));
    assert_eq!(server.deadline(), Some(deadline));
    assert!(matches!(
        server.read(pair.server.conn(), Phase::Bootstrap, deadline),
        Err(Error::PartialTimeout)
    ));

    pair.client.conn().set_send_window(256);
    assert_eq!(
        client.write(pair.client.conn(), pair.now).unwrap(),
        Write::Admitted {
            kind: Kind::Admit,
            at: pair.now
        }
    );
    assert_eq!(
        client.write(pair.client.conn(), pair.now).unwrap(),
        Write::Empty
    );
    pair.advance(100);
    let Read::Frame(frame) = server
        .read(pair.server.conn(), Phase::Bootstrap, pair.now)
        .unwrap()
    else {
        panic!("fully written admission frame must arrive");
    };
    assert_eq!(frame.kind, Kind::Admit);
    assert_eq!(frame.body, body);
    assert_eq!(server.deadline(), None);
}

#[cfg(all(
    target_os = "linux",
    not(any(feature = "sim", feature = "portable-scalar"))
))]
#[test]
fn moving_reactors_preserves_the_descriptor_and_the_single_pending_transmit() {
    use super::socket::{self, Egress, Flush};

    fn assert_send<T: Send>(_: &T) {}
    let first = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    let second = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap();
    let (source, detached, mut egress) = first.block_on(async {
        let socket = socket::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap()
            .activate()
            .unwrap();
        let source = socket.local_addr();
        let mut egress = Egress::default();
        assert!(
            egress
                .stage(source, |out| {
                    out.extend_from_slice(b"pending");
                    Some(Transmit {
                        destination: source,
                        ecn: None,
                        size: out.len(),
                        segment_size: None,
                        src_ip: Some(source.ip()),
                    })
                })
                .unwrap()
        );
        assert!(
            !egress
                .stage(source, |_| panic!(
                    "existing transmit must not be overwritten"
                ))
                .unwrap()
        );
        (source, socket.detach(), egress)
    });
    assert_send(&detached);
    drop(first);
    second.block_on(async {
        let mut socket = detached.activate().map_err(|(_, e)| e).unwrap();
        assert_eq!(socket.local_addr(), source);
        socket.writable().await.unwrap();
        assert_eq!(egress.flush(&mut socket).unwrap(), Flush::Sent);
        assert_eq!(egress.flush(&mut socket).unwrap(), Flush::Empty);
        tokio::time::timeout(Duration::from_secs(2), socket.readable())
            .await
            .unwrap()
            .unwrap();
        let batch = socket.try_recv_batch().unwrap();
        let datagram = batch.iter().next().unwrap();
        assert_eq!(datagram.payload, b"pending");
        assert_eq!(datagram.source, source);
    });
}

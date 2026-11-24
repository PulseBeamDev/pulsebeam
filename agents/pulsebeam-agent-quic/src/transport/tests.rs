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
            assert!(tx.size + 48 <= 1280, "IPv6 packet exceeds minimum IP MTU");
            assert!(tx.src_ip.is_none_or(|ip| ip == self.address.ip()));
            if tx.destination != other.address {
                continue;
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

    fn offer(&mut self, bytes: &[u8]) -> Result<(), DatagramAdmission> {
        self.scheduler
            .offer(&mut self.connection.as_mut().unwrap().1, bytes)
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
        let (roots, server_config) = credentials(trusted);
        let mut client_config = client_config_for_alpn(roots, Arc::new(Clock), alpn).unwrap();
        if !datagrams {
            let mut limits = limits();
            limits.datagram_receive_buffer_size(None);
            client_config.transport_config(Arc::new(limits));
        }
        let now = Instant::now();
        let server = Peer {
            address: "[2001:db8:1::1]:8443".parse().unwrap(),
            endpoint: endpoint(Some(server_config)).unwrap(),
            connection: None,
            rejected: None,
            out: Vec::new(),
            scheduler: PacketScheduler::default(),
            control_runnable: false,
        };
        let mut client_endpoint = endpoint(None).unwrap();
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

#[test]
fn ipv6_floor_accepts_full_data_record_before_and_after_rebind() {
    let mut pair = Pair::new("edge.example", true);
    assert_eq!(data_readiness(pair.client.conn()), Readiness::Handshaking);
    pair.advance(100);
    assert_eq!(data_readiness(pair.client.conn()), Readiness::Ready);
    assert_eq!(data_readiness(pair.server.conn()), Readiness::Ready);
    assert!(!pair.client.conn().has_0rtt());
    assert!(!pair.server.conn().is_multipath_negotiated());

    let payload = [0x72; MAX_ENDPOINT_RECORD_BYTES + DATA_ENVELOPE_BYTES];
    pair.client.offer(&payload).unwrap();
    pair.advance(100);
    assert_eq!(
        pair.server.conn().datagrams().recv().unwrap().as_ref(),
        payload
    );

    pair.client.address.set_port(9001);
    pair.client.offer(&payload).unwrap();
    pair.advance(100);
    assert_eq!(
        pair.server.conn().datagrams().recv().unwrap().as_ref(),
        payload
    );
    assert_eq!(
        pair.server
            .conn()
            .network_path(PathId::ZERO)
            .unwrap()
            .remote(),
        pair.client.address
    );
    pair.client.address = "[2001:db8:3::1]:9002".parse().unwrap();
    let now = pair.now;
    pair.client.conn().handle_network_change(None, now);
    pair.client.offer(&payload).unwrap();
    pair.advance(100);
    assert_eq!(
        pair.server.conn().datagrams().recv().unwrap().as_ref(),
        payload
    );
    assert_eq!(
        pair.server
            .conn()
            .network_path(PathId::ZERO)
            .unwrap()
            .remote(),
        pair.client.address
    );
    pair.server.offer(&payload).unwrap();
    pair.advance(100);
    assert_eq!(
        pair.client.conn().datagrams().recv().unwrap().as_ref(),
        payload
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

    let payload = [1; MAX_ENDPOINT_RECORD_BYTES];
    let offer = |peer: &mut Peer| {
        peer.scheduler.offer_bound(
            &mut peer.connection.as_mut().unwrap().1,
            Datagram {
                kind: DatagramKind::Data,
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
        assert_eq!(record.kind, DatagramKind::Data);
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
    let body = Admit {
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
        status: 200, headers: vec![], body: br#"{"profile":"pulsebeam-rt-v1","authority":{"host":"edge.example","port":4443},"owner":"owner","valid_for_ms":5000}"#.to_vec(),
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
        if let Some(event) = carrier.poll_event(now) {
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
    let body = [0x61; 18];
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

#[test]
fn moving_reactors_preserves_the_descriptor_and_the_single_pending_transmit() {
    use super::socket::{Flush, SocketIo};

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
    let (source, detached) = first.block_on(async {
        let mut io = SocketIo::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let source = io.socket().local_addr();
        assert!(
            io.stage(|out| {
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
            !io.stage(|_| panic!("existing transmit must not be overwritten"))
                .unwrap()
        );
        (source, io.detach())
    });
    assert_send(&detached);
    drop(first);
    second.block_on(async {
        let mut io = detached.activate().map_err(|(_, e)| e).unwrap();
        assert_eq!(io.socket().local_addr(), source);
        io.socket().writable().await.unwrap();
        assert_eq!(io.flush().unwrap(), Flush::Sent);
        assert_eq!(io.flush().unwrap(), Flush::Empty);
        tokio::time::timeout(Duration::from_secs(2), io.socket().readable())
            .await
            .unwrap()
            .unwrap();
        let batch = io.socket().try_recv_batch().unwrap();
        let datagram = batch.iter().next().unwrap();
        assert_eq!(datagram.payload, b"pending");
        assert_eq!(datagram.source, source);
    });
}

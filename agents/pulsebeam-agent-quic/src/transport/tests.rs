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
        let cert = rcgen::generate_simple_self_signed(vec!["edge.example".into()]).unwrap();
        let mut roots = RootCertStore::empty();
        if trusted {
            roots.add(cert.cert.der().clone()).unwrap();
        }
        let server_config = server_config(
            vec![cert.cert.der().clone()],
            PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
            Arc::new(Clock),
            Arc::new(Clock),
            NonZeroUsize::MIN,
        )
        .unwrap();
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
    let payload = [1; MAX_ENDPOINT_RECORD_BYTES + DATA_ENVELOPE_BYTES];
    pair.client.offer(&payload).unwrap();
    assert_eq!(pair.client.offer(&payload), Err(DatagramAdmission::Full));

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

    assert_eq!(
        pair.server.conn().datagrams().recv().unwrap().as_ref(),
        payload
    );
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
    pair.client.offer(&payload).unwrap();
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

//! Narrow native proof: real engines, one shared server UDP socket, virtual time.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "bounded loopback socket smoke with one explicit clock anchor and invariant assertions"
)]

use pulsebeam_rtc::{
    ConnectionConfig, ConnectionEntropy, Event, LocalCandidate, NetworkInput, Output,
    RtcConnection, SdpOffer, TransmitTarget,
};
use pulsebeam_transport::{
    Credential as SecretCredential, NetworkClass, PacketKind, Peer, PeerRole, QuicCredential,
    QuicPeer, QuicTuning, ReceiveTime, SendOutcome, Time, Transport, classify,
};
use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

fn socket() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    socket
}
fn send(socket: &UdpSocket, destination: SocketAddr, payload: &[u8]) -> SendOutcome {
    match socket.send_to(payload, destination) {
        Ok(bytes) => SendOutcome::Accepted { bytes },
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => SendOutcome::Rejected,
        Err(error) => panic!("loopback send: {error}"),
    }
}
fn receive(socket: &UdpSocket, mut on_packet: impl FnMut(SocketAddr, &[u8])) {
    let mut buffer = [0; 2048];
    for _ in 0..16 {
        match socket.recv_from(&mut buffer) {
            Ok((length, remote)) => on_packet(remote, &buffer[..length]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("loopback receive: {error}"),
        }
    }
}
struct Pending {
    remote: SocketAddr,
    bytes: Vec<u8>,
    observed: ReceiveTime,
}
fn queue(queue: &mut VecDeque<Pending>, remote: SocketAddr, bytes: &[u8], observed: ReceiveTime) {
    assert!(
        queue.len() < 64,
        "per-peer input retention must stay bounded"
    );
    queue.push_back(Pending {
        remote,
        bytes: bytes.to_vec(),
        observed,
    });
}
fn input(transport: &mut Transport, pending: &mut VecDeque<Pending>) {
    if transport.needs_progress() || transport.health().pending_udp_bytes != 0 {
        return;
    }
    if let Some(packet) = pending.pop_front() {
        transport
            .receive(packet.remote, &packet.bytes, packet.observed)
            .unwrap();
    }
}
fn engines(server: SocketAddr, client: SocketAddr, quic: bool) -> (Transport, Transport) {
    if quic {
        let a = QuicCredential::generate().unwrap();
        let b = QuicCredential::generate().unwrap();
        (
            Transport::new(QuicPeer {
                endpoint: client,
                credential: a.clone(),
                expected_peer: b.fingerprint(),
                role: PeerRole::Responder,
                class: NetworkClass::Lan,
                tuning: QuicTuning::default(),
            })
            .unwrap(),
            Transport::new(QuicPeer {
                endpoint: server,
                credential: b,
                expected_peer: a.fingerprint(),
                role: PeerRole::Initiator,
                class: NetworkClass::Lan,
                tuning: QuicTuning::default(),
            })
            .unwrap(),
        )
    } else {
        let a = SecretCredential::generate().unwrap();
        let b = SecretCredential::generate().unwrap();
        (
            Transport::new(Peer {
                endpoint: client,
                credential: a.clone(),
                expected_peer: b.fingerprint(),
                role: PeerRole::Responder,
                class: NetworkClass::Lan,
            })
            .unwrap(),
            Transport::new(Peer {
                endpoint: server,
                credential: b,
                expected_peer: a.fingerprint(),
                role: PeerRole::Initiator,
                class: NetworkClass::Lan,
            })
            .unwrap(),
        )
    }
}

#[test]
fn real_quic_dtls_and_webrtc_share_udp_without_cross_peer_backpressure() {
    let server_socket = socket();
    let q_socket = socket();
    let d_socket = socket();
    let rtc_socket = socket();
    let server_addr = server_socket.local_addr().unwrap();
    let q_addr = q_socket.local_addr().unwrap();
    let d_addr = d_socket.local_addr().unwrap();
    let rtc_addr = rtc_socket.local_addr().unwrap();
    let origin = Instant::now();
    let (mut qs, mut qc) = engines(server_addr, q_addr, true);
    let (mut ds, mut dc) = engines(server_addr, d_addr, false);
    for engine in [&mut qs, &mut qc, &mut ds, &mut dc] {
        engine.start(Time(0), origin).unwrap();
    }

    let mut rtc_peer = str0m::Rtc::builder().build(origin);
    rtc_peer.add_local_candidate(str0m::Candidate::host(rtc_addr, "udp").unwrap());
    let mut changes = rtc_peer.sdp_api();
    let mid = changes.add_media(
        str0m::media::MediaKind::Audio,
        str0m::media::Direction::SendOnly,
        None,
        None,
        None,
    );
    let (offer, pending) = changes.apply().unwrap();
    let accepted = RtcConnection::accept(
        ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp(server_addr)],
            ..ConnectionConfig::default()
        },
        SdpOffer::new(offer.to_sdp_string()),
        origin,
        SystemTime::UNIX_EPOCH,
        ConnectionEntropy::new([42; 32]),
    )
    .unwrap();
    let answer = str0m::change::SdpAnswer::from_sdp_string(accepted.answer.as_str()).unwrap();
    rtc_peer.sdp_api().accept_answer(pending, answer).unwrap();
    let mut rtc_server = accepted.connection;
    let mut peer_deadline = origin;
    let mut rtc_connected = false;
    let mut rtc_peer_connected = false;
    let mut started = None;
    let mut q_inputs = VecDeque::new();
    let mut d_inputs = VecDeque::new();
    let mut qc_inputs = VecDeque::new();
    let mut dc_inputs = VecDeque::new();
    let mut quic_delivered = 0;
    let mut dtls_delivered = 0;
    let mut rtc_delivered = 0;
    let mut dtls_during_backpressure = false;
    let mut rtc_during_backpressure = false;
    let mut rejected = 0;
    let mut protocol_packets = [0usize; 4];

    for tick in 1u64..=5000 {
        let elapsed = Duration::from_millis(tick);
        let now = Time::from_nanos(tick * 1_000_000);
        let at = origin + elapsed;
        let observed = ReceiveTime {
            monotonic: now,
            wall: Some(SystemTime::UNIX_EPOCH + elapsed),
        };
        let blocked = started.is_some_and(|start| tick < start + 50);
        for engine in [&mut qs, &mut qc, &mut ds, &mut dc] {
            engine
                .poll(
                    now,
                    0,
                    |_| panic!("zero-budget send"),
                    |_| panic!("zero-budget receive"),
                )
                .unwrap();
        }
        qs.poll(
            now,
            16,
            |datagram| {
                assert!(datagram.payload.len() <= 1200);
                if blocked {
                    rejected += 1;
                    SendOutcome::Rejected
                } else {
                    send(&server_socket, datagram.destination, datagram.payload)
                }
            },
            |_| panic!("unsolicited reverse QUIC application"),
        )
        .unwrap();
        qc.poll(
            now,
            16,
            |datagram| send(&q_socket, datagram.destination, datagram.payload),
            |application| {
                assert_eq!(application.context, [1, 2, 3, 4]);
                assert_eq!(application.payload, b"quic");
                quic_delivered += 1;
            },
        )
        .unwrap();
        ds.poll(
            now,
            16,
            |datagram| send(&server_socket, datagram.destination, datagram.payload),
            |application| {
                assert_eq!(application.payload, b"dtls");
                dtls_delivered += 1;
                dtls_during_backpressure |= blocked;
            },
        )
        .unwrap();
        dc.poll(
            now,
            16,
            |datagram| send(&d_socket, datagram.destination, datagram.payload),
            |_| panic!("unsolicited reverse DTLS application"),
        )
        .unwrap();

        for _ in 0..16 {
            match rtc_server.poll(at) {
                Output::Transmit(transmit) => {
                    let TransmitTarget::Udp { local, remote, .. } = transmit.target else {
                        panic!("unexpected TCP");
                    };
                    assert_eq!(local, server_addr);
                    assert!(
                        matches!(send(&server_socket, remote, &transmit.payload), SendOutcome::Accepted { bytes } if bytes == transmit.payload.len())
                    );
                }
                Output::Event(Event::Connected) => rtc_connected = true,
                Output::Event(Event::Rtp { packet, .. }) => {
                    assert_eq!(packet.payload.as_ref(), [0xf8, 0xff, 0xfe]);
                    rtc_delivered += 1;
                    rtc_during_backpressure |= blocked;
                }
                Output::Idle { .. } => break,
                Output::Closed(reason) => panic!("WebRTC server closed: {reason:?}"),
                _ => {}
            }
        }
        if at >= peer_deadline {
            rtc_peer.handle_input(str0m::Input::Timeout(at)).unwrap();
        }
        for _ in 0..16 {
            match rtc_peer.poll_output().unwrap() {
                str0m::Output::Transmit(transmit) => {
                    assert_eq!(transmit.source, rtc_addr);
                    assert_eq!(transmit.destination, server_addr);
                    assert!(
                        matches!(send(&rtc_socket, transmit.destination, &transmit.contents), SendOutcome::Accepted { bytes } if bytes == transmit.contents.len())
                    );
                }
                str0m::Output::Event(str0m::Event::Connected) => rtc_peer_connected = true,
                str0m::Output::Timeout(deadline) => {
                    peer_deadline = deadline;
                    break;
                }
                _ => {}
            }
        }
        if started.is_none()
            && rtc_connected
            && rtc_peer_connected
            && [&qs, &qc, &ds, &dc]
                .iter()
                .all(|engine| engine.health().ready && !engine.needs_progress())
        {
            qs.try_submit([1, 2, 3, 4], b"quic", Time(u64::MAX))
                .unwrap();
            dc.try_submit([4; 4], b"dtls", Time(u64::MAX)).unwrap();
            let writer = rtc_peer.writer(mid).unwrap();
            let params = *writer.payload_params().next().unwrap();
            let pt = writer.match_params(params).unwrap();
            writer
                .write(
                    pt,
                    at,
                    str0m::media::MediaTime::from_millis(0),
                    Arc::<[u8]>::from([0xf8, 0xff, 0xfe]),
                )
                .unwrap();
            started = Some(tick);
        }

        receive(&server_socket, |remote, bytes| {
            let kind = classify(bytes, false);
            if remote == q_addr && kind == PacketKind::Quic {
                protocol_packets[0] += 1;
                queue(&mut q_inputs, remote, bytes, observed);
            } else if remote == d_addr && kind == PacketKind::Dtls {
                protocol_packets[1] += 1;
                queue(&mut d_inputs, remote, bytes, observed);
            } else if remote == rtc_addr
                && matches!(
                    kind,
                    PacketKind::Stun | PacketKind::Dtls | PacketKind::RtpRtcp
                )
            {
                protocol_packets[2] += 1;
                if kind == PacketKind::RtpRtcp {
                    protocol_packets[3] += 1;
                }
                rtc_server
                    .receive(
                        at,
                        at,
                        NetworkInput::Udp {
                            local: server_addr,
                            remote,
                            ecn: None,
                            payload: bytes::Bytes::copy_from_slice(bytes),
                        },
                    )
                    .unwrap();
            } else {
                panic!("wrong shared-port dispatch: {remote} {kind:?}");
            }
        });
        receive(&q_socket, |remote, bytes| {
            queue(&mut qc_inputs, remote, bytes, observed);
        });
        receive(&d_socket, |remote, bytes| {
            queue(&mut dc_inputs, remote, bytes, observed);
        });
        receive(&rtc_socket, |remote, bytes| {
            assert_eq!(remote, server_addr);
            rtc_peer
                .handle_input(str0m::Input::Receive(
                    at,
                    str0m::net::Receive::new(str0m::net::Protocol::Udp, remote, rtc_addr, bytes)
                        .unwrap(),
                ))
                .unwrap();
        });
        input(&mut qs, &mut q_inputs);
        input(&mut qc, &mut qc_inputs);
        input(&mut ds, &mut d_inputs);
        input(&mut dc, &mut dc_inputs);
        if quic_delivered == 1
            && dtls_delivered == 1
            && rtc_delivered == 1
            && qs.health().bytes_in_flight == 0
            && dc.health().bytes_in_flight == 0
        {
            assert!(rejected > 0);
            assert!(
                dtls_during_backpressure && rtc_during_backpressure,
                "one QUIC peer stalled other protocol owners"
            );
            assert!(protocol_packets.iter().all(|count| *count > 0));
            assert_eq!(qs.health().sent, 1);
            assert_eq!(qs.health().pending_udp_bytes, 0);
            assert_eq!(qs.health().staging.datagrams, 0);
            return;
        }
    }
    panic!(
        "bounded native shared-port smoke did not settle: QUIC={quic_delivered} DTLS={dtls_delivered} RTP={rtc_delivered} started={started:?}"
    );
}

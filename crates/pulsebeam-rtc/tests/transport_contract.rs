#![allow(
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::panic,
    reason = "integration fixtures exercise Bytes-backed input and stop at violated protocol invariants"
)]

use bytes::Bytes;
use pulsebeam_rtc::{
    AuthenticatedFlow, Command, CommandError, Connection, ConnectionConfig, ConnectionEntropy,
    Event, GlobalMediaTime, IceTcpFlowId, LocalCandidate, LocalIceCredentials, NetworkInput,
    Output, ReceiveError, SdpOffer, TimePoint, TransmitTarget,
};
use std::{net::SocketAddr, time::Instant};

const UFRAG: &str = "0123456789ABCDEF";
const PASSWORD: &str = "0123456789ABCDEFGHIJKLMN";

fn request(username: &str, password: Option<&[u8]>) -> Bytes {
    request_with_nomination(username, password, false)
}

fn request_with_nomination(username: &str, password: Option<&[u8]>, nominate: bool) -> Bytes {
    use is::Sha1HmacProvider;
    let message = is::stun::StunMessage::binding_request(
        username,
        is::stun::TransId::new(),
        true,
        1,
        1_000,
        nominate,
    );
    let mut bytes = [0; 512];
    let length = message
        .to_bytes(password, &mut bytes, |key, payloads| {
            is::DefaultSha1HmacProvider.sha1_hmac(key, payloads)
        })
        .expect("serialize STUN request");
    Bytes::copy_from_slice(bytes.get(..length).expect("serialized STUN length"))
}

fn connection(local: SocketAddr, tcp: bool) -> (Connection, TimePoint) {
    let at = TimePoint {
        monotonic: Instant::now(),
        global: GlobalMediaTime::from_micros(1_000_000),
    };
    let accepted = Connection::accept(
        ConnectionConfig {
            local_candidates: vec![if tcp {
                LocalCandidate::TcpPassive(local)
            } else {
                LocalCandidate::Udp(local)
            }],
            local_ice_credentials: Some(
                LocalIceCredentials::new(UFRAG, PASSWORD).expect("credentials"),
            ),
            ..ConnectionConfig::default()
        },
        SdpOffer::new(include_str!("fixtures/chrome-representative.sdp")),
        at,
        ConnectionEntropy::new([42; 32]),
    )
    .expect("accept session");
    assert!(
        accepted
            .answer
            .as_str()
            .contains(&format!("a=ice-ufrag:{UFRAG}\r\n"))
    );
    assert!(
        accepted
            .answer
            .as_str()
            .contains(&format!("a=ice-pwd:{PASSWORD}\r\n"))
    );
    (accepted.connection, at)
}

fn input(local: SocketAddr, remote: SocketAddr, tcp: bool, payload: Bytes) -> NetworkInput {
    if tcp {
        tcp_input(
            IceTcpFlowId::from_value(u64::from(remote.port())),
            local,
            remote,
            payload,
        )
    } else {
        NetworkInput::Udp {
            local,
            remote,
            ecn: None,
            payload,
        }
    }
}

fn tcp_input(
    flow: IceTcpFlowId,
    local: SocketAddr,
    remote: SocketAddr,
    payload: Bytes,
) -> NetworkInput {
    let mut frame = u16::try_from(payload.len())
        .expect("STUN frame length")
        .to_be_bytes()
        .to_vec();
    frame.extend_from_slice(&payload);
    NetworkInput::IceTcp {
        flow,
        local,
        remote,
        frame: Bytes::from(frame),
    }
}

fn admit_if_tcp(
    connection: &mut Connection,
    at: TimePoint,
    local: SocketAddr,
    remote: SocketAddr,
    tcp: bool,
) {
    if tcp {
        connection
            .command(
                at,
                Command::AdmitIceTcpFlow {
                    flow: IceTcpFlowId::from_value(u64::from(remote.port())),
                    local,
                    remote,
                },
            )
            .expect("admit accepted stream");
    }
}

fn observations(connection: &mut Connection, at: TimePoint) -> Vec<AuthenticatedFlow> {
    let mut flows = Vec::new();
    for _ in 0..512 {
        match connection.poll(at) {
            Output::Event(Event::IceAuthenticated(flow)) => flows.push(flow),
            Output::Event(Event::Connected) => panic!("non-nominated STUN cannot establish DTLS"),
            Output::Idle { .. } => return flows,
            Output::Closed(reason) => panic!("unexpected close: {reason:?}"),
            // This virtual peer discards replies, which remain committed loss.
            Output::Transmit(_) | Output::Event(_) => {}
            _ => panic!("unexpected RTC output"),
        }
    }
    panic!("connection did not reach idle");
}

#[test]
fn ice_authentication_reports_exact_udp_and_tcp_tuples_before_nomination() {
    for local in ["192.0.2.1:5000", "[2001:db8::1]:5000"] {
        let local: SocketAddr = local.parse().expect("local");
        let remote: SocketAddr = if local.is_ipv4() {
            "192.0.2.2:5001"
        } else {
            "[2001:db8::2]:5001"
        }
        .parse()
        .expect("remote");
        for tcp in [false, true] {
            let (mut connection, at) = connection(local, tcp);
            admit_if_tcp(&mut connection, at, local, remote, tcp);
            let username = format!("{UFRAG}:chromeufrag");
            let payload = request(&username, Some(PASSWORD.as_bytes()));
            for _ in 0..2 {
                connection
                    .receive(at, input(local, remote, tcp, payload.clone()))
                    .expect("STUN input");
            }
            let expected = if tcp {
                AuthenticatedFlow::IceTcp {
                    flow: IceTcpFlowId::from_value(5001),
                    local,
                    remote,
                }
            } else {
                AuthenticatedFlow::Udp { local, remote }
            };
            assert_eq!(observations(&mut connection, at), vec![expected]);

            let mut rebound = remote;
            rebound.set_port(5002);
            admit_if_tcp(&mut connection, at, local, rebound, tcp);
            connection
                .receive(at, input(local, rebound, tcp, payload))
                .expect("rebinding input");
            let expected = if tcp {
                AuthenticatedFlow::IceTcp {
                    flow: IceTcpFlowId::from_value(5002),
                    local,
                    remote: rebound,
                }
            } else {
                AuthenticatedFlow::Udp {
                    local,
                    remote: rebound,
                }
            };
            assert_eq!(observations(&mut connection, at), vec![expected]);
        }
    }
}

#[test]
fn route_hints_and_invalid_ice_credentials_never_authenticate_a_flow() {
    let local = "192.0.2.1:5000".parse().expect("local");
    let remote = "192.0.2.2:5001".parse().expect("remote");
    let username = format!("{UFRAG}:chromeufrag");
    for payload in [
        request(&username, None),
        request(&username, Some(b"wrongpasswordlongenough22")),
        request(&format!("{UFRAG}:wrongpeer"), Some(PASSWORD.as_bytes())),
        request("wrongroute:chromeufrag", Some(PASSWORD.as_bytes())),
        Bytes::from_static(&[0x16, 0, 0]),
    ] {
        for tcp in [false, true] {
            let (mut connection, at) = connection(local, tcp);
            admit_if_tcp(&mut connection, at, local, remote, tcp);
            connection
                .receive(at, input(local, remote, tcp, payload.clone()))
                .expect("hostile input is isolated");
            assert!(observations(&mut connection, at).is_empty());
            assert!(connection.stats().connection.dropped_network_inputs > 0);
        }
    }
}

#[test]
fn ice_tcp_input_carries_one_complete_frame_and_an_opaque_flow() {
    let local = SocketAddr::from(([127, 0, 0, 1], 5000));
    let input = NetworkInput::IceTcp {
        flow: IceTcpFlowId::from_value(9),
        local,
        remote: local,
        frame: Bytes::from_static(&[0, 1, 0]),
    };
    assert!(matches!(input, NetworkInput::IceTcp { .. }));
}

#[test]
fn tcp_retirement_rejects_stale_frames_and_routes_same_tuple_replacement() {
    for (local, remote) in [
        ("192.0.2.1:5000", "192.0.2.2:5001"),
        ("[2001:db8::1]:5000", "[2001:db8::2]:5001"),
    ] {
        let local = local.parse().expect("local");
        let remote = remote.parse().expect("remote");
        let (mut connection, at) = connection(local, true);
        let first = IceTcpFlowId::from_value(1);
        let second = IceTcpFlowId::from_value(2);
        let admit = |flow| Command::AdmitIceTcpFlow {
            flow,
            local,
            remote,
        };
        connection.command(at, admit(first)).expect("first stream");
        connection
            .command(at, admit(first))
            .expect("idempotent admission");
        assert_eq!(
            connection
                .command(at, admit(second))
                .map_err(|rejected| rejected.reason),
            Err(CommandError::InvalidIceTcpFlow)
        );
        let request = request_with_nomination(
            &format!("{UFRAG}:chromeufrag"),
            Some(PASSWORD.as_bytes()),
            true,
        );
        connection
            .receive(at, tcp_input(first, local, remote, request.clone()))
            .expect("nomination");
        assert_tcp_reply(&mut connection, at, first, local, remote);

        connection
            .command(at, Command::RetireIceTcpFlow { flow: first })
            .expect("close first");
        connection
            .command(at, Command::RetireIceTcpFlow { flow: first })
            .expect("idempotent retirement");
        assert_eq!(
            connection.receive(at, tcp_input(first, local, remote, request.clone())),
            Err(ReceiveError::UnknownIceTcpFlow)
        );
        connection
            .command(at, admit(second))
            .expect("replacement stream");
        let invalid = request_with_nomination(
            &format!("{UFRAG}:chromeufrag"),
            Some(b"wrongpasswordlongenough22"),
            true,
        );
        connection
            .receive(at, tcp_input(second, local, remote, invalid))
            .expect("invalid credentials are isolated");
        assert!(observations(&mut connection, at).is_empty());
        connection
            .receive(at, tcp_input(second, local, remote, request))
            .expect("replacement nomination");
        assert_tcp_reply(&mut connection, at, second, local, remote);
    }
}

fn assert_tcp_reply(
    connection: &mut Connection,
    at: TimePoint,
    flow: IceTcpFlowId,
    local: SocketAddr,
    remote: SocketAddr,
) {
    let mut authenticated = false;
    let mut transmitted = false;
    for _ in 0..512 {
        match connection.poll(at) {
            Output::Event(Event::IceAuthenticated(observed)) => {
                assert_eq!(
                    observed,
                    AuthenticatedFlow::IceTcp {
                        flow,
                        local,
                        remote
                    }
                );
                authenticated = true;
            }
            Output::Transmit(transmit) => {
                assert_eq!(transmit.target, TransmitTarget::IceTcp { flow });
                transmitted = true;
            }
            Output::Idle { .. } => {
                assert!(authenticated, "new incarnation must authenticate");
                assert!(transmitted, "ICE reply must reach this incarnation");
                return;
            }
            Output::Closed(reason) => panic!("unexpected close: {reason:?}"),
            Output::Event(_) => {}
            _ => panic!("unexpected RTC output"),
        }
    }
    panic!("connection did not reach idle");
}

#[test]
fn tcp_retirement_cancels_unpublished_work_and_reclaims_capacity() {
    let local = "192.0.2.1:5000".parse().expect("local");
    let remote = "192.0.2.2:5001".parse().expect("remote");
    let (mut connection, at) = connection(local, true);
    let payload = request(&format!("{UFRAG}:chromeufrag"), Some(PASSWORD.as_bytes()));
    for value in 0..128 {
        let flow = IceTcpFlowId::from_value(value);
        connection
            .command(
                at,
                Command::AdmitIceTcpFlow {
                    flow,
                    local,
                    remote,
                },
            )
            .expect("admit next incarnation");
        connection
            .receive(at, tcp_input(flow, local, remote, payload.clone()))
            .expect("authenticate stream");
        connection
            .command(at, Command::RetireIceTcpFlow { flow })
            .expect("retire before polling");
        for _ in 0..512 {
            match connection.poll(at) {
                Output::Idle { .. } => break,
                Output::Event(Event::IceAuthenticated(_)) | Output::Transmit(_) => {
                    panic!("retired work escaped")
                }
                Output::Closed(reason) => panic!("unexpected close: {reason:?}"),
                Output::Event(_) => {}
                _ => panic!("unexpected RTC output"),
            }
        }
    }
}

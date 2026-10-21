#![allow(
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::panic,
    reason = "integration fixtures exercise Bytes-backed input and stop at violated protocol invariants"
)]

use bytes::Bytes;
use pulsebeam_rtc::{
    Connection, ConnectionConfig, ConnectionEntropy, Event, LocalCandidate, LocalIceCredentials,
    NetworkInput, Output, SdpOffer, Transmit, TransmitTarget,
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

fn connection(local: SocketAddr, tcp: bool) -> (Connection, Instant) {
    let at = Instant::now();
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
        std::time::UNIX_EPOCH,
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
        let mut frame = u16::try_from(payload.len())
            .expect("frame length")
            .to_be_bytes()
            .to_vec();
        frame.extend_from_slice(&payload);
        NetworkInput::IceTcp {
            local,
            remote,
            frame: Bytes::from(frame),
        }
    } else {
        NetworkInput::Udp {
            local,
            remote,
            ecn: None,
            payload,
        }
    }
}

fn replies(connection: &mut Connection, at: Instant) -> Vec<Transmit> {
    let mut replies = Vec::new();
    for _ in 0..512 {
        match connection.poll(at) {
            Output::Transmit(packet) => replies.push(packet),
            Output::Idle { next_wakeup } if next_wakeup.is_none_or(|next| next > at) => {
                return replies;
            }
            Output::Idle { .. } | Output::Event(Event::BandwidthEstimate(_)) => {}
            output => panic!("unexpected protocol output: {output:?}"),
        }
    }
    panic!("bounded protocol work did not reach idle");
}

#[test]
fn caller_budgets_alternate_owned_batches_without_draining_a_connection() {
    let mut nodes = [
        connection("192.0.2.1:5000".parse().expect("first"), false),
        connection("192.0.2.1:5002".parse().expect("second"), false),
    ];
    let remote = "192.0.2.2:5001".parse().expect("remote");
    let payload = request(&format!("{UFRAG}:chromeufrag"), Some(PASSWORD.as_bytes()));
    for (index, (connection, at)) in nodes.iter_mut().enumerate() {
        let local = if index == 0 {
            "192.0.2.1:5000"
        } else {
            "192.0.2.1:5002"
        }
        .parse()
        .expect("local");
        for _ in 0..3 {
            connection
                .receive(*at, *at, input(local, remote, false, payload.clone()))
                .expect("authenticated STUN");
        }
    }
    let mut next_wakeup = [Some(nodes[0].1), Some(nodes[1].1)];
    let mut emitted = [0usize; 2];
    let mut finalized_bytes = [0u64; 2];
    let mut socket_accepted = Vec::new();
    let mut local_loss = 0;
    let mut alternated_before_idle = false;
    for _ in 0..16 {
        for index in 0..2 {
            let (connection, at) = &mut nodes[index];
            if next_wakeup[index].is_none_or(|next| next > *at) {
                continue;
            }
            // Budget exhaustion schedules an immediate continuation, not a forced drain.
            next_wakeup[index] = Some(*at);
            let mut batch = std::collections::VecDeque::new();
            for _ in 0..2 {
                match connection.poll(*at) {
                    Output::Transmit(transmit) => {
                        assert_eq!(transmit.kind, pulsebeam_rtc::TransmitKind::Control);
                        finalized_bytes[index] +=
                            u64::try_from(transmit.payload.len()).expect("bounded datagram");
                        emitted[index] += 1;
                        batch.push_back(transmit);
                    }
                    Output::Idle { next_wakeup: next } => {
                        next_wakeup[index] = next;
                        break;
                    }
                    Output::Event(_) => {}
                    output => panic!("unexpected output: {output:?}"),
                }
            }
            assert!(batch.len() <= 2);
            // A virtual UDP submission accepts one whole prefix item. Drop the
            // unsent suffix as local loss rather than replaying/refinalizing it.
            if let Some(accepted) = batch.pop_front() {
                socket_accepted.push(accepted);
            }
            local_loss += batch.len();
            drop(batch);
            assert_eq!(
                connection.stats().connection.transmitted_protocol_bytes,
                finalized_bytes[index]
            );
        }
        if emitted.iter().all(|count| *count > 0)
            && next_wakeup.iter().flatten().count() == 2
            && next_wakeup
                .iter()
                .enumerate()
                .all(|(index, next)| *next == Some(nodes[index].1))
        {
            alternated_before_idle = true;
        }
        if next_wakeup
            .iter()
            .enumerate()
            .all(|(index, next)| next.is_none_or(|next| next > nodes[index].1))
        {
            break;
        }
    }
    assert!(alternated_before_idle);
    assert_eq!(emitted, [3, 3]);
    assert!(
        local_loss > 0,
        "a partial UDP prefix leaves a real finalized suffix"
    );
    assert_eq!(
        socket_accepted.len() + local_loss,
        6,
        "no accepted prefix is replayed"
    );
    assert!(
        socket_accepted
            .iter()
            .all(|transmit| !transmit.payload.is_empty())
    );
}

#[test]
fn authenticated_ice_replies_use_exact_udp_and_tcp_tuples() {
    for (local, remote) in [
        ("192.0.2.1:5000", "192.0.2.2:5001"),
        ("[2001:db8::1]:5000", "[2001:db8::2]:5001"),
    ] {
        let local = local.parse().expect("local");
        let remote: SocketAddr = remote.parse().expect("remote");
        for tcp in [false, true] {
            let (mut connection, at) = connection(local, tcp);
            let payload = request(&format!("{UFRAG}:chromeufrag"), Some(PASSWORD.as_bytes()));
            for remote in [remote, SocketAddr::new(remote.ip(), 5002)] {
                connection
                    .receive(at, at, input(local, remote, tcp, payload.clone()))
                    .expect("authenticated input");
                let replies = replies(&mut connection, at);
                assert!(!replies.is_empty());
                for packet in replies {
                    let expected = if tcp {
                        TransmitTarget::IceTcp { local, remote }
                    } else {
                        TransmitTarget::Udp {
                            local,
                            remote,
                            ecn: None,
                        }
                    };
                    assert_eq!(packet.target, expected);
                    let message = if tcp {
                        let length = u16::from_be_bytes(packet.payload[..2].try_into().unwrap());
                        assert_eq!(usize::from(length), packet.payload.len() - 2);
                        &packet.payload[2..]
                    } else {
                        packet.payload.as_ref()
                    };
                    assert_eq!(
                        &message[..2],
                        &[0x01, 0x01],
                        "STUN success, not an unauthenticated error or DTLS"
                    );
                }
            }
        }
    }
}

#[test]
fn route_hints_and_invalid_credentials_never_get_success_replies() {
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
            connection
                .receive(at, at, input(local, remote, tcp, payload.clone()))
                .expect("hostile input is isolated");
            assert!(replies(&mut connection, at).is_empty());
            assert!(connection.stats().connection.dropped_network_inputs > 0);
        }
    }
}

#[test]
fn tcp_input_requires_exactly_one_complete_rfc4571_frame() {
    let local = "192.0.2.1:5000".parse().expect("local");
    let remote = "192.0.2.2:5001".parse().expect("remote");
    let (mut connection, at) = connection(local, true);
    for bytes in [vec![], vec![0], vec![0, 2, 0], vec![0, 1, 0, 0]] {
        let dropped = connection.stats().connection.dropped_network_inputs;
        connection
            .receive(
                at,
                at,
                NetworkInput::IceTcp {
                    local,
                    remote,
                    frame: Bytes::from(bytes),
                },
            )
            .expect("malformed input is isolated");
        assert!(replies(&mut connection, at).is_empty());
        assert_eq!(
            connection.stats().connection.dropped_network_inputs,
            dropped + 1
        );
    }
    let payload = request(&format!("{UFRAG}:chromeufrag"), Some(PASSWORD.as_bytes()));
    connection
        .receive(at, at, input(local, remote, true, payload))
        .expect("complete authenticated frame");
    assert!(!replies(&mut connection, at).is_empty());
}

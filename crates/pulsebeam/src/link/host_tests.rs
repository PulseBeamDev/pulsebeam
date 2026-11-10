use super::{PEERS, Peer, Pending, remember_closed};
use pulsebeam_link::{
    LinkControl, LinkTime, NetworkClass, NetworkPeer, PeerRole, ReceiveTime, SecretCredential,
};
use pulsebeam_runtime::time::Instant;
use std::collections::{HashMap, VecDeque};

#[test]
fn retained_input_cannot_cross_an_endpoint_replacement() {
    let endpoint = "127.0.0.1:19001".parse().unwrap();
    let credential = SecretCredential::generate().unwrap();
    let peer = || NetworkPeer {
        endpoint,
        credential: credential.clone(),
        expected_peer: credential.fingerprint(),
        role: PeerRole::Initiator,
        class: NetworkClass::Lan,
    };
    let mut control = LinkControl::new(|| LinkTime::from_nanos(0)).unwrap();
    let codec = || crate::link::ShardCodec {
        origin: Instant::now(),
    };
    let old = control.open_network(peer(), codec()).unwrap();
    control.close_network(old);
    let replacement = control.open_network(peer(), codec()).unwrap();
    let mut peers = HashMap::from([(
        endpoint,
        Peer {
            id: replacement,
            driver: control.take_network_driver(replacement).unwrap(),
            queued_input: 0,
        },
    )]);
    let mut pending = Pending {
        peer: old,
        source: endpoint,
        bytes: [0; 1200],
        length: 0,
        observed: ReceiveTime {
            monotonic: LinkTime::from_nanos(0),
            wall: None,
        },
    };
    assert_eq!(pending.receive(&mut peers), None);
    assert_eq!(peers.len(), 1);
    pending.peer = replacement;
    assert_eq!(
        pending.receive(&mut peers),
        Some(Err(pulsebeam_link::io::Error::NotStarted))
    );

    let mut history = VecDeque::new();
    let transport = peers.get(&endpoint).unwrap().driver.health();
    for index in 0..=PEERS {
        let mut transport = transport;
        transport.sent = u64::try_from(index).unwrap();
        remember_closed(
            &mut history,
            crate::link::PeerHealth {
                peer: replacement,
                endpoint,
                transport,
                failure: Some(pulsebeam_link::io::Error::Closed),
            },
        );
    }
    assert_eq!(history.len(), PEERS);
    assert_eq!(history.front().unwrap().transport.sent, 1);
    assert_eq!(
        history.back().unwrap().transport.sent,
        u64::try_from(PEERS).unwrap()
    );
}

#[test]
fn busy_quic_input_does_not_block_another_peers_real_dtls_handshake() {
    use pulsebeam_link::{QuicCredential, QuicPeer, QuicTuning};
    let origin = Instant::now();
    let blocked_addr = "127.0.0.1:19100".parse().unwrap();
    let ready_addr = "127.0.0.1:19101".parse().unwrap();
    let server_addr = "127.0.0.1:19102".parse().unwrap();
    let qlocal = QuicCredential::generate().unwrap();
    let qremote = QuicCredential::generate().unwrap();
    let local = SecretCredential::generate().unwrap();
    let remote = SecretCredential::generate().unwrap();
    let mut control = LinkControl::new(|| LinkTime::from_nanos(0)).unwrap();
    let blocked = control
        .open_network(
            QuicPeer {
                endpoint: blocked_addr,
                credential: qlocal,
                expected_peer: qremote.fingerprint(),
                role: PeerRole::Responder,
                class: NetworkClass::Lan,
                tuning: QuicTuning::default(),
            },
            crate::link::ShardCodec { origin },
        )
        .unwrap();
    let ready = control
        .open_network(
            NetworkPeer {
                endpoint: ready_addr,
                credential: local.clone(),
                expected_peer: remote.fingerprint(),
                role: PeerRole::Responder,
                class: NetworkClass::Lan,
            },
            crate::link::ShardCodec { origin },
        )
        .unwrap();
    let mut blocked_driver = control.take_network_driver(blocked).unwrap();
    let mut ready_driver = control.take_network_driver(ready).unwrap();
    blocked_driver
        .start(LinkTime::from_nanos(0), origin.into_std())
        .unwrap();
    ready_driver
        .start(LinkTime::from_nanos(0), origin.into_std())
        .unwrap();
    for _ in 0..4 {
        ready_driver
            .poll(LinkTime::from_nanos(1_000_000), 64, |_| {
                pulsebeam_link::io::SendOutcome::Rejected
            })
            .unwrap();
        if ready_driver.input_ready() {
            break;
        }
    }
    assert!(!blocked_driver.input_ready());
    assert!(ready_driver.input_ready());
    let mut client_control = LinkControl::new(|| LinkTime::from_nanos(0)).unwrap();
    let client = client_control
        .open_network(
            NetworkPeer {
                endpoint: server_addr,
                credential: remote,
                expected_peer: local.fingerprint(),
                role: PeerRole::Initiator,
                class: NetworkClass::Lan,
            },
            crate::link::ShardCodec { origin },
        )
        .unwrap();
    let mut client = client_control.take_network_driver(client).unwrap();
    client
        .start(LinkTime::from_nanos(0), origin.into_std())
        .unwrap();
    let mut hello = None;
    for _ in 0..4 {
        client
            .poll(LinkTime::from_nanos(1_000_000), 64, |datagram| {
                if hello.is_none() {
                    hello = Some(datagram.payload.to_vec());
                }
                pulsebeam_link::io::SendOutcome::Accepted {
                    bytes: datagram.payload.len(),
                }
            })
            .unwrap();
        if hello.is_some() {
            break;
        }
    }
    let hello = hello.unwrap();
    let mut bytes = [0; 1200];
    bytes
        .get_mut(..hello.len())
        .unwrap()
        .copy_from_slice(&hello);
    let observed = ReceiveTime {
        monotonic: LinkTime::from_nanos(1_000_000),
        wall: None,
    };
    let mut pending = VecDeque::from([
        Pending {
            peer: blocked,
            source: blocked_addr,
            bytes: [0; 1200],
            length: 0,
            observed,
        },
        Pending {
            peer: ready,
            source: ready_addr,
            bytes,
            length: hello.len(),
            observed,
        },
    ]);
    let mut peers = HashMap::from([
        (
            blocked_addr,
            Peer {
                id: blocked,
                driver: blocked_driver,
                queued_input: 1,
            },
        ),
        (
            ready_addr,
            Peer {
                id: ready,
                driver: ready_driver,
                queued_input: 1,
            },
        ),
    ]);
    let mut closed = VecDeque::new();
    super::drain_pending(&mut pending, &mut peers, &mut closed);
    assert!(closed.is_empty());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending.front().unwrap().peer, blocked);
    assert_eq!(peers.get(&blocked_addr).unwrap().queued_input, 1);
    assert_eq!(peers.get(&ready_addr).unwrap().queued_input, 0);
    assert!(!peers.get(&ready_addr).unwrap().driver.input_ready());
    let mut flights = 0;
    peers
        .get_mut(&ready_addr)
        .unwrap()
        .driver
        .poll(LinkTime::from_nanos(1_000_000), 64, |datagram| {
            flights += 1;
            pulsebeam_link::io::SendOutcome::Accepted {
                bytes: datagram.payload.len(),
            }
        })
        .unwrap();
    assert!(flights > 0);
}

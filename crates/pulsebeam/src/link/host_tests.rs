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

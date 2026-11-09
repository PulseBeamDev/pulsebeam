#![allow(
    clippy::panic,
    reason = "conformance health helper fails explicitly when the bounded mailbox cannot progress"
)]

use super::*;
use crate::link::{Control, HostHealth};
use pulsebeam_link::{LinkError, NetworkClass, NetworkPeer, PeerRole, SecretCredential};

async fn health(control: &Control) -> HostHealth {
    for _ in 0..100 {
        match control.health().await {
            Ok(snapshot) => return snapshot,
            Err(LinkError::Capacity) => {
                pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
            }
            result => panic!("unexpected Link health result: {result:?}"),
        }
    }
    panic!("Link health mailbox did not drain");
}

#[test]
fn trusted_control_drives_real_hosts_and_reports_peer_lifecycle() {
    let host = pulsebeam_runtime::testing::test_host_ip("192.168.250.103");
    pulsebeam_runtime::testing::run_local(host, async {
        pulsebeam_runtime::time::timeout(Duration::from_secs(5), async {
            let shutdown = CancellationToken::new();
            let _cleanup = shutdown.clone().drop_guard();
            let mut tasks = Vec::new();
            let mut controls = Vec::new();
            for seed in [71, 72] {
                let (ready, control) = pulsebeam_runtime::sync::oneshot::channel();
                let builder = NodeBuilder::new()
                    .with_current_runtime()
                    .local_addr("127.0.0.1:0".parse().unwrap())
                    .advertise_bound_udp()
                    .ice_tcp(IceTcpConfig::Disabled)
                    .rng(rand::Rng::seed_from_u64(seed))
                    .with_link_control("127.0.0.1:0".parse().unwrap(), ready);
                tasks.push(pulsebeam_runtime::task::spawn_local(
                    builder.run(shutdown.child_token()),
                ));
                controls.push(control.await.unwrap());
            }
            let a = &controls[0];
            let b = &controls[1];
            let a_address = health(a).await.local_addr;
            let b_address = health(b).await.local_addr;
            assert_ne!(a_address, b_address);
            let ca = SecretCredential::generate().unwrap();
            let cb = SecretCredential::generate().unwrap();
            let aid = a
                .open_peer(NetworkPeer {
                    endpoint: b_address,
                    credential: ca.clone(),
                    expected_peer: cb.fingerprint(),
                    role: PeerRole::Initiator,
                    class: NetworkClass::Lan,
                })
                .await
                .unwrap();
            let bid = b
                .open_peer(NetworkPeer {
                    endpoint: a_address,
                    credential: cb,
                    expected_peer: ca.fingerprint(),
                    role: PeerRole::Responder,
                    class: NetworkClass::Wan,
                })
                .await
                .unwrap();
            let mut connected = false;
            for _ in 0..4000 {
                let ah = health(a).await;
                let bh = health(b).await;
                if ah
                    .peers
                    .iter()
                    .any(|peer| peer.peer == aid && peer.transport.ready)
                    && bh
                        .peers
                        .iter()
                        .any(|peer| peer.peer == bid && peer.transport.ready)
                {
                    connected = true;
                    break;
                }
                assert!(ah.peers.iter().all(|peer| peer.failure.is_none()));
                assert!(bh.peers.iter().all(|peer| peer.failure.is_none()));
                pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
            }
            assert!(
                connected,
                "real UDP hosts did not complete their pinned handshake"
            );
            let unknown = crate::route::NodeRouteAddress::new(
                crate::route::RouteId::new(ShardId::new(0), 999),
                1,
            );
            assert_eq!(
                a.bind_receive(aid, vec![unknown]).await,
                Err(pulsebeam_link::BindError::Closed)
            );
            a.request_close_peer(aid).unwrap();
            b.request_close_peer(bid).unwrap();
            let mut closed = false;
            for _ in 0..100 {
                let ah = health(a).await;
                let bh = health(b).await;
                if ah.peers.iter().any(|peer| {
                    peer.peer == aid && peer.failure == Some(pulsebeam_link::io::Error::Closed)
                }) && bh.peers.iter().any(|peer| {
                    peer.peer == bid && peer.failure == Some(pulsebeam_link::io::Error::Closed)
                }) {
                    assert!(ah.peers.iter().all(|peer| !peer.transport.ready));
                    assert!(bh.peers.iter().all(|peer| !peer.transport.ready));
                    closed = true;
                    break;
                }
                pulsebeam_runtime::time::sleep(Duration::from_millis(1)).await;
            }
            assert!(closed, "explicit peer closure was not observable");
            shutdown.cancel();
            for task in tasks {
                task.await.unwrap().unwrap();
            }
            assert!(matches!(a.health().await, Err(LinkError::Closed)));
            assert!(matches!(b.health().await, Err(LinkError::Closed)));
        })
        .await
        .expect("bounded virtual-time controller/host scenario");
    });
}

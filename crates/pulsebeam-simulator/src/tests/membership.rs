//! Real membership and Chitchat tasks using its public transport and wire codec.
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, ensure};
use async_trait::async_trait;
use chitchat::transport::{ChannelTransport, RecvOutcome, SendOutcome, Socket, Transport};
use chitchat::{ChitchatEnvelope, ChitchatMessage, Deserializable, Serializable};
use pulsebeam_membership::{Config, Handle, Incarnation, Liveness, Membership, Metadata, NodeId};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

type Capture = Arc<Mutex<Option<Vec<ChitchatEnvelope>>>>;

// A bounded tap on upstream's socket, not another network model. Only ACKs need
// replay; their delta supports the public serialization traits.
fn copy_ack(envelope: &ChitchatEnvelope) -> Result<Option<ChitchatEnvelope>> {
    let ChitchatMessage::Ack { delta } = &envelope.message else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    delta.serialize(&mut bytes);
    let delta = Deserializable::deserialize(&mut bytes.as_slice())?;
    Ok(Some(ChitchatEnvelope {
        version: envelope.version,
        message: ChitchatMessage::Ack { delta },
    }))
}

struct TapSocket {
    inner: Box<dyn Socket>,
    capture: Capture,
}

#[async_trait]
impl Socket for TapSocket {
    fn local_addr(&self) -> Result<SocketAddr> {
        self.inner.local_addr()
    }
    async fn send(&mut self, to: SocketAddr, envelope: ChitchatEnvelope) -> Result<SendOutcome> {
        {
            let mut capture = self.capture.lock().unwrap();
            if let Some(frames) = capture.as_mut()
                && frames.len() < 12
                && let Some(copy) = copy_ack(&envelope)?
            {
                frames.push(copy);
            }
        }
        self.inner.send(to, envelope).await
    }
    async fn recv(&mut self) -> Result<RecvOutcome> {
        self.inner.recv().await
    }
}

fn addr(peer: u8) -> SocketAddr {
    ([10, 77, 0, peer], 7946).into()
}
fn node(peer: u8) -> NodeId {
    NodeId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, peer])
}
fn incarnation(peer: u8) -> Incarnation {
    Incarnation {
        node_id: node(if peer == 5 { 2 } else { peer }),
        generation_id: if peer == 5 { 2 } else { 1 },
        gossip_advertise_addr: addr(peer),
    }
}

struct Peer {
    handle: Handle,
    shutdown: CancellationToken,
    done: Option<oneshot::Receiver<Result<(), String>>>,
}
impl Peer {
    async fn stop(&mut self) {
        self.shutdown.cancel();
        self.done.take().unwrap().await.unwrap().unwrap();
    }
}

async fn run_peer(
    peer: u8,
    transport: ChannelTransport,
    capture: Capture,
    reports: mpsc::Sender<(u8, Peer)>,
) -> Result<()> {
    let socket = transport.open(addr(peer)).await?;
    let socket = if peer == 2 {
        Box::new(TapSocket {
            inner: socket,
            capture,
        }) as Box<dyn Socket>
    } else {
        socket
    };
    let mut metadata = Metadata::new();
    if peer == 2 {
        metadata.insert("build".into(), "old".into());
    }
    let config = Config {
        cluster_id: if peer == 4 { "other" } else { "primary" }.into(),
        incarnation: incarnation(peer),
        listen_addr: addr(peer),
        seeds: if peer == 1 {
            Vec::new()
        } else {
            vec![addr(if peer == 5 { 3 } else { 1 })]
        },
        metadata,
    };
    let (membership, handle) = Membership::start(config, socket).await?;
    let shutdown = CancellationToken::new();
    let (done, finished) = oneshot::channel();
    reports
        .send((
            peer,
            Peer {
                handle,
                shutdown: shutdown.clone(),
                done: Some(finished),
            },
        ))
        .await
        .map_err(|_| anyhow::anyhow!("scenario driver stopped"))?;
    let result = membership
        .run(shutdown)
        .await
        .map_err(|error| error.to_string());
    let _ = done.send(result);
    Ok(())
}

async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(120), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("membership convergence deadline");
}

#[test]
fn membership_discovery_partition_restart_and_stale_replay() {
    let seed = std::env::var("PULSEBEAM_SIM_SEED")
        .map(|s| s.parse().unwrap())
        .unwrap_or(1);
    let _clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(1500))
        .tick_duration(Duration::from_millis(250))
        .rng_seed(seed)
        .build();
    let transport = ChannelTransport::default();
    let capture: Capture = Arc::new(Mutex::new(Some(Vec::new())));
    let (reports, mut received) = mpsc::channel(8);
    for peer in 1..=4 {
        let transport = transport.clone();
        let capture = capture.clone();
        let reports = reports.clone();
        sim.host(addr(peer).ip(), move || {
            let transport = transport.clone();
            let capture = capture.clone();
            let reports = reports.clone();
            async move {
                run_peer(peer, transport, capture, reports)
                    .await
                    .map_err(Into::into)
            }
        });
    }
    let (start_new, starting) = oneshot::channel();
    let new_transport = transport.clone();
    let new_capture = capture.clone();
    sim.client(addr(5).ip(), async move {
        starting.await?;
        run_peer(5, new_transport, new_capture, reports)
            .await
            .map_err(Into::into)
    });
    sim.client(addr(99).ip(), async move {
        let scenario = async move {
            let mut peers = BTreeMap::new();
            for _ in 0..4 {
                let (id, peer) = received.recv().await.unwrap();
                peers.insert(id, peer);
            }
            let old = incarnation(2);
            let new = incarnation(5);
            until(|| {
                [1, 2, 3].into_iter().all(|peer| {
                    peers[&peer]
                        .handle
                        .snapshot()
                        .members
                        .values()
                        .filter(|member| member.is_usable())
                        .count()
                        == 3
                })
            })
            .await;
            ensure!(
                peers[&4].handle.snapshot().members.len() == 1,
                "cluster domain crossed"
            );
            until(|| capture.lock().unwrap().as_ref().unwrap().len() >= 2).await;
            let frames = capture.lock().unwrap().take().unwrap();
            ensure!(
                frames
                    .iter()
                    .any(|frame| format!("{frame:?}").contains("old")),
                "tap missed old metadata delta"
            );

            let mut slow = peers[&3].handle.subscribe();
            let initial = slow.borrow_and_update().revision;
            peers[&2]
                .handle
                .replace_metadata(Metadata::from([
                    ("build".into(), "intermediate".into()),
                    ("keep".into(), "first".into()),
                ]))
                .await?;
            peers[&2]
                .handle
                .replace_metadata(Metadata::from([("keep".into(), "latest".into())]))
                .await?;
            until(|| {
                peers[&3]
                    .handle
                    .snapshot()
                    .members
                    .get(&old)
                    .is_some_and(|member| {
                        member.metadata == Metadata::from([("keep".into(), "latest".into())])
                    })
            })
            .await;
            let recovered = slow.borrow_and_update().clone();
            ensure!(
                recovered.revision > initial
                    && !recovered.members[&old].metadata.contains_key("build"),
                "coalesced snapshot did not recover deletion"
            );

            peers.get_mut(&1).unwrap().stop().await;
            peers[&2]
                .handle
                .replace_metadata(Metadata::from([("keep".into(), "after-seed".into())]))
                .await?;
            until(|| {
                peers[&3].handle.snapshot().members[&old]
                    .metadata
                    .get("keep")
                    .is_some_and(|value| value == "after-seed")
            })
            .await;
            transport.remove_link(addr(2), addr(3)).await;
            until(|| peers[&3].handle.snapshot().members[&old].liveness == Liveness::Suspected)
                .await;
            transport.add_link(addr(2), addr(3)).await;
            until(|| peers[&3].handle.snapshot().members[&old].liveness == Liveness::Live).await;

            start_new.send(()).unwrap();
            let (id, peer) = received.recv().await.unwrap();
            peers.insert(id, peer);
            until(|| {
                let snapshot = peers[&3].handle.snapshot();
                snapshot.highest_generations.get(&old.node_id) == Some(&2)
                    && snapshot
                        .members
                        .get(&new)
                        .is_some_and(pulsebeam_membership::Member::is_usable)
                    && snapshot.members[&old].superseded
            })
            .await;
            peers.get_mut(&5).unwrap().stop().await;
            until(|| peers[&3].handle.snapshot().members[&new].liveness == Liveness::Suspected)
                .await;
            // Replay after production tombstone GC, not just while deletions are retained.
            tokio::time::sleep(Duration::from_secs(16 * 60)).await;
            let mut injector = transport.open(addr(254)).await?;
            for frame in frames.into_iter().rev() {
                injector.send(addr(3), copy_ack(&frame)?.unwrap()).await?;
                injector.send(addr(3), frame).await?;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            let snapshot = peers[&3].handle.snapshot();
            ensure!(
                snapshot.highest_generations[&old.node_id] == 2,
                "stale replay lowered generation"
            );
            ensure!(
                snapshot.members[&old].superseded
                    && snapshot.members[&old].liveness == Liveness::Live,
                "older live generation was promoted"
            );
            ensure!(
                snapshot.members[&new].liveness == Liveness::Suspected
                    && !snapshot.members[&new].superseded,
                "suspected current incarnation lost its status"
            );
            ensure!(
                snapshot.members[&old]
                    .metadata
                    .get("keep")
                    .is_some_and(|value| value == "after-seed")
                    && !snapshot.members[&old].metadata.contains_key("build"),
                "stale replay resurrected metadata"
            );
            ensure!(
                !snapshot.members.keys().any(|id| id.node_id == node(4)),
                "foreign cluster appeared"
            );
            for peer in [2, 3, 4] {
                peers.get_mut(&peer).unwrap().stop().await;
            }
            Ok::<(), anyhow::Error>(())
        };
        scenario.await.map_err(Into::into)
    });
    sim.run().unwrap();
}

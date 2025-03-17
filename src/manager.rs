use crate::proto::{Message, PeerInfo};
use std::{collections::BTreeMap, ops::RangeBounds, sync::Arc};

use futures::FutureExt;
use moka::{
    future::Cache,
    notification::{ListenerFuture, RemovalCause},
};
use parking_lot::RwLock;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const EVENT_CHANNEL_CAPACITY: usize = 64;
pub type GroupId = String;
pub type PeerId = String;

pub struct ManagerConfig {
    pub capacity: u64,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self { capacity: 65536 }
    }
}

pub type Index = BTreeMap<PeerInfo, PeerStats>;

#[derive(Clone)]
pub struct Manager {
    pub cfg: Arc<ManagerConfig>,
    pub conns: Cache<PeerInfo, mpsc::Sender<Message>, ahash::RandomState>,
    pub index: IndexManager,
    pub event_ch: mpsc::Sender<ConnEvent>,
}

impl Manager {
    pub fn spawn(token: CancellationToken, cfg: ManagerConfig) -> Self {
        let event_ch = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let sender = event_ch.0.clone();
        let eviction_listener = move |k: Arc<PeerInfo>,
                                      _v: mpsc::Sender<Message>,
                                      _cause: RemovalCause|
              -> ListenerFuture {
            let event_ch = sender.clone();
            async move {
                let peer = k.as_ref().clone();
                if let Err(err) = event_ch.send(ConnEvent::Removed(peer)).await {
                    tracing::warn!(
                        "unexpected event_ch ended prematurely on eviction: {:?}",
                        err
                    );
                }
            }
            .boxed()
        };
        let conns = Cache::builder()
            .max_capacity(cfg.capacity)
            .async_eviction_listener(eviction_listener)
            .build_with_hasher(ahash::RandomState::default());

        let index = IndexManager::new();
        let index_worker = index.clone();
        tokio::spawn(async move {
            token
                .run_until_cancelled_owned(index_worker.spawn(event_ch.1))
                .await
        });
        Self {
            cfg: Arc::new(cfg),
            conns,
            index,
            event_ch: event_ch.0,
        }
    }

    pub async fn allocate(&self, peer: PeerInfo) -> mpsc::Receiver<Message> {
        let (sender, receiver) = mpsc::channel(1);
        tracing::info!("allocated connection: {}", peer);
        self.conns.insert(peer.clone(), sender).await;
        if let Err(err) = self.event_ch.send(ConnEvent::Inserted(peer)).await {
            tracing::warn!("unexpected event_ch ended prematurely on insert: {:?}", err);
        }
        receiver
    }
}

#[derive(Clone, Debug)]
pub enum ConnEvent {
    Inserted(PeerInfo),
    Removed(PeerInfo),
}

#[derive(Clone)]
pub struct IndexManager {
    state: Arc<RwLock<IndexManagerState>>,
}

impl IndexManager {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(IndexManagerState {
                index: BTreeMap::new(),
            })),
        }
    }

    pub async fn spawn(&self, mut event_ch: mpsc::Receiver<ConnEvent>) {
        tracing::info!("spawned index worker");
        let mut buf = Vec::with_capacity(EVENT_CHANNEL_CAPACITY);
        loop {
            let received = event_ch.recv_many(&mut buf, EVENT_CHANNEL_CAPACITY).await;

            {
                let mut state = self.state.write();
                for event in buf[..received].iter() {
                    state.handle_event(event);
                }
                buf.clear();
            }
        }
    }

    pub fn select(&self, range: impl RangeBounds<PeerInfo>) -> Vec<(PeerInfo, PeerStats)> {
        let state = self.state.read();
        let mut result = Vec::new();
        for (k, v) in state.index.range(range) {
            result.push((k.clone(), v.clone()))
        }
        result
    }

    pub fn select_one(&self, range: impl RangeBounds<PeerInfo>) -> Option<PeerInfo> {
        let state = self.state.read();
        // pick the youngest connection
        let found = state
            .index
            .range(range)
            .max_by_key(|(_, p)| p.inserted_at)?;
        Some(found.0.clone())
    }

    pub fn select_group(&self, group_id: GroupId) -> Vec<(PeerInfo, PeerStats)> {
        let start = PeerInfo {
            group_id: group_id.clone(),
            peer_id: "".to_string(),
            conn_id: u32::MIN,
        };

        let end = PeerInfo {
            group_id,
            peer_id: "~".to_string(),
            conn_id: u32::MAX,
        };
        self.select(start..=end)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PeerStats {
    inserted_at: chrono::DateTime<chrono::Utc>,
}

pub struct IndexManagerState {
    index: BTreeMap<PeerInfo, PeerStats>,
}

impl IndexManagerState {
    pub fn handle_event(&mut self, e: &ConnEvent) {
        tracing::trace!("handle event: {:?}", e);
        match e {
            // TODO: update PeerStats
            ConnEvent::Inserted(peer) => self.index.insert(
                peer.clone(),
                PeerStats {
                    inserted_at: chrono::Utc::now(),
                },
            ),
            ConnEvent::Removed(peer) => self.index.remove(peer),
        };
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn select_one() {
        let conn_a = PeerInfo {
            group_id: "default".to_string(),
            peer_id: "a".to_string(),
            conn_id: 2818993334,
        };
        let conn_b = PeerInfo {
            group_id: "default".to_string(),
            peer_id: "b".to_string(),
            conn_id: 2913253855,
        };

        let manager = IndexManager::new();
        manager
            .state
            .write()
            .handle_event(&ConnEvent::Inserted(conn_a.clone()));
        manager
            .state
            .write()
            .handle_event(&ConnEvent::Inserted(conn_b.clone()));

        let start = PeerInfo {
            conn_id: 0,
            ..conn_a.clone()
        };
        let end = PeerInfo {
            conn_id: u32::MAX,
            ..conn_a.clone()
        };
        let conn = manager.select_one(start.clone()..=end.clone()).unwrap();
        assert_eq!(conn.peer_id, conn_a.peer_id);
        assert_eq!(conn.conn_id, conn_a.conn_id);

        let conn_a_new = PeerInfo {
            group_id: "default".to_string(),
            peer_id: "a".to_string(),
            conn_id: 1,
        };
        manager
            .state
            .write()
            .handle_event(&ConnEvent::Inserted(conn_a_new.clone()));
        let conn = manager.select_one(start..=end).unwrap();
        assert_eq!(conn.peer_id, conn_a_new.peer_id);
        assert_eq!(conn.conn_id, conn_a_new.conn_id);
    }

    #[test]
    fn insert_multiple_peers() {
        let conn_a = PeerInfo {
            group_id: "default".to_string(),
            peer_id: "a".to_string(),
            conn_id: 2818993334,
        };
        let conn_b = PeerInfo {
            group_id: "default".to_string(),
            peer_id: "b".to_string(),
            conn_id: 2913253855,
        };

        let manager1 = IndexManager::new();
        let manager2 = manager1.clone();
        manager1
            .state
            .write()
            .handle_event(&ConnEvent::Inserted(conn_a.clone()));
        manager2
            .state
            .write()
            .handle_event(&ConnEvent::Inserted(conn_b.clone()));

        for manager in [manager1, manager2] {
            let mut peers: Vec<PeerInfo> = manager
                .select_group("default".to_string())
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            peers.sort();
            assert_eq!(peers.len(), 2);
            assert_eq!(peers[0].peer_id, conn_a.peer_id);
            assert_eq!(peers[0].conn_id, conn_a.conn_id);
            assert_eq!(peers[1].peer_id, conn_b.peer_id);
            assert_eq!(peers[1].conn_id, conn_b.conn_id);
        }
    }
}

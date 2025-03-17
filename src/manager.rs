use crate::proto::{Message, PeerInfo};
use std::{collections::BTreeMap, fmt::Debug, sync::Arc, time::Instant};

use parking_lot::RwLock;
use quick_cache::sync::Cache;
use tokio::sync::mpsc;
use tokio_stream::{wrappers::ReceiverStream, StreamExt};
use tokio_util::sync::CancellationToken;
use tracing::event;

const EVENT_CHANNEL_CAPACITY: usize = 64;
pub type GroupId = String;
pub type PeerId = String;

pub struct ManagerConfig {
    pub max_groups: u32,
    pub max_peers_per_group: u16,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            max_groups: 65536,
            max_peers_per_group: 16,
        }
    }
}

pub type Index = BTreeMap<PeerInfo, PeerStats>;

#[derive(Clone)]
pub struct Manager {
    cfg: Arc<ManagerConfig>,
    conns: Arc<Cache<PeerInfo, mpsc::Sender<Message>>>,
    index: Arc<RwLock<Index>>,
    event_ch: mpsc::Sender<ConnEvent>,
}

pub struct PeerStats {}

pub enum ConnEvent {
    Inserted(PeerInfo),
    Removed(PeerInfo),
}

async fn index_worker(event_ch: mpsc::Receiver<ConnEvent>, index: Arc<RwLock<Index>>) {
    let handle_event = |e: ConnEvent| {
        let mut idx = index.write();
        match e {
            // TODO: update PeerStats
            ConnEvent::Inserted(peer) => idx.insert(peer, PeerStats {}),
            ConnEvent::Removed(peer) => idx.remove(&peer),
        }
    };

    let mut event_stream = ReceiverStream::new(event_ch);
    while let Some(e) = event_stream.next().await {
        handle_event(e);
    }
}

impl Manager {
    pub fn start(token: CancellationToken, cfg: ManagerConfig) -> Self {
        let conns = Arc::new(Cache::new(
            cfg.max_groups as usize * cfg.max_peers_per_group as usize,
        ));
        let index = Arc::new(RwLock::new(BTreeMap::new()));
        let event_ch = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            cfg: Arc::new(cfg),
            conns,
            index,
            event_ch,
        }
    }

    pub fn insert(&self, peer: PeerInfo) -> mpsc::Receiver<Message> {
        let (sender, receiver) = mpsc::channel(1);
        self.conns.insert(peer, sender);
        receiver
    }

    pub fn get(&self, peer: &PeerInfo) -> Option<mpsc::Sender<Message>> {
        self.conns.get(peer)
    }
}

#[derive(Clone)]
pub struct Group {
    state: Arc<RwLock<GroupState>>,
    capacity: u16,
}

#[derive(Default)]
pub struct GroupState {
    peers: BTreeMap<PeerConn, Peer>,
}

#[derive(Clone, Debug, Eq)]
pub struct PeerConn {
    pub peer_id: PeerId,
    pub conn_id: u32,
}

#[derive(Clone)]
pub struct Peer {
    pub mailbox: mpsc::Sender<Message>,
    pub started_at: Instant,
}

impl Group {
    pub fn new(capacity: u16) -> Self {
        Self {
            capacity,
            state: Arc::new(RwLock::new(GroupState::default())),
        }
    }

    pub fn collect(&self) -> Vec<PeerConn> {
        let state = self.state.read();
        state.peers.keys().cloned().collect()
    }

    pub fn upsert(&self, conn: PeerConn) -> mpsc::Receiver<Message> {
        let (sender, receiver) = mpsc::channel(1);
        let mut state = self.state.write();
        state.peers.insert(
            conn,
            Peer {
                started_at: Instant::now(),
                mailbox: sender,
            },
        );
        receiver
    }

    pub fn get(&self, conn: PeerConn) -> Option<Peer> {
        let state = self.state.read();
        state.peers.get(&conn).cloned()
    }

    pub fn select_one(&self, peer_id: PeerId) -> Option<(PeerConn, Peer)> {
        let state = self.state.read();
        let start = PeerConn {
            peer_id: peer_id.clone(),
            conn_id: 0,
        };
        let end = PeerConn {
            peer_id,
            conn_id: u32::MAX,
        };

        // pick the youngest connection
        let found = state
            .peers
            .range(start..=end)
            .max_by_key(|(_, p)| p.started_at)?;
        let result = (found.0.clone(), found.1.clone());
        Some(result)
    }

    pub fn remove(&self, peer_id: &PeerConn) -> usize {
        let mut state = self.state.write();
        state.peers.remove(peer_id);
        state.peers.len()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn select_one() {
        let group = Group::new(8);
        let conn_a = PeerConn {
            peer_id: "a".to_string(),
            conn_id: 2818993334,
        };
        let conn_b = PeerConn {
            peer_id: "b".to_string(),
            conn_id: 2913253855,
        };

        group.upsert(conn_a.clone());
        group.upsert(conn_b);

        let result = group.select_one("a".to_string());
        let (conn, _) = result.unwrap();
        assert_eq!(conn.peer_id, conn_a.peer_id);
        assert_eq!(conn.conn_id, conn_a.conn_id);

        let conn_a_new = PeerConn {
            peer_id: "a".to_string(),
            conn_id: 1,
        };
        group.upsert(conn_a_new.clone());
        let result = group.select_one("a".to_string());
        let (conn, _) = result.unwrap();
        assert_eq!(conn.peer_id, conn_a_new.peer_id);
        assert_eq!(conn.conn_id, conn_a_new.conn_id);
    }

    #[test]
    fn insert_multiple_peers() {
        let manager1 = Manager::new(ManagerConfig::default());
        let manager2 = manager1.clone();
        let group_id = "default";
        let conn_a = PeerConn {
            peer_id: "a".to_string(),
            conn_id: 2818993334,
        };
        let conn_b = PeerConn {
            peer_id: "b".to_string(),
            conn_id: 2913253855,
        };

        manager1
            .get_or_insert(group_id.to_string())
            .upsert(conn_a.clone());
        manager2
            .get_or_insert(group_id.to_string())
            .upsert(conn_b.clone());

        for manager in [manager1, manager2] {
            let mut peers = manager.get(group_id).unwrap().collect();
            peers.sort();
            assert_eq!(peers.len(), 2);
            assert_eq!(peers[0].peer_id, conn_a.peer_id);
            assert_eq!(peers[0].conn_id, conn_a.conn_id);
            assert_eq!(peers[1].peer_id, conn_b.peer_id);
            assert_eq!(peers[1].conn_id, conn_b.conn_id);

            let group = manager.get(group_id).unwrap();
            let result = group.select_one(conn_b.peer_id.clone()).unwrap();
            assert_eq!(result.0.peer_id, conn_b.peer_id);
            assert_eq!(result.0.conn_id, conn_b.conn_id);
        }
    }
}

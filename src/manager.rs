use crate::proto::{Message, PeerInfo};
use std::{collections::BTreeMap, fmt::Debug, sync::Arc, time::Instant};

use ahash::HashMap;
use parking_lot::RwLock;

pub const MAILBOX_CAPACITY: usize = 8;
pub type Mailbox = (flume::Sender<Message>, flume::Receiver<Message>);
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

#[derive(Clone)]
pub struct Manager {
    cfg: Arc<ManagerConfig>,
    state: Arc<RwLock<ManagerState>>,
}

pub struct ManagerState {
    groups: HashMap<GroupId, Group>,
}

impl Manager {
    pub fn new(cfg: ManagerConfig) -> Self {
        Self {
            cfg: Arc::new(cfg),
            state: Arc::new(RwLock::new(ManagerState {
                groups: HashMap::default(),
            })),
        }
    }

    pub fn insert(&self, group_id: GroupId) -> Group {
        let mut state = self.state.write();
        let group = state
            .groups
            .entry(group_id.clone())
            .or_insert_with(|| Group::new(group_id, self.cfg.max_peers_per_group));
        group.clone()
    }

    pub fn collect_peers(&self, group_id: &GroupId) -> Option<Vec<PeerConn>> {
        let group = {
            let state = self.state.read();
            state.groups.get(group_id).cloned()?
        };
        Some(group.collect())
    }

    pub fn get(&self, group_id: GroupId) -> Group {
        let group_maybe = {
            let state = self.state.read();
            state.groups.get(&group_id).cloned()
        };

        match group_maybe {
            Some(mailbox) => mailbox,
            None => self.insert(group_id),
        }
    }

    pub fn remove(&self, peer: PeerInfo) {
        let mut state = self.state.write();
        if let Some(group) = state.groups.get(&peer.group_id) {
            let conn = PeerConn {
                peer_id: peer.peer_id,
                conn_id: peer.conn_id,
            };
            let group_size = group.remove(&conn);
            if group_size == 0 {
                state.groups.remove(&peer.group_id);
            }
        }
    }
}

#[derive(Clone)]
pub struct Group {
    state: Arc<RwLock<GroupState>>,
    group_id: GroupId,
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
    pub mailbox: Mailbox,
    pub started_at: Instant,
}

impl Ord for PeerConn {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match self.peer_id.cmp(&other.peer_id) {
            std::cmp::Ordering::Equal => self.conn_id.cmp(&other.conn_id),
            ordering => ordering,
        }
    }
}

impl PartialOrd for PeerConn {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for PeerConn {
    fn eq(&self, other: &Self) -> bool {
        self.peer_id == other.peer_id && self.conn_id == other.conn_id
    }
}

impl std::fmt::Debug for Group {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.group_id)
    }
}

impl Group {
    pub fn new(group_id: String, capacity: u16) -> Self {
        Self {
            group_id,
            capacity,
            state: Arc::new(RwLock::new(GroupState::default())),
        }
    }

    pub fn collect(&self) -> Vec<PeerConn> {
        let state = self.state.read();
        state.peers.keys().cloned().collect()
    }

    #[tracing::instrument]
    pub fn insert(&self, conn: PeerConn) -> Peer {
        let mut state = self.state.write();
        let peer = state
            .peers
            .entry(conn)
            .and_modify(|p| p.started_at = Instant::now())
            .or_insert_with(|| Peer {
                started_at: Instant::now(),
                mailbox: flume::bounded(MAILBOX_CAPACITY),
            });

        peer.clone()
    }

    #[tracing::instrument]
    pub fn get(&self, conn: PeerConn) -> Peer {
        let peer_maybe = {
            let state = self.state.read();
            state.peers.get(&conn).cloned()
        };

        match peer_maybe {
            Some(peer) => peer,
            None => self.insert(conn),
        }
    }

    #[tracing::instrument]
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
        let group = Group::new("default".to_string(), 8);
        let conn_a = PeerConn {
            peer_id: "a".to_string(),
            conn_id: 2818993334,
        };
        let conn_b = PeerConn {
            peer_id: "b".to_string(),
            conn_id: 2913253855,
        };

        group.insert(conn_a.clone());
        group.insert(conn_b);

        let result = group.select_one("a".to_string());
        let (conn, _) = result.unwrap();
        assert_eq!(conn.peer_id, conn_a.peer_id);
        assert_eq!(conn.conn_id, conn_a.conn_id);
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

        manager1.get(group_id.to_string()).get(conn_a.clone());
        manager2.get(group_id.to_string()).get(conn_b.clone());

        for manager in [manager1, manager2] {
            let mut peers = manager.collect_peers(&group_id.to_string()).unwrap();
            peers.sort();
            assert_eq!(peers.len(), 2);
            assert_eq!(peers[0].peer_id, conn_a.peer_id);
            assert_eq!(peers[0].conn_id, conn_a.conn_id);
            assert_eq!(peers[1].peer_id, conn_b.peer_id);
            assert_eq!(peers[1].conn_id, conn_b.conn_id);

            let group = manager.get(group_id.to_string());
            let result = group.select_one(conn_b.peer_id.clone()).unwrap();
            assert_eq!(result.0.peer_id, conn_b.peer_id);
            assert_eq!(result.0.conn_id, conn_b.conn_id);
        }
    }
}

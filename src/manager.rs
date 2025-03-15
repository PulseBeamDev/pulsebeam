use crate::proto::Message;
use std::{collections::BTreeMap, sync::Arc};

use ahash::HashMap;
use tokio::sync::RwLock;

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

    pub async fn insert(&self, group_id: GroupId) -> Group {
        let mut state = self.state.write().await;
        let group = state
            .groups
            .entry(group_id)
            .or_insert_with(|| Group::new(self.cfg.max_peers_per_group));
        group.clone()
    }

    pub async fn collect_peers(&self, group_id: &GroupId) -> Option<Vec<PeerConn>> {
        let group = {
            let state = self.state.read().await;
            state.groups.get(group_id).cloned()?
        };
        Some(group.collect().await)
    }

    pub async fn get(&self, group_id: GroupId) -> Group {
        let group_maybe = {
            let state = self.state.read().await;
            state.groups.get(&group_id).cloned()
        };

        match group_maybe {
            Some(mailbox) => mailbox,
            None => self.insert(group_id).await,
        }
    }
}

#[derive(Clone)]
pub struct Group {
    state: Arc<RwLock<GroupState>>,
    capacity: u16,
}

#[derive(Default)]
pub struct GroupState {
    peers: BTreeMap<PeerConn, Mailbox>,
}

#[derive(Clone, PartialEq, PartialOrd, Ord, Eq, Debug)]
pub struct PeerConn {
    pub peer_id: PeerId,
    pub conn_id: u32,
}

impl Group {
    pub fn new(capacity: u16) -> Self {
        Self {
            capacity,
            state: Arc::new(RwLock::new(GroupState::default())),
        }
    }

    pub async fn collect(&self) -> Vec<PeerConn> {
        let state = self.state.read().await;
        state.peers.keys().cloned().collect()
    }

    pub async fn insert(&self, conn: PeerConn) -> Mailbox {
        let mut state = self.state.write().await;
        let mailbox = state
            .peers
            .entry(conn)
            .or_insert_with(|| flume::bounded(MAILBOX_CAPACITY));
        mailbox.clone()
    }

    pub async fn get(&self, conn: PeerConn) -> Mailbox {
        let mailbox_maybe = {
            let state = self.state.read().await;
            state.peers.get(&conn).cloned()
        };

        match mailbox_maybe {
            Some(mailbox) => mailbox,
            None => self.insert(conn).await,
        }
    }

    pub async fn select_one(&self, peer_id: PeerId) -> Option<(PeerConn, Mailbox)> {
        let state = self.state.read().await;
        let start = PeerConn {
            peer_id: peer_id.clone(),
            conn_id: 0,
        };
        let end = PeerConn {
            peer_id,
            conn_id: u32::MAX,
        };
        let found = state.peers.range(start..=end).next()?;
        let result = (found.0.clone(), found.1.clone());
        Some(result)
    }
}

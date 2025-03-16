use std::{sync::Arc, time::Duration};

use ahash::HashMap;
use tokio::sync::RwLock;
use tokio_stream::StreamExt;
use tokio_util::time::{delay_queue, DelayQueue};

use crate::{
    manager::Manager,
    proto::{self, PeerInfo},
};

#[derive(Clone)]
pub struct Evicter {
    state: Arc<RwLock<EvicterState>>,
    timeout: Duration,
}

pub struct EvicterState {
    manager: Manager,
    timer: DelayQueue<proto::PeerInfo>,
    index: HashMap<proto::PeerInfo, delay_queue::Key>,
}

impl Evicter {
    pub fn new(manager: Manager, timeout: Duration) -> Self {
        let state = EvicterState {
            manager,
            timer: DelayQueue::new(),
            index: HashMap::default(),
        };
        Self {
            state: Arc::new(RwLock::new(state)),
            timeout,
        }
    }

    pub async fn insert(&self, peer: PeerInfo) {
        let mut state = self.state.write().await;
        let key = state.timer.insert(peer.clone(), self.timeout);
        state.index.insert(peer, key);
        state.poll_purge().await;
    }

    pub async fn remove(&self, peer: &PeerInfo) {
        let mut state = self.state.write().await;
        if let Some(key) = state.index.remove(peer) {
            state.timer.remove(&key);
        }
        state.poll_purge().await;
    }
}

impl EvicterState {
    pub async fn poll_purge(&mut self) {
        // TODO: maybe add a safeguard when we spend too much time envicting
        loop {
            tokio::select! {
                Some(key) = self.timer.next() => {
                    tracing::info!("{:?} has been evicted", key);
                    self.index.remove(key.get_ref());
                    self.manager.remove(key.into_inner()).await;
                }
                else => {}
            }
        }
    }
}

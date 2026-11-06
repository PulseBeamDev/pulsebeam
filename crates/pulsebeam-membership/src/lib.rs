//! Opt-in control-side membership. The host allocates identity and provisions sockets.
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Result, ensure};
use chitchat::transport::Socket;
use chitchat::{ChitchatConfig, ChitchatHandle, ChitchatId, ProtocolVersion, spawn_chitchat};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

pub mod transport;
mod view;
pub use view::{
    Change, HISTORY_RETENTION, Incarnation, Liveness, Member, Metadata, NodeId, Snapshot,
};

const GOSSIP_INTERVAL: Duration = Duration::from_secs(1);
const TOMBSTONE_RETENTION: Duration = Duration::from_secs(15 * 60);

pub struct Config {
    pub cluster_id: String,
    pub incarnation: Incarnation,
    pub listen_addr: SocketAddr,
    pub seeds: Vec<SocketAddr>,
    pub metadata: Metadata,
}

impl Config {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.cluster_id.trim().is_empty()
                && self.cluster_id.len() <= 128
                && !self.cluster_id.chars().any(char::is_control),
            "cluster ID must contain 1..128 bytes without control characters"
        );
        ensure!(
            self.incarnation.generation_id > 0,
            "membership generation must be positive"
        );
        let _: NodeId = self.incarnation.node_id.to_string().parse()?;
        ensure!(
            view::usable_address(self.incarnation.gossip_advertise_addr),
            "invalid gossip advertised address"
        );
        ensure!(
            self.listen_addr.port() != 0
                && !self.listen_addr.ip().is_multicast()
                && !matches!(self.listen_addr.ip(), std::net::IpAddr::V4(ip) if ip.is_broadcast()),
            "invalid gossip listen address"
        );
        ensure!(
            self.listen_addr.is_ipv4() == self.incarnation.gossip_advertise_addr.is_ipv4()
                && self
                    .seeds
                    .iter()
                    .all(|addr| addr.is_ipv4() == self.listen_addr.is_ipv4()),
            "gossip listen, advertise and seed addresses must use the same IP family"
        );
        ensure!(
            self.listen_addr.port() == self.incarnation.gossip_advertise_addr.port(),
            "gossip listen and advertised ports differ"
        );
        ensure!(
            self.seeds.iter().all(|addr| view::usable_address(*addr)),
            "invalid gossip seed address"
        );
        view::validate_metadata(&self.metadata)
    }
}

struct Update {
    metadata: Metadata,
    response: oneshot::Sender<()>,
}

/// Cloneable control-side access. Notifications can coalesce; snapshots are complete.
#[derive(Clone)]
pub struct Handle {
    snapshots: watch::Receiver<Snapshot>,
    updates: mpsc::Sender<Update>,
}

impl Handle {
    pub fn snapshot(&self) -> Snapshot {
        self.snapshots.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.snapshots.clone()
    }

    pub async fn replace_metadata(&self, metadata: Metadata) -> Result<()> {
        view::validate_metadata(&metadata)?;
        let (response, received) = oneshot::channel();
        self.updates
            .send(Update { metadata, response })
            .await
            .map_err(|_| anyhow::anyhow!("membership stopped"))?;
        received
            .await
            .map_err(|_| anyhow::anyhow!("membership stopped during metadata update"))?;
        Ok(())
    }
}

/// Owns the Chitchat task. Run to completion on cancellation before releasing identity.
pub struct Membership {
    chitchat: Option<ChitchatHandle>,
    view: view::View,
    snapshots: watch::Sender<Snapshot>,
    updates: mpsc::Receiver<Update>,
}

impl Membership {
    pub async fn start(config: Config, socket: Box<dyn Socket>) -> Result<(Self, Handle)> {
        config.validate()?;
        let transport = transport::BoundTransport::new(socket);
        let id = ChitchatId::new(
            config.incarnation.node_id.to_string(),
            config.incarnation.generation_id,
            config.incarnation.gossip_advertise_addr,
        );
        let chitchat_config = ChitchatConfig {
            chitchat_id: id,
            cluster_id: config.cluster_id,
            gossip_interval: GOSSIP_INTERVAL,
            listen_addr: config.listen_addr,
            seed_nodes: config
                .seeds
                .into_iter()
                .map(|addr| addr.to_string())
                .collect(),
            failure_detector_config: Default::default(),
            marked_for_deletion_grace_period: TOMBSTONE_RETENTION,
            catchup_callback: None,
            extra_liveness_predicate: None,
            protocol_version: ProtocolVersion::V0,
        };
        let mut metadata = vec![("pb.schema".to_owned(), "1".to_owned())];
        metadata.extend(
            config
                .metadata
                .into_iter()
                .map(|(key, value)| (format!("meta.{key}"), value)),
        );
        let chitchat = spawn_chitchat(chitchat_config, metadata, &transport).await?;
        let mut view = view::View::new();
        chitchat
            .with_chitchat(|state| view.reconcile(state))
            .await?;
        let (snapshots, receiver) = watch::channel(view.snapshot.clone());
        let (sender, updates) = mpsc::channel(16);
        Ok((
            Self {
                chitchat: Some(chitchat),
                view,
                snapshots,
                updates,
            },
            Handle {
                snapshots: receiver,
                updates: sender,
            },
        ))
    }

    pub async fn run(mut self, shutdown: CancellationToken) -> Result<()> {
        let chitchat = self
            .chitchat
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("membership already stopped"))?;
        let termination = chitchat.termination_watcher();
        tokio::pin!(termination);
        let mut interval = tokio::time::interval(GOSSIP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let result = loop {
            tokio::select! {
                _ = shutdown.cancelled() => break Ok(()),
                result = &mut termination => break result.and_then(|()| Err(anyhow::anyhow!("gossip task terminated unexpectedly"))),
                update = self.updates.recv(), if !self.updates.is_closed() => {
                    if let Some(update) = update {
                        chitchat.with_chitchat(|state| {
                            let local = state.self_node_state();
                            let removed: Vec<_> = local.iter_prefix("meta.").filter_map(|(key, _)| {
                                key.strip_prefix("meta.").filter(|key| !update.metadata.contains_key(*key)).map(|_| key.to_owned())
                            }).collect();
                            for key in removed { local.delete(&key); }
                            for (key, value) in &update.metadata { local.set(format!("meta.{key}"), value); }
                        }).await;
                        let changed = chitchat.with_chitchat(|state| self.view.reconcile(state)).await;
                        match changed {
                            Ok(true) => { self.snapshots.send_replace(self.view.snapshot.clone()); }
                            Ok(false) => {}
                            Err(error) => break Err(error),
                        }
                        let _ = update.response.send(());
                    }
                }
                _ = interval.tick() => {
                    match chitchat.with_chitchat(|state| self.view.reconcile(state)).await {
                        Ok(true) => { self.snapshots.send_replace(self.view.snapshot.clone()); }
                        Ok(false) => {}
                        Err(error) => break Err(error),
                    }
                }
            }
        };
        let joined = self
            .chitchat
            .take()
            .ok_or_else(|| anyhow::anyhow!("membership already stopped"))?
            .shutdown()
            .await;
        result.and(joined)
    }
}

impl Drop for Membership {
    fn drop(&mut self) {
        // Cancellation normally joins in run(). A dropped owner must not leave gossip detached.
        if let Some(handle) = &self.chitchat {
            handle.abort();
        }
    }
}

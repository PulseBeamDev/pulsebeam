//! Optional trusted-network gossip. This service owns persistence and lifecycle,
//! independently of shard execution and room authority.
mod config;
pub(crate) mod declarations;
pub(crate) mod handoff;
mod hierarchy;
mod identity;
pub(crate) mod mesh;
#[cfg(not(feature = "sim"))]
pub(crate) mod regional;

// One catalog entry plus its envelope. The datagram lane keeps its MTU bound.
pub(crate) const REGIONAL_FRAME_BYTES: usize =
    pulsebeam_proto::catalog::MAX_CATALOG_ENTRY_BYTES + 1024;

pub(crate) use config::MAX_PROVISIONED_PEERS;
pub use config::{Options, ProvisionedPeer, RegionalControlConfig, ServerConfig, Topology};
pub(crate) use hierarchy::MAX_CATALOG_MEMBERS;
pub use hierarchy::RegionTree;
#[cfg(feature = "sim")]
pub use {declarations::Role as SimRegionalRole, mesh::SimLinkCost};

use anyhow::{Context, Result};
use chitchat::transport::Socket;
#[cfg(not(feature = "sim"))]
use chitchat::transport::UdpSocket;
use pulsebeam_membership::{Config, Handle, Incarnation, Membership, Metadata};
use pulsebeam_runtime::sync::CancellationToken;
use pulsebeam_runtime::task::JoinHandle;

// Keep the identity lease with the socket, including cancelled startup. Field
// order ensures the socket closes before its lease can be released.
struct IdentitySocket {
    inner: Box<dyn Socket>,
    _identity: identity::Identity,
}

#[async_trait::async_trait]
impl Socket for IdentitySocket {
    fn local_addr(&self) -> Result<std::net::SocketAddr> {
        self.inner.local_addr()
    }

    async fn send(
        &mut self,
        to: std::net::SocketAddr,
        envelope: chitchat::ChitchatEnvelope,
    ) -> Result<chitchat::transport::SendOutcome> {
        self.inner.send(to, envelope).await
    }

    async fn recv(&mut self) -> Result<chitchat::transport::RecvOutcome> {
        self.inner.recv().await
    }
}

/// Reserve the identity of an isolated test node before provisioning the next
/// generation. The server still allocates and validates its own generation.
#[cfg(feature = "sim")]
pub fn prepare_provisioned_fixture(
    directory: &std::path::Path,
) -> Result<(pulsebeam_membership::NodeId, std::num::NonZeroU64)> {
    let identity = identity::Identity::allocate(directory)?;
    let next = identity
        .generation
        .checked_add(1)
        .and_then(std::num::NonZeroU64::new)
        .context("fixture membership generation exhausted")?;
    Ok((identity.node_id, next))
}

pub struct Service {
    shutdown: CancellationToken,
    task: Option<JoinHandle<Result<()>>>,
    observations: Handle,
    incarnation: Incarnation,
}

impl Service {
    pub async fn start(options: Options) -> Result<Self> {
        options.validate_topology()?;
        #[cfg(feature = "sim")]
        anyhow::ensure!(
            options.topology == Topology::SingleNode
                || (!options.ownership_peers.is_empty() && options.seeds.is_empty()),
            "multi-node simulation requires provisioned ownership and no gossip seeds"
        );
        let data_dir = options.data_dir.clone();
        let expected = options.ownership_peers.first().map(|peer| peer.local);
        let identity = pulsebeam_runtime::task::spawn_blocking(move || match expected {
            Some(expected) => identity::Identity::allocate_provisioned(&data_dir, expected),
            None => identity::Identity::allocate(&data_dir),
        })
        .await
        .context("membership identity worker failed")??;
        #[cfg(not(feature = "sim"))]
        let socket: Box<dyn Socket> = Box::new(
            UdpSocket::open(options.listen)
                .await
                .context("binding gossip UDP")?,
        );
        #[cfg(feature = "sim")]
        let socket = chitchat::transport::Transport::open(
            &chitchat::transport::ChannelTransport::default(),
            options.listen,
        )
        .await?;
        Self::start_bound(options, identity, socket).await
    }

    async fn start_bound(
        options: Options,
        identity: identity::Identity,
        socket: Box<dyn Socket>,
    ) -> Result<Self> {
        let incarnation = Incarnation {
            node_id: identity.node_id,
            generation_id: identity.generation,
            gossip_advertise_addr: options.advertise,
        };
        let config = Config {
            cluster_id: options.cluster_id,
            incarnation: incarnation.clone(),
            listen_addr: options.listen,
            seeds: options.seeds,
            metadata: Metadata::new(),
        };
        let socket = Box::new(IdentitySocket {
            inner: socket,
            _identity: identity,
        });
        let (membership, observations) = Membership::start(config, socket).await?;
        let shutdown = CancellationToken::new();
        let cancellation = shutdown.clone();
        let task = pulsebeam_runtime::task::spawn(membership.run(cancellation));
        tracing::info!(node_id = %incarnation.node_id, generation = incarnation.generation_id, gossip = %incarnation.gossip_advertise_addr, "cluster membership started (trusted network only)");
        Ok(Self {
            shutdown,
            task: Some(task),
            observations,
            incarnation,
        })
    }

    pub(crate) fn incarnation(&self) -> &Incarnation {
        &self.incarnation
    }

    pub async fn supervise(
        self,
        node: impl std::future::Future<Output = Result<()>>,
        shutdown: CancellationToken,
    ) -> Result<()> {
        let mut service = self;
        pulsebeam_runtime::pin!(node);
        let result = pulsebeam_runtime::select! {
            result = &mut node => result,
            result = service.wait() => {
                shutdown.cancel();
                let _ = (&mut node).await;
                match result {
                    Ok(()) => Err(anyhow::anyhow!("membership stopped unexpectedly")),
                    Err(error) => Err(error).context("membership failed; node shut down"),
                }
            }
        };
        shutdown.cancel();
        result.and(service.finish().await)
    }

    pub fn observations(&self) -> &Handle {
        &self.observations
    }

    pub async fn wait(&mut self) -> Result<()> {
        let result = self
            .task
            .as_mut()
            .context("membership task already joined")?
            .await;
        self.task.take();
        result.context("membership supervisor task failed")?
    }

    pub async fn finish(mut self) -> Result<()> {
        self.shutdown.cancel();
        if let Some(task) = self.task.take() {
            task.await.context("membership supervisor task failed")??;
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "cluster-tests"))]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chitchat::ChitchatEnvelope;
    use chitchat::transport::{RecvOutcome, SendOutcome};
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::time::Duration;

    struct ProbeSocket {
        inner: UdpSocket,
        state: PathBuf,
        generation: u64,
        failure: &'static str,
    }

    impl Drop for ProbeSocket {
        fn drop(&mut self) {
            assert!(
                self.state
                    .parent()
                    .is_some_and(|directory| identity::Identity::allocate(directory).is_err()),
                "identity lease released before socket closed"
            );
        }
    }

    #[async_trait]
    impl Socket for ProbeSocket {
        fn local_addr(&self) -> Result<SocketAddr> {
            self.inner.local_addr()
        }
        async fn send(
            &mut self,
            to: SocketAddr,
            envelope: ChitchatEnvelope,
        ) -> Result<SendOutcome> {
            let state: serde_json::Value = serde_json::from_slice(&std::fs::read(&self.state)?)?;
            anyhow::ensure!(
                state.get("generation").and_then(serde_json::Value::as_u64)
                    == Some(self.generation),
                "first packet preceded durable generation commit"
            );
            if self.failure == "send" {
                anyhow::bail!("injected fatal send");
            }
            self.inner.send(to, envelope).await
        }
        async fn recv(&mut self) -> Result<RecvOutcome> {
            if self.failure == "recv" {
                anyhow::bail!("injected fatal receive");
            }
            self.inner.recv().await
        }
    }

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn real_udp_commit_precedes_packets_and_shutdown_releases_socket_and_lock() {
        let dir = tempfile::tempdir().unwrap();
        let mut peer = UdpSocket::open("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let seed = peer.local_addr().unwrap();
        for (generation, failure) in [(1, ""), (2, "recv"), (3, "send")] {
            let socket = UdpSocket::open("127.0.0.1:0".parse().unwrap())
                .await
                .unwrap();
            let listen = socket.local_addr().unwrap();
            let identity = identity::Identity::allocate(dir.path()).unwrap();
            let id = identity.node_id;
            assert_eq!(identity.generation, generation);
            let options = Options {
                cluster_id: "udp-smoke".into(),
                topology: Topology::MultiNode,
                listen,
                advertise: listen,
                seeds: vec![seed],
                data_dir: dir.path().to_owned(),
                ownership_peers: Vec::new(),
                region_tree: None,
                protected_base_stream_bps: None,
            };
            let socket = ProbeSocket {
                inner: socket,
                state: dir.path().join("membership-state.json"),
                generation,
                failure,
            };
            let service = Service::start_bound(options, identity, Box::new(socket))
                .await
                .unwrap();
            assert!(identity::Identity::allocate(dir.path()).is_err());
            assert_eq!(
                service.observations().snapshot().highest_generations[&id],
                generation
            );
            if failure.is_empty() {
                let packet = pulsebeam_runtime::time::timeout(Duration::from_secs(2), peer.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    matches!(packet.envelope.message, chitchat::ChitchatMessage::Syn { cluster_id, .. } if cluster_id == "udp-smoke")
                );
                service.finish().await.unwrap();
            } else {
                let shutdown = CancellationToken::new();
                let stopped = std::cell::Cell::new(false);
                let node = async {
                    shutdown.cancelled().await;
                    stopped.set(true);
                    Ok(())
                };
                assert!(
                    pulsebeam_runtime::time::timeout(
                        Duration::from_secs(2),
                        service.supervise(node, shutdown.clone())
                    )
                    .await
                    .unwrap()
                    .is_err()
                );
                assert!(shutdown.is_cancelled() && stopped.get());
            }
            let rebound = UdpSocket::open(listen).await.unwrap();
            drop(rebound);
        }
        assert_eq!(
            identity::Identity::allocate(dir.path()).unwrap().generation,
            4
        );
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // The socket retains the identity lease until detached cleanup closes it.
        self.shutdown.cancel();
    }
}

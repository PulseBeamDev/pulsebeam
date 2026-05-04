use crate::tests::common::{client::SimClientBuilder, run_sim_or_timeout};
use pulsebeam_agent::{MediaKind, SimulcastLayer, TransceiverDirection};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use turmoil::{Result as TurmoilResult, Sim};

/// A handle to a simulated client that can be used across stages.
pub type ClientHandle = Arc<tokio::sync::Mutex<Option<crate::tests::common::client::SimClient>>>;

/// Per-client information stored in the scenario context.
#[derive(Clone)]
pub struct ClientInfo {
    pub ip: IpAddr,
    pub handle: ClientHandle,
}

/// Shared data available to all stages.
pub struct ScenarioCtx {
    pub server_ip: IpAddr,
    pub clients: Vec<ClientInfo>,
    pub subnet: u8,
    next_client_host: u8,
    next_controller_host: u8,
}

impl ScenarioCtx {
    pub fn new(server_ip: IpAddr) -> Self {
        let subnet = match server_ip {
            IpAddr::V4(v4) => v4.octets()[2],
            IpAddr::V6(_) => panic!("IPv6 not supported for simulation IPs"),
        };

        Self {
            server_ip,
            subnet,
            clients: Vec::new(),
            next_client_host: 2, // reserve .1 for server
            next_controller_host: 240,
        }
    }

    pub fn alloc_client_ip(&mut self) -> IpAddr {
        let ip = format!("192.168.{}.{}", self.subnet, self.next_client_host);
        self.next_client_host = self.next_client_host.wrapping_add(1);
        ip.parse().unwrap()
    }

    pub fn alloc_controller_ip(&mut self) -> IpAddr {
        let ip = format!("192.168.{}.{}", self.subnet, self.next_controller_host);
        self.next_controller_host = self.next_controller_host.wrapping_add(1);
        ip.parse().unwrap()
    }
}

/// A single stage of a simulation scenario.
///
/// Each stage can configure the simulation (register hosts/clients) and validate
/// its expectations. This lets us keep high-level steps (A → B → … → Z) while
/// still enforcing invariants after each stage.
pub trait Stage: Send + Sync {
    fn name(&self) -> &'static str;

    fn apply(&self, sim: &mut Sim<'_>, ctx: &mut ScenarioCtx) -> TurmoilResult<()>;
}

/// A scenario is a sequence of ordered stages that are configured into a
/// turbulence simulation, then executed.
pub struct Scenario {
    pub name: &'static str,
    pub stages: Vec<Box<dyn Stage>>,
    pub ctx: ScenarioCtx,
    pub sim: Sim<'static>,
}

impl Scenario {
    pub fn new(name: &'static str, server_ip: IpAddr, sim: Sim<'static>) -> Self {
        Self {
            name,
            stages: Vec::new(),
            ctx: ScenarioCtx::new(server_ip),
            sim,
        }
    }

    pub fn add_stage(mut self, stage: impl Stage + 'static) -> Self {
        self.stages.push(Box::new(stage));
        self
    }

    pub fn run(mut self) -> TurmoilResult<()> {
        tracing::info!(scenario = self.name, "running scenario");

        for stage in &self.stages {
            tracing::info!(stage = stage.name(), "running stage");
            stage.apply(&mut self.sim, &mut self.ctx)?;
        }

        tracing::info!("starting simulation run");
        run_sim_or_timeout(&mut self.sim, Duration::from_secs(120))?;
        tracing::info!("simulation run completed");
        Ok(())
    }
}

/// Stage that starts an SFU host with UDP candidates suppressed (TCP-only mode).
pub struct StartSfuTcpOnlyStage;

impl Stage for StartSfuTcpOnlyStage {
    fn name(&self) -> &'static str {
        "start_sfu_tcp_only"
    }

    fn apply(&self, sim: &mut Sim<'_>, ctx: &mut ScenarioCtx) -> TurmoilResult<()> {
        let server_ip = ctx.server_ip;
        sim.host(server_ip, move || async move {
            crate::tests::common::start_sfu_node_tcp_only(
                server_ip,
                pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF),
            )
            .await
            .map_err(|e| e.into())
        });
        Ok(())
    }
}

/// Stage that connects N peers using the TCP active path and asserts bidirectional flow.
pub struct ConnectPeersTcpOnlyStage {
    pub peers: usize,
    pub min_tx_bytes: u64,
    pub min_rx_bytes: u64,
    pub max_wait: Duration,
}

impl Stage for ConnectPeersTcpOnlyStage {
    fn name(&self) -> &'static str {
        "connect_peers_tcp_only"
    }

    fn apply(&self, sim: &mut Sim<'_>, ctx: &mut ScenarioCtx) -> TurmoilResult<()> {
        let barrier = Arc::new(tokio::sync::Barrier::new(self.peers));

        for _ in 0..self.peers {
            let ip = ctx.alloc_client_ip();
            let barrier = barrier.clone();
            let server_ip = ctx.server_ip;
            let min_tx_bytes = self.min_tx_bytes;
            let min_rx_bytes = self.min_rx_bytes;
            let max_wait = self.max_wait;

            let client_handle: ClientHandle = Arc::new(tokio::sync::Mutex::new(None));
            ctx.clients.push(ClientInfo {
                ip,
                handle: client_handle.clone(),
            });

            sim.client(ip, async move {
                let client = SimClientBuilder::bind_tcp(ip, server_ip)
                    .await?
                    .with_track(
                        MediaKind::Video,
                        TransceiverDirection::SendOnly,
                        Some(vec![
                            SimulcastLayer::new("f"),
                            SimulcastLayer::new("h"),
                            SimulcastLayer::new("q"),
                        ]),
                    )
                    .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
                    .connect("room1")
                    .await?;

                {
                    let mut guard = client_handle.lock().await;
                    *guard = Some(client);
                }

                {
                    let mut guard = client_handle.lock().await;
                    let client = guard.as_mut().expect("client must be initialized");
                    client
                        .drive_until(max_wait, |stats| {
                            let Some(peer) = &stats.peer else {
                                return false;
                            };
                            peer.peer_bytes_tx > min_tx_bytes && peer.peer_bytes_rx > min_rx_bytes
                        })
                        .await?;
                }

                barrier.wait().await;
                Ok(())
            });
        }

        Ok(())
    }
}

/// Stage that starts an SFU host.
pub struct StartSfuStage;

impl Stage for StartSfuStage {
    fn name(&self) -> &'static str {
        "start_sfu"
    }

    fn apply(&self, sim: &mut Sim<'_>, ctx: &mut ScenarioCtx) -> TurmoilResult<()> {
        let server_ip = ctx.server_ip;
        sim.host(server_ip, move || async move {
            crate::tests::common::start_sfu_node(
                server_ip,
                pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF),
            )
            .await
            .map_err(|e| e.into())
        });
        Ok(())
    }
}

/// Stage that connects N peers and asserts bidirectional flow.
pub struct ConnectPeersStage {
    pub peers: usize,
    pub min_tx_bytes: u64,
    pub min_rx_bytes: u64,
    pub max_wait: Duration,
}

impl Stage for ConnectPeersStage {
    fn name(&self) -> &'static str {
        "connect_peers"
    }

    fn apply(&self, sim: &mut Sim<'_>, ctx: &mut ScenarioCtx) -> TurmoilResult<()> {
        let barrier = Arc::new(tokio::sync::Barrier::new(self.peers));

        for _ in 0..self.peers {
            let ip = ctx.alloc_client_ip();
            let barrier = barrier.clone();
            let server_ip = ctx.server_ip;
            let min_tx_bytes = self.min_tx_bytes;
            let min_rx_bytes = self.min_rx_bytes;
            let max_wait = self.max_wait;

            // Create a handle so later stages (e.g. disconnect) can interact with this client.
            let client_handle: ClientHandle = Arc::new(tokio::sync::Mutex::new(None));
            ctx.clients.push(ClientInfo {
                ip,
                handle: client_handle.clone(),
            });

            sim.client(ip, async move {
                // Build the client and store it in the shared handle.
                let client = SimClientBuilder::bind(ip, server_ip)
                    .await?
                    .with_track(
                        MediaKind::Video,
                        TransceiverDirection::SendOnly,
                        Some(vec![
                            SimulcastLayer::new("f"),
                            SimulcastLayer::new("h"),
                            SimulcastLayer::new("q"),
                        ]),
                    )
                    .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
                    .connect("room1")
                    .await?;

                {
                    let mut guard = client_handle.lock().await;
                    *guard = Some(client);
                }

                // Drive the client until it has established bidirectional flow.
                {
                    let mut guard = client_handle.lock().await;
                    let client = guard.as_mut().expect("client must be initialized");
                    client
                        .drive_until(max_wait, |stats| {
                            let Some(peer) = &stats.peer else {
                                return false;
                            };

                            peer.peer_bytes_tx > min_tx_bytes && peer.peer_bytes_rx > min_rx_bytes
                        })
                        .await?;
                }

                barrier.wait().await;
                Ok(())
            });
        }

        Ok(())
    }
}

/// Stage that applies a partition between a specific peer and the SFU for a duration.
pub struct PartitionStage {
    /// Index into the scenario's client list.
    pub client_index: usize,
    pub delay: Duration,
    pub duration: Duration,
}

impl Stage for PartitionStage {
    fn name(&self) -> &'static str {
        "partition"
    }

    fn apply(&self, sim: &mut Sim<'_>, ctx: &mut ScenarioCtx) -> TurmoilResult<()> {
        let client_info = ctx
            .clients
            .get(self.client_index)
            .expect("client index out of range");

        // We keep the client IP separate so we can partition without needing to await.
        let peer_ip = client_info.ip;

        let server_ip = ctx.server_ip;
        let delay = self.delay;
        let duration = self.duration;

        // Use a controller host inside the same subnet as the SFU to avoid IP conflicts.
        let controller_ip = ctx.alloc_controller_ip();
        sim.host(controller_ip, move || async move {
            tokio::time::sleep(delay).await;
            tracing::info!(peer = ?peer_ip, "applying partition");
            turmoil::partition(peer_ip, server_ip);
            tokio::time::sleep(duration).await;
            tracing::info!(peer = ?peer_ip, "repairing partition");
            turmoil::repair(peer_ip, server_ip);
            Ok(())
        });

        Ok(())
    }
}

/// Stage that holds and then releases packets between a specific peer and the SFU.
pub struct HoldStage {
    /// Index into the scenario's client list.
    pub client_index: usize,
    pub delay: Duration,
    pub duration: Duration,
}

impl Stage for HoldStage {
    fn name(&self) -> &'static str {
        "hold"
    }

    fn apply(&self, sim: &mut Sim<'_>, ctx: &mut ScenarioCtx) -> TurmoilResult<()> {
        let client_info = ctx
            .clients
            .get(self.client_index)
            .expect("client index out of range");

        let peer_ip = client_info.ip;
        let server_ip = ctx.server_ip;
        let delay = self.delay;
        let duration = self.duration;

        let controller_ip = ctx.alloc_controller_ip();
        sim.host(controller_ip, move || async move {
            tokio::time::sleep(delay).await;
            tracing::info!(peer = ?peer_ip, "holding packets");
            turmoil::hold(peer_ip, server_ip);
            tokio::time::sleep(duration).await;
            tracing::info!(peer = ?peer_ip, "releasing packets");
            turmoil::release(peer_ip, server_ip);
            Ok(())
        });

        Ok(())
    }
}

/// Stage that repeatedly joins and leaves a set of ephemeral peers.
pub struct ChurnStage {
    /// How many churn cycles to perform.
    pub cycles: usize,
    /// How many peers to create per cycle.
    pub num_peers: usize,
    /// How long each peer stays connected.
    pub join_duration: Duration,
    /// How long to pause between churn cycles.
    pub pause_between: Duration,
}

impl Stage for ChurnStage {
    fn name(&self) -> &'static str {
        "churn"
    }

    fn apply(&self, sim: &mut Sim<'_>, ctx: &mut ScenarioCtx) -> TurmoilResult<()> {
        for cycle in 0..self.cycles {
            let base_octet = 200u8.wrapping_add(cycle as u8);
            for peer in 0..self.num_peers {
                let ip = IpAddr::from([192, 168, base_octet, (peer as u8).wrapping_add(1)]);
                let server_ip = ctx.server_ip;
                let join_duration = self.join_duration;

                sim.client(ip, async move {
                    let mut client = SimClientBuilder::bind(ip, server_ip)
                        .await?
                        .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
                        .connect("room1")
                        .await?;
                    tokio::time::sleep(join_duration).await;
                    client.agent.disconnect().await?;
                    Ok(())
                });
            }

            let pause = self.pause_between;
            let controller_ip = ctx.alloc_controller_ip();
            sim.host(controller_ip, move || async move {
                tokio::time::sleep(pause).await;
                Ok(())
            });
        }

        Ok(())
    }
}

/// Stage that disconnects all connected clients after a delay.
pub struct DisconnectStage {
    pub after: Duration,
}

impl Stage for DisconnectStage {
    fn name(&self) -> &'static str {
        "disconnect"
    }

    fn apply(&self, sim: &mut Sim<'_>, ctx: &mut ScenarioCtx) -> TurmoilResult<()> {
        let after = self.after;
        let clients = ctx.clients.clone();

        // Use a controller host inside the same subnet to avoid collisions.
        let controller_ip = ctx.alloc_controller_ip();
        sim.host(controller_ip, move || {
            let clients = clients.clone();
            async move {
                tokio::time::sleep(after).await;
                for client in &clients {
                    let mut guard: tokio::sync::MutexGuard<
                        '_,
                        Option<crate::tests::common::client::SimClient>,
                    > = client.handle.lock().await;
                    let client = guard.as_mut().expect("client should have been initialized");
                    tracing::info!(ip = ?client.ip, "disconnecting client");
                    client.agent.disconnect().await?;
                }
                Ok(())
            }
        });

        Ok(())
    }
}

/// Stage that asserts all clients have no active peer connection after a delay.
pub struct AssertAllDisconnectedStage {
    pub after: Duration,
}

impl Stage for AssertAllDisconnectedStage {
    fn name(&self) -> &'static str {
        "assert_all_disconnected"
    }

    fn apply(&self, sim: &mut Sim<'_>, ctx: &mut ScenarioCtx) -> TurmoilResult<()> {
        let after = self.after;
        let clients = ctx.clients.clone();

        // Use a controller host inside the same subnet to avoid collisions.
        let controller_ip = ctx.alloc_controller_ip();
        sim.host(controller_ip, move || {
            let clients = clients.clone();
            async move {
                tokio::time::sleep(after).await;
                for client in &clients {
                    let client_guard: tokio::sync::MutexGuard<
                        '_,
                        Option<crate::tests::common::client::SimClient>,
                    > = client.handle.lock().await;
                    let client = client_guard
                        .as_ref()
                        .expect("client should have been initialized");
                    let stats = client.agent.get_stats().await.unwrap_or_default();
                    if stats.peer.is_some() {
                        return Err(format!(
                            "Expected client {} to have no peer, got {:?}",
                            client.ip, stats.peer
                        )
                        .into());
                    }
                }
                Ok(())
            }
        });

        Ok(())
    }
}

/// Stage that asserts the server has no established TCP streams after a delay.
pub struct AssertNoTcpStreamsStage {
    pub after: Duration,
}

impl Stage for AssertNoTcpStreamsStage {
    fn name(&self) -> &'static str {
        "assert_no_tcp_streams"
    }

    fn apply(&self, sim: &mut Sim<'_>, ctx: &mut ScenarioCtx) -> TurmoilResult<()> {
        let after = self.after;
        let server_ip = ctx.server_ip;

        let controller_ip = ctx.alloc_controller_ip();
        sim.host(controller_ip, move || async move {
            tokio::time::sleep(after).await;

            // In Turmoil, established_tcp_stream_count_on returns the number of
            // active TCP connections on the host. It should be zero after all
            // clients have disconnected.
            let conn_count = turmoil::established_tcp_stream_count_on(server_ip);
            if conn_count != 0 {
                return Err(format!(
                    "Expected 0 established tcp streams on server, got {}",
                    conn_count
                )
                .into());
            }

            Ok(())
        });

        Ok(())
    }
}

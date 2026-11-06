//! Shared-state exception: `Arc<ShardMetrics>`, one per shard, handed over
//! before any shard runs. See `shard::metrics`.

use anyhow::{Context, Result};
use pulsebeam_core::auth::KeyRegistry;
use pulsebeam_queue as mailbox;
use pulsebeam_runtime::net::UdpMode;
use pulsebeam_runtime::net::tcp::TcpListener;
use pulsebeam_runtime::rand;
use pulsebeam_runtime::rand::{RngCore, SeedableRng};
use std::collections::HashSet;
use std::future::Future;
use std::net::{Ipv6Addr, SocketAddr};
#[allow(
    clippy::disallowed_types,
    reason = "Arc<ShardMetrics>, handed over once before any shard runs, see module note"
)]
use std::sync::Arc;
use std::time::Duration;

use crate::clock::WallAnchor;
use pulsebeam_rtc::LocalCandidate;
use pulsebeam_runtime::sync::CancellationToken;
use pulsebeam_runtime::task::JoinSet;
use tower_http::compression::CompressionLayer;
use tower_http::decompression::RequestDecompressionLayer;

use crate::control::api;
use crate::control::controller::ControllerActor;
use crate::control::steering::Steering;
use crate::id::ShardId;
use crate::shard::ShardContext;
use crate::shard::metrics::ShardMetrics;
use crate::shard::worker::ShardWorker;

/// Defines how a service listener is acquired.
enum ListenerSource {
    /// Bind to this address internally.
    Bind(SocketAddr),
    /// Use this pre-bound listener.
    PreBound(TcpListener),
}

pub(crate) use pulsebeam_runtime::net::tcp::bind as bind_tcp_listener;
use pulsebeam_runtime::rt::{DataExecutor, ExecutionConfig, Profile};

type SteeringFactory =
    Box<dyn FnOnce(&[pulsebeam_udp::BoundUdpSocket]) -> Result<Box<dyn Steering>> + Send>;

pub use crate::ice_tcp::Limits as IceTcpLimits;

/// Media TCP is independent of HTTP signaling and metrics listeners.
#[derive(Clone, Debug)]
pub enum IceTcpConfig {
    Disabled,
    Passive {
        bind: SocketAddr,
        limits: IceTcpLimits,
    },
}

pub struct NodeBuilder {
    #[cfg(test)]
    runtime_control:
        Option<pulsebeam_runtime::sync::oneshot::Sender<pulsebeam_runtime::rt::MigrationHandle>>,
    #[cfg(feature = "sim")]
    sim_startup_notification: Option<pulsebeam_runtime::sync::mpsc::Sender<()>>,
    // Data-plane topology
    //
    // `data_threads` is physical executor width. Logical shard count is derived
    // from it and `shard_runtime`.
    data_threads: usize,
    shard_runtime: ExecutionConfig,

    // Configuration
    local_addr: Option<SocketAddr>,
    external_addrs: Vec<SocketAddr>,
    advertise_bound_udp: bool,

    // Dependencies (Transport / Logic)
    rng: Option<rand::Rng>,
    udp_mode: UdpMode,

    // Services
    cluster: Option<crate::cluster::Options>,
    http_api: Option<ListenerSource>,
    key_registry: Option<KeyRegistry>,
    internal_metrics: Option<ListenerSource>,
    steering: Option<SteeringFactory>,

    /// When `true`, UDP candidates are suppressed so that clients are forced
    /// to use the TCP path. Used in simulation tests that exercise TCP-only
    /// connectivity.
    tcp_only: bool,

    /// When `true`, TCP candidates are suppressed. This lets tests that model
    /// UDP datagram loss select exactly the transport their shaper controls.
    udp_only: bool,
    ice_tcp: Option<IceTcpConfig>,

    /// How many participants of one room share a shard before the next join
    /// spills to another.
    room_partition_size: usize,

    /// How a spilled room chooses its next shard.
    room_placement: crate::control::core::RoomPartitionPlacement,
}

impl Default for NodeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeBuilder {
    pub fn new() -> Self {
        Self {
            #[cfg(test)]
            runtime_control: None,
            #[cfg(feature = "sim")]
            sim_startup_notification: None,
            data_threads: 1,
            shard_runtime: ExecutionConfig::default(),
            local_addr: None,
            external_addrs: Vec::new(),
            advertise_bound_udp: false,
            rng: None,
            udp_mode: UdpMode::Batch,
            cluster: None,
            http_api: None,
            key_registry: None,
            internal_metrics: None,
            steering: {
                #[cfg(feature = "sim")]
                {
                    Some(Box::new(crate::control::steering::sim::attach))
                }
                #[cfg(not(feature = "sim"))]
                {
                    None
                }
            },
            tcp_only: false,
            udp_only: false,
            ice_tcp: None,
            room_partition_size: crate::control::core::DEFAULT_ROOM_PARTITION_SIZE,
            room_placement: crate::control::core::RoomPartitionPlacement::Balanced,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_runtime_control(
        mut self,
        control: pulsebeam_runtime::sync::oneshot::Sender<pulsebeam_runtime::rt::MigrationHandle>,
    ) -> Self {
        self.runtime_control = Some(control);
        self
    }

    /// Notify when the controller mailbox exists, before controller spawn and HTTP binding.
    #[cfg(feature = "sim")]
    pub fn with_sim_startup_notification(
        mut self,
        notification: pulsebeam_runtime::sync::mpsc::Sender<()>,
    ) -> Self {
        self.sim_startup_notification = Some(notification);
        self
    }

    /// Place a room's local partitions round-robin instead of by hash.
    ///
    /// For simulations that must actually cross a shard boundary: hashing picks
    /// independently per slot, so a small room lands co-located often enough
    /// that a plan depending on the split would pass without ever running the
    /// path it claims to cover.
    pub fn round_robin_rooms(mut self) -> Self {
        self.room_placement = crate::control::core::RoomPartitionPlacement::RoundRobin;
        self
    }

    /// Spill a room onto another shard after this many participants.
    ///
    /// Lowering it is how a test reaches the cross-shard media path without
    /// needing a room large enough to spill naturally: below the threshold a
    /// room is co-located and its fanout never leaves one shard.
    pub fn room_partition_size(mut self, participants: usize) -> Self {
        assert!(
            participants > 0,
            "a room partition must hold at least one participant"
        );
        self.room_partition_size = participants;
        self
    }

    /// Set the number of physical data-plane executor threads.
    ///
    /// In thread-per-core mode this is also the number of logical shards. In
    /// work-stealing mode the logical shard count is this value multiplied by
    /// `shards_per_worker`.
    pub fn data_threads(mut self, data_threads: usize) -> Self {
        assert!(data_threads > 0, "a node needs at least one data thread");
        self.data_threads = data_threads;
        self.shard_runtime.workers = data_threads;
        self
    }

    /// Backward-compatible alias for `data_threads`.
    ///
    /// Prefer `data_threads`: a Tokio work-stealing worker is no longer the same
    /// thing as a logical PulseBeam shard.
    pub fn workers(self, workers: usize) -> Self {
        self.data_threads(workers)
    }

    /// Execute one logical shard per dedicated OS thread/current-thread runtime.
    pub fn thread_per_core(mut self) -> Self {
        self.shard_runtime = ExecutionConfig::new(Profile::ThreadPerCore, self.data_threads, 1);
        self
    }

    /// Execute oversharded logical shards on a Tokio multi-thread work-stealing runtime.
    ///
    /// For example, `data_threads(4).work_stealing(16)` creates four Tokio
    /// scheduler workers and sixty-four independently owned logical shards.
    pub fn work_stealing(mut self, shards_per_worker: usize) -> Self {
        assert!(
            shards_per_worker > 0,
            "a work-stealing worker must own at least one shard"
        );
        self.shard_runtime =
            ExecutionConfig::new(Profile::WorkStealing, self.data_threads, shards_per_worker);
        self
    }

    /// Run shards as local tasks on the caller's runtime.
    ///
    /// This is retained for simulation. It deliberately does not mean
    /// work-stealing: `spawn_local` tasks stay on the local executor thread.
    pub fn with_current_runtime(mut self) -> Self {
        self.shard_runtime = ExecutionConfig::on_current_runtime(self.data_threads);
        self
    }

    /// Select a restart-time process runtime profile and logical oversharding.
    pub fn runtime_profile(mut self, profile: Profile, shards_per_worker: usize) -> Self {
        self.shard_runtime = ExecutionConfig::new(profile, self.data_threads, shards_per_worker);
        self
    }

    pub fn adaptive(self, shards_per_worker: usize) -> Self {
        self.runtime_profile(Profile::Adaptive, shards_per_worker)
    }

    fn configured_shard_count(&self) -> std::io::Result<usize> {
        self.shard_runtime.shard_count()
    }

    /// Set the local bind address for UDP/TCP transports.
    /// Ignored if transports are injected manually.
    pub fn local_addr(mut self, addr: SocketAddr) -> Self {
        self.local_addr = Some(addr);
        self
    }

    /// Set multiple external addresses (e.g. dual-stack IPv4/IPv6) advertised to peers.
    pub fn external_addrs(mut self, addrs: Vec<SocketAddr>) -> Self {
        self.external_addrs = addrs;
        self
    }

    /// Advertise the UDP addresses assigned by the kernel after binding.
    ///
    /// This is for a loopback caller binding `:0`; production deployments
    /// should continue to provide stable external addresses explicitly.
    pub fn advertise_bound_udp(mut self) -> Self {
        self.advertise_bound_udp = true;
        self
    }

    /// Inject a specific RNG (useful for deterministic simulation).
    pub fn rng(mut self, rng: rand::Rng) -> Self {
        self.rng = Some(rng);
        self
    }

    /// Set either scalar or batch mode. Default to batch.
    pub fn with_udp_mode(mut self, mode: UdpMode) -> Self {
        self.udp_mode = mode;
        self
    }

    /// Opt into trusted-network membership. This does not change room placement.
    pub fn with_cluster(mut self, options: crate::cluster::Options) -> Self {
        self.cluster = Some(options);
        self
    }

    /// Configure the HTTP Signaling API to bind to the specified address.
    pub fn with_http_api(mut self, addr: SocketAddr) -> Self {
        self.http_api = Some(ListenerSource::Bind(addr));
        self
    }

    pub fn with_key_registry(mut self, key_registry: KeyRegistry) -> Self {
        self.key_registry = Some(key_registry);
        self
    }

    /// Configure the HTTP Signaling API to use a pre-bound listener.
    /// Useful for testing with port 0 (ephemeral ports).
    pub fn with_http_api_listener(mut self, listener: impl Into<TcpListener>) -> Self {
        self.http_api = Some(ListenerSource::PreBound(listener.into()));
        self
    }

    /// Configure the Internal Metrics server to bind to the specified address.
    pub fn with_internal_metrics(mut self, addr: SocketAddr) -> Self {
        self.internal_metrics = Some(ListenerSource::Bind(addr));
        self
    }

    /// Configure the Internal Metrics server to use a pre-bound listener.
    pub fn with_internal_metrics_listener(mut self, listener: impl Into<TcpListener>) -> Self {
        self.internal_metrics = Some(ListenerSource::PreBound(listener.into()));
        self
    }

    /// Install an optional UDP steering extension after PulseBeam binds its sockets.
    ///
    /// The extension owns how steering is implemented; PulseBeam only supplies
    /// the bound reuseport sockets and consumes the generic `Steering` trait.
    pub fn with_steering<F>(mut self, attach: F) -> Self
    where
        F: FnOnce(&[pulsebeam_udp::BoundUdpSocket]) -> Result<Box<dyn Steering>> + Send + 'static,
    {
        self.steering = Some(Box::new(attach));
        self
    }

    /// Suppress UDP host candidates so that only the TCP passive candidate is
    /// advertised. Clients that support TCP active will be forced onto TCP.
    /// Useful for integration / simulation tests that exercise the TCP path.
    pub fn tcp_only(mut self) -> Self {
        self.tcp_only = true;
        self.udp_only = false;
        self.ice_tcp = None;
        self
    }

    /// Suppress TCP candidates so that clients use UDP. This is useful when a
    /// test intentionally applies datagram loss and must not silently select a
    /// reliable stream transport that the datagram shaper does not model.
    pub fn udp_only(mut self) -> Self {
        self.udp_only = true;
        self.tcp_only = false;
        self.ice_tcp = Some(IceTcpConfig::Disabled);
        self
    }

    /// Configure media ICE-TCP explicitly. A disabled fallback creates no media
    /// TCP listener, executor, tasks, shard inboxes or candidates.
    pub fn ice_tcp(mut self, config: IceTcpConfig) -> Self {
        self.udp_only = matches!(config, IceTcpConfig::Disabled);
        if self.udp_only {
            self.tcp_only = false;
        }
        self.ice_tcp = Some(config);
        self
    }

    /// Consumes the builder and runs the node until `shutdown` is cancelled.
    pub async fn run(mut self, shutdown: CancellationToken) -> Result<()> {
        let Some(options) = self.cluster.take() else {
            return self.run_media(shutdown).await;
        };
        let membership = crate::cluster::Service::start(options).await?;
        membership
            .supervise(self.run_media(shutdown.clone()), shutdown)
            .await
    }

    async fn run_media(self, shutdown: CancellationToken) -> Result<()> {
        // Default to an IPv6-any listener and disable v6-only mode so one socket can serve
        // both IPv6 and IPv4 peers.
        let local_addr = self
            .local_addr
            .unwrap_or_else(|| SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0));
        let advertised_addrs = self.advertised_udp_addrs()?;
        let udp_sockets = bind_udp_sockets(
            local_addr,
            advertised_addrs.first().copied(),
            self.configured_shard_count()?,
            self.udp_mode,
        )
        .await?;
        self.run_bound(udp_sockets, advertised_addrs, shutdown)
            .await
    }

    fn advertised_udp_addrs(&self) -> Result<Vec<SocketAddr>> {
        if self.external_addrs.is_empty() && !self.advertise_bound_udp {
            return Err(anyhow::anyhow!(
                "NodeBuilder requires external addresses; call `.external_addrs(...)` or `.advertise_bound_udp()` for loopback :0"
            ));
        }

        let advertised_addrs = self.external_addrs.iter().copied();

        let mut deduped = Vec::with_capacity(self.external_addrs.len());
        let mut seen = HashSet::with_capacity(self.external_addrs.len());
        for addr in advertised_addrs {
            if seen.insert(addr) {
                deduped.push(addr);
            }
        }
        let mut v4_addrs = Vec::new();
        let mut v6_addrs = Vec::new();
        for addr in deduped {
            if addr.is_ipv4() {
                v4_addrs.push(addr);
            } else {
                v6_addrs.push(addr);
            }
        }

        if v4_addrs.len() > 1 {
            return Err(anyhow::anyhow!(
                "NodeBuilder currently supports exactly one external IPv4 address"
            ));
        }
        if v6_addrs.len() > 1 {
            return Err(anyhow::anyhow!(
                "NodeBuilder currently supports at most one external IPv6 address"
            ));
        }

        let mut advertised_addrs = Vec::with_capacity(2);
        advertised_addrs.extend(v4_addrs);
        advertised_addrs.extend(v6_addrs);
        Ok(advertised_addrs)
    }

    async fn run_bound(
        self,
        udp_sockets: Vec<pulsebeam_udp::BoundUdpSocket>,
        advertised_addrs: Vec<SocketAddr>,
        shutdown: CancellationToken,
    ) -> Result<()> {
        let data_threads = self.data_threads;
        let shard_runtime = self.shard_runtime;
        let requested_shard_count = self.configured_shard_count()?;
        let local_addr = self
            .local_addr
            .unwrap_or_else(|| SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0));
        let mut control_tasks = JoinSet::new();
        // Initialised here so the global recorder is in place before anything
        // runs, but served after the shards exist — it needs their metrics lane.
        let internal_ctx = match self.internal_metrics {
            Some(source) => {
                let listener = match source {
                    ListenerSource::Bind(addr) => bind_tcp_listener(addr)
                        .await
                        .context("binding internal metrics")?,
                    ListenerSource::PreBound(l) => l,
                };
                Some(
                    internal::InternalContext::init(listener)
                        .context("failed to spawn internal server")?,
                )
            }
            None => None,
        };

        let spare_core = pulsebeam_runtime::rt::spare_core(data_threads);

        debug_assert!(!udp_sockets.is_empty());
        let shard_count = udp_sockets.len();

        if shard_count != requested_shard_count {
            tracing::warn!(
                requested = requested_shard_count,
                running = shard_count,
                "node is running fewer logical shards than configured"
            );
        }

        let steering = match self.steering {
            Some(attach) => Some(attach(&udp_sockets).context("attaching UDP steering")?),
            None => None,
        };

        // This is the only owner of data-plane execution. The caller runtime
        // remains the control runtime for controller/API/metrics tasks.
        tracing::info!(profile = %shard_runtime.profile, workers = data_threads, shards = shard_count, "starting node runtime");
        let mut shard_executor = DataExecutor::new(shard_runtime, shutdown.clone()).await?;
        #[cfg(test)]
        if let Some(control) = self.runtime_control {
            if let Some(handle) = shard_executor.migration_handle() {
                let _ = control.send(handle);
            }
        }

        let mut tcp_service = None;
        let result: Result<()> = async {
        let mut tcp_rxs = Vec::with_capacity(shard_count);
        let mut tcp_txs = Vec::new();
        let tcp_config = self.ice_tcp.unwrap_or_else(|| {
            if self.udp_only {
                IceTcpConfig::Disabled
            } else {
                IceTcpConfig::Passive {
                    bind: local_addr,
                    limits: IceTcpLimits::default(),
                }
            }
        });
        let mut candidates = sockets_to_candidates(&udp_sockets, &advertised_addrs);
        if self.tcp_only {
            candidates.clear();
        }
        tcp_service = match tcp_config {
            IceTcpConfig::Disabled => {
                tcp_rxs.resize_with(shard_count, || None);
                None
            }
            IceTcpConfig::Passive { bind, limits } => {
                limits.validate(shard_count)?;
                for _ in 0..shard_count {
                    let (tx, rx) =
                        crate::ice_tcp::inbox(limits.inbox_capacity, limits.max_frame_bytes)?;
                    tcp_txs.push(tx);
                    tcp_rxs.push(Some(rx));
                }
                let (service, bound) = crate::ice_tcp::Service::start(
                    bind,
                    advertised_addrs.clone(),
                    crate::ice_tcp::Routing {
                        cluster_id: 0,
                        node_id: 0,
                        shard_count,
                    },
                    limits,
                    tcp_txs,
                    spare_core,
                    shutdown.clone(),
                )
                .await
                .context("starting passive ICE-TCP")?;
                let tcp_addrs = if advertised_addrs.is_empty() {
                    vec![bound]
                } else {
                    advertised_addrs.clone()
                };
                candidates.extend(tcp_addrs.into_iter().map(LocalCandidate::TcpPassive));
                Some(service)
            }
        };


        let (shard_event_tx, shard_event_rx) = mailbox::bounded(
            const { core::num::NonZeroUsize::new(crate::shard::worker::SHARD_EVENT_CAPACITY).unwrap() },
        );
        let mut frame_txs = Vec::with_capacity(shard_count);
        let mut frame_rxs = Vec::with_capacity(shard_count);
        for _ in 0..shard_count {
            let (tx, rx) = mailbox::bounded(
                const {
                    core::num::NonZeroUsize::new(crate::shard::worker::SHARD_FRAME_CAPACITY)
                        .unwrap()
                },
            );
            frame_txs.push(tx);
            frame_rxs.push(rx);
        }

        let mut rng = self.rng.ok_or_else(|| {
            anyhow::anyhow!(
                "NodeBuilder requires an RNG; call `.rng(...)` when constructing the node"
            )
        })?;

        let controller_rng = rand::Rng::seed_from_u64(rng.next_u64());

        // One NTP<->Instant anchor for the whole node: read the wall clock once,
        // here, so every shard shares a timeline and nothing reads it again.
        let wall_anchor = WallAnchor::from(pulsebeam_runtime::net::ReceiveTime::now());

        let mut shard_contexts = Vec::with_capacity(shard_count);

        // Only built when something will scrape it, so a node without the
        // internal server never pays for a snapshot it would throw away.
        let stats_lane = if internal_ctx.is_some() {
            Some(mailbox::bounded::<
                Box<crate::shard::recorder::ShardStatsReport>,
            >(
                core::num::NonZeroUsize::new(
                    crate::shard::worker::STATS_CAPACITY.saturating_mul(shard_count.max(1)),
                )
                .ok_or_else(|| anyhow::anyhow!("stats queue capacity must be nonzero"))?,
            ))
        } else {
            None
        };
        let (stats_tx, stats_rx) = match stats_lane {
            Some((tx, rx)) => (Some(tx), Some(rx)),
            None => (None, None),
        };

        let mut view_writers = Vec::with_capacity(shard_count);

        for (shard_idx, ((udp_sock, tcp_rx), frame_rx)) in udp_sockets
            .into_iter()
            .zip(tcp_rxs)
            .zip(frame_rxs)
            .enumerate()
        {
            let shard_id = ShardId::new(shard_idx);
            let (update_writer, update_reader) = crate::shard_update::new_shard_update(shard_id);
            view_writers.push(update_writer);
            let (shard_command_tx, shard_command_rx) = mailbox::bounded(
                const {
                    core::num::NonZeroUsize::new(crate::shard::worker::SHARD_COMMAND_CAPACITY)
                        .unwrap()
                },
            );
            let shard_event_tx = shard_event_tx.clone();
            let frame_txs = frame_txs.clone();
            #[allow(
                clippy::disallowed_types,
                reason = "Arc<ShardMetrics>, handed over once before any shard runs, see module note"
            )]
            let occupancy = Arc::new(ShardMetrics::new());
            let worker_occupancy = occupancy.clone();
            let shard_stats_tx = stats_tx.clone();

            let launch =
                move || -> std::io::Result<pulsebeam_runtime::rt::ManagedService<ShardWorker>> {
                    let udp_sock = udp_sock.activate()?;
                    let shard = ShardWorker::new(
                        shard_id,
                        udp_sock.capabilities().max_gso_segments,
                        tcp_rx,
                        shard_command_rx,
                        update_reader,
                        shard_event_tx,
                        frame_rx,
                        frame_txs,
                        worker_occupancy,
                        shard_stats_tx,
                        wall_anchor,
                    );
                    Ok(pulsebeam_runtime::rt::ManagedService::new(shard, udp_sock))
                };

            let worker = crate::control::placement::initial_worker(shard_id, shard_executor.worker_count())
                .context("missing dataplane workers")?;
            shard_executor.spawn_on(shard_id.index(), worker, launch).await?;

            shard_contexts.push(ShardContext {
                command_tx: shard_command_tx,
                metrics: occupancy,
            });
        }

        if let (Some(ctx), Some(stats_rx)) = (internal_ctx, stats_rx) {
            control_tasks.spawn(ignore(ctx.serve_internal_http(
                shard_count,
                stats_rx,
                shutdown.child_token(),
            )));
        }

        // Lower control priority only after production data threads have been
        // created, so they do not inherit the control-plane scheduling policy.
        if shard_executor.should_tune_control_thread() {
            tune_current_control_thread();
        }

        let mut controller = ControllerActor::with_placement(
            controller_rng,
            shard_contexts,
            candidates,
            self.room_partition_size,
            self.room_placement,
            view_writers,
        );
        controller.set_execution(shard_executor.migration_handle());
        controller.set_steering(steering);
        controller.set_key_registry(self.key_registry.clone());
        // intentionally small so backpressure is applied early
        // with 62.5 ms pacing rate, at most we get 1s latency here.
        let (controller_command_tx, controller_command_rx) =
            mailbox::bounded(const { core::num::NonZeroUsize::new(16).unwrap() });
        #[cfg(feature = "sim")]
        if let Some(notification) = self.sim_startup_notification {
            let _ = notification.try_send(());
        }

        control_tasks.spawn(ignore(controller.run(
            controller_command_rx,
            shard_event_rx,
            shutdown.child_token(),
        )));

        if let Some(source) = self.http_api {
            // Resolve listener
            let listener = match source {
                ListenerSource::Bind(addr) => {
                    bind_tcp_listener(addr).await.context("binding http api")?
                }
                ListenerSource::PreBound(l) => l,
            };

            let local_addr = listener.local_addr().ok();
            tracing::info!("signaling api listening on {:?}", local_addr);

            let api_cfg = api::ApiConfig {
                base_path: "/api/v1".to_string(),
                // Best effort to guess host if bound randomly
                default_host: local_addr
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "[::]:0".to_string()),
            };

            let cors = api::cors();

            let key_registry = self
                .key_registry
                .context("HTTP signaling requires a key registry")?;
            let router = api::router(controller_command_tx, api_cfg, key_registry)
                .layer(CompressionLayer::new().zstd(true))
                .layer(RequestDecompressionLayer::new().zstd(true).gzip(true))
                .layer(cors);
            // https://github.com/tokio-rs/axum/issues/3112
            // missing graceful shutdown will cause task leaks
            let api_server =
                pulsebeam_runtime::net::tcp::serve_http(listener, router, shutdown.child_token().cancelled_owned());

            control_tasks.spawn(async move {
                if let Err(e) = api_server.await {
                    tracing::error!("http server error: {e}");
                }
            });
        }

        // Wait for shutdown
        loop {
            pulsebeam_runtime::select! {
                result = control_tasks.join_next() => match result {
                    Some(result) => { result.context("control service task failed")?; }
                    None => break,
                },
                _ = shutdown.cancelled() => {
                    tracing::info!("node received shutdown");
                    break;
                }
            }
        }

        Ok(())
        }.await;
        shutdown.cancel();
        let mut control_error = None;
        while let Some(result) = control_tasks.join_next().await {
            if let Err(error) = result {
                control_error.get_or_insert(error);
            }
        }
        let tcp_result = if let Some(service) = tcp_service {
            service.finish().await
        } else {
            Ok(())
        };
        let data_result = shard_executor.finish().await;
        result?;
        tcp_result?;
        data_result?;
        if let Some(error) = control_error {
            return Err(error.into());
        }
        Ok(())
    }
}

pub struct NodeContext {
    pub rng: pulsebeam_runtime::rand::Rng,
}

async fn bind_udp_socket(
    local_addr: SocketAddr,
    mode: UdpMode,
    advertised_addr: Option<SocketAddr>,
    shard_index: u16,
) -> std::io::Result<pulsebeam_udp::BoundUdpSocket> {
    #[cfg(feature = "sim")]
    {
        pulsebeam_runtime::net::bind_udp_socket(local_addr, mode, advertised_addr, shard_index)
            .await
    }
    #[cfg(not(feature = "sim"))]
    {
        let _ = (advertised_addr, shard_index);
        let mut options = pulsebeam_udp::BindOptions::new(local_addr);
        options.reuse_port = true;
        options.max_datagram_bytes = pulsebeam_udp::MAX_DATAGRAM_BYTES;
        options.mode = match mode {
            UdpMode::Batch => pulsebeam_udp::Mode::Auto,
            UdpMode::Scalar => pulsebeam_udp::Mode::Scalar,
        };
        pulsebeam_udp::bind(options).await
    }
}

async fn bind_udp_sockets(
    local_addr: SocketAddr,
    advertised_addr: Option<SocketAddr>,
    shards: usize,
    mode: UdpMode,
) -> Result<Vec<pulsebeam_udp::BoundUdpSocket>> {
    let mut sockets = Vec::with_capacity(shards);

    for shard_index in 0..shards {
        let shard_index = u16::try_from(shard_index).unwrap_or(u16::MAX);
        let socket = match bind_udp_socket(local_addr, mode, advertised_addr, shard_index).await {
            Ok(s) => s,
            Err(e) if sockets.is_empty() => {
                return Err(anyhow::Error::new(e).context("failed to bind first udp socket"));
            }
            Err(e) => {
                // Shedding workers here is not a capacity trade-off: a
                // single-shard node never executes a cross-shard path at all.
                tracing::warn!(
                    requested = shards,
                    running = sockets.len(),
                    "SO_REUSEPORT unavailable or failed after the first bind; running fewer \
                     shards than requested: {e}"
                );
                break;
            }
        };
        sockets.push(socket);
    }
    Ok(sockets)
}

fn sockets_to_candidates(
    sockets: &[pulsebeam_udp::BoundUdpSocket],
    advertised_addrs: &[SocketAddr],
) -> Vec<LocalCandidate> {
    let candidate_addrs = if advertised_addrs.is_empty() {
        let mut unique = Vec::with_capacity(sockets.len());
        let mut seen = HashSet::with_capacity(sockets.len());
        for socket in sockets {
            let addr = socket.local_addr();
            if seen.insert(addr) {
                unique.push(addr);
            }
        }
        unique
    } else {
        advertised_addrs.to_vec()
    };

    let mut candidates = Vec::with_capacity(candidate_addrs.len());
    for addr in candidate_addrs {
        let candidate = LocalCandidate::Udp(addr);
        candidates.push(candidate);
    }

    candidates
}

pub async fn ignore<T>(fut: impl Future<Output = T>) {
    let _ = fut.await;
}

pub use pulsebeam_runtime::rt::{
    tune_control_thread as tune_current_control_thread,
    tune_data_thread as tune_current_data_thread,
};

mod internal {
    use super::*;
    use anyhow::Result;
    use axum::{
        Router,
        extract::Query,
        response::{Html, IntoResponse},
        routing::get,
    };
    use hyper::{
        StatusCode,
        header::{CONTENT_DISPOSITION, CONTENT_TYPE},
    };

    use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
    use pprof::ProfilerGuard;
    use pprof::protos::Message;
    use pulsebeam_runtime::sync::oneshot;
    use serde::Deserialize;

    #[derive(Deserialize)]
    pub struct ProfileParams {
        #[serde(default = "default_seconds")]
        seconds: u64,
        #[serde(default)]
        flamegraph: bool,
    }

    fn default_seconds() -> u64 {
        30
    }

    fn create_exponential_buckets(start: f64, factor: f64, count: usize) -> Vec<f64> {
        let mut buckets = Vec::with_capacity(count);
        let mut current = start;
        for _ in 0..count {
            buckets.push(current);
            current *= factor;
        }
        buckets
    }

    pub struct InternalContext {
        listener: TcpListener,
        prometheus: PrometheusHandle,
    }

    impl InternalContext {
        pub fn init(listener: TcpListener) -> anyhow::Result<Self> {
            #[allow(
                clippy::disallowed_methods,
                reason = "the one global recorder in the process, for control-plane, HTTP and runtime metrics. Shards never write to it — they install their own per tick and report by message. See clippy.toml."
            )]
            let prometheus = PrometheusBuilder::new()
                .set_buckets_for_metric(
                    Matcher::Suffix("_delay_us".to_string()),
                    &create_exponential_buckets(1.0, 4.0, 6), // 1us -> 4ms,
                )
                .context("metrics bucket configuration is invalid")?
                .install_recorder()?;

            Ok(Self {
                listener,
                prometheus,
            })
        }

        pub async fn serve_internal_http(
            self,
            shard_count: usize,
            stats_rx: mailbox::Receiver<Box<crate::shard::recorder::ShardStatsReport>>,
            shutdown: CancellationToken,
        ) -> Result<()> {
            const INDEX_HTML: &str = r#"
<ul>
  <li><a href="/healthz">Healthcheck</a></li>
  <li><a href="/metrics">Metrics</a></li>
  <li><a href="/debug/pprof/profile?seconds=30">CPU Profile (pprof)</a></li>
  <li><a href="/debug/pprof/profile?seconds=30&flamegraph=true">CPU Flamegraph</a></li>
  <li><a href="/debug/pprof/allocs?seconds=30">Memory Profile (pprof)</a></li>
  <li><a href="/debug/pprof/allocs?seconds=30&flamegraph=true">Memory Flamegraph</a></li>
</ul>
"#;

            // One scrape at a time is plenty; a queue here would only serve
            // several callers the same numbers a moment apart.
            let (scrape_tx, scrape_rx) =
                mailbox::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
            let mut services = JoinSet::new();
            services.spawn(crate::control::stats_aggregator::run(
                shard_count,
                stats_rx,
                scrape_rx,
                shutdown.child_token(),
            ));

            let router = {
                let prometheus = self.prometheus.clone();
                Router::new()
                    .route("/debug/pprof/profile", get(pprof_profile))
                    // .route("/debug/pprof/allocs", get(heap_profile))
                    .route("/healthz", get(healthcheck))
                    .route("/", get(async move || Html(INDEX_HTML)))
                    .route(
                        "/metrics",
                        get(async move || {
                            // The exporter still owns control-plane, HTTP and
                            // runtime metrics; the shards' own numbers arrive
                            // as values and are rendered separately.
                            let shards = scrape_shards(&scrape_tx).await;
                            format!("{}{shards}", prometheus.render())
                        }),
                    )
            };
            services.spawn(pulsebeam_runtime::rt::monitor(
                move || self.prometheus.run_upkeep(),
                shutdown.child_token(),
            ));

            tracing::info!(
                "internal metrics listening on {:?}",
                self.listener.local_addr().ok()
            );

            let stop = shutdown.child_token();
            services.spawn(async move {
                if let Err(error) = pulsebeam_runtime::net::tcp::serve_http(
                    self.listener,
                    router,
                    stop.cancelled_owned(),
                )
                .await
                {
                    tracing::error!(?error, "internal http server error");
                }
            });
            pulsebeam_runtime::select! {
                _ = services.join_next() => {},
                _ = shutdown.cancelled() => {},
            }
            shutdown.cancel();
            while let Some(result) = services.join_next().await {
                result.context("joining internal service")?;
            }
            Ok(())
        }
    }

    /// Ask the aggregator for the shards' exposition.
    ///
    /// A scrape must never be able to stall the node, so a wedged or departed
    /// aggregator yields an empty block: the exporter's own metrics still get
    /// served, and the missing shard series say plainly that something is wrong.
    async fn scrape_shards(scrape_tx: &mailbox::Sender<oneshot::Sender<String>>) -> String {
        let (tx, rx) = oneshot::channel();
        if scrape_tx.try_send(tx).is_err() {
            return String::new();
        }
        rx.await.unwrap_or_default()
    }

    // pub async fn heap_profile(
    //     Query(params): Query<ProfileParams>,
    // ) -> Result<Response, (StatusCode, String)> {
    //     // Safe access to jemalloc control
    //     let mut prof_ctl = match jemalloc_pprof::PROF_CTL.as_ref() {
    //         Some(ctl) => ctl.lock().await,
    //         None => {
    //             return Err((
    //                 StatusCode::NOT_IMPLEMENTED,
    //                 "Jemalloc not enabled or configured".to_string(),
    //             ));
    //         }
    //     };
    //
    //     require_profiling_activated(&prof_ctl)?;
    //
    //     let resp = if params.flamegraph {
    //         let svg = prof_ctl
    //             .dump_flamegraph()
    //             .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    //
    //         (
    //             StatusCode::OK,
    //             [
    //                 (CONTENT_TYPE, "image/svg+xml"),
    //                 (CONTENT_DISPOSITION, "attachment; filename=allocs.svg"),
    //             ],
    //             svg,
    //         )
    //             .into_response()
    //     } else {
    //         let pprof = prof_ctl
    //             .dump_pprof()
    //             .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    //
    //         (
    //             StatusCode::OK,
    //             [
    //                 (CONTENT_TYPE, "application/octet-stream"),
    //                 (CONTENT_DISPOSITION, "attachment; filename=allocs.pprof"),
    //             ],
    //             pprof,
    //         )
    //             .into_response()
    //     };
    //     Ok(resp)
    // }
    //
    // fn require_profiling_activated(
    //     prof_ctl: &jemalloc_pprof::JemallocProfCtl,
    // ) -> Result<(), (StatusCode, String)> {
    //     if prof_ctl.activated() {
    //         Ok(())
    //     } else {
    //         Err((StatusCode::FORBIDDEN, "heap profiling not activated".into()))
    //     }
    // }

    async fn pprof_profile(
        Query(params): Query<ProfileParams>,
    ) -> Result<impl IntoResponse, (StatusCode, String)> {
        let guard = ProfilerGuard::new(100).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to start profiler: {e}"),
            )
        })?;

        pulsebeam_runtime::time::sleep(Duration::from_secs(params.seconds)).await;

        let resp = match guard.report().build() {
            Ok(report) => {
                if params.flamegraph {
                    let mut body = Vec::new();
                    report
                        .flamegraph(&mut body)
                        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

                    (
                        StatusCode::OK,
                        [
                            (CONTENT_TYPE, "image/svg+xml"),
                            (CONTENT_DISPOSITION, "attachment; filename=cpu.svg"),
                        ],
                        body,
                    )
                        .into_response()
                } else {
                    let profile = report
                        .pprof()
                        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

                    let body = profile.encode_to_vec();
                    (
                        StatusCode::OK,
                        [
                            (CONTENT_TYPE, "application/octet-stream"),
                            (CONTENT_DISPOSITION, "attachment; filename=cpu.pprof"),
                        ],
                        body,
                    )
                        .into_response()
                }
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build pprof report: {e}"),
            )
                .into_response(),
        };

        Ok(resp)
    }

    async fn healthcheck() -> impl IntoResponse {
        StatusCode::OK
    }
}

#[cfg(all(test, feature = "transport-conformance"))]
#[path = "node/transport_conformance.rs"]
mod transport_conformance;

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;

    /// The defaults are the deployment nobody configures, so they are a contract.
    ///
    /// `run()` is orchestration the simulation covers end to end; what a unit test can hold is the
    /// configuration it starts from, where a silent change turns into a differently-shaped node
    /// with every test still green.
    #[test]
    fn the_defaults_describe_a_single_thread_per_core_shard() {
        let builder = NodeBuilder::new();

        assert_eq!(builder.data_threads, 1);
        assert_eq!(builder.configured_shard_count().unwrap(), 1);
        assert!(builder.local_addr.is_none());
        assert!(builder.external_addrs.is_empty());
        assert!(builder.http_api.is_none(), "no API is exposed unless asked");
        assert!(builder.internal_metrics.is_none());
        assert!(!builder.tcp_only, "UDP candidates are offered by default");
        assert!(!builder.udp_only, "TCP candidates are offered by default");
        assert!(matches!(builder.udp_mode, UdpMode::Batch));
        #[cfg(feature = "sim")]
        assert!(builder.steering.is_some());
        #[cfg(not(feature = "sim"))]
        assert!(builder.steering.is_none());
        assert_eq!(builder.shard_runtime.profile, Profile::Adaptive);
        assert!(matches!(
            builder.room_placement,
            crate::control::core::RoomPartitionPlacement::Balanced
        ));
        assert_eq!(
            builder.room_partition_size,
            crate::control::core::DEFAULT_ROOM_PARTITION_SIZE
        );
    }

    #[cfg(feature = "sim")]
    #[test]
    fn disabled_media_tcp_does_not_bind_an_occupied_tcp_port() {
        let host = pulsebeam_runtime::testing::test_host_ip("192.168.250.102");
        pulsebeam_runtime::testing::run_local(host, async {
            let occupied = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
                .await
                .unwrap();
            let media = occupied.local_addr().unwrap();
            let (notification, mut ready) = pulsebeam_runtime::sync::mpsc::channel(1);
            let shutdown = CancellationToken::new();
            let builder = NodeBuilder::new()
                .with_current_runtime()
                .local_addr(media)
                .external_addrs(vec![media])
                .ice_tcp(IceTcpConfig::Disabled)
                .rng(rand::Rng::seed_from_u64(9))
                .with_sim_startup_notification(notification);
            let mut node = pulsebeam_runtime::task::spawn_local(builder.run(shutdown.clone()));
            pulsebeam_runtime::select! {
                notification = ready.recv() => assert!(notification.is_some()),
                result = &mut node => panic!("disabled node failed startup: {result:?}"),
            }
            shutdown.cancel();
            node.await.unwrap().unwrap();
            drop(occupied);
        });
    }

    #[test]
    fn adaptive_and_current_runtime_preserve_explicit_topology() {
        let adaptive = NodeBuilder::new().data_threads(2).adaptive(16);
        assert_eq!(adaptive.shard_runtime.profile, Profile::Adaptive);
        assert_eq!(adaptive.configured_shard_count().unwrap(), 32);
        let current = adaptive.with_current_runtime();
        assert!(current.shard_runtime.is_current_runtime());
        assert_eq!(current.configured_shard_count().unwrap(), 2);
        assert!(
            NodeBuilder::new()
                .runtime_profile(Profile::ThreadPerCore, 16)
                .configured_shard_count()
                .is_err()
        );
    }

    #[test]
    fn work_stealing_overshards_each_scheduler_worker() {
        let builder = NodeBuilder::new().data_threads(4).work_stealing(16);

        assert_eq!(builder.data_threads, 4);
        assert_eq!(builder.configured_shard_count().unwrap(), 64);
        assert_eq!(builder.shard_runtime.profile, Profile::WorkStealing);
        assert_eq!(builder.shard_runtime.shards_per_worker, 16);
    }

    #[test]
    fn thread_per_core_keeps_one_shard_per_data_thread() {
        let builder = NodeBuilder::new().data_threads(8).thread_per_core();

        assert_eq!(builder.data_threads, 8);
        assert_eq!(builder.configured_shard_count().unwrap(), 8);
    }

    #[test]
    fn the_workers_alias_still_sets_data_threads() {
        let builder = NodeBuilder::new().workers(6);

        assert_eq!(builder.data_threads, 6);
        assert_eq!(builder.configured_shard_count().unwrap(), 6);
    }

    #[test]
    fn the_builder_records_what_it_was_asked_for() {
        let addr: SocketAddr = "127.0.0.1:7070".parse().unwrap();
        let builder = NodeBuilder::new()
            .data_threads(4)
            .local_addr(addr)
            .external_addrs(vec![addr])
            .room_partition_size(9)
            .round_robin_rooms()
            .tcp_only();

        assert_eq!(builder.data_threads, 4);
        assert_eq!(builder.local_addr, Some(addr));
        assert_eq!(builder.external_addrs, vec![addr]);
        assert_eq!(builder.room_partition_size, 9);
        assert!(builder.tcp_only);
        assert!(!builder.udp_only);
        assert!(NodeBuilder::new().udp_only().udp_only);
        assert!(!NodeBuilder::new().udp_only().tcp_only);
        assert!(matches!(
            builder.room_placement,
            crate::control::core::RoomPartitionPlacement::RoundRobin
        ));
    }

    #[test]
    #[should_panic(expected = "at least one data thread")]
    fn a_node_must_have_a_data_thread() {
        let _ = NodeBuilder::new().data_threads(0);
    }

    #[test]
    #[should_panic(expected = "at least one shard")]
    fn a_work_stealing_worker_must_have_a_shard() {
        let _ = NodeBuilder::new().work_stealing(0);
    }

    /// A zero-participant slot would divide by zero when placement asks which slot a room is on.
    /// Rejecting it at the builder is the difference between a misconfiguration and a crash on the
    /// first join.
    #[test]
    #[should_panic(expected = "at least one participant")]
    fn a_shard_slot_must_hold_somebody() {
        let _ = NodeBuilder::new().room_partition_size(0);
    }

    /// `Default` and `new` must not drift: one is what most callers get and the other is what the
    /// tests above pin down.
    #[test]
    fn default_is_new() {
        let (a, b) = (NodeBuilder::default(), NodeBuilder::new());
        assert_eq!(a.data_threads, b.data_threads);
        assert_eq!(a.shard_runtime, b.shard_runtime);
        assert_eq!(a.room_partition_size, b.room_partition_size);
        assert_eq!(a.tcp_only, b.tcp_only);
        assert_eq!(a.udp_only, b.udp_only);
    }
}

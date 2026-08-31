//! Shared-state exception: `Arc<ShardMetrics>`, one per shard, handed over
//! before any shard runs. See `shard::metrics`.

use anyhow::{Context, Result};
use core_affinity::get_core_ids;
use pulsebeam_core::net::TcpListener;
use pulsebeam_runtime::mailbox;
use pulsebeam_runtime::net;
use pulsebeam_runtime::net::UdpMode;
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
#[allow(
    clippy::disallowed_types,
    reason = "one control-plane health flag is published to the health handler and monitor"
)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use tokio::time::Instant;

use crate::clock::WallAnchor;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::decompression::RequestDecompressionLayer;

use crate::control::api;
use crate::control::controller::ControllerActor;
use crate::id::ShardId;
use crate::shard::ShardContext;
use crate::shard::metrics::ShardMetrics;
use crate::shard::worker::ShardWorker;

pub const CLOCK_DRIFT_HEALTH_LIMIT: Duration = Duration::from_millis(250);

#[derive(Clone)]
struct NodeHealth {
    healthy: Arc<AtomicBool>,
}

impl NodeHealth {
    fn new() -> Self {
        Self {
            healthy: Arc::new(AtomicBool::new(true)),
        }
    }

    fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    fn observe_clock(&self, anchor: WallAnchor, wall: SystemTime, mono: Instant) {
        let drift = clock_drift(anchor, wall, mono);
        let healthy = drift.is_some_and(|value| value <= CLOCK_DRIFT_HEALTH_LIMIT);
        self.healthy.store(healthy, Ordering::Relaxed);
        metrics::gauge!("node_clock_healthy").set(if healthy { 1.0 } else { 0.0 });
        if let Some(drift) = drift {
            metrics::gauge!("node_clock_drift_ms").set(drift.as_secs_f64() * 1_000.0);
        }
    }
}

fn clock_drift(anchor: WallAnchor, wall: SystemTime, mono: Instant) -> Option<Duration> {
    anchor.project(mono).ok().map(|projected| {
        projected
            .duration_since(wall)
            .unwrap_or_else(|error| error.duration())
    })
}

/// Defines how a service listener is acquired.
enum ListenerSource {
    /// Bind to this address internally.
    Bind(SocketAddr),
    /// Use this pre-bound listener.
    PreBound(TcpListener),
}

/// How logical shards are executed.
///
/// A shard remains the ownership, routing, socket, and metrics unit in every
/// mode. This enum changes only how shard futures receive CPU time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShardRuntime {
    /// One logical shard on one dedicated current-thread Tokio runtime.
    ThreadPerCore,

    /// Many logical shards scheduled by a Tokio multi-thread work-stealing runtime.
    WorkStealing {
        /// Number of logical shards created for every Tokio scheduler worker.
        shards_per_worker: usize,
    },

    /// Run every shard as a local task on the caller's runtime.
    ///
    /// This exists for simulation, whose synthetic reuseport sockets are
    /// deliberately thread-local. It is not the production work-stealing mode.
    CurrentRuntime,
}

impl ShardRuntime {
    fn shard_count(self, data_threads: usize) -> usize {
        match self {
            Self::ThreadPerCore | Self::CurrentRuntime => data_threads,
            #[allow(clippy::expect_used, reason = "too many shards")]
            Self::WorkStealing { shards_per_worker } => data_threads
                .checked_mul(shards_per_worker)
                .expect("configured shard count overflowed usize"),
        }
    }
}

mod platform {
    use super::*;

    #[cfg(not(feature = "sim"))]
    mod imp {
        use super::*;

        pub(in crate::node) async fn bind_tcp_listener(
            addr: SocketAddr,
        ) -> std::io::Result<TcpListener> {
            let socket = socket2::Socket::new(
                socket2::Domain::for_address(addr),
                socket2::Type::STREAM,
                Some(socket2::Protocol::TCP),
            )?;

            if addr.is_ipv6() {
                // Prefer dual-stack listeners so a single IPv6 socket can
                // accept IPv4-mapped peers.
                socket.set_only_v6(false)?;
            }

            socket.set_nonblocking(true)?;
            socket.set_reuse_address(true)?;
            socket.bind(&addr.into())?;
            socket.listen(1024)?;

            tokio::net::TcpListener::from_std(socket.into())
        }
    }

    #[cfg(feature = "sim")]
    mod imp {
        use super::*;

        pub(in crate::node) async fn bind_tcp_listener(
            addr: SocketAddr,
        ) -> std::io::Result<TcpListener> {
            TcpListener::bind(addr).await
        }
    }

    pub(super) use imp::bind_tcp_listener;
}

mod shard_executor {
    use super::*;

    #[cfg(not(feature = "sim"))]
    mod imp {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Owns the dedicated Tokio runtime used only by data-plane shard tasks.
        ///
        /// The runtime is deliberately separate from the caller/control runtime:
        /// controller, HTTP, metrics, and other control-plane tasks never run here.
        struct WorkStealingRuntime {
            runtime: Option<tokio::runtime::Runtime>,
        }

        impl WorkStealingRuntime {
            fn build(data_threads: usize, cpu_cores: &[core_affinity::CoreId]) -> Result<Self> {
                let next_worker = AtomicUsize::new(0);
                let worker_cores = cpu_cores.to_vec();

                let mut builder = tokio::runtime::Builder::new_multi_thread();
                // TODO: tune these with real benchmark. Default seems good
                builder
                    .worker_threads(data_threads)
                    .thread_name("pb-data")
                    .enable_all()
                    .disable_lifo_slot()
                    .enable_alt_timer();

                // Apply exactly the same OS tuning as thread-per-core, but to
                // Tokio's physical scheduler workers rather than logical shards.
                // `on_thread_start` can also observe later blocking-pool threads;
                // the scheduler workers are created with the runtime, so only the
                // first `data_threads` callbacks are data workers we tune/pin.
                builder.on_thread_start(move || {
                    let worker_idx = next_worker.fetch_add(1, Ordering::Relaxed);
                    if worker_idx >= data_threads {
                        return;
                    }

                    let core_id = if worker_cores.is_empty() {
                        None
                    } else {
                        worker_cores.get(worker_idx % worker_cores.len()).copied()
                    };

                    let realtime = tune_current_data_thread(core_id);
                    metrics::gauge!(
                        "data_worker_realtime",
                        "worker" => worker_idx.to_string()
                    )
                    .set(f64::from(realtime));
                });

                let runtime = builder
                    .build()
                    .context("building Tokio work-stealing data runtime")?;

                Ok(Self {
                    runtime: Some(runtime),
                })
            }

            fn handle(&self) -> &tokio::runtime::Handle {
                self.runtime
                    .as_ref()
                    .expect("work-stealing data runtime is alive")
                    .handle()
            }
        }

        impl Drop for WorkStealingRuntime {
            fn drop(&mut self) {
                if let Some(runtime) = self.runtime.take() {
                    // `NodeBuilder::run` itself is async on the control runtime.
                    // A normal nested Runtime drop may block and panic there;
                    // background shutdown cancels its data tasks without doing so.
                    runtime.shutdown_background();
                }
            }
        }

        enum Execution {
            /// Compatibility/local-test path. Production deployments should use
            /// one of the two dedicated data-plane execution modes below.
            CurrentRuntime,

            ThreadPerCore {
                cpu_cores: Vec<core_affinity::CoreId>,
                threads: Vec<std::thread::JoinHandle<()>>,
            },

            WorkStealing {
                runtime: WorkStealingRuntime,
            },
        }

        pub(in crate::node) struct Executor {
            execution: Execution,
        }

        impl Executor {
            pub(in crate::node) fn new(
                mode: ShardRuntime,
                data_threads: usize,
                cpu_cores: Vec<core_affinity::CoreId>,
            ) -> Result<Self> {
                let execution = match mode {
                    ShardRuntime::CurrentRuntime => Execution::CurrentRuntime,
                    ShardRuntime::ThreadPerCore => Execution::ThreadPerCore {
                        cpu_cores,
                        threads: Vec::with_capacity(data_threads),
                    },
                    ShardRuntime::WorkStealing { shards_per_worker } => {
                        tracing::info!(
                            data_threads,
                            shards_per_worker,
                            shards = data_threads.saturating_mul(shards_per_worker),
                            "starting dedicated Tokio work-stealing data runtime"
                        );

                        Execution::WorkStealing {
                            runtime: WorkStealingRuntime::build(data_threads, &cpu_cores)?,
                        }
                    }
                };

                Ok(Self { execution })
            }

            /// Start one logical shard on the configured data-plane executor.
            ///
            /// `control_tasks` is used only by the compatibility CurrentRuntime
            /// path. In both production modes shard execution is completely
            /// separate from the caller/control runtime.
            pub(in crate::node) fn spawn<F>(
                &mut self,
                shard_id: ShardId,
                launch: F,
                control_tasks: &mut JoinSet<()>,
            ) -> Result<()>
            where
                F: FnOnce() -> Result<ShardWorker> + Send + 'static,
            {
                match &mut self.execution {
                    Execution::CurrentRuntime => {
                        let shard = launch()?;
                        control_tasks.spawn_local(ignore(shard.run()));
                    }
                    Execution::WorkStealing { runtime } => {
                        let shard = launch()?;
                        // Dropping a Tokio JoinHandle detaches the task; the
                        // dedicated Runtime remains its owner and shuts it down.
                        drop(runtime.handle().spawn(ignore(shard.run())));
                    }
                    Execution::ThreadPerCore { cpu_cores, threads } => {
                        let core_id = if cpu_cores.is_empty() {
                            None
                        } else {
                            cpu_cores.get(shard_id.index() % cpu_cores.len()).copied()
                        };

                        let handle = std::thread::Builder::new()
                            .name(format!("pb-w-{shard_id}"))
                            .spawn(move || {
                                let rt = tokio::runtime::Builder::new_current_thread()
                                    .enable_all()
                                    .enable_alt_timer()
                                    .build_local(tokio::runtime::LocalOptions::default())
                                    .unwrap_or_else(|err| {
                                        pulsebeam_runtime::fatal!(
                                            "shard {shard_id} cannot build its runtime: {err}"
                                        )
                                    });

                                let realtime = tune_current_data_thread(core_id);
                                metrics::gauge!(
                                    "shard_realtime",
                                    "shard" => shard_id.index().to_string()
                                )
                                .set(f64::from(realtime));

                                #[allow(
                                    clippy::disallowed_methods,
                                    reason = "the shard's dedicated OS thread enters its async runtime exactly once, here"
                                )]
                                rt.block_on(async move {
                                    let shard = launch().unwrap_or_else(|err| {
                                        pulsebeam_runtime::fatal!(
                                            "shard {shard_id} cannot start: {err}"
                                        )
                                    });

                                    // Nothing else competes for this executor,
                                    // so Tokio's cooperative budget is unnecessary.
                                    tokio::task::unconstrained(shard.run()).await;
                                });
                            })
                            .with_context(|| {
                                format!("cannot spawn the thread for shard {shard_id}")
                            })?;

                        threads.push(handle);
                    }
                }

                Ok(())
            }

            /// The caller/control runtime may be deprioritized only after all
            /// physical data threads have been created, otherwise they could
            /// inherit the control thread's scheduling policy.
            pub(in crate::node) fn should_tune_control_thread(&self) -> bool {
                !matches!(self.execution, Execution::CurrentRuntime)
            }

            /// Finish ownership of the data executor after the control tasks are
            /// gone. TPC threads are joined; dropping WorkStealingRuntime starts
            /// non-blocking runtime shutdown and cancels its shard tasks.
            pub(in crate::node) fn finish(self) {
                if let Execution::ThreadPerCore { threads, .. } = self.execution {
                    for handle in threads {
                        let _ = handle.join();
                    }
                }
            }
        }
    }

    #[cfg(feature = "sim")]
    mod imp {
        use super::*;

        pub(in crate::node) struct Executor;

        impl Executor {
            pub(in crate::node) fn new(
                mode: ShardRuntime,
                _data_threads: usize,
                _cpu_cores: Vec<core_affinity::CoreId>,
            ) -> Result<Self> {
                if !matches!(mode, ShardRuntime::CurrentRuntime) {
                    return Err(anyhow::anyhow!(
                        "a simulated node must use `.with_current_runtime()`"
                    ));
                }

                Ok(Self)
            }

            pub(in crate::node) fn spawn<F>(
                &mut self,
                _shard_id: ShardId,
                launch: F,
                control_tasks: &mut JoinSet<()>,
            ) -> Result<()>
            where
                F: FnOnce() -> Result<ShardWorker> + 'static,
            {
                let shard = launch()?;
                control_tasks.spawn_local(ignore(shard.run()));
                Ok(())
            }

            pub(in crate::node) fn should_tune_control_thread(&self) -> bool {
                false
            }

            pub(in crate::node) fn finish(self) {}
        }
    }

    pub(super) use imp::Executor;
}

use platform::bind_tcp_listener;
pub struct NodeBuilder {
    // Data-plane topology
    //
    // `data_threads` is physical executor width. Logical shard count is derived
    // from it and `shard_runtime`.
    data_threads: usize,
    shard_runtime: ShardRuntime,

    // Configuration
    local_addr: Option<SocketAddr>,
    external_addrs: Vec<SocketAddr>,
    advertise_bound_udp: bool,

    // Dependencies (Transport / Logic)
    rng: Option<rand::Rng>,
    udp_mode: UdpMode,

    // Services
    http_api: Option<ListenerSource>,
    internal_metrics: Option<ListenerSource>,

    ebpf: bool,

    /// When `true`, UDP candidates are suppressed so that clients are forced
    /// to use the TCP path. Used in simulation tests that exercise TCP-only
    /// connectivity.
    tcp_only: bool,

    /// How many participants of one room share a shard before the next join
    /// spills to another.
    room_shard_slot: usize,

    /// How a spilled room chooses its next shard.
    room_placement: crate::control::core::RoomPlacement,
}

impl Default for NodeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeBuilder {
    pub fn new() -> Self {
        Self {
            data_threads: 1,
            shard_runtime: ShardRuntime::ThreadPerCore,
            local_addr: None,
            external_addrs: Vec::new(),
            advertise_bound_udp: false,
            rng: None,
            udp_mode: UdpMode::Batch,
            http_api: None,
            internal_metrics: None,
            ebpf: true,
            tcp_only: false,
            room_shard_slot: crate::control::core::DEFAULT_ROOM_SHARD_SLOT,
            room_placement: crate::control::core::RoomPlacement::Hashed,
        }
    }

    /// Place a room's slots round-robin instead of by hash.
    ///
    /// For simulations that must actually cross a shard boundary: hashing picks
    /// independently per slot, so a small room lands co-located often enough
    /// that a plan depending on the split would pass without ever running the
    /// path it claims to cover.
    pub fn round_robin_rooms(mut self) -> Self {
        self.room_placement = crate::control::core::RoomPlacement::RoundRobin;
        self
    }

    /// Spill a room onto another shard after this many participants.
    ///
    /// Lowering it is how a test reaches the cross-shard media path without
    /// needing a room large enough to spill naturally: below the threshold a
    /// room is co-located and its fanout never leaves one shard.
    pub fn room_shard_slot(mut self, participants: usize) -> Self {
        assert!(
            participants > 0,
            "a shard slot must hold at least one participant"
        );
        self.room_shard_slot = participants;
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
        self.shard_runtime = ShardRuntime::ThreadPerCore;
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
        self.shard_runtime = ShardRuntime::WorkStealing { shards_per_worker };
        self
    }

    /// Run shards as local tasks on the caller's runtime.
    ///
    /// This is retained for simulation. It deliberately does not mean
    /// work-stealing: `spawn_local` tasks stay on the local executor thread.
    pub fn with_current_runtime(mut self) -> Self {
        self.shard_runtime = ShardRuntime::CurrentRuntime;
        self
    }

    fn configured_shard_count(&self) -> usize {
        self.shard_runtime.shard_count(self.data_threads)
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

    /// Configure the HTTP Signaling API to bind to the specified address.
    pub fn with_http_api(mut self, addr: SocketAddr) -> Self {
        self.http_api = Some(ListenerSource::Bind(addr));
        self
    }

    /// Configure the HTTP Signaling API to use a pre-bound listener.
    /// Useful for testing with port 0 (ephemeral ports).
    pub fn with_http_api_listener(mut self, listener: TcpListener) -> Self {
        self.http_api = Some(ListenerSource::PreBound(listener));
        self
    }

    /// Configure the Internal Metrics server to bind to the specified address.
    pub fn with_internal_metrics(mut self, addr: SocketAddr) -> Self {
        self.internal_metrics = Some(ListenerSource::Bind(addr));
        self
    }

    /// Configure the Internal Metrics server to use a pre-bound listener.
    pub fn with_internal_metrics_listener(mut self, listener: TcpListener) -> Self {
        self.internal_metrics = Some(ListenerSource::PreBound(listener));
        self
    }

    pub fn without_ebpf(mut self) -> Self {
        self.ebpf = false;
        self
    }

    /// Suppress UDP host candidates so that only the TCP passive candidate is
    /// advertised. Clients that support TCP active will be forced onto TCP.
    /// Useful for integration / simulation tests that exercise the TCP path.
    pub fn tcp_only(mut self) -> Self {
        self.tcp_only = true;
        self
    }

    /// Consumes the builder and runs the node until `shutdown` is cancelled.
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        let data_threads = self.data_threads;
        let shard_runtime = self.shard_runtime;
        let requested_shard_count = self.configured_shard_count();

        // Default to an IPv6-any listener and disable v6-only mode so one socket can serve
        // both IPv6 and IPv4 peers.
        let local_addr = self
            .local_addr
            .unwrap_or_else(|| SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0));
        if self.external_addrs.is_empty() && !self.advertise_bound_udp {
            return Err(anyhow::anyhow!(
                "NodeBuilder requires external addresses; call `.external_addrs(...)` or `.advertise_bound_udp()` for loopback :0"
            ));
        }

        pulsebeam_runtime::clock::ensure_synchronized()
            .context("kernel clock is not synchronized")?;
        let wall_anchor = WallAnchor::try_new(SystemTime::now(), Instant::now())
            .map_err(|error| anyhow::anyhow!("cannot capture node clock anchor: {error:?}"))?;
        let node_health = NodeHealth::new();

        let advertised_addrs = self.external_addrs;

        let mut deduped = Vec::with_capacity(advertised_addrs.len());
        let mut seen = HashSet::with_capacity(advertised_addrs.len());
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
        let primary_external_addr = advertised_addrs.first().copied();

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

        let cpu_cores = get_core_ids().unwrap_or_default();
        if cpu_cores.is_empty() {
            tracing::warn!(
                "no CPU cores detected for thread affinity; dedicated data threads will not be pinned"
            );
        } else {
            tracing::info!(
                count = cpu_cores.len(),
                "detected CPU cores for data-plane affinity"
            );
        }

        let udp_sockets = bind_udp_sockets(
            local_addr,
            primary_external_addr,
            requested_shard_count,
            self.udp_mode,
        )
        .await?;

        debug_assert!(!udp_sockets.is_empty());
        let shard_count = udp_sockets.len();

        if shard_count != requested_shard_count {
            tracing::warn!(
                requested = requested_shard_count,
                running = shard_count,
                "node is running fewer logical shards than configured"
            );
        }

        let steering = if self.ebpf {
            match crate::control::steering::attach(&udp_sockets) {
                Ok(steering) => {
                    metrics::gauge!("ebpf_steering_attached").set(1.0);
                    tracing::info!("attached eBPF UDP steering");
                    Some(steering)
                }
                Err(err) => {
                    metrics::gauge!("ebpf_steering_attached").set(0.0);
                    tracing::warn!(
                        "eBPF UDP steering disabled; using userspace bootstrap forwarding: {:?}",
                        err
                    );
                    None
                }
            }
        } else {
            metrics::gauge!("ebpf_steering_attached").set(0.0);
            tracing::info!("eBPF UDP steering disabled by configuration");
            None
        };

        let tcp_listener = bind_tcp_listener(local_addr)
            .await
            .context("binding tcp listener")?;
        let bound_tcp_addr = tcp_listener.local_addr()?;
        tracing::info!(
            local_addr = ?bound_tcp_addr,
            data_threads,
            shards = shard_count,
            runtime = ?shard_runtime,
            "RTC listeners ready"
        );
        let tcp_local_addr = primary_external_addr.unwrap_or(bound_tcp_addr);

        let tcp_sockets: Vec<net::tcp::TcpTransport> = (0..shard_count)
            .map(|_| net::tcp::TcpTransport::new(tcp_local_addr))
            .collect();

        let mut candidates = sockets_to_candidates(&udp_sockets, &advertised_addrs);
        if self.tcp_only {
            candidates.clear();
        }
        if !tcp_sockets.is_empty() {
            let tcp_candidate_addrs = if advertised_addrs.is_empty() {
                vec![tcp_local_addr]
            } else {
                advertised_addrs.clone()
            };

            for addr in tcp_candidate_addrs {
                let sdp = format!(
                    "candidate:1 1 TCP 2130706431 {} {} typ host tcptype passive",
                    addr.ip(),
                    addr.port()
                );
                let Some(candidate) = pulsebeam_rtc::IceCandidate::new(sdp) else {
                    pulsebeam_runtime::fatal!("cannot advertise a TCP candidate for {addr}");
                };
                candidates.push(candidate);
            }
        }

        // This is the only owner of data-plane execution. The caller runtime
        // remains the control runtime for controller/API/metrics tasks.
        let mut shard_executor =
            shard_executor::Executor::new(shard_runtime, data_threads, cpu_cores.clone())?;

        let (shard_event_tx, shard_event_rx) =
            mailbox::new(crate::shard::worker::SHARD_EVENT_CAPACITY);
        let mut frame_txs = Vec::with_capacity(shard_count);
        let mut frame_rxs = Vec::with_capacity(shard_count);
        for _ in 0..shard_count {
            let (tx, rx) = mailbox::new(crate::shard::worker::SHARD_FRAME_CAPACITY);
            frame_txs.push(tx);
            frame_rxs.push(rx);
        }

        let mut rng = self.rng.ok_or_else(|| {
            anyhow::anyhow!(
                "NodeBuilder requires an RNG; call `.rng(...)` when constructing the node"
            )
        })?;

        let controller_rng = rand::Rng::seed_from_u64(rng.next_u64());

        let mut shard_contexts = Vec::with_capacity(shard_count);

        // Only built when something will scrape it, so a node without the
        // internal server never pays for a snapshot it would throw away.
        let stats_lane = internal_ctx.is_some().then(|| {
            mailbox::new::<Box<crate::shard::recorder::ShardStatsReport>>(
                crate::shard::worker::STATS_CAPACITY.saturating_mul(shard_count.max(1)),
            )
        });
        let (stats_tx, stats_rx) = match stats_lane {
            Some((tx, rx)) => (Some(tx), Some(rx)),
            None => (None, None),
        };

        let mut view_writers = Vec::with_capacity(shard_count);

        for (shard_idx, ((udp_sock, tcp_sock), frame_rx)) in udp_sockets
            .into_iter()
            .zip(tcp_sockets.into_iter())
            .zip(frame_rxs)
            .enumerate()
        {
            let shard_id = ShardId::new(shard_idx);
            let (update_writer, update_reader) = crate::shard_update::new_shard_update(shard_id);
            view_writers.push(update_writer);
            let (shard_command_tx, shard_command_rx) =
                mailbox::new(crate::shard::worker::SHARD_COMMAND_CAPACITY);
            let shard_event_tx = shard_event_tx.clone();
            let frame_txs = frame_txs.clone();
            #[allow(
                clippy::disallowed_types,
                reason = "Arc<ShardMetrics>, handed over once before any shard runs, see module note"
            )]
            let occupancy = Arc::new(ShardMetrics::new());
            let worker_occupancy = occupancy.clone();
            let shard_stats_tx = stats_tx.clone();

            let launch = move || -> Result<ShardWorker> {
                let udp_sock = udp_sock.into_unified_socket()?;
                Ok(ShardWorker::new(
                    shard_id,
                    udp_sock,
                    tcp_sock,
                    shard_command_rx,
                    update_reader,
                    shard_event_tx,
                    frame_rx,
                    frame_txs,
                    worker_occupancy,
                    shard_stats_tx,
                    wall_anchor,
                ))
            };

            shard_executor.spawn(shard_id, launch, &mut control_tasks)?;

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
                wall_anchor,
                node_health.clone(),
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
            candidates.into(),
            tcp_listener,
            self.room_shard_slot,
            self.room_placement,
            view_writers,
        );
        controller.set_steering(steering);
        // intentionally small so backpressure is applied early
        // with 62.5 ms pacing rate, at most we get 1s latency here.
        let (controller_command_tx, controller_command_rx) = mailbox::new(16);

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

            let cors = CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([
                    hyper::Method::GET,
                    hyper::Method::POST,
                    hyper::Method::PATCH,
                    hyper::Method::PUT,
                    hyper::Method::DELETE,
                    hyper::Method::OPTIONS,
                ])
                .allow_headers([
                    hyper::header::AUTHORIZATION,
                    hyper::header::CONTENT_TYPE,
                    hyper::header::CONTENT_ENCODING,
                    hyper::header::IF_MATCH,
                    hyper::header::ACCEPT,
                ])
                .expose_headers([hyper::header::LOCATION, hyper::header::ETAG])
                .max_age(Duration::from_secs(86400));

            let router = api::router(controller_command_tx, api_cfg)
                .layer(CompressionLayer::new().zstd(true))
                .layer(RequestDecompressionLayer::new().zstd(true).gzip(true))
                .layer(cors);
            // https://github.com/tokio-rs/axum/issues/3112
            // missing graceful shutdown will cause task leaks
            let api_server = axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.child_token().cancelled_owned());

            control_tasks.spawn(async move {
                if let Err(e) = api_server.await {
                    tracing::error!("http server error: {e}");
                }
            });
        }

        // Wait for shutdown
        tokio::select! {
            _ = control_tasks.join_all() => {}
            _ = shutdown.cancelled() => {
                tracing::info!("node received shutdown");
            }
        }

        // At this point the control task set has been consumed/dropped by the
        // select above. Finish the independently-owned data executor afterwards.
        shard_executor.finish();

        Ok(())
    }
}

pub struct NodeContext {
    pub rng: pulsebeam_runtime::rand::Rng,
}

async fn bind_udp_sockets(
    local_addr: SocketAddr,
    advertised_addr: Option<SocketAddr>,
    shards: usize,
    mode: UdpMode,
) -> Result<Vec<net::BoundUdpSocket>> {
    let mut sockets = Vec::with_capacity(shards);

    for shard_index in 0..shards {
        let shard_index = u16::try_from(shard_index).unwrap_or(u16::MAX);
        let socket =
            match net::bind_udp_socket(local_addr, mode, advertised_addr, shard_index).await {
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
    sockets: &[net::BoundUdpSocket],
    advertised_addrs: &[SocketAddr],
) -> Vec<pulsebeam_rtc::IceCandidate> {
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
        let sdp = format!(
            "candidate:1 1 UDP 2130706431 {} {} typ host",
            addr.ip(),
            addr.port()
        );
        let Some(candidate) = pulsebeam_rtc::IceCandidate::new(sdp) else {
            pulsebeam_runtime::fatal!("cannot advertise a UDP candidate for {addr}");
        };
        candidates.push(candidate);
    }

    candidates
}

pub async fn ignore<T>(fut: impl Future<Output = T>) {
    let _ = fut.await;
}

pub fn tune_current_control_thread() {
    #[cfg(unix)]
    {
        use thread_priority::{
            NormalThreadSchedulePolicy, ThreadPriority, ThreadSchedulePolicy, thread_native_id,
        };

        let current_thread_id = thread_native_id();

        #[cfg(target_os = "linux")]
        let policy = ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Batch);

        #[cfg(not(target_os = "linux"))]
        let policy = ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Other);

        let result = thread_priority::set_thread_priority_and_policy(
            current_thread_id,
            ThreadPriority::Min,
            policy,
        );

        if let Err(e) = result {
            tracing::warn!("Failed to lower Control Thread priority: {:?}", e);
        } else {
            tracing::info!("Control thread tuned: Minimum Priority");
        }
    }

    #[cfg(not(unix))]
    {
        // Fallback for Windows or other non-Unix targets if necessary
        tracing::debug!("Thread tuning is a no-op on non-Unix platforms.");
    }
}

pub fn tune_current_data_thread(core_id: Option<core_affinity::CoreId>) -> bool {
    #[cfg(target_os = "linux")]
    {
        use rustix::thread::{current_timer_slack, set_current_timer_slack};
        use std::num::NonZero;
        use thread_priority::{
            RealtimeThreadSchedulePolicy, ScheduleParams, ThreadPriority, ThreadSchedulePolicy,
            thread_native_id,
        };

        let current_thread_id = thread_native_id();
        // small enough priority to avoid inversion with IRQ
        let policy = ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo);
        let priority = ThreadPriority::from_posix(ScheduleParams { sched_priority: 10 });
        let realtime = if let Err(e) =
            thread_priority::set_thread_priority_and_policy(current_thread_id, priority, policy)
        {
            tracing::warn!(
                "Failed to set Data Thread to SCHED_FIFO at priority 10 (requires CAP_SYS_NICE): {:?}",
                e
            );
            false
        } else {
            tracing::info!("Data thread successfully elevated to SCHED_FIFO (Priority 10)");
            true
        };

        // attempt to get closer to SCHED_FIFO without CAP_SYS_ADMIN
        let slack_value = NonZero::new(1);
        if let Err(err) = set_current_timer_slack(slack_value) {
            tracing::warn!(?err, "Failed to set timer slack on data thread");
        } else {
            let current_slack = current_timer_slack().unwrap_or(0);
            tracing::info!(current_slack, "Data thread timer slack successfully tuned");
        }

        // https://developers.redhat.com/articles/2025/03/26/rhel-real-time-cpu-throttling-and-risks#is_that_a_bug_
        // set higher priority first before pinning to avoid a potential lockup
        if let Some(core) = core_id {
            if core_affinity::set_for_current(core) {
                tracing::info!(?core, "Data thread pinned to CPU core");
            } else {
                tracing::warn!(?core, "Failed to pin Data thread to CPU core");
            }
        }
        realtime
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = core_id;
        false
    }
}

mod internal {
    use super::*;
    use anyhow::Result;
    use axum::{
        Router,
        extract::{Query, State},
        response::{Html, IntoResponse},
        routing::get,
    };
    use hyper::{
        StatusCode,
        header::{CONTENT_DISPOSITION, CONTENT_TYPE},
    };
    use metrics::{Unit, describe_gauge, gauge};
    use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
    use pprof::ProfilerGuard;
    use pprof::protos::Message;
    use serde::Deserialize;
    use tokio::runtime::Handle;
    use tokio::sync::oneshot;

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
            wall_anchor: WallAnchor,
            node_health: NodeHealth,
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
            let (scrape_tx, scrape_rx) = mailbox::new(1);
            let aggregator_join = tokio::spawn(crate::control::stats_aggregator::run(
                shard_count,
                stats_rx,
                scrape_rx,
                shutdown.child_token(),
            ));

            let router = {
                let prometheus = self.prometheus.clone();
                Router::new()
                    .route("/debug/pprof/profile", get(pprof_profile))
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
                    .with_state(node_health.clone())
            };
            let rt_monitor_join = tokio::spawn(rt_background_monitor(
                self.prometheus,
                wall_anchor,
                node_health,
            ));

            tracing::info!(
                "internal metrics listening on {:?}",
                self.listener.local_addr().ok()
            );

            tokio::select! {
                res = axum::serve(self.listener, router) => {
                    if let Err(e) = res {
                        tracing::error!("internal http server error: {e}");
                    }
                }
                _ = aggregator_join => {}
                _ = rt_monitor_join => {}
                _ = shutdown.cancelled() => {
                    tracing::info!("internal http server shutting down");
                }
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

    async fn rt_background_monitor(
        prometheus: PrometheusHandle,
        wall_anchor: WallAnchor,
        node_health: NodeHealth,
    ) {
        // This task is spawned on the caller/control runtime, and these metric
        // names have always described that runtime. Data-runtime metrics, if
        // added, should use a separate prefix rather than changing this meaning.
        let metrics = Handle::current().metrics();

        describe_gauge!(
            "tokio_active_tasks",
            Unit::Count,
            "Current number of active tasks"
        );
        describe_gauge!(
            "tokio_injection_queue_depth",
            Unit::Count,
            "Current depth of the global injection queue"
        );
        describe_gauge!(
            "tokio_worker_count",
            Unit::Count,
            "Total number of worker threads"
        );
        describe_gauge!(
            "tokio_blocking_threads",
            Unit::Count,
            "Current number of blocking threads"
        );
        describe_gauge!(
            "tokio_idle_blocking_threads",
            Unit::Count,
            "Current number of idle blocking threads"
        );
        describe_gauge!(
            "tokio_spawned_tasks_total",
            Unit::Count,
            "Total number of tasks spawned since runtime start"
        );

        // Worker specific
        describe_gauge!(
            "tokio_worker_park_count",
            Unit::Count,
            "Total number of times this worker parked"
        );
        describe_gauge!(
            "tokio_worker_steal_count",
            Unit::Count,
            "Total number of times this worker stole tasks"
        );
        describe_gauge!(
            "tokio_worker_poll_count",
            Unit::Count,
            "Total number of times this worker polled"
        );
        describe_gauge!(
            "tokio_worker_busy_duration_seconds",
            Unit::Seconds,
            "Total duration this worker has been busy"
        );
        describe_gauge!(
            "tokio_worker_local_queue_depth",
            Unit::Count,
            "Current depth of this worker's local queue"
        );
        describe_gauge!(
            "tokio_worker_mean_poll_time_us",
            Unit::Microseconds,
            "Mean poll time for this worker"
        );

        let mut interval = tokio::time::interval(Duration::from_secs(5));

        loop {
            interval.tick().await;

            node_health.observe_clock(wall_anchor, SystemTime::now(), Instant::now());

            // Current State (Gauges)
            gauge!("tokio_num_alive_tasks").set(metrics.num_alive_tasks() as f64);
            gauge!("tokio_injection_queue_depth",).set(metrics.global_queue_depth() as f64);
            gauge!("tokio_blocking_queue_depth",).set(metrics.blocking_queue_depth() as f64);
            gauge!("tokio_worker_count").set(metrics.num_workers() as f64);
            gauge!("tokio_blocking_threads").set(metrics.num_blocking_threads() as f64);
            gauge!("tokio_idle_blocking_threads").set(metrics.num_idle_blocking_threads() as f64);

            // Cumulative Totals (Technically Counters, but we emit as Gauges because
            // Tokio gives us the absolute total, not the delta).
            gauge!("tokio_spawned_tasks_total").set(metrics.spawned_tasks_count() as f64);
            gauge!("tokio_remote_schedule_total").set(metrics.remote_schedule_count() as f64);
            gauge!("tokio_budget_forced_yield_total")
                .set(metrics.budget_forced_yield_count() as f64);

            gauge!("tokio_io_driver_ready_total").set(metrics.io_driver_ready_count() as f64);

            for i in 0..metrics.num_workers() {
                let labels = [("worker", i.to_string())];

                gauge!("tokio_worker_local_queue_depth", &labels,)
                    .set(metrics.worker_local_queue_depth(i) as f64);

                gauge!("tokio_worker_park_count", &labels).set(metrics.worker_park_count(i) as f64);

                gauge!("tokio_worker_noop_count", &labels).set(metrics.worker_noop_count(i) as f64);

                gauge!("tokio_worker_steal_count", &labels)
                    .set(metrics.worker_steal_count(i) as f64);

                gauge!("tokio_worker_poll_count", &labels).set(metrics.worker_poll_count(i) as f64);

                gauge!("tokio_worker_overflow_count", &labels)
                    .set(metrics.worker_overflow_count(i) as f64);

                gauge!("tokio_worker_busy_duration_seconds", &labels)
                    .set(metrics.worker_total_busy_duration(i).as_secs_f64());

                gauge!("tokio_worker_mean_poll_time_us", &labels)
                    .set(metrics.worker_mean_poll_time(i).as_micros() as f64);
            }

            prometheus.run_upkeep();
        }
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

        tokio::time::sleep(Duration::from_secs(params.seconds)).await;

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

    async fn healthcheck(State(node_health): State<NodeHealth>) -> impl IntoResponse {
        if node_health.is_healthy() {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
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
        assert_eq!(builder.configured_shard_count(), 1);
        assert!(builder.local_addr.is_none());
        assert!(builder.external_addrs.is_empty());
        assert!(builder.http_api.is_none(), "no API is exposed unless asked");
        assert!(builder.internal_metrics.is_none());
        assert!(!builder.tcp_only, "UDP candidates are offered by default");
        assert!(matches!(builder.udp_mode, UdpMode::Batch));
        assert!(builder.ebpf);
        assert_eq!(builder.shard_runtime, ShardRuntime::ThreadPerCore);
        assert!(matches!(
            builder.room_placement,
            crate::control::core::RoomPlacement::Hashed
        ));
        assert_eq!(
            builder.room_shard_slot,
            crate::control::core::DEFAULT_ROOM_SHARD_SLOT
        );
    }

    #[test]
    fn work_stealing_overshards_each_scheduler_worker() {
        let builder = NodeBuilder::new().data_threads(4).work_stealing(16);

        assert_eq!(builder.data_threads, 4);
        assert_eq!(builder.configured_shard_count(), 64);
        assert_eq!(
            builder.shard_runtime,
            ShardRuntime::WorkStealing {
                shards_per_worker: 16,
            }
        );
    }

    #[test]
    fn thread_per_core_keeps_one_shard_per_data_thread() {
        let builder = NodeBuilder::new().data_threads(8).thread_per_core();

        assert_eq!(builder.data_threads, 8);
        assert_eq!(builder.configured_shard_count(), 8);
    }

    #[test]
    fn the_workers_alias_still_sets_data_threads() {
        let builder = NodeBuilder::new().workers(6);

        assert_eq!(builder.data_threads, 6);
        assert_eq!(builder.configured_shard_count(), 6);
    }

    #[test]
    fn the_builder_records_what_it_was_asked_for() {
        let addr: SocketAddr = "127.0.0.1:7070".parse().unwrap();
        let builder = NodeBuilder::new()
            .data_threads(4)
            .local_addr(addr)
            .external_addrs(vec![addr])
            .room_shard_slot(9)
            .round_robin_rooms()
            .tcp_only()
            .without_ebpf();

        assert_eq!(builder.data_threads, 4);
        assert_eq!(builder.local_addr, Some(addr));
        assert_eq!(builder.external_addrs, vec![addr]);
        assert_eq!(builder.room_shard_slot, 9);
        assert!(builder.tcp_only);
        assert!(!builder.ebpf);
        assert!(matches!(
            builder.room_placement,
            crate::control::core::RoomPlacement::RoundRobin
        ));
    }

    #[test]
    fn clock_health_accepts_the_limit_and_rejects_excess_drift() {
        let mono = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let anchor = WallAnchor::try_new(wall, mono).unwrap();
        let health = NodeHealth::new();
        let sample_mono = mono + Duration::from_secs(3);
        let projected = anchor.project(sample_mono).unwrap();

        let at_limit = projected.checked_add(CLOCK_DRIFT_HEALTH_LIMIT).unwrap();
        health.observe_clock(anchor, at_limit, sample_mono);
        assert!(health.is_healthy());

        let beyond_limit = at_limit.checked_add(Duration::from_nanos(1)).unwrap();
        health.observe_clock(anchor, beyond_limit, sample_mono);
        assert!(!health.is_healthy());
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
        let _ = NodeBuilder::new().room_shard_slot(0);
    }

    /// `Default` and `new` must not drift: one is what most callers get and the other is what the
    /// tests above pin down.
    #[test]
    fn default_is_new() {
        let (a, b) = (NodeBuilder::default(), NodeBuilder::new());
        assert_eq!(a.data_threads, b.data_threads);
        assert_eq!(a.shard_runtime, b.shard_runtime);
        assert_eq!(a.room_shard_slot, b.room_shard_slot);
        assert_eq!(a.tcp_only, b.tcp_only);
    }
}

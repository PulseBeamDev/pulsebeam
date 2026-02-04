use crate::{api, controller, gateway};
use anyhow::{Context, Result};
use axum::http::HeaderName;
use pulsebeam_core::net::TcpListener;
use pulsebeam_runtime::actor::RunnerConfig;
use pulsebeam_runtime::net::UdpMode;
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::{actor, net, rand};
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::decompression::RequestDecompressionLayer;

/// A pair of reader/writer for a transport connection.
pub type TransportPair = (net::UnifiedSocketReader, net::UnifiedSocketWriter);

/// Defines how a service listener is acquired.
enum ListenerSource {
    /// Bind to this address internally.
    Bind(SocketAddr),
    /// Use this pre-bound listener.
    PreBound(TcpListener),
}

pub struct NodeBuilder {
    // Configuration
    workers: usize,
    local_addr: Option<SocketAddr>,
    external_addr: Option<SocketAddr>,

    // Dependencies (Transport / Logic)
    rng: Option<rand::Rng>,
    udp_mode: UdpMode,
    gateway_handle: Option<gateway::GatewayHandle>,

    // Services
    http_api: Option<ListenerSource>,
    internal_metrics: Option<ListenerSource>,
}

impl Default for NodeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeBuilder {
    pub fn new() -> Self {
        Self {
            workers: 1,
            local_addr: None,
            external_addr: None,
            rng: None,
            udp_mode: UdpMode::Batch,
            gateway_handle: None,
            http_api: None,
            internal_metrics: None,
        }
    }

    /// Set the number of UDP workers (default: 1).
    pub fn workers(mut self, workers: usize) -> Self {
        self.workers = workers;
        self
    }

    /// Set the local bind address for UDP/TCP transports.
    /// Ignored if transports are injected manually.
    pub fn local_addr(mut self, addr: SocketAddr) -> Self {
        self.local_addr = Some(addr);
        self
    }

    /// Set the external address advertised to peers.
    pub fn external_addr(mut self, addr: SocketAddr) -> Self {
        self.external_addr = Some(addr);
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

    /// Inject an existing Gateway handle.
    pub fn with_gateway(mut self, gateway: gateway::GatewayHandle) -> Self {
        self.gateway_handle = Some(gateway);
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

    /// Consumes the builder and runs the node until `shutdown` is cancelled.
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        let workers_count = self.workers;
        // Default to binding 0.0.0.0:0 if no address is provided but binding is required
        let local_addr = self
            .local_addr
            .unwrap_or_else(|| SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0));
        let external_addr = self.external_addr;

        let (udp_readers, udp_writers) =
            bind_udp_workers(local_addr, external_addr, workers_count, self.udp_mode).await?;

        let (tcp_reader, tcp_writer) =
            net::bind(local_addr, net::Transport::Tcp, external_addr).await?;

        let mut all_readers = udp_readers;
        all_readers.push(tcp_reader);

        let rng = self.rng.unwrap_or_else(rand::Rng::from_os_rng);

        let mut join_set = JoinSet::new();

        let gateway_handle = if let Some(handle) = self.gateway_handle {
            handle
        } else {
            let (handle, task) = actor::prepare(
                gateway::GatewayActor::new(all_readers),
                RunnerConfig::default(),
            );
            join_set.spawn(ignore(task));
            handle
        };

        let node_ctx = NodeContext {
            rng,
            gateway: gateway_handle.clone(),
            udp_sockets: udp_writers,
            tcp_socket: tcp_writer,
            udp_egress_counter: Arc::new(AtomicUsize::new(0)),
        };

        let controller_actor =
            controller::ControllerActor::new(node_ctx, Arc::new("root".to_string()));
        let (controller_handle, controller_task) =
            actor::prepare(controller_actor, RunnerConfig::default());
        join_set.spawn(ignore(controller_task));

        if let Some(source) = self.http_api {
            // Resolve listener
            let listener = match source {
                ListenerSource::Bind(addr) => {
                    TcpListener::bind(addr).await.context("binding http api")?
                }
                ListenerSource::PreBound(l) => l,
            };

            let local_addr = listener.local_addr().ok();
            tracing::debug!("signaling api listening on {:?}", local_addr);

            let api_cfg = api::ApiConfig {
                base_path: "/api/v1".to_string(),
                // Best effort to guess host if bound randomly
                default_host: local_addr
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "0.0.0.0:0".to_string()),
            };

            let cors = CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers([
                    hyper::header::AUTHORIZATION,
                    hyper::header::CONTENT_TYPE,
                    hyper::header::CONTENT_ENCODING,
                    hyper::header::IF_MATCH,
                ])
                .expose_headers([
                    hyper::header::LOCATION,
                    hyper::header::ETAG,
                    HeaderName::from_static(api::HeaderExt::ParticipantId.as_str()),
                ])
                .max_age(Duration::from_secs(86400));

            let router = api::router(controller_handle, api_cfg)
                .layer(cors)
                .layer(CompressionLayer::new().zstd(true))
                .layer(RequestDecompressionLayer::new().zstd(true).gzip(true));
            let http_shutdown = shutdown.clone();

            join_set.spawn(async move {
                tokio::select! {
                    res = axum::serve(listener, router) => {
                        if let Err(e) = res {
                            tracing::error!("http server error: {e}");
                        }
                    }
                    _ = http_shutdown.cancelled() => {
                        tracing::info!("http server shutting down");
                    }
                }
            });
        }

        if let Some(source) = self.internal_metrics {
            let listener = match source {
                ListenerSource::Bind(addr) => TcpListener::bind(addr)
                    .await
                    .context("binding internal metrics")?,
                ListenerSource::PreBound(l) => l,
            };

            join_set.spawn(ignore(internal::serve_internal_http(
                listener,
                shutdown.child_token(),
            )));
        }

        // Wait for shutdown
        tokio::select! {
            _ = join_set.join_all() => {}
            _ = shutdown.cancelled() => {
                tracing::info!("node received shutdown");
            }
        }

        Ok(())
    }
}

#[derive(Clone)]
pub struct NodeContext {
    pub rng: pulsebeam_runtime::rand::Rng,
    pub gateway: gateway::GatewayHandle,
    pub udp_sockets: Vec<net::UnifiedSocketWriter>,
    pub tcp_socket: net::UnifiedSocketWriter,
    udp_egress_counter: Arc<AtomicUsize>,
}

impl NodeContext {
    pub fn allocate_udp_egress(&self) -> net::UnifiedSocketWriter {
        if self.udp_sockets.is_empty() {
            // If no UDP sockets exist, fallback to TCP to prevent panic/div-by-zero
            return self.tcp_socket.clone();
        }
        let seq = self
            .udp_egress_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.udp_sockets[seq % self.udp_sockets.len()].clone()
    }

    pub fn allocate_tcp_egress(&self) -> net::UnifiedSocketWriter {
        self.tcp_socket.clone()
    }
}

async fn bind_udp_workers(
    local_addr: SocketAddr,
    external_addr: Option<SocketAddr>,
    workers: usize,
    mode: UdpMode,
) -> Result<(Vec<net::UnifiedSocketReader>, Vec<net::UnifiedSocketWriter>)> {
    let mut readers = Vec::with_capacity(workers);
    let mut writers = Vec::with_capacity(workers);

    for _ in 0..workers {
        let (reader, writer) = match net::bind(local_addr, net::Transport::Udp(mode), external_addr)
            .await
        {
            Ok(s) => s,
            Err(e) if writers.is_empty() => {
                return Err(anyhow::Error::new(e).context("failed to bind first udp socket"));
            }
            Err(e) => {
                tracing::warn!(
                    "SO_REUSEPORT not supported or failed after first bind, proceeding with {} workers: {}",
                    writers.len(),
                    e
                );
                break;
            }
        };
        readers.push(reader);
        writers.push(writer);
    }
    Ok((readers, writers))
}

fn unzip_transports(
    pairs: Vec<TransportPair>,
) -> (Vec<net::UnifiedSocketReader>, Vec<net::UnifiedSocketWriter>) {
    pairs.into_iter().unzip()
}

pub async fn ignore<T>(fut: impl Future<Output = T>) {
    let _ = fut.await;
}

mod internal {
    use super::*;
    use anyhow::Result;
    use axum::{
        Router,
        extract::Query,
        response::{Html, IntoResponse, Response},
        routing::get,
    };
    use hyper::{
        StatusCode,
        header::{CONTENT_DISPOSITION, CONTENT_TYPE},
    };
    use metrics::{Unit, describe_gauge, gauge};
    use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
    use pprof::ProfilerGuard;
    use pprof::protos::Message;
    use pulsebeam_runtime::actor::Actor;
    use serde::Deserialize;
    use tokio::runtime::Handle;

    use crate::{controller, gateway, participant, room};

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

    pub async fn serve_internal_http(
        listener: TcpListener,
        shutdown: CancellationToken,
    ) -> Result<()> {
        let addr = listener.local_addr().ok();

        // Try to install the Prometheus recorder.
        // In simulation or test environments running multiple nodes in one process,
        // this might fail if already installed. We proceed gracefully.
        let prometheus_handle = PrometheusBuilder::new().install_recorder()?;

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

        let router_prometheus = prometheus_handle.clone();
        let router = Router::new()
            .route("/debug/pprof/profile", get(pprof_profile))
            .route("/debug/pprof/allocs", get(heap_profile))
            .route("/healthz", get(healthcheck))
            .route("/", get(async move || Html(INDEX_HTML)))
            .route("/metrics", get(async move || router_prometheus.render()));
        let rt_monitor_join = tokio::spawn(rt_background_monitor(prometheus_handle));

        tracing::info!("internal metrics listening on {:?}", addr);

        // Background tasks
        // let runtime_metrics_join = tokio::spawn(
        //     tokio_metrics::RuntimeMetricsReporterBuilder::default()
        //         .with_interval(Duration::from_secs(5))
        //         .describe_and_run(),
        // );
        // let actor_monitor_join = if let Some(handle) = prometheus_handle {
        //     tokio::spawn(actor_background_monitor(handle))
        // } else {
        //     // Spawn a dummy task if we can't monitor
        //     tokio::spawn(async {})
        // };

        tokio::select! {
            res = axum::serve(listener, router) => {
                if let Err(e) = res {
                    tracing::error!("internal http server error: {e}");
                }
            }
            _ = rt_monitor_join => {}
            // _ = runtime_metrics_join => {}
            // _ = actor_monitor_join => {}
            _ = shutdown.cancelled() => {
                tracing::info!("internal http server shutting down");
            }
        }

        Ok(())
    }

    async fn rt_background_monitor(prometheus_handle: PrometheusHandle) {
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

            // Keep memory usage and CPU usage bounded per prometheus interval.
            prometheus_handle.run_upkeep();
        }
    }

    async fn actor_background_monitor(prometheus_handle: PrometheusHandle) {
        let mut monitors = [
            ("gateway", gateway::GatewayActor::monitor().intervals()),
            (
                "gateway_worker",
                gateway::GatewayWorkerActor::monitor().intervals(),
            ),
            (
                "controller",
                controller::ControllerActor::monitor().intervals(),
            ),
            ("room", room::RoomActor::monitor().intervals()),
            (
                "participant",
                participant::ParticipantActor::monitor().intervals(),
            ),
        ];

        let mut interval = tokio::time::interval(Duration::from_secs(5));

        loop {
            interval.tick().await;

            // Keep memory usage and CPU usage bounded per prometheus interval.
            prometheus_handle.run_upkeep();

            for (actor_name, monitor) in &mut monitors {
                let Some(snapshot) = monitor.next() else {
                    continue;
                };

                let labels = [("actor", *actor_name)];

                metrics::gauge!("actor_long_delay_ratio", &labels).set(snapshot.long_delay_ratio());
                metrics::gauge!("actor_slow_poll_ratio", &labels).set(snapshot.slow_poll_ratio());
                metrics::gauge!("actor_mean_first_poll_delay_us", &labels)
                    .set(snapshot.mean_first_poll_delay().as_micros() as f64);
                metrics::gauge!("actor_mean_idle_duration_us", &labels)
                    .set(snapshot.mean_idle_duration().as_micros() as f64);
                metrics::gauge!("actor_mean_scheduled_duration_us", &labels)
                    .set(snapshot.mean_scheduled_duration().as_micros() as f64);
                metrics::gauge!("actor_mean_poll_duration_us", &labels)
                    .set(snapshot.mean_poll_duration().as_micros() as f64);
                metrics::gauge!("actor_mean_fast_poll_duration_us", &labels)
                    .set(snapshot.mean_fast_poll_duration().as_micros() as f64);
                metrics::gauge!("actor_mean_slow_poll_duration_us", &labels)
                    .set(snapshot.mean_slow_poll_duration().as_micros() as f64);
                metrics::gauge!("actor_mean_short_delay_duration_us", &labels)
                    .set(snapshot.mean_short_delay_duration().as_micros() as f64);
                metrics::gauge!("actor_mean_long_delay_duration_us", &labels)
                    .set(snapshot.mean_long_delay_duration().as_micros() as f64);
            }
        }
    }

    pub async fn heap_profile(
        Query(params): Query<ProfileParams>,
    ) -> Result<Response, (StatusCode, String)> {
        // Safe access to jemalloc control
        let mut prof_ctl = match jemalloc_pprof::PROF_CTL.as_ref() {
            Some(ctl) => ctl.lock().await,
            None => {
                return Err((
                    StatusCode::NOT_IMPLEMENTED,
                    "Jemalloc not enabled or configured".to_string(),
                ));
            }
        };

        require_profiling_activated(&prof_ctl)?;

        let resp = if params.flamegraph {
            let svg = prof_ctl
                .dump_flamegraph()
                .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

            (
                StatusCode::OK,
                [
                    (CONTENT_TYPE, "image/svg+xml"),
                    (CONTENT_DISPOSITION, "attachment; filename=allocs.svg"),
                ],
                svg,
            )
                .into_response()
        } else {
            let pprof = prof_ctl
                .dump_pprof()
                .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

            (
                StatusCode::OK,
                [
                    (CONTENT_TYPE, "application/octet-stream"),
                    (CONTENT_DISPOSITION, "attachment; filename=allocs.pprof"),
                ],
                pprof,
            )
                .into_response()
        };
        Ok(resp)
    }

    fn require_profiling_activated(
        prof_ctl: &jemalloc_pprof::JemallocProfCtl,
    ) -> Result<(), (StatusCode, String)> {
        if prof_ctl.activated() {
            Ok(())
        } else {
            Err((StatusCode::FORBIDDEN, "heap profiling not activated".into()))
        }
    }

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

    async fn healthcheck() -> impl IntoResponse {
        StatusCode::OK
    }
}

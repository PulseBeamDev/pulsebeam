use crate::shard::{ShardMessageSet, ShardTask};
use crate::{api, controller, gateway, shard};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::{actor, net, rand};
use std::sync::atomic::AtomicUsize;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};

#[derive(Clone)]
pub struct NodeContext {
    pub rng: pulsebeam_runtime::rand::Rng,
    pub gateway: gateway::GatewayHandle,
    pub sockets: Vec<Arc<net::UnifiedSocket>>,
    pub shards: Vec<ActorHandle<ShardMessageSet>>,
    egress_counter: Arc<AtomicUsize>,
}

impl NodeContext {
    pub fn single(socket: net::UnifiedSocket) -> Self {
        let socket = Arc::new(socket);

        let (gw, _) = actor::spawn_default(gateway::GatewayActor::new(vec![socket.clone()]));

        Self {
            rng: rand::Rng::from_os_rng(),
            gateway: gw,
            shards: vec![],
            sockets: vec![socket.clone()],
            egress_counter: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn allocate_egress(&self) -> Arc<net::UnifiedSocket> {
        let seq = self
            .egress_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.sockets.get(seq % self.sockets.len()).unwrap().clone()
    }
}

pub async fn run(
    shutdown: CancellationToken,
    workers: usize,
    external_addr: SocketAddr,
    local_addr: SocketAddr,
    http_addr: SocketAddr,
    internal_http_addr: SocketAddr,
) -> anyhow::Result<()> {
    let mut sockets: Vec<Arc<net::UnifiedSocket>> = Vec::new();
    for _ in 0..workers {
        let socket =
            match net::UnifiedSocket::bind(local_addr, net::Transport::Udp, Some(external_addr))
                .await
            {
                Ok(socket) => socket,
                Err(err) if sockets.is_empty() => {
                    return Err(anyhow::Error::new(err).context("failed to bind udp"));
                }
                Err(err) => {
                    tracing::warn!("SO_REUSEPORT is not supported, fallback to 1 socket: {err}");
                    break;
                }
            };

        let socket = Arc::new(socket);
        sockets.push(socket);
    }

    let cors = CorsLayer::very_permissive()
        .allow_origin(AllowOrigin::mirror_request())
        .expose_headers([hyper::header::LOCATION])
        .max_age(Duration::from_secs(86400));

    let mut join_set = FuturesUnordered::new();
    let (gateway, gateway_join) = actor::spawn_default(gateway::GatewayActor::new(sockets.clone()));
    join_set.push(gateway_join.map(|_| ()).boxed());

    let shard_count = 2 * workers;
    let mut shard_handles = Vec::with_capacity(shard_count);

    for i in 0..shard_count {
        let (shard, shard_join) = actor::spawn_default(shard::ShardActor::new(i));
        join_set.push(shard_join.map(|_| ()).boxed());
        shard_handles.push(shard);
    }

    let rng = rand::Rng::from_os_rng();
    let node_ctx = NodeContext {
        rng,
        gateway,
        sockets,
        shards: shard_handles,
        egress_counter: Arc::new(AtomicUsize::new(0)),
    };

    let controller_actor = controller::ControllerActor::new(
        node_ctx,
        vec![external_addr],
        Arc::new("root".to_string()),
    );
    let (controller_handle, controller_join) = actor::spawn_default(controller_actor);
    join_set.push(controller_join.map(|_| ()).boxed());

    // HTTP API
    let api_cfg = api::ApiConfig {
        base_path: "/api/v1".to_string(),
        default_host: http_addr.to_string(),
    };
    let router = api::router(controller_handle, api_cfg).layer(cors);

    let shutdown_for_http = shutdown.clone();
    let signaling = async move {
        let listener = tokio::net::TcpListener::bind(http_addr).await.unwrap();
        tracing::debug!("listening on {http_addr}");
        tokio::select! {
            res = axum::serve(listener, router) => {
                if let Err(e) = res {
                    tracing::error!("http server error: {e}");
                }
            }
            _ = shutdown_for_http.cancelled() => {
                tracing::info!("http server shutting down");
            }
        }
    };

    join_set.push(tokio::spawn(signaling).map(|_| ()).boxed());
    join_set.push(
        internal::serve_internal_http(internal_http_addr, shutdown.child_token())
            .map(|_| ())
            .boxed(),
    );

    // Wait for all tasks to complete OR shutdown
    tokio::select! {
        _ = async { while join_set.next().await.is_some() {} } => {}
        _ = shutdown.cancelled() => {
            tracing::info!("node received shutdown");
        }
    }

    Ok(())
}

mod internal {
    use std::net::SocketAddr;
    use std::time::Duration;

    use anyhow::Result;
    use axum::{
        Router,
        extract::Query,
        response::{Html, IntoResponse},
        routing::get,
    };
    use metrics_exporter_prometheus::PrometheusBuilder;
    use pprof::{ProfilerGuard, protos::Message};
    use pulsebeam_runtime::actor::Actor;
    use serde::Deserialize;
    use tokio_util::sync::CancellationToken;

    use crate::{controller, gateway, participant, room, shard};

    #[derive(Deserialize)]
    struct ProfileParams {
        #[serde(default = "default_seconds")]
        seconds: u64,
        #[serde(default)]
        flamegraph: bool,
    }

    fn default_seconds() -> u64 {
        30
    }

    pub async fn serve_internal_http(addr: SocketAddr, shutdown: CancellationToken) -> Result<()> {
        // Initialize Prometheus recorder
        let builder = PrometheusBuilder::new();
        let prometheus_handle = builder.install_recorder()?;

        const INDEX_HTML: &str = r#"
<ul>
  <li><a href="/metrics">Metrics</a></li>
  <li><a href="/debug/pprof/profile?seconds=30">CPU Profile (pprof)</a></li>
  <li><a href="/debug/pprof/profile?seconds=30&flamegraph=true">CPU Flamegraph</a></li>
</ul>
"#;

        // Router
        let router = Router::new()
            .route(
                "/metrics",
                get({
                    let handle = prometheus_handle.clone();
                    move || async move { handle.render() }
                }),
            )
            .route("/debug/pprof/profile", get(pprof_profile))
            .route("/", get(|| async { Html(INDEX_HTML) }))
            .with_state(());

        // Run HTTP server
        let listener = tokio::net::TcpListener::bind(addr).await?;

        let runtime_metrics_join = tokio::spawn(
            tokio_metrics::RuntimeMetricsReporterBuilder::default()
                .with_interval(std::time::Duration::from_secs(5))
                .describe_and_run(),
        );
        let actor_monitor_join = tokio::spawn(monitor_actors());

        tokio::select! {
            res = axum::serve(listener, router) => {
                if let Err(e) = res {
                    tracing::error!("internal http server error: {e}");
                }
            }
            _ = runtime_metrics_join => {}
            _ = actor_monitor_join => {}
            _ = shutdown.cancelled() => {
                tracing::info!("internal http server shutting down");
            }
        }

        Ok(())
    }

    async fn monitor_actors() {
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
            ("shard", shard::ShardActor::monitor().intervals()),
        ];

        let mut interval = tokio::time::interval(Duration::from_secs(5));

        loop {
            interval.tick().await;

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

    /// Handler: /debug/pprof/profile?seconds=30&flamegraph=true
    async fn pprof_profile(Query(params): Query<ProfileParams>) -> impl IntoResponse {
        let guard = ProfilerGuard::new(100).unwrap(); // 100 Hz sampling
        tokio::time::sleep(Duration::from_secs(params.seconds)).await;

        match guard.report().build() {
            Ok(report) => {
                if params.flamegraph {
                    let mut body = Vec::new();
                    if let Err(e) = report.flamegraph(&mut body) {
                        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
                            .into_response();
                    }
                    (
                        axum::http::StatusCode::OK,
                        [("Content-Type", "image/svg+xml")],
                        body,
                    )
                        .into_response()
                } else {
                    let profile = report.pprof().unwrap();
                    let body = profile.encode_to_vec();
                    (
                        axum::http::StatusCode::OK,
                        [
                            ("Content-Type", "application/octet-stream"),
                            ("Content-Disposition", "attachment; filename=cpu.pprof"),
                        ],
                        body,
                    )
                        .into_response()
                }
            }
            Err(e) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build pprof report: {e}"),
            )
                .into_response(),
        }
    }
}

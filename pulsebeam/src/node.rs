use crate::{api, controller, gateway};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::{actor, net, rand};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};

#[derive(Clone)]
pub struct NodeContext {
    pub rng: pulsebeam_runtime::rand::Rng,
    pub main_gateway: gateway::GatewayHandle,
    pub gateways: Vec<gateway::GatewayHandle>,
    pub sockets: Vec<Arc<net::UnifiedSocket>>,
}

pub async fn run(
    shutdown: CancellationToken,
    workers: usize,
    external_addr: SocketAddr,
    local_addr: SocketAddr,
    http_addr: SocketAddr,
) -> anyhow::Result<()> {
    let mut join_set = FuturesUnordered::new();
    let mut sockets: Vec<Arc<net::UnifiedSocket>> = Vec::new();
    let mut gateways = Vec::new();
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
        let (gw, gw_join) = actor::spawn_default(gateway::GatewayActor::new(socket.clone()));
        gateways.push(gw);
        sockets.push(socket);
        join_set.push(gw_join.map(|_| ()).boxed());
    }
    let cors = CorsLayer::very_permissive()
        .allow_origin(AllowOrigin::mirror_request())
        .expose_headers([hyper::header::LOCATION])
        .max_age(Duration::from_secs(86400));

    let rng = rand::Rng::from_os_rng();
    let node_ctx = NodeContext {
        rng,
        main_gateway: gateways.first().expect("one gateway must exist").clone(),
        gateways,
        sockets,
    };

    let controller_actor = controller::ControllerActor::new(
        node_ctx,
        vec![external_addr],
        Arc::new("root".to_string()),
    );
    let (controller_handle, controller_join) =
        actor::spawn(controller_actor, actor::RunnerConfig::default());
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

    // Wait for all tasks to complete OR shutdown
    tokio::select! {
        _ = async { while join_set.next().await.is_some() {} } => {}
        _ = shutdown.cancelled() => {
            tracing::info!("node received shutdown");
        }
    }

    Ok(())
}

use crate::{api, controller, system};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use pulsebeam_runtime::{actor, net, rt};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};

pub async fn run(
    shutdown: CancellationToken,
    cpu_rt: &rt::Runtime,
    external_addr: SocketAddr,
    unified_socket: net::UnifiedSocket,
    http_addr: SocketAddr,
) -> anyhow::Result<()> {
    let cors = CorsLayer::very_permissive()
        .allow_origin(AllowOrigin::mirror_request())
        .expose_headers([hyper::header::LOCATION])
        .max_age(Duration::from_secs(86400));

    let mut join_set = FuturesUnordered::new();

    let (system_ctx, system_join) = system::SystemContext::spawn(unified_socket);
    join_set.push(system_join.map(|_| ()).boxed());

    let (controller_ready_tx, controller_ready_rx) = tokio::sync::oneshot::channel();

    // Spawn controller on CPU runtime
    let shutdown_for_controller = shutdown.clone();
    cpu_rt.spawn(async move {
        let controller_actor = controller::ControllerActor::new(
            system_ctx,
            vec![external_addr],
            Arc::new("root".to_string()),
        );
        let (controller_handle, controller_join) =
            actor::spawn(controller_actor, actor::RunnerConfig::default());
        let _ = controller_ready_tx.send(controller_handle);
        tracing::debug!("controller is ready");

        tokio::select! {
            _ = controller_join => {}
            _ = shutdown_for_controller.cancelled() => {
                tracing::debug!("controller received shutdown");
            }
        }
    });

    tracing::debug!("waiting on controller to be ready");
    let controller_handle = controller_ready_rx.await?;

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

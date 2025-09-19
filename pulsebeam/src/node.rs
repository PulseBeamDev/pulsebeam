use crate::{api, controller, system};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use pulsebeam_runtime::{actor, net, rt};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tower_http::cors::{AllowOrigin, CorsLayer};

pub async fn run(
    cpu_rt: rt::Runtime,
    external_addr: SocketAddr,
    unified_socket: net::UnifiedSocket<'static>,
    http_addr: SocketAddr,
) -> anyhow::Result<()> {
    // Configure CORS
    let cors = CorsLayer::very_permissive()
        .allow_origin(AllowOrigin::mirror_request())
        .expose_headers([hyper::header::LOCATION])
        .max_age(Duration::from_secs(86400));

    // Spawn system and controller actors
    let mut join_set = FuturesUnordered::new();

    let (system_ctx, system_join) = system::SystemContext::spawn(unified_socket);
    join_set.push(system_join.map(|_| ()).boxed());

    let (controller_ready_tx, controller_ready_rx) = tokio::sync::oneshot::channel();

    // TODO: handle cleanup
    cpu_rt.spawn(async move {
        let controller_actor = controller::ControllerActor::new(
            system_ctx,
            vec![external_addr],
            Arc::new("root".to_string()),
        );
        let (controller_handle, controller_join) =
            actor::spawn(controller_actor, actor::RunnerConfig::default());
        controller_ready_tx.send(controller_handle).unwrap();
        tracing::debug!("controller is ready");
        controller_join.await;
    });

    tracing::debug!("waiting on controller to be ready");
    let controller_handle = controller_ready_rx.await?;
    // Set up signaling router
    let api_cfg = api::ApiConfig {
        base_path: "/api/v1".to_string(),
        default_host: http_addr.to_string(),
    };
    let router = api::router(controller_handle, api_cfg).layer(cors);
    tracing::debug!("listening on {http_addr}");

    let signaling = async move {
        let listener = tokio::net::TcpListener::bind(http_addr).await.unwrap();
        axum::serve(listener, router).await.unwrap();
    };
    join_set.push(tokio::spawn(signaling).map(|_| ()).boxed());

    // Wait for all tasks to complete
    while join_set.next().await.is_some() {}
    Ok(())
}

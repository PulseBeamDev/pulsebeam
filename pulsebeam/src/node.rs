use crate::{controller, signaling, system};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use pulsebeam_runtime::{actor, net, rt};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tower_http::cors::{AllowOrigin, CorsLayer};

pub async fn run(
    cpu_rt: rt::Runtime,
    external_addr: SocketAddr,
    unified_socket: net::UnifiedSocket,
    http_socket: SocketAddr,
) -> anyhow::Result<()> {
    // Configure CORS
    let cors = CorsLayer::very_permissive()
        .allow_origin(AllowOrigin::mirror_request())
        .max_age(Duration::from_secs(86400));

    // Spawn system and controller actors
    let mut join_set = FuturesUnordered::new();

    let (system_ctx, system_join) =
        system::SystemContext::spawn(external_addr, unified_socket.clone());
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
        controller_ready_tx.send(controller_handle);
        controller_join.await;
    });

    let controller_handle = controller_ready_rx.await?;
    // Set up signaling router
    let router = signaling::router(controller_handle).layer(cors);
    let listener = tokio::net::TcpListener::bind(http_socket)
        .await
        .expect("bind to http socket");
    let signaling = async move {
        let _ = axum::serve(listener, router).await;
    };
    let signaling_handle = tokio::spawn(signaling);
    join_set.push(signaling_handle.map(|_| ()).boxed());

    // Wait for all tasks to complete
    while join_set.next().await.is_some() {}
    Ok(())
}

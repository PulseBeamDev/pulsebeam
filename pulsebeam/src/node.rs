use crate::{controller, signaling, system};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use pulsebeam_runtime::{actor, net, rt};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tower_http::cors::{AllowOrigin, CorsLayer};

pub async fn run(
    cpu_rt: rt::Runtime,
    external_addr: SocketAddr,
    unified_socket: net::UnifiedSocket<'static>,
    http_socket: SocketAddr,
    enable_tls: bool,
) -> anyhow::Result<()> {
    // Configure CORS
    let cors = CorsLayer::very_permissive()
        .allow_origin(AllowOrigin::mirror_request())
        .expose_headers([hyper::header::LOCATION])
        .max_age(Duration::from_secs(86400));

    // Spawn system and controller actors
    let mut join_set = FuturesUnordered::new();

    let (system_ctx, system_join) = system::SystemContext::spawn(external_addr, unified_socket);
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
    let router = signaling::router(controller_handle).layer(cors);
    tracing::debug!("listening on {http_socket}");

    if enable_tls {
        // TODO: exclude from production build
        use axum_server::tls_rustls::RustlsConfig;
        let cert = include_bytes!("cert.pem");
        let key = include_bytes!("key.pem");
        let config = RustlsConfig::from_pem(cert.to_vec(), key.to_vec()).await?;

        let signaling = async move {
            axum_server::bind_rustls(http_socket, config)
                .serve(router.into_make_service())
                .await
                .unwrap();
        };
        join_set.push(tokio::spawn(signaling).map(|_| ()).boxed());
    } else {
        let signaling = async move {
            axum_server::bind(http_socket)
                .serve(router.into_make_service())
                .await
                .unwrap();
        };
        join_set.push(tokio::spawn(signaling).map(|_| ()).boxed());
    }

    // Wait for all tasks to complete
    while join_set.next().await.is_some() {}
    Ok(())
}

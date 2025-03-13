use std::net::SocketAddr;
use tracing::info;

use std::time::Duration;
use tonic::service::LayerExt;
use tower_http::cors::{AllowOrigin, CorsLayer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cors = CorsLayer::very_permissive()
        // https://github.com/tower-rs/tower-http/issues/194
        .allow_origin(AllowOrigin::mirror_request())
        .max_age(Duration::from_secs(86400));
    let server =
        pulsebeam_server_foss::SignalingServer::new(pulsebeam_server_foss::Server::default());
    let server = tower::ServiceBuilder::new()
        .layer(tonic_web::GrpcWebLayer::new())
        .into_inner()
        .named_layer(server);
    let grpc_routes = tonic::service::Routes::new(server)
        .prepare()
        .into_axum_router();

    let addr: SocketAddr = "[::]:3000".parse().unwrap();
    info!("Listening on {addr}");

    let router = axum::Router::new()
        .nest_service("/grpc", grpc_routes)
        .layer(cors);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;

    Ok(())
}

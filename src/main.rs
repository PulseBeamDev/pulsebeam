use axum::routing::get;
use std::net::SocketAddr;
use tokio_util::sync::CancellationToken;
use tracing::{info, level_filters::LevelFilter};
use tracing_subscriber::fmt::format::FmtSpan;

use pulsebeam_server_foss::server::Server;
use pulsebeam_server_foss::{manager::ManagerConfig, proto::signaling_server::SignalingServer};
use std::time::Duration;
use tonic::service::LayerExt;
use tower_http::cors::{AllowOrigin, CorsLayer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let env_filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .compact()
        .with_span_events(FmtSpan::NONE)
        .with_env_filter(env_filter)
        .init();

    let cors = CorsLayer::very_permissive()
        // https://github.com/tower-rs/tower-http/issues/194
        .allow_origin(AllowOrigin::mirror_request())
        .max_age(Duration::from_secs(86400));
    let token = CancellationToken::new();
    let server = Server::spawn(token, ManagerConfig::default());
    let query_routes = server.query_routes();
    let grpc_server = tower::ServiceBuilder::new()
        .layer(tonic_web::GrpcWebLayer::new())
        .into_inner()
        .named_layer(SignalingServer::new(server));
    let grpc_routes = tonic::service::Routes::new(grpc_server)
        .prepare()
        .into_axum_router()
        .layer(cors)
        .layer(tower_http::trace::TraceLayer::new_for_grpc());

    let addr: SocketAddr = "[::]:3000".parse().unwrap();
    info!("Listening on {addr}");

    let router = axum::Router::new()
        .nest_service("/grpc", grpc_routes)
        .nest_service("/query", query_routes)
        .route("/_ping", get(ping));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;

    Ok(())
}

async fn ping() -> &'static str {
    "Pong\n"
}

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use anyhow::Context;
use axum::{routing::get, Router};
use http::header::{ACCEPT_ENCODING, AUTHORIZATION, CONTENT_TYPE};
use std::net::SocketAddr;
use tower_http::trace::TraceLayer;
use tracing::info;

use http::Method;
use std::time::Duration;
use tower_http::cors::{AllowOrigin, CorsLayer};

const SERVER_CONFIG: pulsebeam_server_foss::ServerConfig = pulsebeam_server_foss::ServerConfig {
    max_capacity: 65536,
};

async fn ping() -> &'static str {
    "Pong\n"
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cors = CorsLayer::new()
        .allow_methods([Method::POST])
        .allow_headers([CONTENT_TYPE, ACCEPT_ENCODING, AUTHORIZATION])
        // https://github.com/tower-rs/tower-http/issues/194
        .allow_origin(AllowOrigin::mirror_request())
        .max_age(Duration::from_secs(86400));
    let api_impl = pulsebeam_server_foss::Server::new(SERVER_CONFIG);
    let twirp_routes = Router::new()
        .nest(
            pulsebeam_server_foss::rpc::SERVICE_FQN,
            pulsebeam_server_foss::rpc::router(api_impl),
        )
        .layer(cors);
    let router = Router::new()
        .nest("/twirp", twirp_routes)
        .route("/_ping", get(ping))
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = "[::]:3000".parse().unwrap();

    info!("Listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("failed to bind port")?;
    axum::serve(listener, router)
        .await
        .context("failed to serve http")?;
    Ok(())
}

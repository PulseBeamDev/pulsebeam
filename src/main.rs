use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use anyhow::Context;
use axum::Router;
use std::net::SocketAddr;
use tracing::info;

const SERVER_MAX_CAPACITY: u64 = 65536;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let api_impl = pulsebeam_server_lite::Server::new(SERVER_MAX_CAPACITY);
    let twirp_routes = Router::new().nest(
        pulsebeam_server_lite::rpc::SERVICE_FQN,
        pulsebeam_server_lite::rpc::router(api_impl),
    );
    let router = Router::new().nest("/twirp", twirp_routes);

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

extern crate jemallocator;

#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

use std::time::Duration;

use pulsebeam::{controller::Controller, signaling};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::level_filters::LevelFilter;

#[tokio::main]
async fn main() {
    let env_filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(env_filter)
        .init();

    let controller = Controller::spawn().await.unwrap();

    let cors = CorsLayer::very_permissive()
        // https://github.com/tower-rs/tower-http/issues/194
        .allow_origin(AllowOrigin::mirror_request())
        .max_age(Duration::from_secs(86400));

    let router = signaling::router(controller).layer(cors);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, router).await.unwrap();
}

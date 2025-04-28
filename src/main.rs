use pulsebeam::{controller::Controller, signaling};
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
    let router = signaling::router(controller);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, router).await.unwrap();
}

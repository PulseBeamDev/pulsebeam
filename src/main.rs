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

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
}

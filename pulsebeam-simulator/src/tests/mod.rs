pub mod bwe;
pub mod common;
pub mod connectivity;
pub mod data_channel;
pub mod properties;
pub mod subscriptions;
pub mod video;

#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init() {
    // Honour RUST_LOG when set so a failing sim can be re-run at TRACE for a specific target
    // (e.g. RUST_LOG=str0m::bwe_=trace). Without it, default to DEBUG for everything.
    if std::env::var("RUST_LOG").is_ok() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    } else {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
    }
}

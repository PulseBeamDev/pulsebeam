pub mod client;

use pulsebeam_runtime::net::UdpMode;
use std::{
    net::{IpAddr, SocketAddr},
    sync::Once,
};
use tracing_subscriber::EnvFilter;

static INIT: Once = Once::new();

pub fn setup_tracing() {
    INIT.call_once(|| {
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("pulsebeam=debug"));
        tracing_subscriber::fmt()
            .pretty()
            .with_env_filter(env_filter)
            .with_target(true)
            .with_ansi(true)
            .init();
    });
}

pub async fn start_sfu_node(ip: IpAddr) -> anyhow::Result<()> {
    let rtc_port = 3478;
    let external_addr: SocketAddr = format!("{}:3478", ip).parse()?;
    let local_addr: SocketAddr = format!("0.0.0.0:{}", rtc_port).parse()?;
    let http_api_addr: SocketAddr = "0.0.0.0:3000".parse()?;

    pulsebeam::node::NodeBuilder::new()
        .workers(1)
        .local_addr(local_addr)
        .external_addr(external_addr)
        .with_udp_mode(UdpMode::Scalar)
        .with_http_api(http_api_addr)
        .run(tokio_util::sync::CancellationToken::new())
        .await?;
    Ok(())
}

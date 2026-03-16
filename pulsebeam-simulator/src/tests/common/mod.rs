pub mod client;

use pulsebeam_runtime::net::UdpMode;
use std::{
    net::{IpAddr, SocketAddr},
    sync::Once,
    time::{Duration, Instant},
};
use tracing_subscriber::EnvFilter;

static INIT: Once = Once::new();

pub fn setup_tracing() {
    INIT.call_once(|| {
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("pulsebeam=info"));
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

/// Run a Turmoil simulation run with a real-time timeout.
///
/// This prevents tests from hanging forever if the simulation time stops advancing.
///
/// The timeout is enforced by periodically stepping the simulation and checking the
/// wall clock.
pub fn run_sim_or_timeout(sim: &mut turmoil::Sim<'_>, timeout: Duration) -> turmoil::Result<()> {
    let start = Instant::now();

    loop {
        if start.elapsed() > timeout {
            return Err(format!(
                "Simulation did not complete within {:?} (wall-clock); aborting.",
                timeout
            )
            .into());
        }

        let is_finished = sim.step()?;
        if is_finished {
            return Ok(());
        }
    }
}

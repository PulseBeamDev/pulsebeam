pub mod client;

use pulsebeam_runtime::net::UdpMode;
use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Once,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};
use tracing_subscriber::EnvFilter;

static INIT: Once = Once::new();

static NEXT_SUBNET: AtomicU8 = AtomicU8::new(1);
static NEXT_TEST_ID: AtomicU8 = AtomicU8::new(1);

pub fn reserve_subnet() -> u8 {
    // Avoid 0 and 255.
    let next = NEXT_SUBNET.fetch_add(1, Ordering::Relaxed);
    1 + (next % 200)
}

pub fn next_test_id() -> u8 {
    NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn subnet_ip(subnet: u8, host: u8) -> IpAddr {
    format!("192.168.{}.{}", subnet, host).parse().unwrap()
}

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

pub async fn start_sfu_node(ip: IpAddr, rng: pulsebeam_runtime::rand::Rng) -> anyhow::Result<()> {
    let rtc_port = 3478;
    let external_addr: SocketAddr = format!("{}:3478", ip).parse()?;
    let local_addr: SocketAddr = format!("0.0.0.0:{}", rtc_port).parse()?;
    let http_api_addr: SocketAddr = "0.0.0.0:7070".parse()?;

    pulsebeam::node::NodeBuilder::new()
        .workers(1)
        .local_addr(local_addr)
        .external_addr(external_addr)
        .rng(rng)
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
    // Allow some extra real-world headroom for slower environments or debugging.
    let wall_timeout = timeout.checked_mul(5).unwrap_or(timeout + Duration::from_secs(10));
    let start = Instant::now();

    loop {
        if start.elapsed() > wall_timeout {
            return Err(format!(
                "Simulation did not complete within {:?} (wall-clock); aborting.",
                wall_timeout
            )
            .into());
        }

        let is_finished = sim.step()?;
        if is_finished {
            return Ok(());
        }
    }
}

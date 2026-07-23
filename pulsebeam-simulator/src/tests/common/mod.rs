pub mod client;

use pulsebeam_runtime::net::UdpMode;
use std::{
    net::{IpAddr, SocketAddr},
    sync::atomic::{AtomicU8, Ordering},
    time::{Duration, Instant},
};

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

pub async fn wait_for_publisher_id(
    publisher_id: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
    ready: &std::sync::Arc<tokio::sync::Notify>,
) -> String {
    loop {
        let notified = ready.notified();
        if let Some(id) = publisher_id.lock().unwrap().clone() {
            return id;
        }
        notified.await;
    }
}

pub async fn start_sfu_node(ip: IpAddr, rng: pulsebeam_runtime::rand::Rng) -> anyhow::Result<()> {
    let rtc_port = 3478;
    let external_addr = SocketAddr::new(ip, rtc_port);
    let local_addr: SocketAddr = format!("0.0.0.0:{}", rtc_port).parse()?;
    let http_api_addr: SocketAddr = "0.0.0.0:7070".parse()?;

    pulsebeam::node::NodeBuilder::new()
        .workers(1)
        .local_addr(local_addr)
        .external_addrs(vec![external_addr])
        .rng(rng)
        .with_udp_mode(UdpMode::Scalar)
        .with_http_api(http_api_addr)
        .with_current_runtime()
        .run(tokio_util::sync::CancellationToken::new())
        .await?;
    Ok(())
}

/// Same as `start_sfu_node` but with UDP candidates suppressed so that
/// clients must use the TCP path (TCP-only simulation tests).
pub async fn start_sfu_node_tcp_only(
    ip: IpAddr,
    rng: pulsebeam_runtime::rand::Rng,
) -> anyhow::Result<()> {
    let rtc_port = 3478;
    let external_addr = SocketAddr::new(ip, rtc_port);
    let local_addr: SocketAddr = format!("0.0.0.0:{}", rtc_port).parse()?;
    let http_api_addr: SocketAddr = "0.0.0.0:7070".parse()?;

    pulsebeam::node::NodeBuilder::new()
        .workers(1)
        .local_addr(local_addr)
        .external_addrs(vec![external_addr])
        .rng(rng)
        .with_udp_mode(UdpMode::Scalar)
        .with_http_api(http_api_addr)
        .with_current_runtime()
        .tcp_only()
        .run(tokio_util::sync::CancellationToken::new())
        .await?;
    Ok(())
}

/// Same as `start_sfu_node_tcp_only` but with two worker shards.
///
/// Using two shards maximises the probability that `hash(peer_addr)` (used for
/// TCP routing) and `hash(room_id)` (used for participant routing) disagree on
/// which shard should own a connection, which is exactly the cross-shard TCP
/// egress scenario we want to exercise.
pub async fn start_sfu_node_tcp_only_multi_shard(
    ip: IpAddr,
    rng: pulsebeam_runtime::rand::Rng,
) -> anyhow::Result<()> {
    let rtc_port = 3478;
    let external_addr = SocketAddr::new(ip, rtc_port);
    let local_addr: SocketAddr = format!("0.0.0.0:{}", rtc_port).parse()?;
    let http_api_addr: SocketAddr = "0.0.0.0:7070".parse()?;

    pulsebeam::node::NodeBuilder::new()
        .workers(2)
        .local_addr(local_addr)
        .external_addrs(vec![external_addr])
        .rng(rng)
        .with_udp_mode(UdpMode::Scalar)
        .with_http_api(http_api_addr)
        .with_current_runtime()
        .tcp_only()
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

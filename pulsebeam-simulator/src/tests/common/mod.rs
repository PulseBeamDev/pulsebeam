pub mod client;
pub mod harness;

pub use harness::{
    Capacity, LinkProfile, LinkReport, LocalNodeSim, Loss, Participant, Property, Reorder, Room,
    Step, VideoQuality,
};

use pulsebeam_runtime::net::UdpMode;
use std::{
    net::{IpAddr, SocketAddr},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

static NEXT_SUBNET: AtomicU32 = AtomicU32::new(0);

/// Subnets available to plans. Excludes 0 and 255, and the address space is a byte.
const SUBNET_CAPACITY: u32 = 200;

/// Hand out a subnet no other plan in this process is using.
///
/// The subnet is what isolates a plan: link capacity, loss, reordering and duplication all live in
/// process-global registries keyed by address, and cargo runs these plans in parallel. Two plans on
/// one subnet therefore share a network - each silently reconfiguring the other's link, producing a
/// failure that depends on which plans happen to overlap and reproduces only under load.
///
/// Wrapping is refused rather than silently reused. Exhaustion is a real limit that a growing
/// suite will reach, and the failure it causes is the hardest possible kind to diagnose, so it must
/// announce itself.
pub fn reserve_subnet() -> u8 {
    let next = NEXT_SUBNET.fetch_add(1, Ordering::Relaxed);
    assert!(
        next < SUBNET_CAPACITY,
        "simulator subnets exhausted after {SUBNET_CAPACITY} plans in one process: plans would \
         start sharing a network and silently reconfiguring each other's link. Give the address \
         space another octet rather than letting the counter wrap."
    );
    // Avoid 0 and 255.
    (1 + next) as u8
}

pub fn subnet_ip(subnet: u8, host: u8) -> IpAddr {
    format!("192.168.{}.{}", subnet, host).parse().unwrap()
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

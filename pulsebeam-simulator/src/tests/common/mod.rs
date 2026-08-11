#![allow(
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::string_slice,
    clippy::indexing_slicing
)] // test / simulation support
pub mod client;
pub mod harness;

pub use harness::{
    Capacity, Content, Experience, LinkProfile, LinkReport, LocalNodeSim, Loss, Participant,
    Property, Reorder, Room, Step, VideoQuality,
};

use pulsebeam_runtime::net::UdpMode;
use std::{
    net::{IpAddr, SocketAddr},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

/// The seed every plan runs under unless it pins its own.
///
/// Fixed, so an ordinary run reproduces exactly and a failure bisects.
pub const DEFAULT_SIM_SEED: u64 = 0xDEAD_BEEF;

/// The seed for this process, `DEFAULT_SIM_SEED` unless `PULSEBEAM_SIM_SEED`
/// overrides it.
///
/// One fixed seed samples one point out of the space of orderings, latencies
/// and losses these plans can produce, and then reports that point forever. The
/// override is what lets the same suite sweep the rest of the space without
/// giving up reproducibility: a sweep failure names its seed, and
/// `make test-sim-seed SEED=<n>` replays exactly that run.
///
/// A malformed value is refused rather than silently ignored — a sweep that
/// quietly ran 200 iterations of the default seed would report the opposite of
/// what it found.
pub fn sim_seed() -> u64 {
    match std::env::var("PULSEBEAM_SIM_SEED") {
        Err(_) => DEFAULT_SIM_SEED,
        Ok(raw) => raw
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("PULSEBEAM_SIM_SEED={raw:?} is not a u64")),
    }
}

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
    u8::try_from(1 + next).expect("the assertion above bounds this below 255")
}

pub fn subnet_ip(subnet: u8, host: u8) -> IpAddr {
    format!("192.168.{subnet}.{host}").parse().unwrap()
}

/// Start an SFU node in the simulation.
///
/// `shards` above one also sets `room_shard_slot(1)` and `round_robin_rooms()`.
/// Both halves matter: extra shards make `hash(peer_addr)` and `hash(room_id)`
/// disagree, which is the cross-shard TCP egress case, but only the slot puts a
/// room's *participants* on different shards. Without it a room of fewer than
/// sixteen is co-located and its media never leaves a core, so none of the
/// route, envelope or reverse-lane machinery is reached.
///
/// `tcp_only` suppresses UDP candidates so clients must take the TCP path. It
/// is independent of `shards`: cross-shard forwarding has to hold on the
/// transport that carries almost all real traffic, not only on the fallback.
pub async fn start_sfu_node_with(
    ip: IpAddr,
    rng: pulsebeam_runtime::rand::Rng,
    shards: usize,
    tcp_only: bool,
) -> anyhow::Result<()> {
    let rtc_port = 3478;
    let external_addr = SocketAddr::new(ip, rtc_port);
    let local_addr: SocketAddr = format!("0.0.0.0:{rtc_port}").parse()?;
    let http_api_addr: SocketAddr = "0.0.0.0:7070".parse()?;

    let mut builder = pulsebeam::node::NodeBuilder::new()
        .workers(shards)
        .local_addr(local_addr)
        .external_addrs(vec![external_addr])
        .rng(rng)
        .with_udp_mode(UdpMode::Scalar)
        .with_http_api(http_api_addr)
        .with_current_runtime();
    if shards > 1 {
        builder = builder.room_shard_slot(1).round_robin_rooms();
    }
    if tcp_only {
        builder = builder.tcp_only();
    }
    builder
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
                "Simulation did not complete within {timeout:?} (wall-clock); aborting."
            )
            .into());
        }

        let is_finished = sim.step()?;
        if is_finished {
            return Ok(());
        }
    }
}

//! Shared scaffolding for the use-case-organized simulcast suite: network
//! profiles grounded in real measured conditions, connection setup
//! boilerplate, and disruption helpers. Test bodies in the sibling modules
//! should read as a scenario narrative; the mechanics live here.

use crate::tests::common;
use pulsebeam_agent::str0m::media::Mid;
use pulsebeam_agent::{MediaKind, SimulcastLayer, TransceiverDirection};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The simulator's network controls (bandwidth shaping, hold/partition) are
/// process-global. Serializing scenarios keeps a neighboring test's
/// artificial impairment from being mistaken for a regression in this one.
pub fn test_guard() -> MutexGuard<'static, ()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Deterministic per-scenario seed so a named test's impairment sequence is
/// reproducible regardless of what order the test binary happens to run
/// tests in.
pub fn seed(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// A named, realistic network condition: latency range plus independent
/// packet loss. Values are order-of-magnitude grounded in publicly known
/// characteristics of each link type, not arbitrary.
#[derive(Clone, Copy)]
pub struct Profile {
    pub min_latency: Duration,
    pub max_latency: Duration,
    pub fail_rate: f64,
}

/// Wired broadband or a clean, uncongested home WiFi hop: low fixed latency,
/// negligible jitter, no meaningful loss.
pub const BROADBAND: Profile = Profile {
    min_latency: Duration::from_millis(3),
    max_latency: Duration::from_millis(15),
    fail_rate: 0.0,
};

/// A home WiFi network sharing spectrum with neighbors and other household
/// devices: real jitter from contention, occasional loss, still usually
/// livable.
pub const CONGESTED_WIFI: Profile = Profile {
    min_latency: Duration::from_millis(15),
    max_latency: Duration::from_millis(120),
    fail_rate: 0.015,
};

/// A crowded venue or conference-hall WiFi: many concurrent clients on one
/// AP, heavy jitter and loss together -- the hardest common indoor
/// condition, not a contrived worst case.
pub const CROWDED_VENUE_WIFI: Profile = Profile {
    min_latency: Duration::from_millis(20),
    max_latency: Duration::from_millis(220),
    fail_rate: 0.03,
};

/// Mid-tier LTE: the latency band and bursty loss rate widely reported for
/// a moderately loaded cell.
pub const CELLULAR_LTE: Profile = Profile {
    min_latency: Duration::from_millis(30),
    max_latency: Duration::from_millis(90),
    fail_rate: 0.02,
};

/// Geostationary satellite: ~250-300ms one-way propagation delay is
/// physics, not congestion, and is essentially fixed (minimal jitter). Used
/// to prove high-but-stable latency alone is never misread as a bandwidth
/// problem.
pub const SATELLITE: Profile = Profile {
    min_latency: Duration::from_millis(280),
    max_latency: Duration::from_millis(300),
    fail_rate: 0.0,
};

/// A weak, congested cell edge or crowded public WiFi: the network a
/// broadcast's *worst* viewer, or a teleoperated device on the move, is
/// realistically stuck with -- high jitter and real loss together.
pub const HOSTILE_MOBILE: Profile = Profile {
    min_latency: Duration::from_millis(40),
    max_latency: Duration::from_millis(260),
    fail_rate: 0.045,
};

/// A one-way (downlink or uplink) transient outage: a real network doesn't
/// just get slower, it sometimes stops completely for a bounded stretch
/// (elevator, tunnel, cell handoff, AP roam).
#[derive(Clone, Copy)]
pub enum Outage {
    /// Packets are held and delivered late once lifted (bufferbloat/stall),
    /// not dropped.
    Hold(Duration),
    /// The link is completely severed and packets in flight are lost.
    Partition(Duration),
}

pub async fn drive_through_downlink_outage(
    client: &mut common::client::SimClient,
    receiver_ip: IpAddr,
    server_ip: IpAddr,
    outage: Outage,
) -> anyhow::Result<()> {
    match outage {
        Outage::Hold(duration) => {
            turmoil::hold(receiver_ip, server_ip);
            client.drive_for(duration).await?;
            turmoil::release(receiver_ip, server_ip);
        }
        Outage::Partition(duration) => {
            turmoil::partition(receiver_ip, server_ip);
            client.drive_for(duration).await?;
            turmoil::repair(receiver_ip, server_ip);
        }
    }
    Ok(())
}

pub fn spawn_sfu(sim: &mut turmoil::Sim<'_>, ip: IpAddr) {
    sim.host(ip, move || async move {
        common::start_sfu_node(ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });
}

pub fn simulcast_layers() -> Option<Vec<SimulcastLayer>> {
    Some(vec![
        SimulcastLayer::new("f"),
        SimulcastLayer::new("h"),
        SimulcastLayer::new("q"),
    ])
}

/// Spawns a standard simulcast video publisher that drives until `done` is
/// cancelled.
pub fn spawn_publisher(
    sim: &mut turmoil::Sim<'_>,
    ip: IpAddr,
    server_ip: IpAddr,
    room: &'static str,
    done: CancellationToken,
) {
    sim.client(ip, async move {
        let mut client = common::client::SimClientBuilder::bind(ip, server_ip)
            .await?
            .with_track(
                MediaKind::Video,
                TransceiverDirection::SendOnly,
                simulcast_layers(),
            )
            .connect(room)
            .await?;
        client.drive(done).await?;
        Ok(())
    });
}

/// Per-track cumulative rx bytes, keyed by mid. Unlike a combined counter,
/// this lets a warmup gate require *every* downstream to be flowing rather
/// than being satisfied by a lucky mix (one starved, one oversized).
pub fn per_track_bytes(client: &common::client::SimClient) -> HashMap<Mid, u64> {
    let stats = client.ctx.driver.stats();
    client
        .ctx
        .remote_tracks
        .keys()
        .map(|mid| {
            let bytes = stats
                .tracks
                .get(mid)
                .map_or(0, |t| t.rx_layers.values().map(|l| l.bytes).sum());
            (*mid, bytes)
        })
        .collect()
}

/// Drives a receiver until it has discovered and started receiving frames
/// on `expected_tracks` remote tracks. Used as the common "call has
/// connected" gate before a scenario's real network condition is applied,
/// so a scenario's own hard condition is what's under test, not connection
/// setup racing against it.
pub async fn warmup_until_all_flowing(
    client: &mut common::client::SimClient,
    timeout: Duration,
    expected_tracks: usize,
) -> anyhow::Result<()> {
    client
        .drive_until(timeout, |ctx| {
            ctx.discovered_tracks.len() >= expected_tracks
        })
        .await?;
    client
        .drive_until(timeout, |ctx| {
            if ctx.remote_tracks.len() < expected_tracks {
                return false;
            }
            ctx.stream_health
                .values()
                .all(|h| h.lock().unwrap_or_else(|p| p.into_inner()).frames_total() > 0)
        })
        .await
}

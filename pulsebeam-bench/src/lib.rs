//! A load harness for the SFU data plane.
//!
//! It runs a real node built by [`pulsebeam::node::NodeBuilder`] and real
//! [`pulsebeam_agent`] clients over loopback, in one process, so a benchmark
//! exercises the same code path production does — GSO/GRO sockets, DTLS/SRTP,
//! the pacer, the shard tick loop.
//!
//! The number it reports is **data-plane CPU per byte delivered to clients**,
//! not wall time. Both halves of that matter: the clients share the machine, so
//! wall time measures the load generator as much as the SFU, and the media is
//! constant-bitrate, so a faster SFU forwards the same bytes for less CPU
//! rather than forwarding more.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use pulsebeam_agent::actor::{AgentBuilder, AgentEvent};
use pulsebeam_agent::agent::RemoteTrack;
use pulsebeam_agent::api::HttpApiClient;
use pulsebeam_agent::media::{H264Looper, SharedH264Asset};
use pulsebeam_agent::{MediaKind, Rid, SimulcastLayer, TransceiverDirection};
use pulsebeam_core::net::UdpSocket;
use pulsebeam_runtime::rand::{Rng, SeedableRng};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub mod cpu;

use cpu::DataPlaneClock;

const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const STEADY_STATE_TIMEOUT: Duration = Duration::from_secs(60);

/// One named load shape. The name is what `criterion` stores baselines under,
/// so changing a scenario's parameters means its history no longer compares.
#[derive(Debug, Clone, Copy)]
pub struct Scenario {
    pub name: &'static str,
    pub shards: usize,
    pub rooms: usize,
    pub peers_per_room: usize,
    pub simulcast: bool,
}

impl Scenario {
    pub const fn new(name: &'static str, shards: usize, rooms: usize, peers_per_room: usize) -> Self {
        Self {
            name,
            shards,
            rooms,
            peers_per_room,
            simulcast: false,
        }
    }

    pub const fn simulcast(mut self) -> Self {
        self.simulcast = true;
        self
    }

    pub fn peers(&self) -> usize {
        self.rooms * self.peers_per_room
    }
}

struct Assets {
    full: Arc<SharedH264Asset>,
    half: Arc<SharedH264Asset>,
    quarter: Arc<SharedH264Asset>,
}

impl Assets {
    fn load() -> Self {
        Self {
            full: Arc::new(SharedH264Asset::new(pulsebeam_testdata::RAW_H264_FULL_CBR)),
            half: Arc::new(SharedH264Asset::new(pulsebeam_testdata::RAW_H264_HALF_CBR)),
            quarter: Arc::new(SharedH264Asset::new(
                pulsebeam_testdata::RAW_H264_QUARTER_CBR,
            )),
        }
    }

    fn looper_for(&self, rid: &Option<Rid>) -> H264Looper {
        let asset = match rid.as_ref().map(|r| r.as_ref()) {
            Some("f") => &self.full,
            Some("q") => &self.quarter,
            _ => &self.half,
        };
        H264Looper::new_shared(asset.clone(), 30)
    }
}

/// A running SFU plus its load generator.
pub struct SfuHarness {
    scenario: Scenario,
    delivered: Arc<AtomicU64>,
    clock: DataPlaneClock,
    shutdown: CancellationToken,
    clients: Option<tokio::runtime::Runtime>,
}

/// What the harness observed while a caller was measuring. Reported so a
/// benchmark can refuse to publish a number taken from a saturated or
/// starved node.
#[derive(Debug, Clone, Copy)]
pub struct LoadReport {
    pub delivered_bytes: u64,
    pub data_plane_cpu: Duration,
    pub wall: Duration,
    pub shards: usize,
}

impl LoadReport {
    /// Fraction of the available shard-thread time that was spent on CPU.
    /// Above ~0.8 the node is shedding work and the CPU-per-byte figure stops
    /// reflecting efficiency.
    pub fn utilization(&self) -> f64 {
        let available = self.wall.as_secs_f64() * self.shards as f64;
        if available <= 0.0 {
            return 0.0;
        }
        self.data_plane_cpu.as_secs_f64() / available
    }

    pub fn throughput_mbps(&self) -> f64 {
        let secs = self.wall.as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        (self.delivered_bytes as f64 * 8.0) / secs / 1e6
    }
}

impl SfuHarness {
    /// Brings up the node and every client, and returns once media is flowing
    /// at a stable rate.
    pub fn start(scenario: Scenario) -> anyhow::Result<Self> {
        anyhow::ensure!(
            cpu::schedstat_available(),
            "per-thread CPU accounting is unavailable (/proc/self/schedstat); \
             the kernel needs CONFIG_SCHEDSTATS for this harness to measure anything"
        );

        let shutdown = CancellationToken::new();
        let shards_before = DataPlaneClock::shard_tids();
        let rtc_port = reserve_udp_port()?;
        let (api_addr_tx, api_addr_rx) = std::sync::mpsc::channel();

        // Not joined on drop: `NodeBuilder::run` joins its shard threads while
        // still holding the command senders that would let them exit, so a
        // cancelled node never returns. Cancelling parks the shards in epoll
        // with no timers armed, which costs a later scenario nothing but the
        // idle threads themselves.
        {
            let shutdown = shutdown.child_token();
            std::thread::Builder::new()
                .name("pb-bench-node".into())
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("bench control runtime");
                    rt.block_on(async move {
                        let listener = match pulsebeam_core::net::TcpListener::bind(SocketAddr::new(LOOPBACK, 0))
                            .await
                        {
                            Ok(listener) => listener,
                            Err(err) => {
                                let _ = api_addr_tx.send(Err(err.to_string()));
                                return;
                            }
                        };
                        match listener.local_addr() {
                            Ok(addr) => {
                                let _ = api_addr_tx.send(Ok(addr));
                            }
                            Err(err) => {
                                let _ = api_addr_tx.send(Err(err.to_string()));
                                return;
                            }
                        }

                        let node = pulsebeam::node::NodeBuilder::new()
                            .workers(scenario.shards)
                            .local_addr(SocketAddr::new(LOOPBACK, rtc_port))
                            .external_addrs(vec![SocketAddr::new(LOOPBACK, rtc_port)])
                            .rng(Rng::seed_from_u64(0xB3C4_1EAF))
                            .with_http_api_listener(listener);
                        let _ = node.run(shutdown).await;
                    });
                })?;
        }

        let api_addr = api_addr_rx
            .recv_timeout(STARTUP_TIMEOUT)
            .map_err(|_| anyhow::anyhow!("node did not report its API address"))?
            .map_err(anyhow::Error::msg)?;
        let api_url = format!("http://{api_addr}");

        let clients = build_client_runtime(scenario.shards)?;
        let clock = wait_for_shard_threads(scenario.shards, &shards_before)?;
        let delivered = Arc::new(AtomicU64::new(0));

        {
            let delivered = delivered.clone();
            let shutdown = shutdown.child_token();
            clients.spawn(async move {
                let assets = Arc::new(Assets::load());
                let mut peers = JoinSet::new();
                for room in 0..scenario.rooms {
                    for _ in 0..scenario.peers_per_room {
                        peers.spawn(run_peer(
                            format!("bench-room-{room}"),
                            api_url.clone(),
                            assets.clone(),
                            delivered.clone(),
                            scenario.simulcast,
                            shutdown.child_token(),
                        ));
                    }
                }
                peers.join_all().await;
            });
        }

        let harness = Self {
            scenario,
            delivered,
            clock,
            shutdown,
            clients: Some(clients),
        };
        harness.await_steady_state()?;
        Ok(harness)
    }

    pub fn scenario(&self) -> Scenario {
        self.scenario
    }

    /// Bytes of media payload every client has received so far. This is the
    /// harness's unit of work: it counts what the SFU actually forwarded, so a
    /// sample stays comparable even if the bitrate drifts between runs.
    pub fn delivered_bytes(&self) -> u64 {
        self.delivered.load(Ordering::Relaxed)
    }

    pub fn data_plane_cpu(&self) -> Duration {
        self.clock.read()
    }

    /// Blocks until the clients have taken delivery of `bytes` more media, and
    /// reports what the data plane spent getting there.
    pub fn measure(&self, bytes: u64) -> LoadReport {
        let target = self.delivered_bytes() + bytes;
        let cpu_before = self.data_plane_cpu();
        let started = Instant::now();

        while self.delivered_bytes() < target {
            std::thread::sleep(Duration::from_micros(500));
        }

        LoadReport {
            delivered_bytes: self.delivered_bytes() - (target - bytes),
            data_plane_cpu: self.data_plane_cpu().saturating_sub(cpu_before),
            wall: started.elapsed(),
            shards: self.clock.thread_count(),
        }
    }

    /// Waits out the bandwidth-estimator ramp: media has to be flowing at a
    /// rate that no longer moves before a sample means anything.
    fn await_steady_state(&self) -> anyhow::Result<()> {
        const WINDOW: Duration = Duration::from_millis(500);
        const TOLERANCE: f64 = 0.05;
        const STABLE_WINDOWS: usize = 4;

        let deadline = Instant::now() + STEADY_STATE_TIMEOUT;
        let mut previous = 0.0f64;
        let mut stable = 0usize;

        while Instant::now() < deadline {
            let before = self.delivered_bytes();
            std::thread::sleep(WINDOW);
            let rate = (self.delivered_bytes() - before) as f64 / WINDOW.as_secs_f64();

            if rate <= 0.0 {
                stable = 0;
                previous = 0.0;
                continue;
            }

            let drift = (rate - previous).abs() / rate;
            stable = if drift <= TOLERANCE { stable + 1 } else { 0 };
            previous = rate;

            if stable >= STABLE_WINDOWS {
                return Ok(());
            }
        }

        anyhow::bail!(
            "media never reached a stable rate within {STEADY_STATE_TIMEOUT:?} \
             ({} peers over {} shards); the machine may be too small for this scenario",
            self.scenario.peers(),
            self.scenario.shards
        )
    }
}

impl Drop for SfuHarness {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(clients) = self.clients.take() {
            clients.shutdown_timeout(Duration::from_secs(5));
        }
    }
}

async fn run_peer(
    room: String,
    api_url: String,
    assets: Arc<Assets>,
    delivered: Arc<AtomicU64>,
    simulcast: bool,
    shutdown: CancellationToken,
) {
    let Ok(api) = HttpApiClient::new(Box::new(reqwest::Client::new()), &api_url) else {
        return;
    };
    let Ok(socket) = UdpSocket::bind(SocketAddr::new(LOOPBACK, 0)).await else {
        return;
    };

    let mut builder = AgentBuilder::new(api, socket).with_local_ip(LOOPBACK);
    let layers = simulcast.then(|| {
        vec![
            SimulcastLayer::new("f"),
            SimulcastLayer::new("h"),
            SimulcastLayer::new("q"),
        ]
    });
    builder = builder
        .with_track(MediaKind::Video, TransceiverDirection::SendOnly, layers)
        .with_track(MediaKind::Audio, TransceiverDirection::SendOnly, None);
    for _ in 0..7 {
        builder = builder.with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None);
    }
    for _ in 0..3 {
        builder = builder.with_track(MediaKind::Audio, TransceiverDirection::RecvOnly, None);
    }

    let Ok(mut driver) = builder.connect(&room).await else {
        return;
    };

    let mut media = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            event = driver.poll() => {
                match event {
                    Some(AgentEvent::LocalTrackAdded(track)) => {
                        if track.kind.is_video() {
                            media.spawn(assets.looper_for(&track.rid).run(track));
                        }
                    }
                    Some(AgentEvent::RemoteTrackAdded(track)) => {
                        media.spawn(drain_track(track, delivered.clone()));
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }
    media.shutdown().await;
}

async fn drain_track(mut track: RemoteTrack, delivered: Arc<AtomicU64>) {
    while let Ok(frame) = track.recv().await {
        delivered.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
    }
}

/// Keeps the load generator off the cores the shards were pinned to.
/// `NodeBuilder` pins shard `i` to core `i`, so the clients get the rest.
fn build_client_runtime(shards: usize) -> anyhow::Result<tokio::runtime::Runtime> {
    let cores = core_affinity::get_core_ids().unwrap_or_default();
    let client_cores: Vec<_> = cores.into_iter().skip(shards).collect();
    let worker_threads = client_cores.len().max(1);
    let next_core = Arc::new(AtomicU64::new(0));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .thread_name("pb-bench-client")
        .on_thread_start(move || {
            if client_cores.is_empty() {
                return;
            }
            let slot = next_core.fetch_add(1, Ordering::Relaxed) as usize;
            core_affinity::set_for_current(client_cores[slot % client_cores.len()]);
        })
        .build()?;
    Ok(runtime)
}

fn wait_for_shard_threads(expected: usize, before: &[u32]) -> anyhow::Result<DataPlaneClock> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Ok(clock) = DataPlaneClock::attach_since(before)
            && clock.thread_count() >= expected
        {
            return Ok(clock);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "node did not spawn {expected} shard threads within {STARTUP_TIMEOUT:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Every shard binds the same UDP port with `SO_REUSEPORT`, so the harness has
/// to pick a concrete free port rather than letting the kernel assign one per
/// socket.
fn reserve_udp_port() -> anyhow::Result<u16> {
    let probe = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    probe.bind(&SocketAddr::new(LOOPBACK, 0).into())?;
    let port = probe
        .local_addr()?
        .as_socket()
        .ok_or_else(|| anyhow::anyhow!("probe socket has no address"))?
        .port();
    drop(probe);
    Ok(port)
}

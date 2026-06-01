use anyhow::Result;
use clap::{Parser, Subcommand};
use core_affinity::{get_core_ids, set_for_current};
use hdrhistogram::Histogram;
use pulsebeam_agent::{
    MediaKind, Mid, Rid, SimulcastLayer, TransceiverDirection,
    actor::{AgentBuilder, AgentEvent, LocalTrack, RemoteTrackRx},
    api::HttpApiClient,
    media::H264Looper,
    wallclock_at,
};
use pulsebeam_core::net::UdpSocket;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::runtime::Builder;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::error;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[allow(non_upper_case_globals)]
#[unsafe(export_name = "malloc_conf")]
pub static malloc_conf: &[u8] = concat!(
    "lg_tcache_max:19,",
    "dirty_decay_ms:30000,",
    "muzzy_decay_ms:0,",
    "abort_conf:true",
    "\0"
)
.as_bytes();

#[derive(Parser)]
struct Cli {
    #[arg(short, long, default_value = "http://localhost:7070")]
    api_url: String,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Bench {
        #[arg(long, default_value_t = 5)]
        rooms: usize,
        #[arg(long, default_value_t = 4)]
        users_per_room: usize,
        #[arg(long, default_value_t = 0.05)]
        arrival_rate: f64,
        #[arg(long, default_value_t = 200)]
        max_rooms: usize,
        #[arg(long, default_value_t = 120)]
        session_duration: u64,
        #[arg(long, default_value_t = 60)]
        join_spread_secs: u64,
        #[arg(long, default_value_t = 30)]
        drain_duration: u64,
        #[arg(long)]
        simulcast: bool,
        #[arg(long, default_value_t = false)]
        fixed_session: bool,
    },
    Connect {
        #[arg(long)]
        room: String,
        #[arg(long)]
        publish: bool,
        #[arg(long)]
        simulcast: bool,
        #[arg(long, default_value_t = 7)]
        recv_video: usize,
        #[arg(long, default_value_t = 3)]
        recv_audio: usize,
    },
}

struct SharedState {
    active_rooms: AtomicUsize,
    active_agents: AtomicUsize,
    degraded: AtomicBool,
}

impl SharedState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            active_rooms: AtomicUsize::new(0),
            active_agents: AtomicUsize::new(0),
            degraded: AtomicBool::new(false),
        })
    }
}

const SILENT_TICKS_THRESHOLD: u32 = 3;
const HISTOGRAM_MAX_US: u64 = 10_000_000;
const HISTOGRAM_SIGFIG: u8 = 3;

#[derive(Debug)]
pub struct AgentDelta {
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub tx_packets: u64,
    pub rx_packets: u64,
    pub tx_nacks: u64,
    pub rx_nacks: u64,
    pub tx_plis: u64,
    pub rx_plis: u64,
    pub tx_active: usize,
    pub rx_active: usize,
    pub rx_loss_sum: f32,
    pub rx_loss_count: usize,
    pub forwarding_samples: Vec<u64>,
    pub rtt_samples: Vec<u64>,
}

impl Default for AgentDelta {
    fn default() -> Self {
        Self {
            tx_bytes: 0,
            rx_bytes: 0,
            tx_packets: 0,
            rx_packets: 0,
            tx_nacks: 0,
            rx_nacks: 0,
            tx_plis: 0,
            rx_plis: 0,
            tx_active: 0,
            rx_active: 0,
            rx_loss_sum: 0.0,
            rx_loss_count: 0,
            forwarding_samples: Vec::with_capacity(256),
            rtt_samples: Vec::with_capacity(8),
        }
    }
}

#[derive(Debug)]
pub struct AgentStatReport {
    pub agent_id: usize,
    pub delta: AgentDelta,
}

#[derive(Default)]
pub struct StatsProcessor {
    prev_tx_layers: HashMap<(Mid, Option<Rid>), LayerState>,
    prev_rx_layers: HashMap<(Mid, Option<Rid>), LayerState>,
}

#[derive(Default, Clone)]
struct LayerState {
    packets: u64,
    silent_ticks: u32,
}

impl StatsProcessor {
    pub fn process(
        &mut self,
        tx_bytes: u64,
        rx_bytes: u64,
        tx_layers: impl Iterator<Item = (Mid, Option<Rid>, u64, u64, u64)>,
        rx_layers: impl Iterator<Item = (Mid, Option<Rid>, u64, u64, u64, Option<f32>)>,
    ) -> AgentDelta {
        let mut delta = AgentDelta::default();

        delta.tx_bytes = tx_bytes;
        delta.rx_bytes = rx_bytes;

        for (mid, rid, packets, nacks, plis) in tx_layers {
            let prev = self.prev_tx_layers.entry((mid, rid)).or_default();
            let d_packets = packets.saturating_sub(prev.packets);
            delta.tx_packets += packets;
            delta.tx_nacks += nacks;
            delta.tx_plis += plis;

            prev.silent_ticks = if d_packets == 0 {
                prev.silent_ticks.saturating_add(1)
            } else {
                0
            };
            if d_packets > 0 || prev.silent_ticks < SILENT_TICKS_THRESHOLD {
                delta.tx_active += 1;
            }
            prev.packets = packets;
        }

        for (mid, rid, packets, nacks, plis, loss) in rx_layers {
            let prev = self.prev_rx_layers.entry((mid, rid)).or_default();
            let d_packets = packets.saturating_sub(prev.packets);
            delta.rx_packets += packets;
            delta.rx_nacks += nacks;
            delta.rx_plis += plis;

            prev.silent_ticks = if d_packets == 0 {
                prev.silent_ticks.saturating_add(1)
            } else {
                0
            };
            if d_packets > 0 || prev.silent_ticks < SILENT_TICKS_THRESHOLD {
                delta.rx_active += 1;
                if let Some(l) = loss {
                    delta.rx_loss_sum += l;
                    delta.rx_loss_count += 1;
                }
            }
            prev.packets = packets;
        }

        delta
    }
}

#[derive(Parser)]
#[allow(dead_code)]
struct FakeUnused {}

fn main() -> Result<()> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("pulsebeam=info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_ansi(true);

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer);
    registry.init();

    let cli = Cli::parse();

    let core_ids = get_core_ids().unwrap_or_default();
    let core_index = AtomicUsize::new(0);
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .enable_alt_timer()
        .on_thread_start(move || {
            if !core_ids.is_empty() {
                if let Some(core) =
                    core_ids.get(core_index.fetch_add(1, Ordering::Relaxed) % core_ids.len())
                {
                    set_for_current(*core);
                }
            }
        })
        .build()?;

    runtime.block_on(async move {
        match cli.command {
            Commands::Bench {
                rooms,
                users_per_room,
                arrival_rate,
                max_rooms,
                session_duration,
                join_spread_secs,
                drain_duration,
                simulcast,
                fixed_session,
            } => {
                run_bench(
                    cli.api_url,
                    rooms,
                    users_per_room,
                    arrival_rate,
                    max_rooms,
                    session_duration,
                    join_spread_secs,
                    drain_duration,
                    simulcast,
                    fixed_session,
                )
                .await?
            }
            Commands::Connect {
                room,
                publish,
                simulcast,
                recv_video,
                recv_audio,
            } => {
                run_connect(
                    cli.api_url,
                    room,
                    publish,
                    simulcast,
                    recv_video,
                    recv_audio,
                )
                .await?
            }
        }
        anyhow::Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_bench(
    api_url: String,
    initial_rooms: usize,
    users_per_room: usize,
    arrival_rate: f64,
    max_rooms: usize,
    session_duration: u64,
    join_spread_secs: u64,
    drain_duration: u64,
    simulcast: bool,
    fixed_session: bool,
) -> Result<()> {
    let (stats_tx, stats_rx) = mpsc::channel::<AgentStatReport>(64_000);
    let state = SharedState::new();
    let mut join_set = JoinSet::new();
    let room_counter = Arc::new(AtomicUsize::new(0));

    println!(
        "timestamp_s,rooms,agents,tx_mbps,rx_mbps,tx_pps,rx_pps,loss_pct,tx_nacks,rx_nacks,tx_plis,rx_plis,fwd_p50_ms,fwd_p95_ms,fwd_p99_ms,rtt_p50_ms,rtt_p95_ms,rtt_p99_ms,tx_active,rx_active"
    );

    let monitor_state = state.clone();
    let monitor_handle = tokio::spawn(monitor_task(stats_rx, monitor_state));

    let mut total_rooms = 0usize;
    let mut ramping = true;

    for _ in 0..initial_rooms {
        spawn_room(
            &mut join_set,
            &api_url,
            &room_counter,
            users_per_room,
            session_duration,
            join_spread_secs,
            &stats_tx,
            &state,
            simulcast,
            fixed_session,
        )
        .await;
        total_rooms += 1;
    }

    loop {
        let u = (rand::random_range(1u64..u64::MAX) as f64) / (u64::MAX as f64);
        let delay = Duration::from_secs_f64((-u.ln() / arrival_rate).max(0.001));
        tokio::time::sleep(delay).await;

        if ramping && state.degraded.load(Ordering::Relaxed) {
            ramping = false;
            eprintln!("DEGRADATION DETECTED — holding current load");
        }

        if !ramping || total_rooms >= max_rooms {
            eprintln!("Ramp ended — rooms: {} (max {})", total_rooms, max_rooms);
            break;
        }

        spawn_room(
            &mut join_set,
            &api_url,
            &room_counter,
            users_per_room,
            session_duration,
            join_spread_secs,
            &stats_tx,
            &state,
            simulcast,
            fixed_session,
        )
        .await;
        total_rooms += 1;
    }

    tokio::time::sleep(Duration::from_secs(drain_duration)).await;

    eprintln!(
        "Benchmark complete. Peak rooms: {}  Peak agents: {}",
        total_rooms,
        state.active_agents.load(Ordering::Relaxed),
    );

    join_set.shutdown().await;
    drop(stats_tx);
    let _ = monitor_handle.await;

    Ok(())
}

async fn spawn_room(
    join_set: &mut JoinSet<()>,
    api_url: &str,
    room_counter: &Arc<AtomicUsize>,
    users_per_room: usize,
    session_duration: u64,
    join_spread_secs: u64,
    stats_tx: &mpsc::Sender<AgentStatReport>,
    state: &Arc<SharedState>,
    simulcast: bool,
    fixed_session: bool,
) {
    let room_id = room_counter.fetch_add(1, Ordering::Relaxed);
    let room = format!("bench-room-{}", room_id);
    state.active_rooms.fetch_add(1, Ordering::Relaxed);

    for user_id in 0..users_per_room {
        let url = api_url.to_string();
        let r = room.clone();
        let tx = stats_tx.clone();
        let st = state.clone();

        let join_delay_ms = rand::random_range(0u64..(join_spread_secs * 1_000).max(1));

        let duration_secs = if fixed_session {
            session_duration.max(30)
        } else {
            let u = (rand::random_range(1u64..u64::MAX) as f64) / (u64::MAX as f64);
            ((-u.ln()) * session_duration as f64).max(30.0) as u64
        };

        join_set.spawn(async move {
            tokio::time::sleep(Duration::from_millis(join_delay_ms)).await;
            st.active_agents.fetch_add(1, Ordering::Relaxed);

            if let Err(e) = spawn_agent(
                room_id * 1000 + user_id,
                url,
                r,
                true,
                simulcast,
                Duration::from_secs(duration_secs),
                tx,
                users_per_room,
            )
            .await
            {
                error!("Agent {}/{} died: {:?}", room_id, user_id, e);
            }

            st.active_agents.fetch_sub(1, Ordering::Relaxed);
        });
    }

    let room_state = state.clone();
    let expire_after = Duration::from_secs(join_spread_secs + session_duration * 5 + 10);
    join_set.spawn(async move {
        tokio::time::sleep(expire_after).await;
        room_state.active_rooms.fetch_sub(1, Ordering::Relaxed);
    });
}

#[derive(Default)]
struct AgentHistory {
    tx_bytes: u64,
    rx_bytes: u64,
    tx_packets: u64,
    rx_packets: u64,
    tx_nacks: u64,
    rx_nacks: u64,
    tx_plis: u64,
    rx_plis: u64,
}

async fn monitor_task(mut stats_rx: mpsc::Receiver<AgentStatReport>, state: Arc<SharedState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let start = Instant::now();
    let mut last_interval = start;

    let mut forwarding_hist =
        Histogram::<u64>::new_with_max(HISTOGRAM_MAX_US, HISTOGRAM_SIGFIG).unwrap();
    let mut rtt_hist = Histogram::<u64>::new_with_max(HISTOGRAM_MAX_US, HISTOGRAM_SIGFIG).unwrap();

    let mut agent_latest: HashMap<usize, AgentDelta> = HashMap::new();
    let mut agent_prev: HashMap<usize, AgentHistory> = HashMap::new();

    let mut consecutive_high_p99 = 0u32;
    let mut consecutive_high_loss = 0u32;

    loop {
        tokio::select! {
            biased;

            now = interval.tick() => {
                let interval_secs = now.duration_since(last_interval).as_secs_f64();
                last_interval = now;

                if interval_secs <= 0.0 {
                    continue;
                }

                let elapsed = start.elapsed().as_secs();
                if elapsed == 0 { continue; }

                let rooms = state.active_rooms.load(Ordering::Relaxed);
                let agents = state.active_agents.load(Ordering::Relaxed);

                let mut total_tx_bytes_delta = 0u64;
                let mut total_rx_bytes_delta = 0u64;
                let mut total_tx_packets_delta = 0u64;
                let mut total_rx_packets_delta = 0u64;
                let mut total_tx_nacks_delta = 0u64;
                let mut total_rx_nacks_delta = 0u64;
                let mut total_tx_plis_delta = 0u64;
                let mut total_rx_plis_delta = 0u64;

                let mut tx_active_streams = 0usize;
                let mut rx_active_streams = 0usize;
                let mut rx_loss_sum = 0.0f32;
                let mut rx_loss_count = 0usize;

                for (&id, latest) in &agent_latest {
                    let prev = agent_prev.entry(id).or_default();

                    total_tx_bytes_delta += latest.tx_bytes.saturating_sub(prev.tx_bytes);
                    total_rx_bytes_delta += latest.rx_bytes.saturating_sub(prev.rx_bytes);
                    total_tx_packets_delta += latest.tx_packets.saturating_sub(prev.tx_packets);
                    total_rx_packets_delta += latest.rx_packets.saturating_sub(prev.rx_packets);
                    total_tx_nacks_delta += latest.tx_nacks.saturating_sub(prev.tx_nacks);
                    total_rx_nacks_delta += latest.rx_nacks.saturating_sub(prev.rx_nacks);
                    total_tx_plis_delta += latest.tx_plis.saturating_sub(prev.tx_plis);
                    total_rx_plis_delta += latest.rx_plis.saturating_sub(prev.rx_plis);

                    tx_active_streams += latest.tx_active;
                    rx_active_streams += latest.rx_active;
                    rx_loss_sum += latest.rx_loss_sum;
                    rx_loss_count += latest.rx_loss_count;

                    prev.tx_bytes = latest.tx_bytes;
                    prev.rx_bytes = latest.rx_bytes;
                    prev.tx_packets = latest.tx_packets;
                    prev.rx_packets = latest.rx_packets;
                    prev.tx_nacks = latest.tx_nacks;
                    prev.rx_nacks = latest.rx_nacks;
                    prev.tx_plis = latest.tx_plis;
                    prev.rx_plis = latest.rx_plis;
                }

                let tx_mbps = (total_tx_bytes_delta as f64 * 8.0) / 1_000_000.0 / interval_secs;
                let rx_mbps = (total_rx_bytes_delta as f64 * 8.0) / 1_000_000.0 / interval_secs;
                let tx_pps = total_tx_packets_delta as f64 / interval_secs;
                let rx_pps = total_rx_packets_delta as f64 / interval_secs;

                let (p50, p95, p99) = if forwarding_hist.len() > 0 {
                    (
                        Some(forwarding_hist.value_at_quantile(0.50) as f64 / 1000.0),
                        Some(forwarding_hist.value_at_quantile(0.95) as f64 / 1000.0),
                        Some(forwarding_hist.value_at_quantile(0.99) as f64 / 1000.0),
                    )
                } else {
                    (None, None, None)
                };

                let (rtt50, rtt95, rtt99) = if rtt_hist.len() > 0 {
                    (
                        Some(rtt_hist.value_at_quantile(0.50) as f64 / 1000.0),
                        Some(rtt_hist.value_at_quantile(0.95) as f64 / 1000.0),
                        Some(rtt_hist.value_at_quantile(0.99) as f64 / 1000.0),
                    )
                } else {
                    (None, None, None)
                };

                let avg_loss_pct = if rx_loss_count > 0 {
                    rx_loss_sum / rx_loss_count as f32 * 100.0
                } else {
                    0.0
                };

                let p50_str = p50.map(|v| format!("{:.3}", v)).unwrap_or_else(|| "NA".to_string());
                let p95_str = p95.map(|v| format!("{:.3}", v)).unwrap_or_else(|| "NA".to_string());
                let p99_str = p99.map(|v| format!("{:.3}", v)).unwrap_or_else(|| "NA".to_string());
                let rtt50_str = rtt50.map(|v| format!("{:.3}", v)).unwrap_or_else(|| "NA".to_string());
                let rtt95_str = rtt95.map(|v| format!("{:.3}", v)).unwrap_or_else(|| "NA".to_string());
                let rtt99_str = rtt99.map(|v| format!("{:.3}", v)).unwrap_or_else(|| "NA".to_string());

                println!(
                    "{},{},{},{:.3},{:.3},{:.1},{:.1},{:.2},{},{},{},{},{},{},{},{},{},{},{},{}",
                    elapsed, rooms, agents, tx_mbps, rx_mbps, tx_pps, rx_pps, avg_loss_pct,
                    total_tx_nacks_delta, total_rx_nacks_delta, total_tx_plis_delta, total_rx_plis_delta,
                    p50_str, p95_str, p99_str, rtt50_str, rtt95_str, rtt99_str,
                    tx_active_streams, rx_active_streams,
                );

                if p99.is_some_and(|v| v > 100.0) {
                    consecutive_high_p99 += 1;
                    if consecutive_high_p99 >= 3 {
                        state.degraded.store(true, Ordering::Relaxed);
                    }
                } else {
                    consecutive_high_p99 = 0;
                }

                if avg_loss_pct > 5.0 && agents > 0 {
                    consecutive_high_loss += 1;
                    if consecutive_high_loss >= 3 {
                        state.degraded.store(true, Ordering::Relaxed);
                    }
                } else {
                    consecutive_high_loss = 0;
                }

                forwarding_hist.reset();
                rtt_hist.reset();
            }

            Some(report) = stats_rx.recv() => {
                for sample in &report.delta.forwarding_samples {
                    let _ = forwarding_hist.record(*sample);
                }
                for sample in &report.delta.rtt_samples {
                    let _ = rtt_hist.record(*sample);
                }
                agent_latest.insert(report.agent_id, report.delta);
            }
        }
    }
}

async fn spawn_agent(
    id: usize,
    api_url: String,
    room: String,
    is_pub: bool,
    simulcast: bool,
    session_duration: Duration,
    stats_tx: mpsc::Sender<AgentStatReport>,
    _users_per_room: usize,
) -> Result<()> {
    let api = HttpApiClient::new(Box::new(reqwest::Client::new()), &api_url)?;
    let socket = UdpSocket::bind("0.0.0.0:0").await?;

    let mut builder = AgentBuilder::new(api, socket).with_local_ip("127.0.0.1".parse().unwrap());

    if is_pub {
        if simulcast {
            builder = builder.with_track(
                MediaKind::Video,
                TransceiverDirection::SendOnly,
                Some(vec![
                    SimulcastLayer::new("f"),
                    SimulcastLayer::new("h"),
                    SimulcastLayer::new("q"),
                ]),
            );
        } else {
            builder = builder.with_track(MediaKind::Video, TransceiverDirection::SendOnly, None);
        }
        builder = builder.with_track(MediaKind::Audio, TransceiverDirection::SendOnly, None);
    }

    for _ in 0..7 {
        builder = builder.with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None);
    }

    for _ in 0..3 {
        builder = builder.with_track(MediaKind::Audio, TransceiverDirection::RecvOnly, None);
    }

    let mut agent = builder.connect(&room).await?;
    let mut stats_processor = StatsProcessor::default();
    let mut stats_interval = tokio::time::interval(Duration::from_secs(5));
    stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let (sample_tx, mut sample_rx) = mpsc::channel::<u64>(4096);

    let session_end = tokio::time::sleep(session_duration);
    tokio::pin!(session_end);

    loop {
        tokio::select! {
            _ = &mut session_end => break,

            Some(event) = agent.next_event() => {
                match event {
                    AgentEvent::LocalTrackAdded(track) => {
                        tokio::spawn(handle_local_track(track));
                    }
                    AgentEvent::RemoteTrackAdded(recv) => {
                        let sample_tx = sample_tx.clone();
                        tokio::spawn(handle_remote_track(recv, sample_tx));
                    }
                    _ => {}
                }
            }

            _ = stats_interval.tick() => {
                let Some(stats) = agent.get_stats().await else { continue };

                let mut delta = {
                    let peer = stats.peer.as_ref();
                    let tx_bytes = peer.map(|p| p.bytes_tx).unwrap_or(0);
                    let rx_bytes = peer.map(|p| p.bytes_rx).unwrap_or(0);

                    let tx_iter = stats.tracks.iter().flat_map(|(mid, track)| {
                        track.tx_layers.iter().map(move |(rid, egress)| {
                            (*mid, *rid, egress.packets, egress.nacks, egress.plis)
                        })
                    });

                    let rx_iter = stats.tracks.iter().flat_map(|(mid, track)| {
                        track.rx_layers.iter().map(move |(rid, ingress)| {
                            (*mid, *rid, ingress.packets, ingress.nacks, ingress.plis, ingress.loss)
                        })
                    });

                    let mut d = stats_processor.process(tx_bytes, rx_bytes, tx_iter, rx_iter);
                    if let Some(peer_stats) = peer {
                        if let Some(rtt) = peer_stats.rtt {
                            d.rtt_samples.push(rtt.as_micros() as u64);
                        }
                    }
                    d
                };

                while let Ok(sample) = sample_rx.try_recv() {
                    delta.forwarding_samples.push(sample);
                }

                if stats_tx.try_send(AgentStatReport { agent_id: id, delta }).is_err() {
                    tracing::warn!("stats channel full, dropping report");
                }
            }
        }
    }

    Ok(())
}

async fn run_connect(
    api_url: String,
    room: String,
    publish: bool,
    simulcast: bool,
    recv_video: usize,
    recv_audio: usize,
) -> Result<()> {
    let api = HttpApiClient::new(Box::new(reqwest::Client::new()), &api_url)?;
    let socket = UdpSocket::bind("0.0.0.0:0").await?;

    let mut builder = AgentBuilder::new(api, socket).with_local_ip("127.0.0.1".parse().unwrap());

    if publish {
        if simulcast {
            builder = builder.with_track(
                MediaKind::Video,
                TransceiverDirection::SendOnly,
                Some(vec![
                    SimulcastLayer::new("f"),
                    SimulcastLayer::new("h"),
                    SimulcastLayer::new("q"),
                ]),
            );
        } else {
            builder = builder.with_track(MediaKind::Video, TransceiverDirection::SendOnly, None);
        }
    }

    for _ in 0..recv_video {
        builder = builder.with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None);
    }
    for _ in 0..recv_audio {
        builder = builder.with_track(MediaKind::Audio, TransceiverDirection::RecvOnly, None);
    }

    let mut agent = builder.connect(&room).await?;
    eprintln!(
        "Connected to room '{}' (participant: {})",
        room,
        agent.participant_id()
    );
    eprintln!("Press Ctrl-C to disconnect.");
    eprintln!(
        "{:>8} {:>8} {:>8} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "time(s)",
        "tx_mbps",
        "rx_mbps",
        "tx_pps",
        "rx_pps",
        "loss%",
        "tx_nack",
        "rx_nack",
        "tx_pli",
        "rx_pli",
        "FWD50ms",
        "FWD95ms",
        "FWD99ms",
        "RTT50ms",
        "RTT95ms",
        "RTT99ms",
        "tx_act",
        "rx_act"
    );

    let mut stats_processor = StatsProcessor::default();
    let mut stats_interval = tokio::time::interval(Duration::from_secs(1));
    let start = Instant::now();
    let mut last_stats = start;

    let mut prev_history = AgentHistory::default();

    let mut forwarding_hist =
        Histogram::<u64>::new_with_max(HISTOGRAM_MAX_US, HISTOGRAM_SIGFIG).unwrap();
    let mut rtt_hist = Histogram::<u64>::new_with_max(HISTOGRAM_MAX_US, HISTOGRAM_SIGFIG).unwrap();
    let (sample_tx, mut sample_rx) = mpsc::channel::<u64>(4096);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\nDisconnecting…");
                agent.disconnect().await?;
                break;
            }

            Some(event) = agent.next_event() => {
                match event {
                    AgentEvent::LocalTrackAdded(track) => {
                        tokio::spawn(handle_local_track(track));
                    }
                    AgentEvent::RemoteTrackAdded(recv) => {
                        let sample_tx = sample_tx.clone();
                        tokio::spawn(handle_remote_track(recv, sample_tx));
                    }
                    AgentEvent::Disconnected(reason) => {
                        eprintln!("Disconnected: {reason}");
                        break;
                    }
                    _ => {}
                }
            }

            _ = stats_interval.tick() => {
                let now = Instant::now();
                let interval_secs = now.duration_since(last_stats).as_secs_f64();
                last_stats = now;

                if interval_secs <= 0.0 {
                    continue;
                }

                let elapsed = start.elapsed().as_secs();
                let Some(stats) = agent.get_stats().await else { continue };

                let peer = stats.peer.as_ref();
                if let Some(peer_stats) = peer {
                    if let Some(rtt) = peer_stats.rtt {
                        let _ = rtt_hist.record(rtt.as_micros() as u64);
                    }
                }

                while let Ok(sample) = sample_rx.try_recv() {
                    let _ = forwarding_hist.record(sample);
                }

                let tx_bytes = peer.map(|p| p.bytes_tx).unwrap_or(0);
                let rx_bytes = peer.map(|p| p.bytes_rx).unwrap_or(0);

                let tx_iter = stats.tracks.iter().flat_map(|(mid, track)| {
                    track.tx_layers.iter().map(move |(rid, egress)| {
                        (*mid, *rid, egress.packets, egress.nacks, egress.plis)
                    })
                });

                let rx_iter = stats.tracks.iter().flat_map(|(mid, track)| {
                    track.rx_layers.iter().map(move |(rid, ingress)| {
                        (*mid, *rid, ingress.packets, ingress.nacks, ingress.plis, ingress.loss)
                    })
                });

                let latest = stats_processor.process(tx_bytes, rx_bytes, tx_iter, rx_iter);

                let d_tx_bytes = latest.tx_bytes.saturating_sub(prev_history.tx_bytes);
                let d_rx_bytes = latest.rx_bytes.saturating_sub(prev_history.rx_bytes);
                let d_tx_packets = latest.tx_packets.saturating_sub(prev_history.tx_packets);
                let d_rx_packets = latest.rx_packets.saturating_sub(prev_history.rx_packets);
                let d_tx_nacks = latest.tx_nacks.saturating_sub(prev_history.tx_nacks);
                let d_rx_nacks = latest.rx_nacks.saturating_sub(prev_history.rx_nacks);
                let d_tx_plis = latest.tx_plis.saturating_sub(prev_history.tx_plis);
                let d_rx_plis = latest.rx_plis.saturating_sub(prev_history.rx_plis);

                prev_history.tx_bytes = latest.tx_bytes;
                prev_history.rx_bytes = latest.rx_bytes;
                prev_history.tx_packets = latest.tx_packets;
                prev_history.rx_packets = latest.rx_packets;
                prev_history.tx_nacks = latest.tx_nacks;
                prev_history.rx_nacks = latest.rx_nacks;
                prev_history.tx_plis = latest.tx_plis;
                prev_history.rx_plis = latest.rx_plis;

                let (p50, p95, p99) = if forwarding_hist.len() > 0 {
                    (
                        Some(forwarding_hist.value_at_quantile(0.50) as f64 / 1000.0),
                        Some(forwarding_hist.value_at_quantile(0.95) as f64 / 1000.0),
                        Some(forwarding_hist.value_at_quantile(0.99) as f64 / 1000.0),
                    )
                } else {
                    (None, None, None)
                };

                let (rtt50, rtt95, rtt99) = if rtt_hist.len() > 0 {
                    (
                        Some(rtt_hist.value_at_quantile(0.50) as f64 / 1000.0),
                        Some(rtt_hist.value_at_quantile(0.95) as f64 / 1000.0),
                        Some(rtt_hist.value_at_quantile(0.99) as f64 / 1000.0),
                    )
                } else {
                    (None, None, None)
                };

                let tx_mbps = (d_tx_bytes as f64 * 8.0) / 1_000_000.0 / interval_secs;
                let rx_mbps = (d_rx_bytes as f64 * 8.0) / 1_000_000.0 / interval_secs;
                let tx_pps = d_tx_packets as f64 / interval_secs;
                let rx_pps = d_rx_packets as f64 / interval_secs;

                let avg_loss_pct = if latest.rx_loss_count > 0 {
                    latest.rx_loss_sum / latest.rx_loss_count as f32 * 100.0
                } else {
                    0.0
                };

                let p50_str = p50.map(|v| format!("{:>10.3}", v)).unwrap_or_else(|| "        NA".to_string());
                let p95_str = p95.map(|v| format!("{:>10.3}", v)).unwrap_or_else(|| "        NA".to_string());
                let p99_str = p99.map(|v| format!("{:>10.3}", v)).unwrap_or_else(|| "        NA".to_string());
                let rtt50_str = rtt50.map(|v| format!("{:>10.3}", v)).unwrap_or_else(|| "        NA".to_string());
                let rtt95_str = rtt95.map(|v| format!("{:>10.3}", v)).unwrap_or_else(|| "        NA".to_string());
                let rtt99_str = rtt99.map(|v| format!("{:>10.3}", v)).unwrap_or_else(|| "        NA".to_string());

                eprintln!(
                    "{:>8} {:>8.2} {:>8.2} {:>10.0} {:>10.0} {:>10.2} {:>10} {:>10} {:>10} {:>10} {} {} {} {} {} {} {:>10} {:>10}",
                    elapsed, tx_mbps, rx_mbps, tx_pps, rx_pps, avg_loss_pct,
                    d_tx_nacks, d_rx_nacks, d_tx_plis, d_rx_plis,
                    p50_str, p95_str, p99_str,
                    rtt50_str, rtt95_str, rtt99_str,
                    latest.tx_active, latest.rx_active
                );

                forwarding_hist.reset();
                rtt_hist.reset();
            }
        }
    }

    Ok(())
}

async fn handle_local_track(track: LocalTrack) {
    if track.kind.is_audio() {
        return;
    }

    let rid_str = track.rid.as_ref().map(|r| r.as_ref());
    let data = match rid_str {
        Some("f") => pulsebeam_testdata::RAW_H264_FULL_CBR,
        Some("h") => pulsebeam_testdata::RAW_H264_HALF_CBR,
        Some("q") => pulsebeam_testdata::RAW_H264_QUARTER_CBR,
        _ => pulsebeam_testdata::RAW_H264_HALF_CBR,
    };
    let looper = H264Looper::new(data, 30);
    let _ = looper.run(track).await;
}

async fn handle_remote_track(mut recv: RemoteTrackRx, sample_tx: mpsc::Sender<u64>) {
    while let Some(frame) = recv.recv().await {
        if let Some(abs_capture_time) = frame.abs_capture_time {
            let receive_time = wallclock_at(tokio::time::Instant::now());
            if let Ok(latency) = receive_time.duration_since(abs_capture_time) {
                let _ = sample_tx.try_send(latency.as_micros() as u64);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_processor_deltas() {
        let mut p = StatsProcessor::default();

        let tx_layers = vec![(Mid::from("track_1"), None, 100, 1, 2)];
        let rx_layers = vec![(Mid::from("track_2"), None, 200, 3, 4, Some(0.05))];

        let delta = p.process(1000, 2000, tx_layers.into_iter(), rx_layers.into_iter());

        assert_eq!(delta.tx_bytes, 1000);
        assert_eq!(delta.rx_bytes, 2000);
        assert_eq!(delta.tx_packets, 100);
        assert_eq!(delta.tx_nacks, 1);
        assert_eq!(delta.tx_plis, 2);
        assert_eq!(delta.rx_packets, 200);
        assert_eq!(delta.rx_nacks, 3);
        assert_eq!(delta.rx_plis, 4);
        assert_eq!(delta.tx_active, 1);
        assert_eq!(delta.rx_active, 1);
        assert_eq!(delta.rx_loss_sum, 0.05);
        assert_eq!(delta.rx_loss_count, 1);
    }
}

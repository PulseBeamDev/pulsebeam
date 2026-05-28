use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
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
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::error;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[allow(non_upper_case_globals)]
#[unsafe(export_name = "malloc_conf")]
pub static malloc_conf: &[u8] = concat!(
    "lg_tcache_max:19,", // 512KB limit: buffers GRO/GSO packets & hash expansions lock-free
    "dirty_decay_ms:30000,", // Soft 1s amortization window prevents huge inline purge spikes
    "muzzy_decay_ms:0,", // Bypass the unpredictable kernel muzzy gray-zone entirely
    "abort_conf:true",   // Safely crash on boot if any setting above is invalid
    "\0"                 // Null-terminator required for C-compatibility
)
.as_bytes();

#[derive(Parser)]
struct Cli {
    #[arg(short, long, default_value = "http://localhost:7070")]
    api_url: String,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, Default, ValueEnum)]
enum OutputFormat {
    #[default]
    Human,
    Csv,
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
        #[arg(long, default_value = "human")]
        output_format: OutputFormat,
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

#[derive(Default, Debug, PartialEq)]
pub struct AgentDelta {
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub tx_nacks: u64,
    pub rx_nacks: u64,
    pub tx_plis: u64,
    pub rx_plis: u64,
    pub tx_active: usize,
    pub rx_active: usize,
    pub rx_loss_sum: f32,
    pub rx_loss_count: usize,
}

#[derive(Debug)]
pub struct AgentStatReport {
    pub forwarding_latencies: Vec<Duration>,
    pub delta: AgentDelta,
}

#[derive(Default)]
pub struct StatsProcessor {
    prev_tx_bytes: u64,
    prev_rx_bytes: u64,
    prev_tx_layers: HashMap<(Mid, Option<Rid>), LayerState>,
    prev_rx_layers: HashMap<(Mid, Option<Rid>), LayerState>,
}

#[derive(Default, Clone)]
struct LayerState {
    packets: u64,
    nacks: u64,
    plis: u64,
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

        delta.tx_bytes = tx_bytes.saturating_sub(self.prev_tx_bytes);
        delta.rx_bytes = rx_bytes.saturating_sub(self.prev_rx_bytes);
        self.prev_tx_bytes = tx_bytes;
        self.prev_rx_bytes = rx_bytes;

        for (mid, rid, packets, nacks, plis) in tx_layers {
            let prev = self.prev_tx_layers.entry((mid, rid)).or_default();
            let d_packets = packets.saturating_sub(prev.packets);
            delta.tx_nacks += nacks.saturating_sub(prev.nacks);
            delta.tx_plis += plis.saturating_sub(prev.plis);

            prev.silent_ticks = if d_packets == 0 {
                prev.silent_ticks.saturating_add(1)
            } else {
                0
            };
            if d_packets > 0 || prev.silent_ticks < SILENT_TICKS_THRESHOLD {
                delta.tx_active += 1;
            }
            prev.packets = packets;
            prev.nacks = nacks;
            prev.plis = plis;
        }

        for (mid, rid, packets, nacks, plis, loss) in rx_layers {
            let prev = self.prev_rx_layers.entry((mid, rid)).or_default();
            let d_packets = packets.saturating_sub(prev.packets);
            delta.rx_nacks += nacks.saturating_sub(prev.nacks);
            delta.rx_plis += plis.saturating_sub(prev.plis);

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
            prev.nacks = nacks;
            prev.plis = plis;
        }

        delta
    }
}

#[derive(Parser)]
#[allow(dead_code)]
struct FakeUnused {}

#[tokio::main]
async fn main() -> Result<()> {
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
            output_format,
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
                output_format,
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
    Ok(())
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
    output_format: OutputFormat,
    fixed_session: bool,
) -> Result<()> {
    let (stats_tx, stats_rx) = mpsc::channel::<AgentStatReport>(64_000);
    let state = SharedState::new();
    let mut join_set = JoinSet::new();
    let room_counter = Arc::new(AtomicUsize::new(0));

    if matches!(output_format, OutputFormat::Human) {
        println!(
            "┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐"
        );
        println!(
            "│  PulseBeam SFU Breaking-Point Benchmark  │  Multi-Room Meeting  │  Ramping until degradation                                                  │"
        );
        println!(
            "├───────┬───────┬────────┬─────────┬─────────┬────────┬────────┬────────┬────────┬────────┬─────────┬─────────┬─────────┬─────────┬─────────┤"
        );
        println!(
            "│  Time │ Rooms │ Agents │ Tx Mbps │ Rx Mbps │ Loss % │ Tx NACK│ Rx NACK│ Tx PLI │ Rx PLI │ FWD50ms │ FWD95ms │ FWD99ms │ Tx Actv │ Rx Actv │"
        );
        println!(
            "├───────┼───────┼────────┼─────────┼─────────┼────────┼────────┼────────┼────────┼────────┼─────────┼─────────┼─────────┼─────────┼─────────┤"
        );
    }

    let monitor_state = state.clone();
    let monitor_handle = tokio::spawn(monitor_task(stats_rx, monitor_state, output_format));

    let mut total_rooms = 0usize;

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

        // if state.degraded.load(Ordering::Relaxed) {
        //     if matches!(output_format, OutputFormat::Human) {
        //         println!("│ {:<140} │", "⚠  DEGRADATION DETECTED — stopping ramp");
        //     } else {
        //         eprintln!("DEGRADATION DETECTED — stopping ramp");
        //     }
        //     break;
        // }
        if total_rooms >= max_rooms {
            if matches!(output_format, OutputFormat::Human) {
                println!(
                    "│ {:<140} │",
                    format!("✓  Max rooms reached ({}) — stopping ramp", max_rooms)
                );
            } else {
                eprintln!("Max rooms reached ({}) — stopping ramp", max_rooms);
            }
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

    if matches!(output_format, OutputFormat::Human) {
        println!(
            "├───────┴───────┴────────┴─────────┴─────────┴────────┴────────┴────────┴────────┴────────┴─────────┴─────────┴─────────┴─────────┴─────────┤"
        );
        println!(
            "│ {:<140} │",
            format!(
                "Benchmark complete. Peak rooms: {}  Peak agents: {}",
                total_rooms,
                state.active_agents.load(Ordering::Relaxed)
            )
        );
        println!(
            "└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘"
        );
    } else {
        eprintln!(
            "Benchmark complete. Peak rooms: {}  Peak agents: {}",
            total_rooms,
            state.active_agents.load(Ordering::Relaxed),
        );
    }

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

async fn monitor_task(
    mut stats_rx: mpsc::Receiver<AgentStatReport>,
    state: Arc<SharedState>,
    output_format: OutputFormat,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let start = Instant::now();

    let mut tx_bytes = 0u64;
    let mut rx_bytes = 0u64;
    let mut tx_nacks = 0u64;
    let mut rx_nacks = 0u64;
    let mut tx_plis = 0u64;
    let mut rx_plis = 0u64;

    let mut tx_active_streams = 0usize;
    let mut rx_active_streams = 0usize;

    let mut rx_loss_sum = 0.0f32;
    let mut rx_loss_count = 0usize;

    let mut fwd_hist = Histogram::<u64>::new_with_bounds(1, 10_000_000, 3).unwrap();
    let mut fwd_has_samples = false;
    let mut consecutive_high_p99 = 0u32;
    let mut consecutive_high_loss = 0u32;

    if matches!(output_format, OutputFormat::Csv) {
        println!(
            "timestamp_s,rooms,agents,tx_mbps,rx_mbps,loss_pct,tx_nacks,rx_nacks,tx_plis,rx_plis,fwd_p50_ms,fwd_p95_ms,fwd_p99_ms,tx_active,rx_active"
        );
    }

    loop {
        tokio::select! {
            biased;

            _ = interval.tick() => {
                let elapsed = start.elapsed().as_secs();
                if elapsed == 0 { continue; }

                let rooms  = state.active_rooms.load(Ordering::Relaxed);
                let agents = state.active_agents.load(Ordering::Relaxed);

                let tx_mbps = (tx_bytes * 8) as f64 / 1_000_000.0;
                let rx_mbps = (rx_bytes * 8) as f64 / 1_000_000.0;

                let (p50, p95, p99) = if fwd_has_samples {
                    (
                        Some(fwd_hist.value_at_quantile(0.50) as f64 / 1000.0),
                        Some(fwd_hist.value_at_quantile(0.95) as f64 / 1000.0),
                        Some(fwd_hist.value_at_quantile(0.99) as f64 /1000.0),
                    )
                } else {
                    (None, None, None)
                };

                let avg_loss_pct = if rx_loss_count > 0 {
                    rx_loss_sum / rx_loss_count as f32 * 100.0
                } else {
                    0.0
                };

                match output_format {
                    OutputFormat::Human => {
                        let p50_str = p50.map(|v| format!("{:>7.0}", v)).unwrap_or_else(|| "    NA ".to_string());
                        let p95_str = p95.map(|v| format!("{:>7.0}", v)).unwrap_or_else(|| "    NA ".to_string());
                        let p99_str = p99.map(|v| format!("{:>7.0}", v)).unwrap_or_else(|| "    NA ".to_string());
                        println!(
                            "│ {:>4}s │ {:>5} │ {:>6} │ {:>7.2} │ {:>7.2} │ {:>6.2} │ {:>6} │ {:>6} │ {:>6} │ {:>6} │{} │{} │{} │ {:>7} │ {:>7} │",
                            elapsed, rooms, agents,
                            tx_mbps, rx_mbps, avg_loss_pct,
                            tx_nacks, rx_nacks, tx_plis, rx_plis,
                            p50_str, p95_str, p99_str,
                            tx_active_streams, rx_active_streams,
                        );
                    }
                    OutputFormat::Csv => {
                        let p50_str = p50.map(|v| format!("{:.3}", v)).unwrap_or_else(|| "NA".to_string());
                        let p95_str = p95.map(|v| format!("{:.3}", v)).unwrap_or_else(|| "NA".to_string());
                        let p99_str = p99.map(|v| format!("{:.3}", v)).unwrap_or_else(|| "NA".to_string());
                        println!(
                            "{},{},{},{:.3},{:.3},{:.2},{},{},{},{},{},{},{},{},{}",
                            elapsed, rooms, agents,
                            tx_mbps, rx_mbps, avg_loss_pct,
                            tx_nacks, rx_nacks, tx_plis, rx_plis,
                            p50_str, p95_str, p99_str,
                            tx_active_streams, rx_active_streams,
                        );
                    }
                }

                if p99.is_some_and(|v| v > 300.0) {
                    consecutive_high_p99 += 1;
                    if consecutive_high_p99 >= 3 {
                        state.degraded.store(true, Ordering::Relaxed);
                    }
                } else {
                    consecutive_high_p99 = 0;
                }

                fwd_has_samples = false;

                if avg_loss_pct > 5.0 && agents > 0 {
                    consecutive_high_loss += 1;
                    if consecutive_high_loss >= 3 {
                        state.degraded.store(true, Ordering::Relaxed);
                    }
                } else {
                    consecutive_high_loss = 0;
                }

                tx_bytes = 0; rx_bytes = 0;
                tx_nacks = 0; rx_nacks = 0;
                tx_plis  = 0; rx_plis  = 0;
                tx_active_streams  = 0; rx_active_streams = 0;
                rx_loss_sum = 0.0; rx_loss_count = 0;
                fwd_hist.reset();
            }

            Some(report) = stats_rx.recv() => {
                for latency in report.forwarding_latencies {
                    if fwd_hist.record(latency.as_micros() as u64).is_ok() {
                        fwd_has_samples = true;
                    }
                }
                tx_bytes += report.delta.tx_bytes;
                rx_bytes += report.delta.rx_bytes;
                tx_nacks += report.delta.tx_nacks;
                rx_nacks += report.delta.rx_nacks;
                tx_plis += report.delta.tx_plis;
                rx_plis += report.delta.rx_plis;
                tx_active_streams += report.delta.tx_active;
                rx_active_streams += report.delta.rx_active;
                rx_loss_sum += report.delta.rx_loss_sum;
                rx_loss_count += report.delta.rx_loss_count;
            }
        }
    }
}

async fn spawn_agent(
    _id: usize,
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
    let mut stats_interval = tokio::time::interval(Duration::from_secs(1));
    let (latency_tx, mut latency_rx) = mpsc::unbounded_channel::<Duration>();

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
                        let latency_tx = latency_tx.clone();
                        tokio::spawn(handle_remote_track(recv, latency_tx));
                    }
                    _ => {}
                }
            }

            _ = stats_interval.tick() => {
                let Some(stats) = agent.get_stats().await else { continue };

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

                let delta = stats_processor.process(tx_bytes, rx_bytes, tx_iter, rx_iter);

                let mut forwarding_latencies = Vec::new();
                while let Ok(sample) = latency_rx.try_recv() {
                    forwarding_latencies.push(sample);
                }

                if stats_tx.try_send(AgentStatReport { forwarding_latencies, delta }).is_err() {
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
        "{:>8} {:>8} {:>8} {:>10} {:>10} {:>10} {:>9}  streams (tx/rx)",
        "time(s)", "tx_mbps", "rx_mbps", "FWD50us", "FWD95us", "FWD99us", "loss%"
    );

    let mut stats_processor = StatsProcessor::default();
    let mut stats_interval = tokio::time::interval(Duration::from_secs(1));
    let start = Instant::now();
    let mut fwd_hist = Histogram::<u64>::new_with_bounds(1, 10_000_000, 3).unwrap();
    let mut fwd_has_samples = false;
    let (latency_tx, mut latency_rx) = mpsc::unbounded_channel::<Duration>();

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
                        let latency_tx = latency_tx.clone();
                        tokio::spawn(handle_remote_track(recv, latency_tx));
                    }
                    AgentEvent::Disconnected(reason) => {
                        eprintln!("Disconnected: {reason}");
                        break;
                    }
                    _ => {}
                }
            }

            _ = stats_interval.tick() => {
                let elapsed = start.elapsed().as_secs();
                let Some(stats) = agent.get_stats().await else { continue };

                while let Ok(sample) = latency_rx.try_recv() {
                    if fwd_hist.record(sample.as_micros() as u64).is_ok() {
                        fwd_has_samples = true;
                    }
                }

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

                let delta = stats_processor.process(tx_bytes, rx_bytes, tx_iter, rx_iter);

                let tx_mbps = (delta.tx_bytes * 8) as f64 / 1_000_000.0;
                let rx_mbps = (delta.rx_bytes * 8) as f64 / 1_000_000.0;

                let (p50, p95, p99) = if fwd_has_samples {
                    (
                        Some(fwd_hist.value_at_quantile(0.50) as f64 / 1000.0),
                        Some(fwd_hist.value_at_quantile(0.95) as f64 / 1000.0),
                        Some(fwd_hist.value_at_quantile(0.99) as f64 / 1000.0),
                    )
                } else {
                    (None, None, None)
                };
                fwd_hist.reset();
                fwd_has_samples = false;

                let avg_loss_pct = if delta.rx_loss_count > 0 {
                    delta.rx_loss_sum / delta.rx_loss_count as f32 * 100.0
                } else {
                    0.0
                };

                let p50_str = p50.map(|v| format!("{:>10.3}", v)).unwrap_or_else(|| "         NA".to_string());
                let p95_str = p95.map(|v| format!("{:>10.3}", v)).unwrap_or_else(|| "         NA".to_string());
                let p99_str = p99.map(|v| format!("{:>10.3}", v)).unwrap_or_else(|| "         NA".to_string());
                eprintln!("{:>8} {:>8.2} {:>8.2} {} {} {} {:>8.1}%  {}/{}",
                          elapsed, tx_mbps, rx_mbps, p50_str, p95_str, p99_str, avg_loss_pct,
                          delta.tx_active, delta.rx_active);
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
        _ => pulsebeam_testdata::RAW_H264_QUARTER_CBR,
    };
    let looper = H264Looper::new(data, 30);
    let _ = looper.run(track).await;
}

async fn handle_remote_track(
    mut recv: RemoteTrackRx,
    latency_tx: tokio::sync::mpsc::UnboundedSender<Duration>,
) {
    while let Some(frame) = recv.recv().await {
        if let Some(abs_capture_time) = frame.abs_capture_time {
            let receive_time = wallclock_at(frame.capture_time);
            if let Ok(latency) = receive_time.duration_since(abs_capture_time) {
                let _ = latency_tx.send(latency);
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
        assert_eq!(delta.tx_nacks, 1);
        assert_eq!(delta.tx_plis, 2);
        assert_eq!(delta.rx_nacks, 3);
        assert_eq!(delta.rx_plis, 4);
        assert_eq!(delta.tx_active, 1);
        assert_eq!(delta.rx_active, 1);
        assert_eq!(delta.rx_loss_sum, 0.05);
        assert_eq!(delta.rx_loss_count, 1);
    }
}

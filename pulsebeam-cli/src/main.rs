use anyhow::Result;
use clap::{Parser, Subcommand};
use pulsebeam_agent::{
    MediaKind, Rid, TransceiverDirection,
    actor::{AgentBuilder, AgentEvent, LocalTrack},
    api::HttpApiClient,
    media::H264Looper,
};
use pulsebeam_core::net::UdpSocket;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::error;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
struct Cli {
    #[arg(short, long, default_value = "http://localhost:3000")]
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
        #[arg(long, default_value_t = 5)]
        ramp_step: usize,
        #[arg(long, default_value_t = 20)]
        ramp_interval: u64,
        #[arg(long, default_value_t = 200)]
        max_rooms: usize,
        #[arg(long, default_value_t = 120)]
        session_duration: u64,
        #[arg(long, default_value_t = 30)]
        session_jitter: u64,
        #[arg(long, default_value_t = 30)]
        drain_duration: u64,
    },
}

// ── Stats ─────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum StatReport {
    Peer {
        rtt_ms: Option<u128>,
    },
    Tx {
        bytes: u64,
        nacks: u64,
        plis: u64,
    },
    Rx {
        bytes: u64,
        packets: u64,
        packets_lost: f32,
        nacks: u64,
        plis: u64,
    },
    StreamHealth {
        tx_active: bool,
        tx_healthy: bool,
        rx_active: bool,
        rx_healthy: bool,
    },
    DegradationDetected {
        reason: String,
    },
}

// ── Shared state ──────────────────────────────────────────────────────────────

struct SharedState {
    active_rooms: AtomicUsize,
    active_agents: AtomicUsize,
    degraded: AtomicBool,
    tx_active_streams: AtomicUsize,
    tx_healthy_streams: AtomicUsize,
    rx_active_streams: AtomicUsize,
    rx_healthy_streams: AtomicUsize,
}

impl SharedState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            active_rooms: AtomicUsize::new(0),
            active_agents: AtomicUsize::new(0),
            degraded: AtomicBool::new(false),
            tx_active_streams: AtomicUsize::new(0),
            tx_healthy_streams: AtomicUsize::new(0),
            rx_active_streams: AtomicUsize::new(0),
            rx_healthy_streams: AtomicUsize::new(0),
        })
    }
}

// ── Latency histogram (lock-free buckets) ─────────────────────────────────────

#[derive(Default)]
struct LatencyHistogram {
    // buckets: [0,1), [1,2), [2,4), [4,8) ... [512,1024), [1024,∞) ms
    buckets: [u64; 12],
    count: u64,
    sum: u128,
}

impl LatencyHistogram {
    fn record(&mut self, ms: u128) {
        self.sum += ms;
        self.count += 1;
        let bucket = if ms == 0 {
            0
        } else {
            ((ms as f64).log2().ceil() as usize).min(11)
        };
        self.buckets[bucket] += 1;
    }

    fn percentile(&self, p: f64) -> u64 {
        let target = (self.count as f64 * p / 100.0).ceil() as u64;
        let mut acc = 0u64;
        for (i, &b) in self.buckets.iter().enumerate() {
            acc += b;
            if acc >= target {
                return if i == 0 { 1 } else { 1u64 << i };
            }
        }
        1024
    }

    fn mean(&self) -> u128 {
        if self.count == 0 {
            0
        } else {
            self.sum / self.count as u128
        }
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Bench {
            rooms,
            users_per_room,
            ramp_step,
            ramp_interval,
            max_rooms,
            session_duration,
            session_jitter,
            drain_duration,
        } => {
            run_bench(
                cli.api_url,
                rooms,
                users_per_room,
                ramp_step,
                ramp_interval,
                max_rooms,
                session_duration,
                session_jitter,
                drain_duration,
            )
            .await?
        }
    }
    Ok(())
}

// ── Bench orchestrator ────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_bench(
    api_url: String,
    initial_rooms: usize,
    users_per_room: usize,
    ramp_step: usize,
    ramp_interval: u64,
    max_rooms: usize,
    session_duration: u64,
    session_jitter: u64,
    drain_duration: u64,
) -> Result<()> {
    let (stats_tx, stats_rx) = mpsc::channel::<StatReport>(16_000);
    let state = SharedState::new();
    let mut join_set = JoinSet::new();
    let room_counter = Arc::new(AtomicUsize::new(0));

    println!(
        "┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐"
    );
    println!(
        "│  PulseBeam SFU Breaking-Point Benchmark  │  Multi-Room Meeting  │  Ramping until degradation                                        │"
    );
    println!(
        "├───────┬───────┬────────┬─────────┬─────────┬──────────┬──────────┬───────┬──────┬──────┬──────┬───────┬────────┬──────────────────────┤"
    );
    println!(
        "│  Time │ Rooms │ Agents │ Tx Mbps │ Rx Mbps │ Loss p/s │ NACK t/s │ p50ms │ p99ms│p999ms│ mean │Tx PLI │ Rx PLI │  TX streams  RX streams  │"
    );
    println!(
        "│       │       │        │         │         │          │          │       │      │      │      │       │        │ actv/hlth    actv/hlth   │"
    );
    println!(
        "├───────┼───────┼────────┼─────────┼─────────┼──────────┼──────────┼───────┼──────┼──────┼──────┼───────┼────────┼──────────────────────────┤"
    );

    let monitor_state = state.clone();
    let monitor_handle = tokio::spawn(monitor_task(stats_rx, monitor_state));

    let mut total_rooms = 0usize;

    for _ in 0..initial_rooms {
        spawn_room(
            &mut join_set,
            &api_url,
            &room_counter,
            users_per_room,
            session_duration,
            session_jitter,
            &stats_tx,
            &state,
        )
        .await;
        total_rooms += 1;
    }

    let mut ramp_ticker = tokio::time::interval(Duration::from_secs(ramp_interval));
    ramp_ticker.tick().await;

    loop {
        tokio::select! {
            _ = ramp_ticker.tick() => {
                if state.degraded.load(Ordering::Relaxed) {
                    println!("│ ⚠  DEGRADATION DETECTED — stopping ramp                                                                                              │");
                    break;
                }
                if total_rooms >= max_rooms {
                    println!("│ ✓  Max rooms reached ({}) — stopping ramp                                                                                           │", max_rooms);
                    break;
                }
                for _ in 0..ramp_step {
                    if total_rooms >= max_rooms { break; }
                    spawn_room(
                        &mut join_set,
                        &api_url,
                        &room_counter,
                        users_per_room,
                        session_duration,
                        session_jitter,
                        &stats_tx,
                        &state,
                    )
                    .await;
                    total_rooms += 1;
                }
            }
        }
    }

    tokio::time::sleep(Duration::from_secs(drain_duration)).await;

    println!(
        "├───────┴───────┴────────┴─────────┴─────────┴──────────┴──────────┴───────┴──────┴──────┴──────┴───────┴────────┴──────────────────────────┤"
    );
    println!(
        "│  Benchmark complete. Peak rooms: {:<5}  Peak agents: {:<6}                                                                              │",
        total_rooms,
        state.active_agents.load(Ordering::Relaxed),
    );
    println!(
        "└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘"
    );

    join_set.shutdown().await;
    drop(stats_tx);
    let _ = monitor_handle.await;

    Ok(())
}

// ── Room spawner ──────────────────────────────────────────────────────────────

async fn spawn_room(
    join_set: &mut JoinSet<()>,
    api_url: &str,
    room_counter: &Arc<AtomicUsize>,
    users_per_room: usize,
    session_duration: u64,
    session_jitter: u64,
    stats_tx: &mpsc::Sender<StatReport>,
    state: &Arc<SharedState>,
) {
    let room_id = room_counter.fetch_add(1, Ordering::Relaxed);
    let room = format!("bench-room-{}", room_id);
    state.active_rooms.fetch_add(1, Ordering::Relaxed);

    for user_id in 0..users_per_room {
        let url = api_url.to_string();
        let r = room.clone();
        let tx = stats_tx.clone();
        let st = state.clone();

        let join_delay_ms = rand::random_range(0u64..5_000);
        let jitter_a = rand::random_range(0u64..session_jitter);
        let jitter_b = rand::random_range(0u64..session_jitter);
        let this_session = session_duration
            .saturating_add(jitter_a)
            .saturating_sub(jitter_b);

        join_set.spawn(async move {
            tokio::time::sleep(Duration::from_millis(join_delay_ms)).await;
            st.active_agents.fetch_add(1, Ordering::Relaxed);

            if let Err(e) = spawn_agent(
                room_id * 1000 + user_id,
                url,
                r,
                true,
                Duration::from_secs(this_session),
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
    let expire_after = Duration::from_secs(session_duration + session_jitter * 2 + 6);
    join_set.spawn(async move {
        tokio::time::sleep(expire_after).await;
        room_state.active_rooms.fetch_sub(1, Ordering::Relaxed);
    });
}

// ── Monitor task ──────────────────────────────────────────────────────────────

async fn monitor_task(mut stats_rx: mpsc::Receiver<StatReport>, state: Arc<SharedState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let start = Instant::now();

    let mut tx_bytes = 0u64;
    let mut rx_bytes = 0u64;
    let mut rx_loss = 0.0f32;
    let mut tx_nacks = 0u64;
    let mut rx_nacks = 0u64;
    let mut tx_plis = 0u64;
    let mut rx_plis = 0u64;

    let mut tx_active_streams = 0usize;
    let mut tx_healthy_streams = 0usize;
    let mut rx_active_streams = 0usize;
    let mut rx_healthy_streams = 0usize;

    let mut rtt_hist = LatencyHistogram::default();
    let mut consecutive_high_p99 = 0u32;

    loop {
        tokio::select! {
            Some(report) = stats_rx.recv() => {
                match report {
                    StatReport::Peer { rtt_ms: Some(rtt) } => {
                        rtt_hist.record(rtt);
                    }
                    StatReport::Tx { bytes, nacks, plis } => {
                        tx_bytes += bytes;
                        tx_nacks += nacks;
                        tx_plis  += plis;
                    }
                    StatReport::Rx { bytes, packets: _, packets_lost, nacks, plis } => {
                        rx_bytes += bytes;
                        rx_loss  += packets_lost;
                        rx_nacks += nacks;
                        rx_plis  += plis;
                    }
                    StatReport::StreamHealth { tx_active, tx_healthy, rx_active, rx_healthy } => {
                        if tx_active  { tx_active_streams  += 1; }
                        if tx_healthy { tx_healthy_streams += 1; }
                        if rx_active  { rx_active_streams  += 1; }
                        if rx_healthy { rx_healthy_streams += 1; }
                    }
                    StatReport::DegradationDetected { reason } => {
                        println!("│ ⚠  {:<133}│", reason);
                    }
                    _ => {}
                }
            }

            _ = interval.tick() => {
                let elapsed = start.elapsed().as_secs();
                if elapsed == 0 { continue; }

                let rooms  = state.active_rooms.load(Ordering::Relaxed);
                let agents = state.active_agents.load(Ordering::Relaxed);

                let tx_mbps = (tx_bytes * 8) as f64 / 1_000_000.0;
                let rx_mbps = (rx_bytes * 8) as f64 / 1_000_000.0;

                let p50  = rtt_hist.percentile(50.0);
                let p99  = rtt_hist.percentile(99.0);
                let p999 = rtt_hist.percentile(99.9);
                let mean = rtt_hist.mean();

                // Update shared state so degradation checks can see stream health
                state.tx_active_streams.store(tx_active_streams, Ordering::Relaxed);
                state.tx_healthy_streams.store(tx_healthy_streams, Ordering::Relaxed);
                state.rx_active_streams.store(rx_active_streams, Ordering::Relaxed);
                state.rx_healthy_streams.store(rx_healthy_streams, Ordering::Relaxed);

                println!(
                    "│ {:>5}s│ {:>5} │ {:>6} │ {:>6.2}  │ {:>6.2}  │ {:>8.1}  │ {:>8}  │ {:>5} │{:>5} │{:>5} │{:>5} │{:>6} │ {:>5}  │ {:>4}/{:<4}  {:>4}/{:<4} │",
                    elapsed, rooms, agents,
                    tx_mbps, rx_mbps,
                    rx_loss, tx_nacks + rx_nacks,
                    p50, p99, p999, mean,
                    tx_plis, rx_plis,
                    tx_active_streams, tx_healthy_streams,
                    rx_active_streams, rx_healthy_streams,
                );

                // ── Degradation heuristics ────────────────────────────────
                if p99 > 300 {
                    consecutive_high_p99 += 1;
                    if consecutive_high_p99 >= 3 {
                        state.degraded.store(true, Ordering::Relaxed);
                    }
                } else {
                    consecutive_high_p99 = 0;
                }

                if rx_loss > 5.0 && agents > 0 {
                    state.degraded.store(true, Ordering::Relaxed);
                }

                // If more than 20% of active rx streams are unhealthy, flag it
                if rx_active_streams > 0 {
                    let unhealthy = rx_active_streams.saturating_sub(rx_healthy_streams);
                    if unhealthy * 5 > rx_active_streams {
                        state.degraded.store(true, Ordering::Relaxed);
                    }
                }

                // Reset accumulators
                tx_bytes = 0; rx_bytes = 0; rx_loss = 0.0;
                tx_nacks = 0; rx_nacks = 0;
                tx_plis  = 0; rx_plis  = 0;
                tx_active_streams  = 0; tx_healthy_streams = 0;
                rx_active_streams  = 0; rx_healthy_streams = 0;
                rtt_hist.reset();
            }
        }
    }
}

// ── Agent ─────────────────────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct CounterSnapshot {
    bytes: u64,
    packets: u64,
    nacks: u64,
    plis: u64,
}

async fn spawn_agent(
    id: usize,
    api_url: String,
    room: String,
    is_pub: bool,
    session_duration: Duration,
    stats_tx: mpsc::Sender<StatReport>,
    users_per_room: usize,
) -> Result<()> {
    let api = HttpApiClient::new(Box::new(reqwest::Client::new()), &api_url)?;
    let socket = UdpSocket::bind("0.0.0.0:0").await?;

    let mut builder = AgentBuilder::new(api, socket).with_local_ip("127.0.0.1".parse().unwrap());

    if is_pub {
        builder = builder.with_track(MediaKind::Video, TransceiverDirection::SendOnly, None);
    }

    for _ in 0..users_per_room {
        builder = builder.with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None);
    }

    let mut agent = builder.connect(&room).await?;
    let mut stats_interval = tokio::time::interval(Duration::from_secs(1));
    let session_end = tokio::time::sleep(session_duration);
    tokio::pin!(session_end);

    let mut prev_tx: HashMap<String, CounterSnapshot> = HashMap::new();
    let mut prev_rx: HashMap<String, CounterSnapshot> = HashMap::new();

    loop {
        tokio::select! {
            _ = &mut session_end => break,

            Some(event) = agent.next_event() => {
                if let AgentEvent::LocalTrackAdded(track) = event {
                    tokio::spawn(handle_local_track(track));
                }
            }

            _ = stats_interval.tick() => {
                let Some(stats) = agent.get_stats().await else { continue };

                // ── Peer-level RTT + byte counters ────────────────────────
                let peer = stats.peer.as_ref();
                let rtt_ms = peer.and_then(|p| p.rtt).map(|r| r.as_millis());
                let _ = stats_tx.try_send(StatReport::Peer { rtt_ms });

                let peer_tx_bytes = peer.map(|p| p.bytes_tx).unwrap_or(0);
                let peer_rx_bytes = peer.map(|p| p.bytes_rx).unwrap_or(0);

                let prev_peer_tx = prev_tx.get("peer").map(|s| s.bytes).unwrap_or(0);
                let prev_peer_rx = prev_rx.get("peer").map(|s| s.bytes).unwrap_or(0);

                let tx_bytes_delta = peer_tx_bytes.saturating_sub(prev_peer_tx);
                let rx_bytes_delta = peer_rx_bytes.saturating_sub(prev_peer_rx);

                prev_tx.insert("peer".into(), CounterSnapshot { bytes: peer_tx_bytes, ..Default::default() });
                prev_rx.insert("peer".into(), CounterSnapshot { bytes: peer_rx_bytes, ..Default::default() });

                if tx_bytes_delta > 0 {
                    let _ = stats_tx.try_send(StatReport::Tx { bytes: tx_bytes_delta, nacks: 0, plis: 0 });
                }
                if rx_bytes_delta > 0 {
                    let _ = stats_tx.try_send(StatReport::Rx { bytes: rx_bytes_delta, packets: 0, packets_lost: 0.0, nacks: 0, plis: 0 });
                }

                // ── Per-track NACK/PLI/loss + stream health ───────────────
                for (_, track_stat) in &stats.tracks {
                    // TX layers
                    let mut tx_active  = false;
                    let mut tx_healthy = true;

                    for (rid, egress) in &track_stat.tx_layers {
                        let key = format!("tx_{}", rid_key(rid));
                        let prev = prev_tx.get(&key).cloned().unwrap_or_default();

                        let packets = egress.packets.saturating_sub(prev.packets);
                        let nacks   = egress.nacks.saturating_sub(prev.nacks);
                        let plis    = egress.plis.saturating_sub(prev.plis);

                        prev_tx.insert(key, CounterSnapshot {
                            packets: egress.packets,
                            nacks: egress.nacks,
                            plis: egress.plis,
                            ..Default::default()
                        });

                        if nacks > 0 || plis > 0 {
                            let _ = stats_tx.try_send(StatReport::Tx { bytes: 0, nacks, plis });
                        }

                        let layer_active = packets > 0;
                        tx_active  |= layer_active;
                        tx_healthy &= layer_active && plis == 0 && nacks < 3;
                    }

                    // TX: healthy implies active
                    let tx_healthy = tx_active && tx_healthy;

                    // RX layers
                    let mut rx_active  = false;
                    let mut rx_healthy = true;

                    for (rid, ingress) in &track_stat.rx_layers {
                        let key = format!("rx_{}", rid_key(rid));
                        let prev = prev_rx.get(&key).cloned().unwrap_or_default();

                        let packets      = ingress.packets.saturating_sub(prev.packets);
                        let nacks        = ingress.nacks.saturating_sub(prev.nacks);
                        let plis         = ingress.plis.saturating_sub(prev.plis);
                        let packets_lost = ingress.loss.unwrap_or(0.0) * packets as f32;

                        prev_rx.insert(key, CounterSnapshot {
                            packets: ingress.packets,
                            nacks:   ingress.nacks,
                            plis:    ingress.plis,
                            ..Default::default()
                        });

                        if packets > 0 || packets_lost > 0.0 || nacks > 0 || plis > 0 {
                            let _ = stats_tx.try_send(StatReport::Rx {
                                bytes: 0, packets, packets_lost, nacks, plis,
                            });
                        }

                        let layer_active = packets > 0;
                        rx_active  |= layer_active;
                        rx_healthy &= layer_active
                            && packets_lost < 0.05 * packets as f32
                            && plis == 0;
                    }

                    // RX: healthy implies active
                    let rx_healthy = rx_active && rx_healthy;

                    let _ = stats_tx.try_send(StatReport::StreamHealth {
                        tx_active,
                        tx_healthy,
                        rx_active,
                        rx_healthy,
                    });
                }
            }
        }
    }

    Ok(())
}

fn rid_key(rid: &Option<Rid>) -> String {
    rid.as_ref()
        .map(|r| r.to_string())
        .unwrap_or_else(|| "none".to_string())
}

async fn handle_local_track(track: LocalTrack) {
    let rid_str = track.rid.as_ref().map(|r| r.as_ref());
    let data = match rid_str {
        Some("f") => pulsebeam_testdata::RAW_H264_FULL,
        Some("h") => pulsebeam_testdata::RAW_H264_HALF,
        Some("q") => pulsebeam_testdata::RAW_H264_QUARTER,
        _ => pulsebeam_testdata::RAW_H264_HALF,
    };
    let looper = H264Looper::new(data, 30);
    let _ = looper.run(track).await;
}

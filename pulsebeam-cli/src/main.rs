use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use pulsebeam_agent::{
    MediaKind, Rid, SimulcastLayer, TransceiverDirection,
    actor::{AgentBuilder, AgentEvent, LocalTrack},
    api::HttpApiClient,
    media::H264Looper,
};
use pulsebeam_core::net::UdpSocket;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

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
        #[arg(short, long, value_enum, default_value = "meeting")]
        scenario: Scenario,
        #[arg(short, long, default_value_t = 60)]
        duration: u64,
        #[arg(short, long, default_value_t = 50)]
        users: usize,
    },
}

#[derive(Copy, Clone, ValueEnum, Debug)]
enum Scenario {
    Meeting, // Many-to-Many
    Webinar, // One-to-Many
}

// --- Statistics Messages ---
#[derive(Debug)]
struct StatReport {
    agent_id: usize,
    rid: String,
    bytes_tx: u64,
    bytes_rx: u64,
    packets_lost: u64,
    nacks: u64,
    plis: u64,
    rtt_ms: Option<u128>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Only log warnings/errors to avoid polluting the stats table
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Bench {
            scenario,
            duration,
            users,
        } => {
            run_bench(cli.api_url, scenario, users, duration).await?;
        }
    }
    Ok(())
}

async fn run_bench(api_url: String, scenario: Scenario, users: usize, duration: u64) -> Result<()> {
    let (stats_tx, mut stats_rx) = mpsc::channel::<StatReport>(1000);
    let mut join_set = JoinSet::new();
    let room = format!("bench-{}", Instant::now().elapsed().as_micros());
    let running = Arc::new(AtomicBool::new(true));

    println!(
        "🚀 Starting Benchmark: {:?} | Users: {} | Duration: {}s",
        scenario, users, duration
    );
    println!(
        "-----------------------------------------------------------------------------------------"
    );
    println!(
        "|  Time  |   Tx Mbps  |   Rx Mbps  |  Loss (pkts) |  NACKs/s |  PLIs/s  | Avg RTT  |"
    );
    println!(
        "-----------------------------------------------------------------------------------------"
    );

    // 1. Spawn the Monitor Task (The Aggregator)
    let monitor_running = running.clone();
    let monitor_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));

        // Aggregation Buckets
        let mut total_tx_bytes = 0u64;
        let mut total_rx_bytes = 0u64;
        let mut total_loss = 0u64;
        let mut total_nacks = 0u64;
        let mut total_plis = 0u64;
        let mut rtt_sum = 0u128;
        let mut rtt_count = 0u128;

        let start = Instant::now();

        loop {
            tokio::select! {
                // Collect incoming stats
                Some(report) = stats_rx.recv() => {
                    total_tx_bytes += report.bytes_tx;
                    total_rx_bytes += report.bytes_rx;
                    total_loss += report.packets_lost;
                    total_nacks += report.nacks;
                    total_plis += report.plis;
                    if let Some(rtt) = report.rtt_ms {
                        rtt_sum += rtt;
                        rtt_count += 1;
                    }
                }
                // Print Summary every second
                _ = interval.tick() => {
                    if !monitor_running.load(Ordering::Relaxed) { break; }

                    // Avoid printing the very first tick (zeros)
                    if start.elapsed().as_secs() == 0 { continue; }

                    let tx_mbps = (total_tx_bytes * 8) as f64 / 1_000_000.0;
                    let rx_mbps = (total_rx_bytes * 8) as f64 / 1_000_000.0;
                    let avg_rtt = if rtt_count > 0 { rtt_sum / rtt_count } else { 0 };

                    println!(
                        "| {:>5}s | {:>9.2} | {:>9.2} | {:>11} | {:>8} | {:>7} | {:>6}ms |",
                        start.elapsed().as_secs(),
                        tx_mbps,
                        rx_mbps,
                        total_loss,
                        total_nacks,
                        total_plis,
                        avg_rtt
                    );

                    // Reset counters for the next second (Windowed Stats)
                    total_tx_bytes = 0;
                    total_rx_bytes = 0;
                    total_loss = 0;
                    total_nacks = 0;
                    total_plis = 0;
                    rtt_sum = 0;
                    rtt_count = 0;
                }
            }
        }
    });

    // 2. Spawn Agents
    for i in 0..users {
        let url = api_url.clone();
        let r_id = room.clone();
        let tx = stats_tx.clone();

        let is_pub = match scenario {
            Scenario::Meeting => true,
            Scenario::Webinar => i == 0,
        };

        join_set.spawn(async move {
            if let Err(e) = spawn_agent(i, url, r_id, is_pub, tx).await {
                error!("Agent {} died: {:?}", i, e);
            }
        });

        // Stagger joins slightly to be realistic
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Drop the local tx so the receiver knows when all agents are gone (if we wanted to wait)
    drop(stats_tx);

    // 3. Wait for Duration
    tokio::time::sleep(Duration::from_secs(duration)).await;

    // Shutdown
    running.store(false, Ordering::Relaxed);
    println!(
        "-----------------------------------------------------------------------------------------"
    );
    println!("🏁 Benchmark Completed.");

    // Cleanup
    join_set.shutdown().await;
    let _ = monitor_handle.await;

    Ok(())
}

async fn spawn_agent(
    id: usize,
    api_url: String,
    room: String,
    is_pub: bool,
    stats_tx: mpsc::Sender<StatReport>,
) -> Result<()> {
    let api = HttpApiClient::new(Box::new(reqwest::Client::new()), &api_url)?;
    let socket = UdpSocket::bind("0.0.0.0:0").await?;

    let mut builder = AgentBuilder::new(api, socket);
    if is_pub {
        builder = builder.with_track(
            MediaKind::Video,
            TransceiverDirection::SendOnly,
            Some(vec![
                SimulcastLayer::new("f"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("q"),
            ]),
        );
    }

    let mut agent = builder.connect(&room).await?;
    let mut stats_interval = tokio::time::interval(Duration::from_secs(1));

    // We need to track the *previous* value to calculate the delta (per second)
    let mut prev_tx_bytes: HashMap<String, u64> = HashMap::new();
    let mut prev_rx_bytes: HashMap<String, u64> = HashMap::new();

    loop {
        tokio::select! {
            Some(event) = agent.next_event() => {
                if let AgentEvent::LocalTrackAdded(track) = event {
                    tokio::spawn(handle_local_track(track));
                }
            }
            _ = stats_interval.tick() => {
                if let Some(stats) = agent.get_stats().await {
                    let mut rtt_ms = None;
                    if let Some(peer) = &stats.peer {
                        if let Some(rtt) = peer.rtt {
                            rtt_ms = Some(rtt.as_millis());
                        }
                    }

                    // Collect TX (Publisher) Stats
                    for (_, track_stat) in &stats.tracks {
                        for (rid, egress) in &track_stat.tx_layers {
                            let rid_key = rid.as_ref().map(|r| r.to_string()).unwrap_or_else(|| "none".to_string());

                            let last = *prev_tx_bytes.get(&rid_key).unwrap_or(&0);
                            let delta = egress.bytes.saturating_sub(last);
                            prev_tx_bytes.insert(rid_key.clone(), egress.bytes);

                            if delta > 0 || egress.nacks > 0 {
                                let _ = stats_tx.send(StatReport {
                                    agent_id: id,
                                    rid: rid_key,
                                    bytes_tx: delta,
                                    bytes_rx: 0, // TX only
                                    packets_lost: 0, // Usually calculated on RX side
                                    nacks: egress.nacks, // NACKs received
                                    plis: egress.plis,
                                    rtt_ms,
                                }).await;
                            }
                        }

                        // Collect RX (Subscriber) Stats
                        for (rid, ingress) in &track_stat.rx_layers {
                            let rid_key = rid.as_ref().map(|r| r.to_string()).unwrap_or_else(|| "none".to_string());

                            let last = *prev_rx_bytes.get(&rid_key).unwrap_or(&0);
                            let delta = ingress.bytes.saturating_sub(last);
                            prev_rx_bytes.insert(rid_key.clone(), ingress.bytes);

                            let lost_pkts = (ingress.loss.unwrap_or(0.0) * ingress.packets as f32) as u64;

                            if delta > 0 {
                                let _ = stats_tx.send(StatReport {
                                    agent_id: id,
                                    rid: rid_key,
                                    bytes_tx: 0,
                                    bytes_rx: delta,
                                    packets_lost: lost_pkts,
                                    nacks: ingress.nacks,
                                    plis: ingress.plis,
                                    rtt_ms,
                                }).await;
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn handle_local_track(track: LocalTrack) {
    let rid_str = track.rid.as_ref().map(|r| r.as_ref());

    let data = match rid_str {
        Some("f") => pulsebeam_testdata::RAW_H264_FULL,
        Some("h") => pulsebeam_testdata::RAW_H264_HALF,
        Some("q") => pulsebeam_testdata::RAW_H264_QUARTER,
        _ => pulsebeam_testdata::RAW_H264_FULL,
    };

    let looper = H264Looper::new(data, 30);
    // Ignore errors here to keep the CLI output clean
    let _ = looper.run(track).await;
}

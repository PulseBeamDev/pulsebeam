use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use pulsebeam_agent::{
    MediaKind, SimulcastLayer, TransceiverDirection,
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
use tracing::error;

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
    Meeting,
    Webinar,
}

#[derive(Debug)]
struct PeerStatReport {
    agent_id: usize,
    rtt_ms: Option<u128>,
}

#[derive(Debug)]
struct TxStatReport {
    agent_id: usize,
    rid: String,
    bytes: u64,
    nacks: u64,
    plis: u64,
}

#[derive(Debug)]
struct RxStatReport {
    agent_id: usize,
    rid: String,
    bytes: u64,
    packets: u64,
    packets_lost: f32,
    nacks: u64,
    plis: u64,
}

#[derive(Debug)]
enum StatReport {
    Peer(PeerStatReport),
    Tx(TxStatReport),
    Rx(RxStatReport),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Bench {
            scenario,
            duration,
            users,
        } => run_bench(cli.api_url, scenario, users, duration).await?,
    }

    Ok(())
}

async fn run_bench(api_url: String, scenario: Scenario, users: usize, duration: u64) -> Result<()> {
    let (stats_tx, mut stats_rx) = mpsc::channel::<StatReport>(5000);
    let mut join_set = JoinSet::new();
    let room = format!("bench-{}", Instant::now().elapsed().as_micros());
    let running = Arc::new(AtomicBool::new(true));

    println!(
        "🚀 Starting Benchmark: {:?} | Users: {} | Duration: {}s | Room: {}",
        scenario, users, duration, room
    );

    println!(
        "--------------------------------------------------------------------------------------------------------"
    );
    println!(
        "| Time  | Tx Mbps | Rx Mbps | Rx Loss pkts/s | Tx NACK/s | Rx NACK/s | Tx PLI/s | Rx PLI/s | Avg RTT |"
    );
    println!(
        "--------------------------------------------------------------------------------------------------------"
    );

    let monitor_running = running.clone();
    let monitor_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let start = Instant::now();

        let mut tx_bytes = 0u64;
        let mut rx_bytes = 0u64;
        let mut rx_loss = 0.0f32;

        let mut tx_nacks = 0u64;
        let mut rx_nacks = 0u64;

        let mut tx_plis = 0u64;
        let mut rx_plis = 0u64;

        let mut rtt_sum = 0u128;
        let mut rtt_count = 0u128;

        loop {
            tokio::select! {
                Some(report) = stats_rx.recv() => {
                    match report {
                        StatReport::Peer(peer) => {
                            if let Some(rtt) = peer.rtt_ms {
                                rtt_sum += rtt;
                                rtt_count += 1;
                            }
                        }
                        StatReport::Tx(tx) => {
                            tx_bytes += tx.bytes;
                            tx_nacks += tx.nacks;
                            tx_plis += tx.plis;
                        }
                        StatReport::Rx(rx) => {
                            rx_bytes += rx.bytes;
                            rx_loss += rx.packets_lost;
                            rx_nacks += rx.nacks;
                            rx_plis += rx.plis;
                        }
                    }
                }

                _ = interval.tick() => {
                    if !monitor_running.load(Ordering::Relaxed) {
                        break;
                    }

                    if start.elapsed().as_secs() == 0 {
                        continue;
                    }

                    let tx_mbps = (tx_bytes * 8) as f64 / 1_000_000.0;
                    let rx_mbps = (rx_bytes * 8) as f64 / 1_000_000.0;

                    let avg_rtt = if rtt_count > 0 {
                        rtt_sum / rtt_count
                    } else {
                        0
                    };

                    println!(
                        "| {:>5}s | {:>6.2} | {:>6.2} | {:>13} | {:>9} | {:>9} | {:>8} | {:>8} | {:>6}ms |",
                        start.elapsed().as_secs(),
                        tx_mbps,
                        rx_mbps,
                        rx_loss,
                        tx_nacks,
                        rx_nacks,
                        tx_plis,
                        rx_plis,
                        avg_rtt,
                    );

                    tx_bytes = 0;
                    rx_bytes = 0;
                    rx_loss = 0.0;
                    tx_nacks = 0;
                    rx_nacks = 0;
                    tx_plis = 0;
                    rx_plis = 0;
                    rtt_sum = 0;
                    rtt_count = 0;
                }
            }
        }
    });

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

        let jitter = rand::random_range(0..2_000);
        tokio::time::sleep(Duration::from_millis(10) + Duration::from_micros(jitter)).await;
    }

    drop(stats_tx);

    tokio::time::sleep(Duration::from_secs(duration)).await;

    running.store(false, Ordering::Relaxed);

    println!(
        "--------------------------------------------------------------------------------------------------------"
    );
    println!("🏁 Benchmark Completed.");

    join_set.shutdown().await;
    let _ = monitor_handle.await;

    Ok(())
}

#[derive(Default, Clone)]
struct CounterSnapshot {
    bytes: u64,
    packets: u64,
    nacks: u64,
    plis: u64,
    lost: f32,
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

    let mut builder = AgentBuilder::new(api, socket).with_local_ip("127.0.0.1".parse().unwrap());

    if is_pub {
        builder = builder.with_track(
            MediaKind::Video,
            TransceiverDirection::SendOnly,
            None,
            // Some(vec![
            //     SimulcastLayer::new("f"),
            //     SimulcastLayer::new("h"),
            //     SimulcastLayer::new("q"),
            // ]),
        );
    }

    builder = builder.with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None);

    let mut agent = builder.connect(&room).await?;
    let mut stats_interval = tokio::time::interval(Duration::from_secs(1));

    let mut prev_tx: HashMap<String, CounterSnapshot> = HashMap::new();
    let mut prev_rx: HashMap<String, CounterSnapshot> = HashMap::new();

    loop {
        tokio::select! {
            Some(event) = agent.next_event() => {
                if let AgentEvent::LocalTrackAdded(track) = event {
                    tokio::spawn(handle_local_track(track));
                }
            }

            _ = stats_interval.tick() => {
                let Some(stats) = agent.get_stats().await else { continue };

                // ---- PEER stats (RTT) ----
                let rtt_ms = stats.peer
                    .as_ref()
                    .and_then(|p| p.rtt)
                    .map(|r| r.as_millis());

                let _ = stats_tx.send(StatReport::Peer(PeerStatReport {
                    agent_id: id,
                    rtt_ms,
                })).await;

                // ---- TRACK stats ----
                for (_, track_stat) in &stats.tracks {

                    // ---- TX layers ----
                    for (rid, egress) in &track_stat.tx_layers {
                        let rid_key = rid.as_ref()
                            .map(|r| r.to_string())
                            .unwrap_or_else(|| "none".to_string());

                        let prev = prev_tx.get(&rid_key).cloned().unwrap_or_default();

                        let bytes_delta = egress.bytes.saturating_sub(prev.bytes);
                        let nacks_delta = egress.nacks.saturating_sub(prev.nacks);
                        let plis_delta = egress.plis.saturating_sub(prev.plis);

                        prev_tx.insert(rid_key.clone(), CounterSnapshot {
                            bytes: egress.bytes,
                            packets: 0,
                            nacks: egress.nacks,
                            plis: egress.plis,
                            lost: 0.0,
                        });

                        if bytes_delta > 0 || nacks_delta > 0 || plis_delta > 0 {
                            let _ = stats_tx.send(StatReport::Tx(TxStatReport {
                                agent_id: id,
                                rid: rid_key,
                                bytes: bytes_delta,
                                nacks: nacks_delta,
                                plis: plis_delta,
                            })).await;
                        }
                    }

                    // ---- RX layers ----
                    for (rid, ingress) in &track_stat.rx_layers {
                        let rid_key = rid.as_ref()
                            .map(|r| r.to_string())
                            .unwrap_or_else(|| "none".to_string());

                        let prev = prev_rx.get(&rid_key).cloned().unwrap_or_default();

                        let bytes_delta = ingress.bytes.saturating_sub(prev.bytes);
                        let packets_delta = ingress.packets.saturating_sub(prev.packets);

                        let loss = ingress.loss.unwrap_or(0.0);

                        let nacks_delta = ingress.nacks.saturating_sub(prev.nacks);
                        let plis_delta = ingress.plis.saturating_sub(prev.plis);

                        prev_rx.insert(rid_key.clone(), CounterSnapshot {
                            bytes: ingress.bytes,
                            packets: ingress.packets,
                            nacks: ingress.nacks,
                            plis: ingress.plis,
                            lost: loss,
                        });

                        if bytes_delta > 0 || packets_delta > 0 || loss > 0.0 || nacks_delta > 0 || plis_delta > 0 {
                            let _ = stats_tx.send(StatReport::Rx(RxStatReport {
                                agent_id: id,
                                rid: rid_key,
                                bytes: bytes_delta,
                                packets: packets_delta,
                                packets_lost: loss,
                                nacks: nacks_delta,
                                plis: plis_delta,
                            })).await;
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
        _ => pulsebeam_testdata::RAW_H264_HALF,
    };

    let looper = H264Looper::new(data, 30);
    let _ = looper.run(track).await;
}

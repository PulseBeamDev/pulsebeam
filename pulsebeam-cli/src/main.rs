use anyhow::Result;
use clap::{Parser, Subcommand};
use pulsebeam_agent::{
    MediaKind, SimulcastLayer, TransceiverDirection,
    actor::{AgentBuilder, AgentEvent, LocalTrack},
    api::HttpApiClient,
    clock::clock_anchor,
    media::H264Looper,
    wallclock_at,
};
use pulsebeam_core::net::UdpSocket;
use std::time::Duration;
use tachyonix as mpsc;
use tokio::{fs::File, io::BufWriter};
use tokio::{io::AsyncWriteExt, task::JoinSet};
use tokio::{runtime::Builder, time::Instant};

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Parser)]
struct Cli {
    #[arg(short, long, default_value = "http://localhost:7070")]
    api_url: String,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Bench(BenchConfig),
}

#[derive(Parser, Clone)]
pub struct BenchConfig {
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
    #[arg(long)]
    simulcast: bool,
    #[arg(long, default_value = "latency.csv")]
    latency_file: String,
    #[arg(long, default_value = "snapshots.csv")]
    snapshots_file: String,
}

#[derive(Clone)]
struct AgentContext {
    api_url: String,
    room_id: usize,
    agent_id: usize,
    logger: Logger,
}

pub struct EventLatency {
    captured_at: Instant,
    room_id: usize,
    agent_id: usize,
    delay_us: u64,
}

pub struct EventSnapshot {
    captured_at: Instant,
    room_id: usize,
    agent_id: usize,
    tx_bytes: u64,
    rx_bytes: u64,
    rtt_us: u64,
    loss_pct: f32,
}

#[derive(Clone)]
pub struct Logger {
    latency: mpsc::Sender<EventLatency>,
    snapshot: mpsc::Sender<EventSnapshot>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut builder = Builder::new_multi_thread();
    builder.enable_all().enable_alt_timer();

    let runtime = builder.build()?;
    runtime.block_on(async move {
        match cli.command {
            Commands::Bench(config) => {
                run_bench(cli.api_url, config).await?;
            }
        }
        anyhow::Ok(())
    })
}

async fn run_bench(api_url: String, config: BenchConfig) -> Result<()> {
    let latency_csv = File::create(&config.latency_file).await?;
    let snapshots_csv = File::create(&config.snapshots_file).await?;

    let (latency_tx, latency_rx) = mpsc::channel::<EventLatency>(128_000);
    let (snapshot_tx, snapshot_rx) = mpsc::channel::<EventSnapshot>(128_000);
    let logger = Logger {
        latency: latency_tx,
        snapshot: snapshot_tx,
    };
    let mut join_set = JoinSet::new();

    let latency_writer_handle = tokio::spawn(latency_writer_task(latency_rx, latency_csv));
    let snapshot_writer_handle = tokio::spawn(snapshot_writer_task(snapshot_rx, snapshots_csv));

    let mut total_rooms = 0;
    for room_id in 0..config.rooms {
        spawn_room(&mut join_set, &api_url, room_id, &config, logger.clone()).await;
        total_rooms += 1;
    }

    // Monitor for room generation loops alongside early manual interruption signals
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\nReceived Ctrl+C, initiating graceful teardown...");
        }
        _ = async {
            loop {
                if total_rooms >= config.max_rooms {
                    break;
                }
                let u = (rand::random_range(1u64..u64::MAX) as f64) / (u64::MAX as f64);
                let delay = Duration::from_secs_f64((-u.ln() / config.arrival_rate).max(0.001));
                tokio::time::sleep(delay).await;

                spawn_room(&mut join_set, &api_url, total_rooms, &config, logger.clone()).await;
                total_rooms += 1;
            }
            // Keep running inside this block until all active agents complete their session schedules
            while join_set.join_next().await.is_some() {}
        } => {}
    }

    // Terminate all executing tasks and explicitly drop core channel senders
    join_set.shutdown().await;
    drop(logger);

    // Block until channels drain and files write their final buffers to disk
    let _ = latency_writer_handle.await;
    let _ = snapshot_writer_handle.await;

    eprintln!("All buffers successfully flushed to disk.");
    Ok(())
}

async fn spawn_room(
    join_set: &mut JoinSet<()>,
    api_url: &str,
    room_id: usize,
    config: &BenchConfig,
    logger: Logger,
) {
    let room_name = format!("bench-room-{}", room_id);

    for user_id in 0..config.users_per_room {
        let delay_ms = rand::random_range(0u64..(config.join_spread_secs * 1_000).max(1));
        let session_duration = Duration::from_secs(config.session_duration);
        let simulcast = config.simulcast;

        let ctx = AgentContext {
            api_url: api_url.to_string(),
            room_id,
            agent_id: room_id * 1000 + user_id,
            logger: logger.clone(),
        };
        let r_name = room_name.clone();

        join_set.spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            let _ = spawn_agent(ctx, r_name, simulcast, session_duration).await;
        });
    }
}

async fn spawn_agent(
    ctx: AgentContext,
    room_name: String,
    simulcast: bool,
    duration: Duration,
) -> Result<()> {
    let api = HttpApiClient::new(Box::new(reqwest::Client::new()), &ctx.api_url)?;
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let mut builder = AgentBuilder::new(api, socket).with_local_ip("127.0.0.1".parse().unwrap());

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

    for _ in 0..7 {
        builder = builder.with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None);
    }
    for _ in 0..3 {
        builder = builder.with_track(MediaKind::Audio, TransceiverDirection::RecvOnly, None);
    }

    let mut driver = builder.connect(&room_name).await?;
    let mut stats_interval = tokio::time::interval(Duration::from_secs(5));
    stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let session_end = tokio::time::sleep(duration);
    tokio::pin!(session_end);

    loop {
        tokio::select! {
            biased;
            _ = &mut session_end => break,
            _ = stats_interval.tick() => {
                let stats = driver.stats();
                let peer = stats.peer.as_ref();

                let tx_bytes = peer.map(|p| p.bytes_tx).unwrap_or(0);
                let rx_bytes = peer.map(|p| p.bytes_rx).unwrap_or(0);
                let rtt_us = peer.and_then(|p| p.rtt).map(|r| r.as_micros() as u64).unwrap_or(0);

                let loss_pct = stats.tracks.values()
                    .flat_map(|t| t.rx_layers.values())
                    .filter_map(|layer| layer.loss)
                    .next()
                    .unwrap_or(0.0);

                let _ = ctx.logger.snapshot.try_send(EventSnapshot {
                    captured_at: Instant::now(),
                    room_id: ctx.room_id,
                    agent_id: ctx.agent_id,
                    tx_bytes,
                    rx_bytes,
                    rtt_us,
                    loss_pct,
                });
            }
            Some(event) = driver.poll() => {
                match event {
                    AgentEvent::LocalTrackAdded(track) => { tokio::spawn(handle_local_track(track)); }
                    AgentEvent::MediaReceived { frame, receive_time, .. } => {
                        if let Some(abs_capture_time) = frame.abs_capture_time {
                            let wallclock = wallclock_at(receive_time);
                            if let Ok(latency) = wallclock.duration_since(abs_capture_time) {
                                let _ = ctx.logger.latency.try_send(EventLatency {
                                    captured_at: Instant::now(),
                                    room_id: ctx.room_id,
                                    agent_id: ctx.agent_id,
                                    delay_us: latency.as_micros() as u64,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

async fn latency_writer_task(mut rx: mpsc::Receiver<EventLatency>, file: File) -> Result<()> {
    let mut writer = BufWriter::with_capacity(16 * 1024, file);
    let _ = writer
        .write_all("elapsed_ms,room_id,agent_id,delay_us\n".as_bytes())
        .await;
    let mut count = 0;

    while let Ok(e) = rx.recv().await {
        let _ = writer
            .write_all(
                format!(
                    "{},{},{},{}\n",
                    clock_anchor().since(e.captured_at).as_millis(),
                    e.room_id,
                    e.agent_id,
                    e.delay_us,
                )
                .as_bytes(),
            )
            .await;
        count += 1;

        if count >= 1000 {
            let _ = writer.flush().await;
            count = 0;
        }
    }
    let _ = writer.flush().await;
    Ok(())
}

async fn snapshot_writer_task(mut rx: mpsc::Receiver<EventSnapshot>, file: File) -> Result<()> {
    let mut writer = BufWriter::with_capacity(4 * 1024, file);
    let _ = writer
        .write_all("elapsed_ms,room_id,agent_id,tx_bytes,rx_bytes,rtt_us,loss_pct\n".as_bytes())
        .await;

    while let Ok(e) = rx.recv().await {
        let _ = writer
            .write_all(
                format!(
                    "{},{},{},{},{},{},{:.4}\n",
                    clock_anchor().since(e.captured_at).as_millis(),
                    e.room_id,
                    e.agent_id,
                    e.tx_bytes,
                    e.rx_bytes,
                    e.rtt_us,
                    e.loss_pct
                )
                .as_bytes(),
            )
            .await;
        let _ = writer.flush().await;
    }
    let _ = writer.flush().await;
    Ok(())
}

async fn handle_local_track(track: LocalTrack) {
    if track.kind.is_audio() {
        return;
    }
    let data = match track.rid.as_ref().map(|r| r.as_ref()) {
        Some("f") => pulsebeam_testdata::RAW_H264_FULL_CBR,
        Some("h") => pulsebeam_testdata::RAW_H264_HALF_CBR,
        _ => pulsebeam_testdata::RAW_H264_QUARTER_CBR,
    };
    let _ = H264Looper::new(data, 30).run(track).await;
}

//! Shared-state exception, crate-wide: A command-line tool. Not a shard.
//! The thread-per-core restriction in `crates/pulsebeam/docs/thread-per-core.md` applies to the
//! `pulsebeam` SFU crate.
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::print_stdout,
    clippy::print_stderr
)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::panic, clippy::unwrap_used))]

use anyhow::Result;
use clap::{Parser, Subcommand};
use pulsebeam_agent_native::agent_core::{
    AgentConfig, ConnectionState, DesiredState, MediaKind, MediaTopology, PublicationIntent,
    Snapshot, VideoSubscription,
};
use pulsebeam_agent_native::{
    Agent, AgentEvent, Config, Host, LocalMedia, MediaFrame, MediaTime, RemoteMedia, SimulcastLayer,
};
use pulsebeam_agent_native::{clock::clock_anchor, wallclock_at};
use pulsebeam_core::net::UdpSocket;
use std::{collections::HashSet, sync::Arc, time::Duration};
use tachyonix as mpsc;
use tokio::sync::{broadcast, watch};
use tokio::{fs::File, io::BufWriter};
use tokio::{io::AsyncWriteExt, task::JoinSet};
use tokio::{runtime::Builder, time::Instant};

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const REMOTE_VIDEO_SLOTS: u8 = 7;
const REMOTE_AUDIO_SLOTS: u8 = 3;

#[derive(Clone)]
struct VideoAssets {
    full: VideoAsset,
    half: VideoAsset,
    quarter: VideoAsset,
}

impl VideoAssets {
    fn new() -> Self {
        Self {
            full: VideoAsset::new(pulsebeam_testdata::RAW_H264_FULL_CBR),
            half: VideoAsset::new(pulsebeam_testdata::RAW_H264_HALF_CBR),
            quarter: VideoAsset::new(pulsebeam_testdata::RAW_H264_QUARTER_CBR),
        }
    }

    fn for_encoding(&self, encoding: Option<&str>) -> VideoAsset {
        match encoding {
            Some("f") => self.full.clone(),
            Some("q") => self.quarter.clone(),
            _ => self.half.clone(),
        }
    }
}

#[derive(Clone)]
struct VideoAsset {
    frames: Arc<[Arc<[u8]>]>,
    first_keyframe: usize,
}

impl VideoAsset {
    fn new(data: &[u8]) -> Self {
        let frames: Arc<[Arc<[u8]>]> = pulsebeam_agent_native::media::H264FrameSlicer::new(data)
            .map(Arc::from)
            .collect::<Vec<_>>()
            .into();
        debug_assert!(!frames.is_empty());
        let first_keyframe = frames
            .iter()
            .position(|frame| annex_b_has_nal(frame, 5))
            .unwrap_or(0);
        Self {
            frames,
            first_keyframe,
        }
    }
}

struct VideoSource {
    asset: VideoAsset,
    index: usize,
    fps: u32,
}

impl VideoSource {
    fn new(asset: VideoAsset, fps: u32) -> Self {
        Self {
            asset,
            index: 0,
            fps: fps.max(1),
        }
    }

    async fn run(
        mut self,
        media: LocalMedia,
        encoding: Option<String>,
        mut events: broadcast::Receiver<AgentEvent>,
    ) {
        let mut frame_count = 0u64;
        let mut interval =
            tokio::time::interval(Duration::from_secs_f64(1.0 / f64::from(self.fps)));
        loop {
            let capture_time = interval.tick().await;
            while let Ok(event) = events.try_recv() {
                if keyframe_matches(&event, encoding.as_deref()) {
                    self.index = self.asset.first_keyframe;
                }
            }
            debug_assert!(self.index < self.asset.frames.len());
            let is_keyframe = self.index == self.asset.first_keyframe;
            let data = self
                .asset
                .frames
                .get(self.index)
                .cloned()
                .unwrap_or_default();
            let frame = MediaFrame {
                audio_level: None,
                voice_activity: None,
                ts: MediaTime::from_90khz(
                    frame_count
                        .saturating_mul(90_000)
                        .checked_div(u64::from(self.fps))
                        .unwrap_or(0),
                ),
                data,
                capture_time,
                abs_capture_time: Some(pulsebeam_agent_native::clock::capture_wallclock()),
                contiguous: true,
                is_keyframe,
                target_bitrate_bps: None,
                resolution: None,
                dependency_descriptor: None,
                temporal_layers: None,
            };
            let _ = media.send_encoding(encoding.clone(), frame).await;
            frame_count = frame_count.saturating_add(1);
            self.index = if self.index.saturating_add(1) < self.asset.frames.len() {
                self.index.saturating_add(1)
            } else {
                0
            };
        }
    }
}

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
    assets: VideoAssets,
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
    tracing_subscriber::fmt::init();

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
    let assets = VideoAssets::new();
    let mut join_set = JoinSet::new();

    let latency_writer_handle = tokio::spawn(latency_writer_task(latency_rx, latency_csv));
    let snapshot_writer_handle = tokio::spawn(snapshot_writer_task(snapshot_rx, snapshots_csv));

    let mut total_rooms = 0usize;
    for room_id in 0..config.rooms {
        spawn_room(
            &mut join_set,
            &api_url,
            room_id,
            &config,
            logger.clone(),
            assets.clone(),
        )
        .await;
        total_rooms = total_rooms.saturating_add(1);
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

                spawn_room(&mut join_set, &api_url, total_rooms, &config, logger.clone(), assets.clone()).await;
                total_rooms = total_rooms.saturating_add(1);
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
    assets: VideoAssets,
) {
    let room_name = format!("bench-room-{room_id}");

    for user_id in 0..config.users_per_room {
        let delay_ms =
            rand::random_range(0u64..config.join_spread_secs.saturating_mul(1_000).max(1));
        let session_duration = Duration::from_secs(config.session_duration);
        let simulcast = config.simulcast;

        let ctx = AgentContext {
            api_url: api_url.to_string(),
            room_id,
            agent_id: room_id.saturating_mul(1000).saturating_add(user_id),
            logger: logger.clone(),
            assets: assets.clone(),
        };
        let r_name = room_name.clone();

        join_set.spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            let agent_id = ctx.agent_id;
            if let Err(error) = spawn_agent(ctx, r_name.clone(), simulcast, session_duration).await
            {
                tracing::error!(room = %r_name, agent_id, %error, "bench agent stopped");
            }
        });
    }
}

async fn spawn_agent(
    ctx: AgentContext,
    room_name: String,
    simulcast: bool,
    duration: Duration,
) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let session = AgentConfig {
        endpoint: ctx.api_url.clone(),
        room_id: room_name,
        request_headers: Vec::new(),
        topology: MediaTopology {
            local_video: vec!["camera".into()],
            local_audio: vec!["microphone".into()],
            remote_video: REMOTE_VIDEO_SLOTS,
            remote_audio: REMOTE_AUDIO_SLOTS,
        },
        manual_subscriptions: true,
        retry: Default::default(),
    };
    let mut config = Config::new(session);
    let encodings = if simulcast {
        vec![
            SimulcastLayer::new("q"),
            SimulcastLayer::new("h"),
            SimulcastLayer::new("f"),
        ]
    } else {
        Vec::new()
    };
    if !encodings.is_empty() {
        config
            .video_encodings
            .insert("camera".into(), encodings.clone());
    }
    let agent = Agent::spawn(config, Host::new(Box::new(reqwest::Client::new()), socket)).await?;
    let mut desired = DesiredState {
        revision: 1,
        connected: true,
        publications: vec![
            PublicationIntent {
                slot: "camera".into(),
                active: true,
            },
            PublicationIntent {
                slot: "microphone".into(),
                active: true,
            },
        ],
        ..Default::default()
    };
    let mut snapshots = agent.snapshots();
    agent.replace_desired(desired.clone()).await?;
    wait_until_connected(&mut snapshots).await?;

    let mut media_tasks = JoinSet::new();
    let camera = agent.local_media("camera");
    let source_encodings = if encodings.is_empty() {
        vec![None]
    } else {
        encodings
            .iter()
            .map(|layer| Some(layer.rid.to_string()))
            .collect()
    };
    for encoding in source_encodings {
        let source = VideoSource::new(ctx.assets.for_encoding(encoding.as_deref()), 30);
        media_tasks.spawn(source.run(camera.clone(), encoding, agent.events()));
    }

    let (received_video_tx, received_video) = watch::channel(false);
    for slot in 0..REMOTE_VIDEO_SLOTS {
        let remote = agent.remote_video(slot).await?;
        media_tasks.spawn(handle_receiving(
            remote,
            ctx.clone(),
            received_video_tx.clone(),
        ));
    }
    drop(received_video_tx);

    let mut subscribed_participants = HashSet::new();
    let initial_snapshot = snapshots.borrow().clone();
    reconcile_video_subscriptions(
        &agent,
        &mut desired,
        &initial_snapshot,
        &mut subscribed_participants,
    )
    .await?;
    let statistics = agent.statistics();
    let mut stats_interval = tokio::time::interval(Duration::from_secs(5));
    stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let session_end = tokio::time::sleep(duration);
    tokio::pin!(session_end);

    loop {
        tokio::select! {
            biased;
            _ = &mut session_end => break,
            _ = stats_interval.tick() => {
                let stats = statistics.borrow().clone();
                let rtt_us = stats
                    .round_trip_time
                    .map(|rtt| u64::try_from(rtt.as_micros()).unwrap_or(u64::MAX))
                    .unwrap_or(0);

                let _ = ctx.logger.snapshot.try_send(EventSnapshot {
                    captured_at: Instant::now(),
                    room_id: ctx.room_id,
                    agent_id: ctx.agent_id,
                    tx_bytes: stats.bytes_sent,
                    rx_bytes: stats.bytes_received,
                    rtt_us,
                    loss_pct: stats.receive_loss.unwrap_or(0.0),
                });
            }
            result = snapshots.changed() => {
                result?;
                let snapshot = snapshots.borrow().clone();
                if snapshot.connection == ConnectionState::TerminalFailure {
                    let failure = snapshot
                        .terminal_failure
                        .map_or_else(|| "native agent failed".to_owned(), |failure| failure.message);
                    anyhow::bail!(failure);
                }
                reconcile_video_subscriptions(
                    &agent,
                    &mut desired,
                    &snapshot,
                    &mut subscribed_participants,
                ).await?;
            }
        }
    }

    agent.close().await?;
    media_tasks.shutdown().await;
    if !subscribed_participants.is_empty() && !*received_video.borrow() {
        tracing::error!(
            room_id = ctx.room_id,
            agent_id = ctx.agent_id,
            subscriptions = subscribed_participants.len(),
            "bench agent received no remote video frames"
        );
    }
    Ok(())
}

async fn handle_receiving(
    mut media: RemoteMedia,
    ctx: AgentContext,
    received_video: watch::Sender<bool>,
) {
    let mut received = false;
    while let Ok(rtp) = media.recv_packet().await {
        if !received {
            received = true;
            received_video.send_replace(true);
            tracing::info!(
                room_id = ctx.room_id,
                agent_id = ctx.agent_id,
                "bench agent received remote video"
            );
        }
        if let Some(abs_capture_time) = rtp.ext_vals.abs_capture_time.map(|a| a.capture_time) {
            let wallclock = wallclock_at(Instant::now());
            if let Ok(latency) = wallclock.duration_since(abs_capture_time) {
                let _ = ctx.logger.latency.try_send(EventLatency {
                    captured_at: Instant::now(),
                    room_id: ctx.room_id,
                    agent_id: ctx.agent_id,
                    delay_us: u64::try_from(latency.as_micros()).unwrap_or(u64::MAX),
                });
            }
        }
    }
}

async fn wait_until_connected(snapshots: &mut watch::Receiver<Snapshot>) -> Result<()> {
    loop {
        let snapshot = snapshots.borrow().clone();
        if snapshot.connection == ConnectionState::Connected && snapshot.participant_id.is_some() {
            return Ok(());
        }
        if snapshot.connection == ConnectionState::TerminalFailure {
            let failure = snapshot.terminal_failure.map_or_else(
                || "native agent failed".to_owned(),
                |failure| failure.message,
            );
            anyhow::bail!(failure);
        }
        snapshots.changed().await?;
    }
}

async fn reconcile_video_subscriptions(
    agent: &Agent,
    desired: &mut DesiredState,
    snapshot: &Snapshot,
    subscribed_participants: &mut HashSet<String>,
) -> Result<()> {
    let (video, participants) = video_subscriptions(snapshot);
    subscribed_participants.extend(participants);
    if desired.video == video {
        return Ok(());
    }
    let mut replacement = desired.clone();
    replacement.revision = desired.revision.saturating_add(1);
    replacement.video = video;
    agent.replace_desired(replacement.clone()).await?;
    *desired = replacement;
    Ok(())
}

fn video_subscriptions(snapshot: &Snapshot) -> (Vec<VideoSubscription>, HashSet<String>) {
    let mut participants = HashSet::new();
    let mut subscriptions = Vec::with_capacity(usize::from(REMOTE_VIDEO_SLOTS));
    for publication in snapshot.publications.values() {
        if publication.kind != MediaKind::Video
            || snapshot.participant_id.as_deref() == Some(&publication.participant_id)
            || participants.contains(&publication.participant_id)
        {
            continue;
        }
        if subscriptions.len() == usize::from(REMOTE_VIDEO_SLOTS) {
            break;
        }
        let Ok(slot) = u8::try_from(subscriptions.len()) else {
            debug_assert!(false, "video subscription index exceeded u8");
            break;
        };
        debug_assert!(slot < REMOTE_VIDEO_SLOTS);
        subscriptions.push(VideoSubscription {
            slot,
            track_id: publication.id.clone(),
            height: 720,
            min_height: 0,
            min_fps: 0,
            priority: 0,
        });
        participants.insert(publication.participant_id.clone());
    }
    (subscriptions, participants)
}

async fn latency_writer_task(mut rx: mpsc::Receiver<EventLatency>, file: File) -> Result<()> {
    let mut writer = BufWriter::with_capacity(16 * 1024, file);
    let _ = writer
        .write_all("elapsed_ms,room_id,agent_id,delay_us\n".as_bytes())
        .await;
    let mut count = 0usize;

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
        count = count.saturating_add(1);

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

fn keyframe_matches(event: &AgentEvent, encoding: Option<&str>) -> bool {
    matches!(
        event,
        AgentEvent::KeyframeRequested {
            slot,
            encoding: requested,
        } if slot == "camera" && requested.as_deref() == encoding
    )
}

fn annex_b_has_nal(data: &[u8], wanted: u8) -> bool {
    let mut index = 0usize;
    while index.saturating_add(3) < data.len() {
        let header = if data.get(index..index.saturating_add(4)) == Some(&[0, 0, 0, 1]) {
            index.saturating_add(4)
        } else if data.get(index..index.saturating_add(3)) == Some(&[0, 0, 1]) {
            index.saturating_add(3)
        } else {
            index = index.saturating_add(1);
            continue;
        };
        if data.get(header).is_some_and(|byte| byte & 0x1f == wanted) {
            return true;
        }
        index = header;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use pulsebeam_agent_native::agent_core::Publication;

    #[test]
    fn benchmark_arguments_keep_the_documented_surface() {
        let cli = Cli::try_parse_from([
            "pulsebeam-cli",
            "--api-url",
            "http://127.0.0.1:7070",
            "bench",
            "--rooms",
            "2",
            "--users-per-room",
            "3",
            "--max-rooms",
            "8",
            "--simulcast",
        ])
        .expect("documented benchmark arguments must parse");
        let Commands::Bench(config) = cli.command;
        assert_eq!(config.rooms, 2);
        assert_eq!(config.users_per_room, 3);
        assert_eq!(config.max_rooms, 8);
        assert!(config.simulcast);
    }

    #[test]
    fn video_policy_excludes_self_audio_and_tracks_beyond_capacity() {
        let mut snapshot = Snapshot {
            participant_id: Some("self".into()),
            ..Default::default()
        };
        snapshot.publications.insert(
            "self-video".into(),
            Publication {
                id: "self-video".into(),
                participant_id: "self".into(),
                kind: MediaKind::Video,
            },
        );
        snapshot.publications.insert(
            "remote-audio".into(),
            Publication {
                id: "remote-audio".into(),
                participant_id: "remote-audio".into(),
                kind: MediaKind::Audio,
            },
        );
        for participant in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            let track_id = format!("video-{participant}");
            snapshot.publications.insert(
                track_id.clone(),
                Publication {
                    id: track_id,
                    participant_id: participant.into(),
                    kind: MediaKind::Video,
                },
            );
        }

        let (subscriptions, participants) = video_subscriptions(&snapshot);

        assert_eq!(subscriptions.len(), usize::from(REMOTE_VIDEO_SLOTS));
        assert_eq!(participants.len(), usize::from(REMOTE_VIDEO_SLOTS));
        assert!(subscriptions.iter().all(|subscription| {
            subscription.track_id != "self-video" && subscription.track_id != "remote-audio"
        }));
    }
}

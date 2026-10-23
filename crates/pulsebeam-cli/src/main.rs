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

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use pulsebeam_agent_native::agent_core::{
    AgentConfig, ConnectionState, DesiredState, MediaKind, MediaSlot, MediaTopology,
    PublicationIntent, Snapshot, VideoSubscription,
};
use pulsebeam_agent_native::{
    Agent, Config, Encoding, H264Frame, Host, InputFrame, LocalSource, MediaStreamUpdate,
    NativeStatistics, ReceiveMode, RemoteMedia, StreamUpdate, VideoInput, VideoPublicationConfig,
    VideoSourceConfig,
};
use pulsebeam_core::auth::{
    ApiKey, ApiSigningKey, DEVELOPMENT_API_KEY_ID, DEVELOPMENT_API_SIGNING_KEY,
    DEVELOPMENT_PROJECT_ID, KeyRegistry, KeyRegistryEntry,
};
use pulsebeam_core::identity::{
    ApiKeyId, AudioTrackId, ConnectionId, DataTrackId, ParticipantExternalId, ParticipantId,
    ProjectId, RoomExternalId, RoomId, VideoTrackId,
};
use std::{
    collections::HashSet,
    io::Read,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tachyonix as mpsc;
use tokio::sync::watch;
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
    fn new() -> Result<Self> {
        use pulsebeam_testdata::QualityVideoLayer::{P180, P360, P720};
        Ok(Self {
            full: VideoAsset::new(P720)?,
            half: VideoAsset::new(P360)?,
            quarter: VideoAsset::new(P180)?,
        })
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
    width: u16,
    height: u16,
}

impl VideoAsset {
    fn new(layer: pulsebeam_testdata::QualityVideoLayer) -> Result<Self> {
        let corpus = pulsebeam_testdata::quality_corpus_video(
            pulsebeam_testdata::QualityVideoSource::Zero,
            layer,
        );
        let frames: Arc<[Arc<[u8]>]> = (0..corpus.len())
            .filter_map(|index| corpus.frame(index).map(|frame| Arc::from(frame.encoded)))
            .collect::<Vec<_>>()
            .into();
        anyhow::ensure!(
            frames.len() == corpus.len() && !frames.is_empty(),
            "incomplete encoded video fixture"
        );
        let first_keyframe = frames
            .iter()
            .position(|frame| annex_b_has_nal(frame, 5))
            .context("video fixture has no keyframe")?;
        let (width, height) = layer.dimensions();
        Ok(Self {
            frames,
            first_keyframe,
            width: width.try_into()?,
            height: height.try_into()?,
        })
    }

    fn frame(&self, index: usize, timestamp_us: i64) -> Option<H264Frame> {
        let payload = self.frames.get(index)?.to_vec();
        Some(H264Frame {
            timestamp_us,
            width: self.width,
            height: self.height,
            keyframe: annex_b_has_nal(&payload, 5),
            temporal_index: None,
            base_layer_sync: false,
            payload,
        })
    }
}

struct VideoSource {
    assets: Vec<VideoAsset>,
    index: usize,
    fps: u32,
}

impl VideoSource {
    fn new(assets: Vec<VideoAsset>, fps: u32) -> Self {
        Self {
            assets,
            index: 0,
            fps: fps.max(1),
        }
    }

    fn rewind(&mut self) {
        if let Some(asset) = self.assets.first() {
            self.index = asset.first_keyframe;
        }
    }

    async fn run(mut self, media: LocalSource) {
        let capture_origin = match media.capture_time_us().await {
            Ok(time) => time,
            Err(error) => {
                tracing::warn!(%error, "native producer capture clock unavailable");
                return;
            }
        };
        let feedback = media.feedback();
        let started = tokio::time::Instant::now();
        let mut interval =
            tokio::time::interval(Duration::from_secs_f64(1.0 / f64::from(self.fps)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                update = feedback.next() => match update {
                    StreamUpdate::Closed => return,
                    StreamUpdate::Lagged { skipped } => { tracing::warn!(skipped, "bench producer feedback lagged"); self.rewind(); },
                    StreamUpdate::Value(feedback) => {
                        if let Some(error) = feedback.encoder_error { tracing::warn!(?error, "native encoder feedback"); }
                        if feedback.keyframe_requested || feedback.encoding_keyframes_requested.iter().any(|request| *request) { self.rewind(); }
                    }
                },
                capture = interval.tick() => {
                    let Some(timestamp_us) = i64::try_from(capture.duration_since(started).as_micros()).ok()
                        .and_then(|offset| capture_origin.checked_add(offset)) else {
                            tracing::warn!("native producer capture timestamp exhausted");
                            return;
                        };
                    let Some(mut frames): Option<Vec<_>> = self.assets.iter().map(|asset| asset.frame(self.index, timestamp_us)).collect() else { return; };
                    let frame = match frames.len() {
                        1 => { let Some(frame) = frames.pop() else { return; }; InputFrame::H264 { frame } },
                        3 => InputFrame::H264Simulcast { frames },
                        _ => return,
                    };
                    if let Err(error) = media.submit(frame).await {
                        if matches!(error, pulsebeam_agent_native::Error::Closed) { return; }
                        tracing::warn!(%error, "bench native input admission refused");
                    }
                    self.index = self.index.saturating_add(1);
                    if self.assets.first().is_some_and(|asset| self.index >= asset.frames.len()) { self.index = 0; }
                }
            }
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
    Key(KeyConfig),
    Bench(BenchConfig),
    Id {
        #[command(subcommand)]
        command: IdCommand,
    },
    /// Mint a bearer token for a local development server
    Token(TokenConfig),
}

#[derive(Args)]
struct KeyConfig {
    #[command(subcommand)]
    command: Option<KeyCommand>,
}

#[derive(Subcommand)]
enum KeyCommand {
    /// Generate another API key for the project in a key registry read from stdin
    Rotate,
}

#[derive(Args)]
struct TokenConfig {
    #[arg(long)]
    room: RoomExternalId,
    #[arg(long)]
    participant: ParticipantExternalId,
    /// Token lifetime in seconds
    #[arg(long, default_value_t = 3_600)]
    ttl: u64,
}

#[derive(Subcommand)]
enum IdCommand {
    Room(RoomIdConfig),
    Participant(ParticipantIdConfig),
    Track(TrackIdConfig),
}

#[derive(Args)]
struct RoomIdConfig {
    #[arg(long)]
    project_id: ProjectId,
    #[arg(long)]
    room_external_id: RoomExternalId,
}

#[derive(Args)]
struct ParticipantIdConfig {
    #[arg(long)]
    project_id: ProjectId,
    #[arg(long)]
    room_external_id: RoomExternalId,
    #[arg(long)]
    participant_external_id: ParticipantExternalId,
}

#[derive(Clone, Copy, ValueEnum)]
enum TrackKindArg {
    Audio,
    Video,
    Data,
}

#[derive(Args)]
struct TrackIdConfig {
    #[arg(long)]
    connection_id: ConnectionId,
    #[arg(long, value_enum)]
    kind: TrackKindArg,
    #[arg(long)]
    label: String,
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
    delay_us: Option<u64>,
}

pub struct EventSnapshot {
    captured_at: Instant,
    room_id: usize,
    agent_id: usize,
    tx_bytes: Option<u64>,
    rx_bytes: Option<u64>,
    rtt_us: Option<u64>,
    loss_pct: Option<f64>,
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
            Commands::Key(config) => match config.command {
                None => generate_key()?,
                Some(KeyCommand::Rotate) => rotate_key()?,
            },
            Commands::Bench(config) => {
                run_bench(cli.api_url, config).await?;
            }
            Commands::Id { command } => println!("{}", derive_id(command)),
            Commands::Token(config) => println!("{}", development_token(config)?),
        }
        anyhow::Ok(())
    })
}

fn mint_development_token(
    room: &RoomExternalId,
    participant: &ParticipantExternalId,
    expiration: u64,
) -> Result<String> {
    let key = ApiKey::from_signing_key(
        *DEVELOPMENT_PROJECT_ID.as_bytes(),
        *DEVELOPMENT_API_KEY_ID.as_bytes(),
        &DEVELOPMENT_API_SIGNING_KEY,
    )?;
    Ok(pulsebeam_server::sign_participant_token(
        &key.to_secret_string(),
        room.as_str(),
        participant.as_str(),
        expiration,
    )?)
}

fn development_token(config: TokenConfig) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    development_token_at(config, now)
}

fn development_token_at(config: TokenConfig, now: u64) -> Result<String> {
    if config.ttl == 0 {
        anyhow::bail!("--ttl must be greater than zero");
    }
    let exp = now
        .checked_add(config.ttl)
        .context("token expiration exceeds the supported timestamp range")?;
    mint_development_token(&config.room, &config.participant, exp)
        .context("cannot mint development token")
}

fn random_signing_key() -> ApiSigningKey {
    let mut seed = [0u8; 32];
    let mut rng: rand::rngs::StdRng = rand::make_rng();
    rand::Rng::fill_bytes(&mut rng, &mut seed);
    ApiSigningKey::from_seed(seed)
}

fn generate_key() -> Result<()> {
    let (api_key, registry) = create_api_key(ProjectId::new(), Vec::new(), random_signing_key())?;
    print_key_material(&api_key, &registry)
}

fn rotate_key() -> Result<()> {
    let mut json = String::new();
    std::io::stdin()
        .read_to_string(&mut json)
        .context("cannot read key registry from stdin")?;
    if json.trim().is_empty() {
        anyhow::bail!("key rotate requires key registry JSON on stdin");
    }
    let registry = KeyRegistry::parse_json(&json).context("invalid key registry on stdin")?;
    let (api_key, updated) = create_api_key(
        registry.project_id(),
        registry.keys().to_vec(),
        random_signing_key(),
    )?;
    print_key_material(&api_key, &updated)
}

fn create_api_key(
    project_id: ProjectId,
    mut keys: Vec<KeyRegistryEntry>,
    signing_key: ApiSigningKey,
) -> Result<(String, KeyRegistry)> {
    let key_id = ApiKeyId::new();
    let api_key =
        ApiKey::from_signing_key(*project_id.as_bytes(), *key_id.as_bytes(), &signing_key)?
            .to_secret_string();
    keys.push(KeyRegistryEntry {
        key_id,
        public_key: signing_key.public_key(),
    });
    Ok((api_key, KeyRegistry::new(project_id, keys)?))
}

fn print_key_material(api_key: &str, registry: &KeyRegistry) -> Result<()> {
    println!("API key (private; pass explicitly to your backend server SDK):");
    println!("{api_key}");
    println!();
    println!("Key registry (public; configure the PulseBeam server with this JSON):");
    println!("{}", registry.to_pretty_json()?);
    Ok(())
}

fn derive_id(command: IdCommand) -> String {
    match command {
        IdCommand::Room(config) => {
            RoomId::derive(&config.project_id, &config.room_external_id).as_str()
        }
        IdCommand::Participant(config) => {
            let room = RoomId::derive(&config.project_id, &config.room_external_id);
            ParticipantId::derive(&room, &config.participant_external_id).as_str()
        }
        IdCommand::Track(config) => match config.kind {
            TrackKindArg::Audio => {
                AudioTrackId::derive(&config.connection_id, &config.label).as_str()
            }
            TrackKindArg::Video => {
                VideoTrackId::derive(&config.connection_id, &config.label).as_str()
            }
            TrackKindArg::Data => {
                DataTrackId::derive(&config.connection_id, &config.label).as_str()
            }
        },
    }
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
    let assets = VideoAssets::new()?;
    let started = Instant::now();
    let mut join_set = JoinSet::new();

    let latency_writer_handle = tokio::spawn(latency_writer_task(latency_rx, latency_csv, started));
    let snapshot_writer_handle =
        tokio::spawn(snapshot_writer_task(snapshot_rx, snapshots_csv, started));

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
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let session = benchmark_agent_config(&ctx.api_url, &room_name, ctx.agent_id, duration, now)?;
    let mut config = Config::new(session);
    config.media.video_input = VideoInput::EncodedH264;
    config.media.video_publications = vec![VideoPublicationConfig {
        slot: "v0".into(),
        source: benchmark_video_profile(simulcast),
    }];
    let agent = Agent::spawn(config, Host::new(Box::new(reqwest::Client::new()))).await?;
    let mut desired = DesiredState {
        revision: 1,
        connected: true,
        publications: vec![
            PublicationIntent {
                slot: "v0".into(),
                label: "camera".into(),
                active: true,
            },
            PublicationIntent {
                slot: "a0".into(),
                label: "microphone".into(),
                active: true,
            },
        ],
        ..Default::default()
    };
    let mut snapshots = agent.snapshots();
    agent.replace_desired(desired.clone()).await?;
    wait_until_connected(&mut snapshots).await?;

    let mut media_tasks = JoinSet::new();
    let camera = agent.source(MediaSlot::LocalVideo("v0".into())).await?;
    let assets = if simulcast {
        ["q", "h", "f"]
            .into_iter()
            .map(|encoding| ctx.assets.for_encoding(Some(encoding)))
            .collect()
    } else {
        vec![ctx.assets.for_encoding(None)]
    };
    media_tasks.spawn(VideoSource::new(assets, pulsebeam_testdata::QUALITY_VIDEO_FPS).run(camera));

    let (received_video_tx, received_video) = watch::channel(false);
    for slot in 0..REMOTE_VIDEO_SLOTS {
        let remote = agent
            .remote(MediaSlot::RemoteVideo(slot), ReceiveMode::Encoded)
            .await?;
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
                if let Some(stats) = stats {
                    match native_counters(&stats) {
                        Ok(counters) => {
                            let _ = ctx.logger.snapshot.try_send(EventSnapshot {
                                captured_at: Instant::now(), room_id: ctx.room_id, agent_id: ctx.agent_id,
                                tx_bytes: counters.tx_bytes, rx_bytes: counters.rx_bytes,
                                rtt_us: counters.rtt_us, loss_pct: counters.loss_pct,
                            });
                        }
                        Err(error) => tracing::warn!(%error, "native statistics report is unreadable"),
                    }
                }
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

fn benchmark_agent_config(
    api_url: &str,
    room_name: &str,
    agent_id: usize,
    duration: Duration,
    now: u64,
) -> Result<AgentConfig> {
    let room = RoomExternalId::new(room_name)?;
    let participant = ParticipantExternalId::new(&format!("bench-{agent_id}"))?;
    let exp = now.saturating_add(duration.as_secs()).saturating_add(60);
    Ok(AgentConfig {
        endpoint: api_url.to_string(),
        token: mint_development_token(&room, &participant, exp)?,
        topology: MediaTopology {
            local_video: 1,
            local_audio: 1,
            remote_video: REMOTE_VIDEO_SLOTS,
            remote_audio: REMOTE_AUDIO_SLOTS,
        },
        retry: Default::default(),
        log_level: Default::default(),
    })
}

async fn handle_receiving(
    media: RemoteMedia,
    ctx: AgentContext,
    received_video: watch::Sender<bool>,
) {
    let mut received = false;
    loop {
        match media.next().await {
            MediaStreamUpdate::Closed => return,
            MediaStreamUpdate::Track { .. } => {}
            MediaStreamUpdate::Lagged { skipped } => {
                tracing::warn!(skipped, "bench media observer lagged");
            }
            MediaStreamUpdate::NativeDropped { skipped } => {
                tracing::warn!(skipped, "native media sink dropped frames");
            }
            MediaStreamUpdate::Frame { .. } => {
                if !received {
                    received = true;
                    received_video.send_replace(true);
                    tracing::info!(
                        room_id = ctx.room_id,
                        agent_id = ctx.agent_id,
                        "bench agent received native video; uncorrelated capture timestamps are not end-to-end latency"
                    );
                }
                let _ = ctx.logger.latency.try_send(EventLatency {
                    captured_at: Instant::now(),
                    room_id: ctx.room_id,
                    agent_id: ctx.agent_id,
                    // Native capture metadata does not establish a common sender/
                    // observer clock. Preserve observations without inventing latency.
                    delay_us: None,
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
            selector: None,
            height: 720,
            min_height: 0,
            min_fps: 0,
            priority: 0,
            playout_delay: Default::default(),
        });
        participants.insert(publication.participant_id.clone());
    }
    (subscriptions, participants)
}

async fn latency_writer_task(
    mut rx: mpsc::Receiver<EventLatency>,
    file: File,
    started: Instant,
) -> Result<()> {
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
                    e.captured_at.saturating_duration_since(started).as_millis(),
                    e.room_id,
                    e.agent_id,
                    csv_cell(e.delay_us),
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

async fn snapshot_writer_task(
    mut rx: mpsc::Receiver<EventSnapshot>,
    file: File,
    started: Instant,
) -> Result<()> {
    let mut writer = BufWriter::with_capacity(4 * 1024, file);
    let _ = writer
        .write_all("elapsed_ms,room_id,agent_id,tx_bytes,rx_bytes,rtt_us,loss_pct\n".as_bytes())
        .await;

    while let Ok(e) = rx.recv().await {
        let _ = writer
            .write_all(
                format!(
                    "{},{},{},{},{},{},{}\n",
                    e.captured_at.saturating_duration_since(started).as_millis(),
                    e.room_id,
                    e.agent_id,
                    csv_cell(e.tx_bytes),
                    csv_cell(e.rx_bytes),
                    csv_cell(e.rtt_us),
                    e.loss_pct
                        .map(|value| format!("{value:.4}"))
                        .unwrap_or_default()
                )
                .as_bytes(),
            )
            .await;
        let _ = writer.flush().await;
    }
    let _ = writer.flush().await;
    Ok(())
}

fn csv_cell<T: ToString>(value: Option<T>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

fn benchmark_video_profile(simulcast: bool) -> VideoSourceConfig {
    let encodings = if simulcast {
        [
            ("q", 4.0, 100_000),
            ("h", 2.0, 400_000),
            ("f", 1.0, 1_000_000),
        ]
        .into_iter()
        .map(|(rid, scale_down_by, max_bitrate_bps)| Encoding {
            rid: Some(rid.into()),
            scale_down_by,
            max_bitrate_bps: Some(max_bitrate_bps),
            max_framerate: Some(30.0),
            temporal_layers: 1,
        })
        .collect()
    } else {
        vec![Encoding {
            max_bitrate_bps: Some(400_000),
            max_framerate: Some(30.0),
            ..Encoding::default()
        }]
    };
    VideoSourceConfig {
        encodings,
        ..VideoSourceConfig::default()
    }
}

#[derive(Debug, PartialEq)]
struct NativeCounters {
    tx_bytes: Option<u64>,
    rx_bytes: Option<u64>,
    rtt_us: Option<u64>,
    loss_pct: Option<f64>,
}

fn sum_counter<'a>(
    records: impl Iterator<Item = &'a serde_json::Value>,
    field: &str,
) -> Option<u64> {
    let mut total: Option<u64> = None;
    for record in records {
        total = Some(
            total
                .unwrap_or(0)
                .checked_add(record.get(field)?.as_u64()?)?,
        );
    }
    total
}

#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    reason = "Presentation ratio of native counters with a nonzero checked denominator"
)]
fn loss_percentage(lost: u64, received: u64) -> Option<f64> {
    let total = lost.checked_add(received)?;
    (total > 0).then(|| (lost as f64) * 100.0 / (total as f64))
}

fn native_counters(report: &NativeStatistics) -> Result<NativeCounters> {
    let json: serde_json::Value = serde_json::from_slice(&report.report_json)?;
    let records: Vec<_> = match &json {
        serde_json::Value::Array(records) => records.iter().collect(),
        serde_json::Value::Object(records) => records.values().collect(),
        _ => anyhow::bail!("native RTC statistics are not a collection"),
    };
    let transports: Vec<_> = records
        .iter()
        .copied()
        .filter(|record| {
            record.get("type").and_then(serde_json::Value::as_str) == Some("transport")
        })
        .collect();
    let inbound: Vec<_> = records
        .iter()
        .copied()
        .filter(|record| {
            record.get("type").and_then(serde_json::Value::as_str) == Some("inbound-rtp")
        })
        .collect();
    let rtt_us = transports
        .iter()
        .filter_map(|transport| {
            let selected = transport.get("selectedCandidatePairId")?.as_str()?;
            let pair = records.iter().find(|record| {
                record.get("id").and_then(serde_json::Value::as_str) == Some(selected)
            })?;
            let seconds = pair.get("currentRoundTripTime")?.as_f64()?;
            u64::try_from(Duration::try_from_secs_f64(seconds).ok()?.as_micros()).ok()
        })
        .max();
    let loss_pct = sum_counter(inbound.iter().copied(), "packetsLost")
        .zip(sum_counter(inbound.iter().copied(), "packetsReceived"))
        .and_then(|(lost, received)| loss_percentage(lost, received));
    Ok(NativeCounters {
        tx_bytes: sum_counter(transports.iter().copied(), "bytesSent"),
        rx_bytes: sum_counter(transports.iter().copied(), "bytesReceived"),
        rtt_us,
        loss_pct,
    })
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
    use pulsebeam_core::auth::{
        DEVELOPMENT_API_KEY_ID, DEVELOPMENT_API_PUBLIC_KEY, DEVELOPMENT_PROJECT_ID,
        KeyRegistryEntry, verify_participant_token,
    };

    fn development_registry() -> KeyRegistry {
        KeyRegistry::new(
            *DEVELOPMENT_PROJECT_ID,
            vec![KeyRegistryEntry {
                key_id: *DEVELOPMENT_API_KEY_ID,
                public_key: *DEVELOPMENT_API_PUBLIC_KEY,
            }],
        )
        .unwrap()
    }

    #[test]
    fn native_fixture_rungs_have_genuine_units_and_matching_dimensions() {
        let assets = VideoAssets::new().unwrap();
        let frames: Vec<_> = ["q", "h", "f"]
            .into_iter()
            .map(|rid| {
                let asset = assets.for_encoding(Some(rid));
                assert_eq!(
                    asset.frames.len(),
                    pulsebeam_testdata::QUALITY_CORPUS_VIDEO_FRAME_COUNT
                );
                asset.frame(asset.first_keyframe, 42).unwrap()
            })
            .collect();
        assert_eq!(
            frames
                .iter()
                .map(|frame| (frame.width, frame.height))
                .collect::<Vec<_>>(),
            [(320, 180), (640, 360), (1280, 720)]
        );
        assert!(
            frames
                .iter()
                .all(|frame| frame.keyframe && frame.timestamp_us == 42)
        );
        let mut payloads = frames.iter().map(|frame| &frame.payload);
        let q = payloads.next().unwrap();
        let h = payloads.next().unwrap();
        let f = payloads.next().unwrap();
        assert_ne!(q, h);
        assert_ne!(h, f);
        assert_ne!(q, f);
        let profile = benchmark_video_profile(true);
        assert_eq!(
            profile
                .encodings
                .iter()
                .map(|encoding| encoding.rid.as_deref())
                .collect::<Vec<_>>(),
            [Some("q"), Some("h"), Some("f")]
        );
        assert_eq!(
            profile
                .encodings
                .iter()
                .map(|encoding| encoding.scale_down_by.to_bits())
                .collect::<Vec<_>>(),
            [4.0f64.to_bits(), 2.0f64.to_bits(), 1.0f64.to_bits()]
        );
        assert_eq!(benchmark_video_profile(false).encodings.len(), 1);
    }

    #[test]
    fn csv_projects_native_counters_without_fabricating_missing_values() {
        let report = NativeStatistics { generation: 1, timestamp_us: 0, report_json: br#"[
            {"type":"transport","bytesSent":1200,"bytesReceived":2400,"selectedCandidatePairId":"pair"},
            {"id":"pair","type":"candidate-pair","currentRoundTripTime":0.0125},
            {"type":"inbound-rtp","packetsLost":2,"packetsReceived":98}
        ]"#.to_vec() };
        let counters = native_counters(&report).unwrap();
        assert_eq!(counters.tx_bytes, Some(1200));
        assert_eq!(counters.rx_bytes, Some(2400));
        assert_eq!(counters.rtt_us, Some(12_500));
        assert_eq!(counters.loss_pct.map(f64::to_bits), Some(2.0f64.to_bits()));
        let empty = native_counters(&NativeStatistics {
            report_json: b"[]".to_vec(),
            ..report
        })
        .unwrap();
        assert_eq!(
            empty,
            NativeCounters {
                tx_bytes: None,
                rx_bytes: None,
                rtt_us: None,
                loss_pct: None
            }
        );
        assert_eq!(csv_cell(empty.tx_bytes), "");
        assert_eq!(csv_cell(Some(0)), "0");
        assert!(loss_percentage(0, 0).is_none());
    }

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
        let Commands::Bench(config) = cli.command else {
            panic!("bench command must parse as bench");
        };
        assert_eq!(config.rooms, 2);
        assert_eq!(config.users_per_room, 3);
        assert_eq!(config.max_rooms, 8);
        assert!(config.simulcast);
    }

    #[test]
    fn benchmark_participants_get_distinct_development_tokens() {
        let duration = Duration::from_secs(120);
        let alice = benchmark_agent_config(
            "http://127.0.0.1:7070",
            "bench-room-7",
            7_000,
            duration,
            1_000,
        )
        .unwrap();
        let bob = benchmark_agent_config(
            "http://127.0.0.1:7070",
            "bench-room-7",
            7_001,
            duration,
            1_000,
        )
        .unwrap();

        assert_eq!(alice.endpoint, "http://127.0.0.1:7070");
        assert_ne!(alice.token, bob.token);
        let alice_auth =
            verify_participant_token(&development_registry(), &alice.token, 1_000).unwrap();
        let bob_auth =
            verify_participant_token(&development_registry(), &bob.token, 1_000).unwrap();
        assert_eq!(alice_auth.room_external_id.as_str(), "bench-room-7");
        assert_eq!(bob_auth.room_external_id.as_str(), "bench-room-7");
        assert_eq!(alice_auth.participant_external_id.as_str(), "bench-7000");
        assert_eq!(bob_auth.participant_external_id.as_str(), "bench-7001");
        assert_ne!(alice_auth.participant_id, bob_auth.participant_id);
        assert_eq!(alice_auth.expiry.unix_seconds(), 1_180);
        assert_eq!(bob_auth.expiry.unix_seconds(), 1_180);
    }

    #[test]
    fn token_command_mints_only_the_development_profile() {
        let cli = Cli::try_parse_from([
            "pulsebeam-cli",
            "token",
            "--room",
            "general",
            "--participant",
            "alice",
            "--ttl",
            "1000",
        ])
        .unwrap();
        let Commands::Token(config) = cli.command else {
            panic!("token command must parse as token");
        };
        let token = development_token_at(config, 1_000).unwrap();
        assert_eq!(
            token,
            "eyJhbGciOiJFZERTQSIsImtpZCI6ImtpZF8wMDAwMDAwMDAwMVIwMTAwMDAwMDAwMDAwMDgiLCJ0eXAiOiJwYitqd3QifQ.eyJpc3MiOiJwXzAwMDAwMDAwMDAxUjAxMDAwMDAwMDAwMDAwNCIsImF1ZCI6InBiIiwic3ViIjoiYWxpY2UiLCJyb29tIjoiZ2VuZXJhbCIsImV4cCI6MjAwMH0.Bp8UJVINeP0cqUiG_0qSk4wjqDg5MGZqRcBel3Qy176HoTy0OQvTpTp5Uuav5k2Wlsgdd58rPt6DLiWVt0U1AQ"
        );
        let registry = development_registry();
        assert!(verify_participant_token(&registry, &token, 1_999).is_ok());

        let default_cli = Cli::try_parse_from([
            "pulsebeam-cli",
            "token",
            "--room",
            "general",
            "--participant",
            "alice",
        ])
        .unwrap();
        let Commands::Token(config) = default_cli.command else {
            panic!("token command must parse as token");
        };
        assert_eq!(config.ttl, 3_600);
        assert!(
            Cli::try_parse_from([
                "pulsebeam-cli",
                "token",
                "--room",
                "general",
                "--participant",
                "alice",
                "--signing-key",
                "secret",
            ])
            .is_err()
        );
    }

    #[test]
    fn token_command_rejects_zero_or_overflowing_ttl() {
        let config = TokenConfig {
            room: RoomExternalId::new("general").unwrap(),
            participant: ParticipantExternalId::new("alice").unwrap(),
            ttl: 0,
        };
        assert!(development_token_at(config, 1_000).is_err());
        let config = TokenConfig {
            room: RoomExternalId::new("general").unwrap(),
            participant: ParticipantExternalId::new("alice").unwrap(),
            ttl: 1,
        };
        assert!(development_token_at(config, u64::MAX).is_err());
    }

    #[test]
    fn identity_commands_use_the_canonical_core_derivation() {
        let project = ProjectId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1]);
        let project_text = project.as_str();
        let connection =
            ConnectionId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1]);
        let connection_text = connection.as_str();
        let track_cli = Cli::try_parse_from([
            "pulsebeam-cli",
            "id",
            "track",
            "--connection-id",
            &connection_text,
            "--kind",
            "video",
            "--label",
            "camera",
        ])
        .expect("documented identity arguments must parse");
        let Commands::Id { command } = track_cli.command else {
            panic!("id command must parse as id");
        };

        let room = RoomId::derive(&project, &RoomExternalId::new("general").unwrap());
        let participant =
            ParticipantId::derive(&room, &ParticipantExternalId::new("alice").unwrap());
        assert_eq!(
            derive_id(command),
            VideoTrackId::derive(&connection, "camera").as_str()
        );

        let room_cli = Cli::try_parse_from([
            "pulsebeam-cli",
            "id",
            "room",
            "--project-id",
            &project_text,
            "--room-external-id",
            "general",
        ])
        .expect("room identity arguments must parse");
        let Commands::Id { command } = room_cli.command else {
            panic!("room command must parse as id");
        };
        assert_eq!(derive_id(command), room.as_str());

        let participant_cli = Cli::try_parse_from([
            "pulsebeam-cli",
            "id",
            "participant",
            "--project-id",
            &project_text,
            "--room-external-id",
            "general",
            "--participant-external-id",
            "alice",
        ])
        .expect("participant identity arguments must parse");
        let Commands::Id { command } = participant_cli.command else {
            panic!("participant command must parse as id");
        };
        assert_eq!(derive_id(command), participant.as_str());
    }

    #[test]
    fn key_command_is_argument_free_and_rotation_is_explicit() {
        let cli = Cli::try_parse_from(["pulsebeam-cli", "key"]).unwrap();
        let Commands::Key(config) = cli.command else {
            panic!("key command must parse");
        };
        assert!(config.command.is_none());

        let cli = Cli::try_parse_from(["pulsebeam-cli", "key", "rotate"]).unwrap();
        let Commands::Key(config) = cli.command else {
            panic!("key rotate command must parse");
        };
        assert!(matches!(config.command, Some(KeyCommand::Rotate)));

        assert!(Cli::try_parse_from(["pulsebeam-cli", "auth-key"]).is_err());
        assert!(Cli::try_parse_from(["pulsebeam-cli", "key", "--project-id", "anything"]).is_err());
        assert!(Cli::try_parse_from(["pulsebeam-cli", "key", "--output-dir", "auth"]).is_err());
    }

    #[test]
    fn key_generation_has_single_project_public_registry() {
        let project_id = ProjectId::new();
        let signing = ApiSigningKey::from_seed([7; 32]);
        let (api_key_text, registry) =
            create_api_key(project_id, Vec::new(), signing.clone()).unwrap();
        let api_key: ApiKey = api_key_text.parse().unwrap();

        assert_eq!(registry.project_id(), project_id);
        assert_eq!(registry.keys().len(), 1);
        assert_eq!(api_key.project_id_bytes(), project_id.as_bytes());
        assert_eq!(api_key.key_id_bytes(), registry.keys()[0].key_id.as_bytes());
        assert_eq!(api_key.signing_key(), signing);
        assert_eq!(registry.keys()[0].public_key, signing.public_key());

        let json = registry.to_pretty_json().unwrap();
        assert!(json.contains("\"projectId\""));
        assert!(json.contains("\"keyId\""));
        assert!(json.contains("\"publicKey\""));
        assert!(!json.contains(&api_key_text));
        assert!(!json.contains("signing"));
    }

    #[test]
    fn key_rotation_preserves_project_and_existing_public_keys() {
        let project_id = ProjectId::new();
        let (first_api_key, registry) =
            create_api_key(project_id, Vec::new(), ApiSigningKey::from_seed([8; 32])).unwrap();
        let first_key_id = registry.keys()[0].key_id;

        let (second_api_key, rotated) = create_api_key(
            registry.project_id(),
            registry.keys().to_vec(),
            ApiSigningKey::from_seed([9; 32]),
        )
        .unwrap();

        assert_eq!(rotated.project_id(), project_id);
        assert_eq!(rotated.keys().len(), 2);
        assert_eq!(rotated.keys()[0].key_id, first_key_id);
        assert_ne!(first_api_key, second_api_key);
        let second: ApiKey = second_api_key.parse().unwrap();
        assert_eq!(second.project_id_bytes(), project_id.as_bytes());
        assert_eq!(second.key_id_bytes(), rotated.keys()[1].key_id.as_bytes());
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
                label: "camera".into(),
                kind: MediaKind::Video,
            },
        );
        snapshot.publications.insert(
            "remote-audio".into(),
            Publication {
                id: "remote-audio".into(),
                participant_id: "remote-audio".into(),
                label: "microphone".into(),
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
                    label: "camera".into(),
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

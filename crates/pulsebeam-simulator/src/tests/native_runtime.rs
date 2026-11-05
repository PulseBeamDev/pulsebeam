use std::cell::Cell;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pulsebeam_agent_core::ffi as core_ffi;
use pulsebeam_agent_native::ffi::{Agent, EventUpdate, NativeEvent, SnapshotUpdate};
use pulsebeam_agent_native::{
    Agent as RuntimeAgent, AudioInput, Config, H264Frame, Host, InputFrame, MediaStreamUpdate,
    OpusFrame, OutputFrame, ReceiveMode, VideoInput,
};
use pulsebeam_core::identity::{ParticipantExternalId, RoomExternalId};

use super::common::client::create_http_client;
use super::common::native::NativeHost;
use super::common::native_audio::NativePcmLog;
use super::common::native_video::NativeDecodedVideoLog;
use super::common::{
    DEFAULT_SIM_SHARDS, reserve_subnet, run_sim_or_timeout, start_sfu_node_with, subnet_ip,
};

const NATIVE_RUNTIME_SEED: u64 = 1;

struct RuntimeRetirement(Rc<Cell<usize>>);

impl Drop for RuntimeRetirement {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_add(1));
    }
}

#[derive(Default)]
struct ExchangeReport {
    video_frames: u64,
    audio_frames: u64,
    latest_messages: u64,
    ordered_messages: u64,
    // Original owning values, held across later callbacks, reconnect and close.
    retained: Vec<OutputFrame>,
}

/// One bounded second of genuine native stereo Opus. Never rewrite packet bits
/// to reconcile a mono fixture with the SFU's negotiated stereo carrier.
fn stock_opus_fixture() -> &'static [OpusFrame] {
    static FIXTURE: OnceLock<Vec<OpusFrame>> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        use pulsebeam_rtc_simulation::{
            AudioEncoderFactory, AudioEncoderOptions, Environment, SdpAudioFormat,
        };
        use pulsebeam_testdata::{QualityAudioSource, quality_corpus_audio};

        let reference = [QualityAudioSource::Zero, QualityAudioSource::One].map(|source| {
            quality_corpus_audio(source)
                .decode_reference()
                .expect("signed little-endian PCM reference")
        });
        let environment = Environment::builder().build().expect("encoder environment");
        let factory = AudioEncoderFactory::builtin().expect("stock audio encoder factory");
        let mut format = SdpAudioFormat::new(b"opus".to_vec(), 48_000, 2);
        format.parameters.insert(b"stereo".to_vec(), b"1".to_vec());
        let encoder = factory
            .create(
                &environment,
                &format,
                &AudioEncoderOptions {
                    payload_type: 111,
                    codec_pair_id: None,
                },
            )
            .expect("native Opus encoder creation")
            .expect("stock Opus support");
        assert_eq!(encoder.channels(), 2);
        assert_eq!(encoder.sample_rate_hz(), 48_000);
        assert_eq!(encoder.frames_in_next_packet(), 2);
        encoder.set_target_bitrate(128_000).expect("stereo bitrate");
        assert!(encoder.set_dtx(false));
        let mut packets = Vec::new();
        for block in 0..100_u32 {
            let offset = usize::try_from(block).unwrap() * 480 * 2;
            let left = reference[0][offset..offset + 960].as_chunks::<2>().0;
            let right = reference[1][offset..offset + 960].as_chunks::<2>().0;
            let pcm: Vec<_> = left
                .iter()
                .zip(right)
                .flat_map(|(left, right)| [i16::from_le_bytes(*left), i16::from_le_bytes(*right)])
                .collect();
            let output = encoder
                .encode(block * 480, &pcm)
                .expect("stock Opus encode");
            if !output.bytes.is_empty() {
                assert_eq!(output.info.primary.encoded_bytes, output.bytes.len());
                packets.push(OpusFrame {
                    timestamp: output.info.primary.encoded_timestamp,
                    samples_per_channel: 960,
                    payload: output.bytes,
                    // Explicit producer declaration for this active reference interval,
                    // not inferred from native encoder speech/CNG metadata.
                    audio_level: Some(30),
                    voice_activity: true,
                });
            }
        }
        assert_eq!(packets.len(), 50);
        packets
    })
}

#[derive(Default)]
struct MediaObservation {
    pcm: NativePcmLog,
    video: NativeDecodedVideoLog,
    video_baseline: NativeDecodedVideoLog,
    active: bool,
    closed: bool,
    blocks: u64,
    retained: Option<OutputFrame>,
    failure: Option<String>,
}

/// Consume bounded native streams during control-plane waits too. Validate the
/// real native output and retain one owning value, never another frame queue.
struct MediaProbe {
    state: Arc<Mutex<MediaObservation>>,
    reader: tokio::task::JoinHandle<()>,
    kind: core_ffi::MediaKind,
    generation: u64,
}

impl Drop for MediaProbe {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl MediaProbe {
    fn start(
        receiver: Arc<pulsebeam_agent_native::ffi::RemoteMediaReceiver>,
        generation: u64,
        kind: core_ffi::MediaKind,
    ) -> Self {
        let state = Arc::new(Mutex::new(MediaObservation::default()));
        let observed = state.clone();
        let expected_kind = kind;
        let reader = tokio::task::spawn_local(async move {
            loop {
                let update = receiver.next().await;
                let mut state = observed.lock().unwrap();
                match update {
                    MediaStreamUpdate::Frame { frame } => {
                        if frame.track.generation != generation {
                            state.failure = Some("cross-generation native media".into());
                            return;
                        }
                        let retain = match (&frame.frame, expected_kind) {
                            (OutputFrame::Pcm { samples, .. }, core_ffi::MediaKind::Audio) => {
                                let nontrivial =
                                    samples.iter().any(|sample| sample.unsigned_abs() > 32);
                                state.pcm.observe(&frame, Instant::now());
                                nontrivial
                            }
                            (OutputFrame::I420 { frame: pixels }, core_ffi::MediaKind::Video)
                                if pixels.width == 320 && pixels.height == 180 =>
                            {
                                if state.active && state.blocks == 0 {
                                    tracing::debug!(generation, now = ?Instant::now(), previous = ?state.video, "first native video frame in media window");
                                }
                                state.video.observe(&frame, Instant::now());
                                true
                            }
                            _ => {
                                state.failure = Some("invalid native decoded media".into());
                                return;
                            }
                        };
                        state.blocks = state.blocks.saturating_add(1);
                        if retain && state.retained.is_none() {
                            state.retained = Some(frame.frame);
                        }
                    }
                    MediaStreamUpdate::Track { .. } => {}
                    MediaStreamUpdate::Lagged { skipped } => {
                        state.failure = Some(format!(
                            "native {expected_kind:?} observer lagged {skipped} frames"
                        ));
                        return;
                    }
                    MediaStreamUpdate::NativeDecodedDropped { skipped } => {
                        state.failure = Some(format!(
                            "native {expected_kind:?} decoded observer dropped {skipped} frames"
                        ));
                        return;
                    }
                    MediaStreamUpdate::NativeDropped { skipped } => {
                        state.failure = Some(format!(
                            "native {expected_kind:?} sink dropped {skipped} frames"
                        ));
                        return;
                    }
                    MediaStreamUpdate::Closed => {
                        state.closed = true;
                        return;
                    }
                }
            }
        });
        Self {
            state,
            reader,
            kind,
            generation,
        }
    }

    fn begin_window(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        anyhow::ensure!(state.failure.is_none(), "{:?}", state.failure);
        anyhow::ensure!(!state.closed, "native media observer closed before window");
        tracing::debug!(kind = ?self.kind, generation = self.generation, now = ?Instant::now(), previous = ?state.video, "native media window begins");
        state.active = true;
        state.pcm.begin_window();
        state.video_baseline = state.video.checkpoint_after_pause();
        state.blocks = 0;
        state.retained = None;
        Ok(())
    }

    fn ready(&self, generation: u64) -> anyhow::Result<bool> {
        anyhow::ensure!(
            generation == self.generation,
            "wrong media observer generation"
        );
        let state = self.state.lock().unwrap();
        anyhow::ensure!(state.failure.is_none(), "{:?}", state.failure);
        anyhow::ensure!(
            !state.active || !state.closed,
            "native media observer closed during window"
        );
        if self.kind == core_ffi::MediaKind::Video {
            return Ok(state
                .video
                .require(
                    &state.video_baseline,
                    generation,
                    (320, 180),
                    2,
                    Instant::now(),
                )
                .is_ok());
        }
        // Reuse the system PCM oracle: complete blocks, nontrivial signal,
        // same native generation and fresh, continuous 10 ms playout.
        Ok(state
            .pcm
            .require(generation, 2880, 1, None, Instant::now())
            .is_ok())
    }

    fn finish_window(&self, generation: u64) -> anyhow::Result<(u64, OutputFrame)> {
        if !self.ready(generation)? {
            let state = self.state.lock().unwrap();
            anyhow::bail!(
                "no qualified native {:?} media at window finish: generation={generation}, now={:?}, video={:?}, pcm={:?}",
                self.kind,
                Instant::now(),
                state.video,
                state.pcm,
            );
        }
        let mut state = self.state.lock().unwrap();
        anyhow::ensure!(state.failure.is_none(), "{:?}", state.failure);
        let retained = state
            .retained
            .take()
            .ok_or_else(|| anyhow::anyhow!("no owning native frame"))?;
        state.active = false;
        Ok((state.blocks, retained))
    }
}

#[derive(Default)]
struct TopicObservation {
    route_ready: bool,
    latest_messages: u64,
    ordered_messages: u64,
    ordered_sequence: Option<u64>,
    failure: Option<String>,
}

/// Validate messages continuously, including while the caller awaits native
/// commands. Retain only bounded oracle state, not another event/message queue.
struct TopicProbe {
    state: Arc<Mutex<TopicObservation>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for TopicProbe {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl TopicProbe {
    fn start(
        events: Arc<pulsebeam_agent_native::ffi::EventStream>,
        local_name: &str,
        latest: &core_ffi::TopicPublisher,
        ordered: &core_ffi::TopicPublisher,
        round: u64,
    ) -> Self {
        let state = Arc::new(Mutex::new(TopicObservation::default()));
        let observed = state.clone();
        let local_name = local_name.to_owned();
        let latest = latest.name.clone();
        let ordered = ordered.name.clone();
        let reader = tokio::task::spawn_local(async move {
            loop {
                let update = events.next().await;
                let mut state = observed.lock().unwrap();
                let result = (|| {
                    match update {
                        EventUpdate::Event {
                            event:
                                NativeEvent::Core {
                                    notification:
                                        core_ffi::Notification::Topic {
                                            notification:
                                                core_ffi::TopicNotification::Message { message },
                                        },
                                },
                        } => match message.mode {
                            core_ffi::TopicMode::Latest => {
                                if message.name == latest
                                    && message.payload == format!("latest-{round}").as_bytes()
                                    && message.participant_id != local_name
                                {
                                    state.latest_messages = state.latest_messages.saturating_add(1);
                                }
                            }
                            core_ffi::TopicMode::Ordered => {
                                if message.name == ordered
                                    && message.payload == b"route-ready"
                                    && message.participant_id != local_name
                                {
                                    state.route_ready = true;
                                } else if message.payload.starts_with(b"ordered-") {
                                    anyhow::ensure!(message.name == ordered, "wrong ordered topic");
                                    anyhow::ensure!(
                                        message.participant_id != local_name,
                                        "ordered topic looped back to sender"
                                    );
                                    anyhow::ensure!(
                                        message.payload
                                            == format!(
                                                "ordered-{round}-{}",
                                                state.ordered_messages
                                            )
                                            .as_bytes(),
                                        "ordered topic payload arrived out of order"
                                    );
                                    if let Some(previous) = state.ordered_sequence {
                                        anyhow::ensure!(
                                            Some(message.sequence) == previous.checked_add(1),
                                            "ordered topic sequence gap"
                                        );
                                    }
                                    state.ordered_sequence = Some(message.sequence);
                                    state.ordered_messages =
                                        state.ordered_messages.saturating_add(1);
                                }
                            }
                            core_ffi::TopicMode::Lossy => {
                                anyhow::bail!("undeclared lossy mode delivered to native fixture")
                            }
                        },
                        EventUpdate::Event {
                            event:
                                NativeEvent::Core {
                                    notification:
                                        core_ffi::Notification::Topic {
                                            notification:
                                                core_ffi::TopicNotification::ChannelFailed {
                                                    name,
                                                    message,
                                                    ..
                                                },
                                        },
                                },
                        } => anyhow::bail!("native topic channel {name} failed: {message}"),
                        EventUpdate::Lagged { skipped } => {
                            anyhow::bail!("native event receiver dropped {skipped} events")
                        }
                        EventUpdate::Closed => anyhow::bail!("native event receiver closed"),
                        EventUpdate::Event { .. } => {}
                    }
                    anyhow::Ok(())
                })();
                if let Err(error) = result {
                    state.failure = Some(error.to_string());
                    return;
                }
            }
        });
        Self { state, reader }
    }

    fn observation(&self) -> anyhow::Result<(bool, u64, u64)> {
        let state = self.state.lock().unwrap();
        anyhow::ensure!(state.failure.is_none(), "{:?}", state.failure);
        Ok((
            state.route_ready,
            state.latest_messages,
            state.ordered_messages,
        ))
    }
}

#[derive(Debug)]
struct PeerReport {
    participant: String,
    reconnected: bool,
    first_generation: u64,
    final_generation: u64,
    video_frames: u64,
    audio_frames: u64,
    latest_messages: u64,
    ordered_messages: u64,
}

#[derive(Clone)]
struct PeerBarriers {
    before_media: Arc<tokio::sync::Barrier>,
    before_reconnect: Arc<tokio::sync::Barrier>,
    after_reconnect: Arc<tokio::sync::Barrier>,
    before_close: Arc<tokio::sync::Barrier>,
}

struct OwnedData {
    sender: Arc<pulsebeam_agent_native::ffi::LocalDataTrack>,
    room: Arc<pulsebeam_agent_native::ffi::DataReceiver>,
    participant: Arc<pulsebeam_agent_native::ffi::DataReceiver>,
    remote: String,
}

impl OwnedData {
    async fn new(agent: &Agent, remote: String) -> anyhow::Result<Self> {
        let sender = agent.data("owned-bytes".into(), core_ffi::TopicMode::Ordered)?;
        let room = sender.receive().await?;
        let participant = agent
            .participant(remote.clone())?
            .data("owned-bytes".into(), core_ffi::TopicMode::Ordered)?
            .receive()
            .await?;
        Ok(Self {
            sender,
            room,
            participant,
            remote,
        })
    }

    async fn message(
        &self,
        cursor: &pulsebeam_agent_native::ffi::DataReceiver,
        expected: &[u8],
    ) -> anyhow::Result<pulsebeam_agent_core::DataEvent> {
        loop {
            let pulsebeam_agent_core::DataUpdate::Event { event } = cursor.next().await? else {
                anyhow::bail!("owned data cursor closed before its message");
            };
            let pulsebeam_agent_core::DataEvent::Message {
                participant_id,
                track_id,
                data,
                ..
            } = &event
            else {
                anyhow::bail!("owned data cursor unexpectedly lost ordered messages");
            };
            anyhow::ensure!(
                participant_id == &self.remote && !track_id.is_empty(),
                "data cursor lost publisher identity"
            );
            if data == expected {
                return Ok(event);
            }
            anyhow::ensure!(data == &[0, 255, 128, 0], "unexpected binary payload");
        }
    }

    async fn exchange(&self, barrier: &tokio::sync::Barrier, round: u8) -> anyhow::Result<()> {
        barrier.wait().await;
        let payload = vec![0, 255, 128, round];
        tokio::time::timeout(Duration::from_secs(5), async {
            // Ordered receive starts at a live frontier, not retained history.
            // Establish it with received setup traffic before sending the marker once.
            let setup = vec![0, 255, 128, 0];
            let ready = async {
                let room = self.message(&self.room, &setup).await?;
                let participant = self.message(&self.participant, &setup).await?;
                anyhow::ensure!(room == participant, "independent setup cursors diverged");
                barrier.wait().await;
                anyhow::Ok(())
            };
            tokio::pin!(ready);
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            loop {
                tokio::select! {
                    result = &mut ready => { result?; break; },
                    _ = tick.tick() => self.sender.send(setup.clone()).await?,
                }
            }
            self.sender.send(payload.clone()).await?;
            let room = self.message(&self.room, &payload).await?;
            let participant = self.message(&self.participant, &payload).await?;
            anyhow::ensure!(room == participant, "independent data cursors diverged");
            anyhow::Ok(())
        })
        .await??;
        // Neither peer may replace its graph until the other has consumed this round.
        barrier.wait().await;
        Ok(())
    }
}

#[test]
fn native_agents_prove_media_topics_reconnect_and_close() {
    let seed = std::env::var("PULSEBEAM_SIM_SEED")
        .map(|value| value.parse().expect("simulation seed must be a u64"))
        .unwrap_or(NATIVE_RUNTIME_SEED);
    let _sim_clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    pulsebeam_runtime::net::shaper::seed_impairments(seed);
    let _ = stock_opus_fixture();

    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(90))
        .rng_seed(seed)
        .build();
    let subnet = reserve_subnet();
    let server_ip = subnet_ip(subnet, 1);
    let alice_ip = subnet_ip(subnet, 2);
    let bob_ip = subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        start_sfu_node_with(
            server_ip,
            pulsebeam_runtime::rand::seeded_rng(seed),
            DEFAULT_SIM_SHARDS,
            false,
        )
        .await
        .map_err(Into::into)
    });

    let (reports_tx, mut reports_rx) = tokio::sync::mpsc::channel(2);
    let barriers = PeerBarriers {
        before_media: Arc::new(tokio::sync::Barrier::new(2)),
        before_reconnect: Arc::new(tokio::sync::Barrier::new(2)),
        after_reconnect: Arc::new(tokio::sync::Barrier::new(2)),
        before_close: Arc::new(tokio::sync::Barrier::new(2)),
    };
    let alice_reports = reports_tx.clone();
    let alice_barriers = barriers.clone();
    sim.client(alice_ip, async move {
        let report = run_peer("alice", alice_ip, server_ip, false, alice_barriers).await?;
        alice_reports.send(report).await?;
        Ok(())
    });
    sim.client(bob_ip, async move {
        let report = run_peer("bob", bob_ip, server_ip, true, barriers).await?;
        reports_tx.send(report).await?;
        Ok(())
    });

    let reports = Arc::new(std::sync::Mutex::new(Vec::new()));
    let collected = Arc::clone(&reports);
    sim.client(subnet_ip(subnet, 254), async move {
        for _ in 0..2 {
            let report = reports_rx
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("native peer report channel closed"))?;
            collected.lock().unwrap().push(report);
        }
        Ok(())
    });

    run_sim_or_timeout(&mut sim, Duration::from_secs(180)).expect("native vertical slice failed");
    let mut reports = reports.lock().unwrap();
    reports.sort_by(|left, right| left.participant.cmp(&right.participant));
    assert_eq!(reports.len(), 2);
    for report in reports.iter() {
        assert!(!report.participant.is_empty());
        if report.reconnected {
            assert_ne!(report.first_generation, report.final_generation);
        } else {
            assert_eq!(report.first_generation, report.final_generation);
        }
        assert!(report.video_frames > 0);
        assert!(report.audio_frames > 0);
        assert!(report.latest_messages > 0);
        assert_eq!(report.ordered_messages, 6);
    }
    // Stable semantic replay evidence, not ciphertext, timing-adaptive codec
    // bytes or an assertion of unavailable per-frame publication provenance.
    let semantics: Vec<_> = reports
        .iter()
        .map(|report| {
            serde_json::json!({
                "participant": report.participant,
                "reconnected": report.reconnected,
                "generation_changed": report.first_generation != report.final_generation,
                "same_peer_media_readded": true,
                "video_progress": report.video_frames > 0,
                "pcm_progress": report.audio_frames > 0,
                "latest_progress": report.latest_messages > 0,
                "ordered_delivered": report.ordered_messages,
                "owned_frames_and_close": true,
            })
        })
        .collect();
    println!("[native-lifecycle] {}", serde_json::Value::Array(semantics));
}

async fn run_peer(
    name: &str,
    ip: IpAddr,
    server_ip: IpAddr,
    reconnect: bool,
    barriers: PeerBarriers,
) -> anyhow::Result<PeerReport> {
    let endpoint = format!("http://{server_ip}:7070");
    let room = RoomExternalId::new("native-vertical")?;
    let participant = ParticipantExternalId::new(name)?;
    let session = pulsebeam_agent_core::AgentConfig {
        endpoint,
        token: pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key().unwrap(),
            (room).as_str(),
            (participant).as_str(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs()
                .saturating_add(3600),
        )?,
        retry: pulsebeam_agent_core::RetryPolicy::default(),
        log_level: pulsebeam_agent_core::LogLevel::default(),
    };
    let mut config = Config::new(session);
    config.media.audio_input = AudioInput::EncodedOpus { channels: 2 };
    config.media.video_input = VideoInput::EncodedH264;
    let host = NativeHost::bind(ip)?;
    let created = Rc::new(Cell::new(0_usize));
    let retired = Rc::new(Cell::new(0_usize));
    let (runtime, actor) =
        RuntimeAgent::managed_with_services(config, Host::new(create_http_client()), {
            let created = created.clone();
            let retired = retired.clone();
            move || {
                let (runtime, owner) = host.runtime()?;
                created.set(created.get().saturating_add(1));
                Ok((runtime, (owner, RuntimeRetirement(retired.clone()))))
            }
        })?;
    tokio::task::spawn_local(actor);
    let agent = Agent::from_runtime(runtime.clone());
    let mut video = agent.create_video_input(Default::default()).await?;
    let mut audio = agent.create_audio_input().await?;
    let mut video_owner = agent
        .video("camera".into())?
        .publish(video.clone(), Default::default())
        .await?;
    let mut audio_owner = agent
        .audio("microphone".into())?
        .publish(audio.clone(), Default::default())
        .await?;
    let latest = core_ffi::TopicPublisher {
        name: "latest-state".into(),
        mode: core_ffi::TopicMode::Latest,
    };
    let ordered = core_ffi::TopicPublisher {
        name: "ordered-events".into(),
        mode: core_ffi::TopicMode::Ordered,
    };
    let desired = core_ffi::DesiredState {
        connected: true,
        topic_publishers: vec![latest.clone(), ordered.clone()],
        topic_subscribers: vec![
            core_ffi::TopicSubscriber {
                name: latest.name.clone(),
                mode: latest.mode,
            },
            core_ffi::TopicSubscriber {
                name: ordered.name.clone(),
                mode: ordered.mode,
            },
        ],
        ..Default::default()
    };
    replace_transport_desired(&runtime, desired.clone()).await?;

    let connected = wait_connected(&agent, None).await?;
    let generation = connected
        .generation
        .ok_or_else(|| anyhow::anyhow!("connected without generation"))?;
    video_owner.ready().await?;
    audio_owner.ready().await?;
    barriers.before_media.wait().await;
    let mut frame_cursor = 0;
    let initial = wait_remote_media(&agent, &video, &mut frame_cursor).await?;
    barriers.before_media.wait().await;
    let participant = initial
        .participant_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("{name} connected without a participant ID"))?;
    let first_generation = initial
        .generation
        .ok_or_else(|| anyhow::anyhow!("{name} connected without a generation"))?;
    let remote_video_publication = initial
        .catalog
        .publications
        .iter()
        .find(|publication| {
            publication.participant_id != participant
                && publication.kind == core_ffi::MediaKind::Video
        })
        .ok_or_else(|| anyhow::anyhow!("{name} did not discover remote video"))?;
    let remote_video_id = remote_video_publication.id.clone();
    let video_subscription = agent
        .participant(remote_video_publication.participant_id.clone())?
        .video(remote_video_publication.label.clone())?
        .subscribe(core_ffi::VideoObservation {
            visible: true,
            height: 720,
            min_height: 0,
            min_fps: 0,
            priority: 1,
            playout_delay: core_ffi::PlayoutDelay::Adaptive,
        })
        .await?;
    let remote_audio_publication = initial
        .catalog
        .publications
        .iter()
        .find(|publication| {
            publication.participant_id != participant
                && publication.kind == core_ffi::MediaKind::Audio
        })
        .ok_or_else(|| anyhow::anyhow!("{name} did not discover remote audio"))?;
    let remote_audio_id = remote_audio_publication.id.clone();
    let audio_subscription = agent
        .participant(remote_audio_publication.participant_id.clone())?
        .audio(remote_audio_publication.label.clone())?
        .subscribe(Default::default())
        .await?;

    wait_media_bindings(&agent, &remote_video_id, &remote_audio_id).await?;
    let mut remote_video = video_subscription.observe(ReceiveMode::Decoded).await?;
    anyhow::ensure!(
        remote_video.source_track_id().as_deref() == Some(remote_video_id.as_str()),
        "named video lost its publication association"
    );
    let mut remote_audio = audio_subscription.observe(ReceiveMode::Decoded).await?;
    anyhow::ensure!(
        remote_audio.source_track_id().as_deref() == Some(remote_audio_id.as_str()),
        "named audio lost its publication association"
    );
    let mut video_probe =
        MediaProbe::start(remote_video.clone(), generation, core_ffi::MediaKind::Video);
    let mut audio_probe =
        MediaProbe::start(remote_audio.clone(), generation, core_ffi::MediaKind::Audio);
    let events = TopicProbe::start(agent.events(), name, &latest, &ordered, 1);
    prime_ordered_topic(&runtime, &agent, &ordered, &events, &barriers.before_media).await?;

    let accepted = agent.snapshot();
    let errors = agent.events();
    // Backend rejection is per resource; unrelated accepted video stays desired.
    let fixed = core_ffi::PlayoutDelay::Fixed {
        min_ms: 100,
        max_ms: 100,
    };
    audio_subscription
        .configure(core_ffi::AudioSubscriptionConfig {
            playout_delay: fixed,
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match errors.next().await {
                EventUpdate::Event {
                    event:
                        NativeEvent::Core {
                            notification: core_ffi::Notification::ServerError { .. },
                        },
                } => return anyhow::Ok(()),
                EventUpdate::Event { .. } => {}
                EventUpdate::Lagged { skipped } => {
                    anyhow::bail!("rejection observation lost {skipped} events")
                }
                EventUpdate::Closed => anyhow::bail!("rejection observation closed"),
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("unsupported fixed audio was not rejected"))??;
    let rejected = agent.snapshot();
    anyhow::ensure!(
        rejected.desired_revision == accepted.desired_revision && !audio_subscription.is_closed(),
        "owned audio configuration changed the declaration revision or ended its parent"
    );
    anyhow::ensure!(
        rejected.generation == accepted.generation
            && rejected.connection == core_ffi::ConnectionState::Connected,
        "unsupported audio reset the established session"
    );
    anyhow::ensure!(
        rejected
            .mapping
            .video
            .iter()
            .any(|binding| binding.publication_id == remote_video_id),
        "unsupported audio disturbed unchanged video binding"
    );
    // The existing exchange must decode fresh audio and video after rejection.
    let first = exchange(
        &barriers.before_reconnect,
        &agent,
        &runtime,
        &video,
        &audio,
        &video_probe,
        &audio_probe,
        &events,
        &latest,
        &ordered,
        &desired,
        1,
        &mut frame_cursor,
    )
    .await?;
    drop(events);
    let (original_payloads, closed_before) =
        native_payload_channels(&runtime, first_generation, 4, None).await?;
    let old_latest = agent
        .snapshot()
        .topics
        .publishers
        .into_iter()
        .find(|publisher| publisher.publisher == latest)
        .and_then(|publisher| publisher.data_track_id)
        .ok_or_else(|| anyhow::anyhow!("latest source was not installed"))?;
    let mut reduced = desired.clone();
    reduced
        .topic_publishers
        .retain(|publisher| publisher == &ordered);
    reduced
        .topic_subscribers
        .retain(|subscriber| subscriber.name == ordered.name);
    replace_transport_desired(&runtime, reduced).await?;
    // Channel rows disappear before signaling observes kClosed. Only the
    // withdrawn pair may close in this phase; wait for their terminal counter.
    let closed_after = closed_before
        .checked_add(2)
        .ok_or_else(|| anyhow::anyhow!("native closed-channel counter overflow"))?;
    let (continuing, _) =
        native_payload_channels(&runtime, first_generation, 2, Some(closed_after)).await?;
    for (stream, label) in &continuing {
        anyhow::ensure!(
            original_payloads.get(stream) == Some(label),
            "continuing payload was replaced"
        );
    }
    barriers.before_reconnect.wait().await;
    replace_transport_desired(&runtime, desired.clone()).await?;
    let rebound = wait_connected(&agent, None).await?;
    let new_latest = rebound
        .topics
        .publishers
        .iter()
        .find(|publisher| publisher.publisher == latest)
        .and_then(|publisher| publisher.data_track_id.as_ref())
        .ok_or_else(|| anyhow::anyhow!("latest replacement was not installed"))?;
    anyhow::ensure!(
        new_latest != &old_latest,
        "retired latest source identity was revived"
    );
    let (reused, _) =
        native_payload_channels(&runtime, first_generation, 4, Some(closed_after)).await?;
    anyhow::ensure!(
        original_payloads.keys().eq(reused.keys()),
        "payload stream IDs grew instead of being reused"
    );
    for (stream, label) in &reused {
        if continuing.contains_key(stream) {
            anyhow::ensure!(
                original_payloads.get(stream) == Some(label),
                "continuing payload identity changed"
            );
        } else {
            anyhow::ensure!(
                original_payloads.get(stream) != Some(label),
                "reused stream retained an old physical identity"
            );
        }
    }
    anyhow::ensure!(
        rebound.generation == Some(first_generation) && created.get() == 1 && retired.get() == 0,
        "payload reuse replaced the native peer"
    );
    audio_subscription
        .configure(core_ffi::AudioSubscriptionConfig {
            playout_delay: core_ffi::PlayoutDelay::Adaptive,
        })
        .await?;

    let first_expected = first.retained.clone();
    let retired_video_input = video.clone();
    let retired_audio_input = audio.clone();
    let retired_video_observation = remote_video.clone();
    let retired_audio_observation = remote_audio.clone();
    video_owner.close().await?;
    audio_owner.close().await?;
    anyhow::ensure!(
        !video.is_closed() && !audio.is_closed(),
        "publication retirement stopped independent producers"
    );
    video.close().await?;
    audio.close().await?;
    anyhow::ensure!(
        video.is_closed() && audio.is_closed(),
        "explicit producer retirement retained an input"
    );
    barriers.before_reconnect.wait().await;
    wait_media_absent(&agent, &remote_video_id, &remote_audio_id).await?;
    anyhow::ensure!(
        matches!(remote_video.next().await, MediaStreamUpdate::Closed)
            && matches!(remote_audio.next().await, MediaStreamUpdate::Closed),
        "retired source observation stayed open"
    );
    anyhow::ensure!(
        !video_subscription.is_closed() && !audio_subscription.is_closed(),
        "source absence ended a logical media interest"
    );
    let absent = agent.snapshot();
    anyhow::ensure!(
        absent.generation == Some(first_generation)
            && absent.participant_id.as_deref() == Some(participant.as_str())
            && absent.connection == core_ffi::ConnectionState::Connected,
        "media retirement replaced the established session"
    );
    barriers.before_reconnect.wait().await;
    video = agent.create_video_input(Default::default()).await?;
    audio = agent.create_audio_input().await?;
    video_owner = agent
        .video("camera".into())?
        .publish(video.clone(), Default::default())
        .await?;
    audio_owner = agent
        .audio("microphone".into())?
        .publish(audio.clone(), Default::default())
        .await?;
    video_owner.ready().await?;
    audio_owner.ready().await?;
    barriers.before_reconnect.wait().await;
    let readded = wait_remote_media(&agent, &video, &mut frame_cursor).await?;
    let readded_video_id = readded
        .catalog
        .publications
        .iter()
        .find(|publication| {
            publication.participant_id != participant
                && publication.kind == core_ffi::MediaKind::Video
        })
        .map(|publication| publication.id.clone())
        .ok_or_else(|| anyhow::anyhow!("{name} did not rediscover re-added video"))?;
    let readded_audio_id = readded
        .catalog
        .publications
        .iter()
        .find(|publication| {
            publication.participant_id != participant
                && publication.kind == core_ffi::MediaKind::Audio
        })
        .map(|publication| publication.id.clone())
        .ok_or_else(|| anyhow::anyhow!("{name} did not rediscover re-added audio"))?;
    anyhow::ensure!(
        readded_video_id != remote_video_id && readded_audio_id != remote_audio_id,
        "re-added media reused a retired source identity"
    );
    let readded = wait_media_bindings(&agent, &readded_video_id, &readded_audio_id).await?;
    let replaced_video_input = video;
    let replaced_audio_input = audio;
    video = agent.create_video_input(Default::default()).await?;
    audio = agent.create_audio_input().await?;
    video_owner.set_source(Some(video.clone())).await?;
    audio_owner.set_source(Some(audio.clone())).await?;
    video_owner.ready().await?;
    audio_owner.ready().await?;
    anyhow::ensure!(
        !replaced_video_input.is_closed() && !replaced_audio_input.is_closed(),
        "source replacement stopped borrowed producers"
    );
    replaced_video_input.close().await?;
    replaced_audio_input.close().await?;
    anyhow::ensure!(
        replaced_video_input.is_closed() && replaced_audio_input.is_closed(),
        "explicit old-producer retirement failed"
    );
    anyhow::ensure!(
        readded.generation == Some(first_generation)
            && readded.participant_id.as_deref() == Some(participant.as_str())
            && readded.connection == core_ffi::ConnectionState::Connected,
        "media re-addition replaced the established session"
    );
    remote_video = video_subscription.observe(ReceiveMode::Decoded).await?;
    remote_audio = audio_subscription.observe(ReceiveMode::Decoded).await?;
    anyhow::ensure!(
        remote_video.source_track_id().as_deref() == Some(readded_video_id.as_str())
            && remote_audio.source_track_id().as_deref() == Some(readded_audio_id.as_str()),
        "re-added observations lost their exact-source association"
    );
    video_probe = MediaProbe::start(
        remote_video.clone(),
        first_generation,
        core_ffi::MediaKind::Video,
    );
    audio_probe = MediaProbe::start(
        remote_audio.clone(),
        first_generation,
        core_ffi::MediaKind::Audio,
    );
    let events = TopicProbe::start(agent.events(), name, &latest, &ordered, 2);
    prime_ordered_topic(
        &runtime,
        &agent,
        &ordered,
        &events,
        &barriers.before_reconnect,
    )
    .await?;
    let same_peer = exchange(
        &barriers.before_reconnect,
        &agent,
        &runtime,
        &video,
        &audio,
        &video_probe,
        &audio_probe,
        &events,
        &latest,
        &ordered,
        &desired,
        2,
        &mut frame_cursor,
    )
    .await?;
    drop(events);
    let owned_data =
        OwnedData::new(&agent, remote_video_publication.participant_id.clone()).await?;
    owned_data.exchange(&barriers.before_reconnect, 1).await?;
    let same_peer_expected = same_peer.retained.clone();
    anyhow::ensure!(
        agent.snapshot().generation == Some(first_generation)
            && created.get() == 1
            && retired.get() == 0,
        "same-peer media exchange reconstructed its native graph"
    );
    anyhow::ensure!(
        matches!(
            retired_video_observation.next().await,
            MediaStreamUpdate::Closed
        ) && matches!(
            retired_audio_observation.next().await,
            MediaStreamUpdate::Closed
        ),
        "retired exact-source observations rebound to re-added media"
    );
    anyhow::ensure!(
        matches!(
            retired_video_input.submit(video_frame(0)).await,
            Err(pulsebeam_agent_native::ffi::NativeError::Closed)
        ) && matches!(
            retired_audio_input
                .submit(InputFrame::Opus {
                    frame: stock_opus_fixture()[0].clone()
                })
                .await,
            Err(pulsebeam_agent_native::ffi::NativeError::Closed)
        ),
        "a retired input submitted to re-added media"
    );
    anyhow::ensure!(
        matches!(
            replaced_video_input.submit(video_frame(0)).await,
            Err(pulsebeam_agent_native::ffi::NativeError::Closed)
        ) && matches!(
            replaced_audio_input
                .submit(InputFrame::Opus {
                    frame: stock_opus_fixture()[0].clone()
                })
                .await,
            Err(pulsebeam_agent_native::ffi::NativeError::Closed)
        ),
        "replaced input submitted after its same-generation replacement became current"
    );
    anyhow::ensure!(
        first.retained == first_expected,
        "returned owning frames changed across same-peer re-addition"
    );
    let (final_generation, replacement) = if reconnect {
        agent.reconnect().await?;
        let connected = wait_connected(&agent, Some(first_generation)).await?;
        let generation = connected
            .generation
            .ok_or_else(|| anyhow::anyhow!("reconnected without generation"))?;
        anyhow::ensure!(
            !video.is_closed()
                && !audio.is_closed()
                && !video_subscription.is_closed()
                && !audio_subscription.is_closed(),
            "generation retirement ended logical inputs or media interests"
        );
        video_owner.ready().await?;
        audio_owner.ready().await?;
        let replacement = wait_remote_media(&agent, &video, &mut frame_cursor).await?;
        if replacement.participant_id.as_deref() != Some(participant.as_str()) {
            anyhow::bail!("{name} participant identity changed across reconnect");
        }
        let replacement_generation = replacement
            .generation
            .ok_or_else(|| anyhow::anyhow!("{name} replacement has no generation"))?;
        anyhow::ensure!(
            replacement_generation == generation,
            "replacement media escaped the connected generation"
        );
        (replacement_generation, Some(replacement))
    } else {
        (first_generation, None)
    };
    barriers.after_reconnect.wait().await;
    anyhow::ensure!(
        created.get() == usize::from(reconnect).saturating_add(1)
            && retired.get() == usize::from(reconnect),
        "reconstruction did not retire exactly its previous runtime owner"
    );
    let replacement = match replacement {
        Some(snapshot) => snapshot,
        None => wait_remote_media(&agent, &video, &mut frame_cursor).await?,
    };
    let expected_video_id = replacement
        .catalog
        .publications
        .iter()
        .find(|publication| {
            publication.participant_id != participant
                && publication.kind == core_ffi::MediaKind::Video
        })
        .map(|publication| publication.id.clone())
        .ok_or_else(|| anyhow::anyhow!("{name} did not rediscover remote video"))?;
    let expected_audio_id = replacement
        .catalog
        .publications
        .iter()
        .find(|publication| {
            publication.participant_id != participant
                && publication.kind == core_ffi::MediaKind::Audio
        })
        .map(|publication| publication.id.clone())
        .ok_or_else(|| anyhow::anyhow!("{name} did not rediscover remote audio"))?;
    wait_media_bindings(&agent, &expected_video_id, &expected_audio_id).await?;
    anyhow::ensure!(
        matches!(remote_video.next().await, MediaStreamUpdate::Closed),
        "retired video observer stayed open"
    );
    anyhow::ensure!(
        matches!(remote_audio.next().await, MediaStreamUpdate::Closed),
        "retired audio observer stayed open"
    );
    video_probe.ready(first_generation)?;
    audio_probe.ready(first_generation)?;
    remote_video = video_subscription.observe(ReceiveMode::Decoded).await?;
    anyhow::ensure!(
        remote_video.source_track_id().as_deref() == Some(expected_video_id.as_str()),
        "fresh named video observation has the wrong source"
    );
    remote_audio = audio_subscription.observe(ReceiveMode::Decoded).await?;
    anyhow::ensure!(
        remote_audio.source_track_id().as_deref() == Some(expected_audio_id.as_str()),
        "fresh named audio observation has the wrong source"
    );
    video_probe = MediaProbe::start(
        remote_video.clone(),
        final_generation,
        core_ffi::MediaKind::Video,
    );
    audio_probe = MediaProbe::start(
        remote_audio.clone(),
        final_generation,
        core_ffi::MediaKind::Audio,
    );
    barriers.after_reconnect.wait().await;
    let events = TopicProbe::start(agent.events(), name, &latest, &ordered, 3);
    prime_ordered_topic(
        &runtime,
        &agent,
        &ordered,
        &events,
        &barriers.after_reconnect,
    )
    .await?;

    anyhow::ensure!(
        first.retained == first_expected,
        "returned frames changed across reconnect"
    );
    let second = exchange(
        &barriers.before_close,
        &agent,
        &runtime,
        &video,
        &audio,
        &video_probe,
        &audio_probe,
        &events,
        &latest,
        &ordered,
        &desired,
        3,
        &mut frame_cursor,
    )
    .await?;
    drop(events);
    owned_data.exchange(&barriers.before_close, 2).await?;
    let second_expected = second.retained.clone();
    agent.disconnect().await?;
    anyhow::ensure!(
        runtime.snapshot().connection == pulsebeam_agent_core::ConnectionState::Disconnected,
        "graceful close did not disconnect"
    );
    anyhow::ensure!(
        runtime.statistics().borrow().is_none()
            && created.get() == retired.get()
            && retired.get() == usize::from(reconnect).saturating_add(1),
        "native graph or runtime services not retired with retained media handles"
    );
    anyhow::ensure!(
        !owned_data.room.is_closed()
            && !owned_data.participant.is_closed()
            && !video.is_closed()
            && !audio.is_closed()
            && !video_subscription.is_closed()
            && !audio_subscription.is_closed(),
        "disconnect ended logical inputs or media interests"
    );
    anyhow::ensure!(
        video.submit(video_frame(0)).await.is_err(),
        "retired video target admitted input"
    );
    anyhow::ensure!(
        audio
            .submit(InputFrame::Opus {
                frame: stock_opus_fixture()[0].clone()
            })
            .await
            .is_err(),
        "retired audio target admitted input"
    );
    video_owner.close().await?;
    audio_owner.close().await?;
    anyhow::ensure!(
        !video.is_closed() && !audio.is_closed(),
        "publication close stopped independently owned producers"
    );
    video.close().await?;
    audio.close().await?;
    anyhow::ensure!(
        video.is_closed() && audio.is_closed(),
        "explicit producer close retained inputs"
    );
    anyhow::ensure!(
        matches!(remote_video.next().await, MediaStreamUpdate::Closed),
        "closed video observer stayed open"
    );
    anyhow::ensure!(
        matches!(remote_audio.next().await, MediaStreamUpdate::Closed),
        "closed audio observer stayed open"
    );
    video_subscription.close().await?;
    audio_subscription.close().await?;
    anyhow::ensure!(
        video_subscription.is_closed() && audio_subscription.is_closed(),
        "released media interests remained open"
    );
    anyhow::ensure!(
        first.retained == first_expected
            && same_peer.retained == same_peer_expected
            && second.retained == second_expected,
        "returned owning frames changed after close"
    );

    let mut pending_data = Box::pin(owned_data.participant.next());
    anyhow::ensure!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
            pending_data.as_mut(),
            cx
        )))
        .await
        .is_pending(),
        "disconnect ended logical data demand"
    );
    owned_data.room.close().await?;
    anyhow::ensure!(
        matches!(
            owned_data.room.next().await?,
            pulsebeam_agent_core::DataUpdate::Closed
        ) && !owned_data.participant.is_closed(),
        "cursor close ended independent demand"
    );
    agent.close().await?;
    anyhow::ensure!(
        matches!(
            pending_data.await?,
            pulsebeam_agent_core::DataUpdate::Closed
        ),
        "terminal Agent close did not finish a pending data read"
    );

    Ok(PeerReport {
        participant,
        reconnected: reconnect,
        first_generation,
        final_generation,
        video_frames: first
            .video_frames
            .saturating_add(same_peer.video_frames)
            .saturating_add(second.video_frames),
        audio_frames: first
            .audio_frames
            .saturating_add(same_peer.audio_frames)
            .saturating_add(second.audio_frames),
        latest_messages: first
            .latest_messages
            .saturating_add(same_peer.latest_messages)
            .saturating_add(second.latest_messages),
        ordered_messages: first
            .ordered_messages
            .saturating_add(same_peer.ordered_messages)
            .saturating_add(second.ordered_messages),
    })
}

async fn native_payload_channels(
    runtime: &RuntimeAgent,
    generation: u64,
    count: usize,
    closed_at_least: Option<u64>,
) -> anyhow::Result<(BTreeMap<u16, String>, u64)> {
    let mut statistics = runtime.statistics();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = statistics.borrow_and_update().clone();
            if let Some(current) = current.filter(|current| current.generation == generation) {
                let report: serde_json::Value = serde_json::from_slice(&current.report_json)?;
                let entries = report
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("native stats are not an array"))?;
                let closed = entries
                    .iter()
                    .find(|entry| entry["type"] == "peer-connection")
                    .and_then(|entry| entry["dataChannelsClosed"].as_u64())
                    .ok_or_else(|| anyhow::anyhow!("native stats omit closed-channel counter"))?;
                let mut channels = BTreeMap::new();
                for entry in entries {
                    if entry["type"] != "data-channel" || entry["state"] != "open" {
                        continue;
                    }
                    let Some(label) = entry["label"]
                        .as_str()
                        .filter(|label| label.starts_with("native/payload/"))
                    else {
                        continue;
                    };
                    let stream =
                        u16::try_from(entry["dataChannelIdentifier"].as_u64().ok_or_else(
                            || anyhow::anyhow!("native data channel has no stream ID"),
                        )?)?;
                    anyhow::ensure!(
                        stream != 0 && stream != u16::MAX,
                        "payload uses reserved SID"
                    );
                    anyhow::ensure!(
                        channels.insert(stream, label.to_owned()).is_none(),
                        "simultaneous payload SID alias"
                    );
                }
                if channels.len() == count
                    && closed_at_least.is_none_or(|minimum| closed >= minimum)
                {
                    return Ok((channels, closed));
                }
            }
            statistics.changed().await?;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("native payload channel count did not reach {count}"))?
}

async fn wait_connected(
    agent: &Agent,
    previous: Option<u64>,
) -> anyhow::Result<core_ffi::Snapshot> {
    let snapshots = agent.snapshots();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let SnapshotUpdate::Snapshot { snapshot } = snapshots.next().await else {
                anyhow::bail!("native snapshot stream closed")
            };
            let generation_ready = snapshot
                .generation
                .is_some_and(|generation| Some(generation) != previous);
            let topics_ready = snapshot
                .topics
                .publishers
                .iter()
                .all(|publisher| publisher.connected && publisher.data_track_id.is_some())
                && snapshot
                    .topics
                    .subscribers
                    .iter()
                    .all(|subscriber| subscriber.connected);
            if snapshot.connection == core_ffi::ConnectionState::Connected
                && generation_ready
                && topics_ready
            {
                return Ok(snapshot);
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("native agent did not connect"))?
}

async fn wait_ordered_admitted(
    agent: &Agent,
    publisher: &core_ffi::TopicPublisher,
    previous: u64,
) -> anyhow::Result<()> {
    let snapshots = agent.snapshots();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let SnapshotUpdate::Snapshot { snapshot } = snapshots.next().await else {
                anyhow::bail!("native snapshot stream closed")
            };
            if snapshot
                .topics
                .publishers
                .iter()
                .any(|state| state.publisher == *publisher && state.next_sequence > previous)
            {
                return Ok(());
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("native ordered topic send was not admitted"))?
}

async fn wait_media_absent(agent: &Agent, video_id: &str, audio_id: &str) -> anyhow::Result<()> {
    let snapshots = agent.snapshots();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let SnapshotUpdate::Snapshot { snapshot } = snapshots.next().await else {
                anyhow::bail!("native snapshot stream closed")
            };
            if snapshot
                .catalog
                .publications
                .iter()
                .all(|publication| publication.id != video_id && publication.id != audio_id)
                && snapshot
                    .mapping
                    .video
                    .iter()
                    .all(|binding| binding.publication_id != video_id)
                && snapshot
                    .mapping
                    .audio
                    .iter()
                    .all(|binding| binding.publication_id != audio_id)
            {
                return Ok(());
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("retired media remained in the native catalog or mapping"))?
}

async fn wait_media_bindings(
    agent: &Agent,
    video_id: &str,
    audio_id: &str,
) -> anyhow::Result<core_ffi::Snapshot> {
    let snapshots = agent.snapshots();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let SnapshotUpdate::Snapshot { snapshot } = snapshots.next().await else {
                anyhow::bail!("native snapshot stream closed")
            };
            let video_ready = snapshot
                .mapping
                .video
                .iter()
                .any(|binding| binding.publication_id == video_id);
            let audio_ready = snapshot
                .mapping
                .audio
                .iter()
                .any(|binding| binding.publication_id == audio_id);
            if video_ready && audio_ready {
                return Ok(snapshot);
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("native media bindings did not become ready"))?
}

// Raw transport churn is a controlled-host fixture, not a generated SDK policy path.
async fn replace_transport_desired(
    runtime: &RuntimeAgent,
    state: core_ffi::DesiredState,
) -> anyhow::Result<()> {
    let revision = runtime
        .snapshot()
        .desired_revision
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("transport fixture revision overflow"))?;
    let desired = state
        .into_core(revision)
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    runtime.replace_desired(desired).await?;
    Ok(())
}

async fn send_transport_topic(
    runtime: &RuntimeAgent,
    publisher: core_ffi::TopicPublisher,
    payload: Vec<u8>,
) -> anyhow::Result<()> {
    runtime
        .send_topic(pulsebeam_agent_core::TopicSend {
            publisher: publisher.into(),
            payload,
        })
        .await?;
    Ok(())
}

async fn prime_ordered_topic(
    runtime: &RuntimeAgent,
    agent: &Agent,
    publisher: &core_ffi::TopicPublisher,
    events: &TopicProbe,
    barrier: &tokio::sync::Barrier,
) -> anyhow::Result<()> {
    // Setup traffic must not evict its own history while waiting for the live
    // cursor. Still require a real peer message, not admission or elapsed time.
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let synchronized = barrier.wait();
    tokio::pin!(synchronized);
    // Ordered subscriptions start at a live frontier. Catalog presence and local
    // send admission do not establish that the peer's live cursor is ready.
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let (received, _, _) = events.observation()?;
            tokio::select! {
                _ = &mut synchronized, if received => return Ok(()),
                _ = tick.tick() => {
                    send_transport_topic(runtime, publisher.clone(), b"route-ready".to_vec()).await?;
                }
            }
        }
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => {
            let counters = agent
                .statistics()
                .and_then(|statistics| {
                    serde_json::from_slice::<serde_json::Value>(&statistics.report_json).ok()
                })
                .map(|report| {
                    if let Some(entries) = report.as_array() {
                        serde_json::Value::Array(
                            entries
                                .iter()
                                .filter(|entry| entry["type"] == "data-channel")
                                .cloned()
                                .collect(),
                        )
                    } else {
                        report
                    }
                });
            anyhow::bail!(
                "native ordered peer message did not establish readiness: observed={:?}, snapshot={:?}, native_channels={counters}",
                events.observation(),
                agent.snapshots().next().await,
                counters = counters.unwrap_or(serde_json::Value::Null),
            )
        }
    }
}

async fn wait_remote_media(
    agent: &Agent,
    video: &pulsebeam_agent_native::ffi::VideoSource,
    frame_number: &mut u64,
) -> anyhow::Result<core_ffi::Snapshot> {
    let snapshots = agent.snapshots();
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let snapshot = tokio::select! {
                _ = tick.tick() => {
                    video.submit(video_frame(video.capture_time_us().await?)).await?;
                    *frame_number = frame_number.checked_add(1).unwrap();
                    continue;
                }
                update = snapshots.next() => {
                    let SnapshotUpdate::Snapshot { snapshot } = update else {
                        anyhow::bail!("native snapshot stream closed")
                    };
                    snapshot
                }
            };
            let remote_media = snapshot.participant_id.as_ref().is_some_and(|participant| {
                let remote: Vec<_> = snapshot
                    .catalog
                    .publications
                    .iter()
                    .filter(|publication| &publication.participant_id != participant)
                    .collect();
                remote
                    .iter()
                    .any(|publication| publication.kind == core_ffi::MediaKind::Video)
                    && remote
                        .iter()
                        .any(|publication| publication.kind == core_ffi::MediaKind::Audio)
            });
            if remote_media {
                return Ok(snapshot);
            }
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "native agent did not discover remote media: {:?}",
            agent.snapshot()
        )
    })?
}

fn video_frame(timestamp_us: i64) -> InputFrame {
    video_frame_for(timestamp_us, pulsebeam_testdata::QualityVideoSource::Zero)
}

fn video_frame_for(
    timestamp_us: i64,
    source: pulsebeam_testdata::QualityVideoSource,
) -> InputFrame {
    let corpus = pulsebeam_testdata::quality_corpus_video(
        source,
        pulsebeam_testdata::QualityVideoLayer::P180,
    );
    let frame = corpus.frame(0).expect("genuine corpus keyframe");
    InputFrame::H264 {
        frame: H264Frame {
            timestamp_us,
            payload: frame.encoded.to_vec(),
            width: 320,
            height: 180,
            keyframe: true,
            temporal_index: None,
            base_layer_sync: false,
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn exchange(
    completion: &tokio::sync::Barrier,
    agent: &Agent,
    runtime: &RuntimeAgent,
    video: &pulsebeam_agent_native::ffi::VideoSource,
    audio: &pulsebeam_agent_native::ffi::AudioSource,
    video_probe: &MediaProbe,
    audio_probe: &MediaProbe,
    events: &TopicProbe,
    latest: &core_ffi::TopicPublisher,
    ordered: &core_ffi::TopicPublisher,
    desired: &core_ffi::DesiredState,
    round: u64,
    frame_number: &mut u64,
) -> anyhow::Result<ExchangeReport> {
    send_transport_topic(
        runtime,
        latest.clone(),
        format!("latest-{round}-old").into_bytes(),
    )
    .await?;
    send_transport_topic(
        runtime,
        latest.clone(),
        format!("latest-{round}").into_bytes(),
    )
    .await?;
    let snapshot = agent.snapshots().next().await;
    let SnapshotUpdate::Snapshot { snapshot } = snapshot else {
        anyhow::bail!("native snapshot stream closed")
    };
    let next_sequence = snapshot
        .topics
        .publishers
        .iter()
        .find(|state| state.publisher == *ordered)
        .map(|state| state.next_sequence)
        .ok_or_else(|| anyhow::anyhow!("native ordered topic publisher is not connected"))?;
    send_transport_topic(
        runtime,
        ordered.clone(),
        format!("ordered-{round}-0").into_bytes(),
    )
    .await?;
    wait_ordered_admitted(agent, ordered, next_sequence).await?;
    if round == 1 {
        // The actor processes these commands separately; this exercises real
        // registration churn between ordered sends, not a same-turn batch.
        let mut without_latest = desired.clone();
        without_latest
            .topic_subscribers
            .retain(|subscriber| subscriber.name != latest.name);
        replace_transport_desired(runtime, without_latest).await?;
        replace_transport_desired(runtime, desired.clone()).await?;
        events.state.lock().unwrap().latest_messages = 0;
    }
    send_transport_topic(
        runtime,
        ordered.clone(),
        format!("ordered-{round}-1").into_bytes(),
    )
    .await?;
    wait_ordered_admitted(agent, ordered, next_sequence.saturating_add(1)).await?;

    let generation = snapshot
        .generation
        .ok_or_else(|| anyhow::anyhow!("exchange without generation"))?;
    video_probe.begin_window()?;
    audio_probe.begin_window()?;
    let mut video_ready = false;
    let mut latest_messages = 0u64;
    let mut ordered_messages = 0u64;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut audio_ready = false;
    let started = Instant::now();
    let mut video_diagnosed = false;
    let native_rtp_report = || {
        agent
            .statistics()
            .and_then(|statistics| {
                serde_json::from_slice::<serde_json::Value>(&statistics.report_json).ok()
            })
            .map(|report| {
                if let Some(entries) = report.as_array() {
                    serde_json::Value::Array(
                        entries
                            .iter()
                            .filter(|entry| {
                                matches!(
                                    entry["type"].as_str(),
                                    Some("inbound-rtp" | "outbound-rtp" | "media-source")
                                )
                            })
                            .cloned()
                            .collect(),
                    )
                } else {
                    report
                }
            })
    };
    let mut proof = None;
    let completed = completion.wait();
    tokio::pin!(completed);

    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            (_, latest_messages, ordered_messages) = events.observation()?;
            if proof.is_none() {
                video_ready = video_probe.ready(generation)?;
                audio_ready = audio_probe.ready(generation)?;
                if video_ready && audio_ready && latest_messages > 0 && ordered_messages >= 2 {
                    // Finish at the actual fresh observation, not after waiting
                    // for the peer. Keep sourcing until both proofs are finished.
                    let (video_frames, retained_video) = video_probe.finish_window(generation)?;
                    let (audio_frames, retained_audio) = audio_probe.finish_window(generation)?;
                    proof = Some(ExchangeReport {
                        video_frames,
                        audio_frames,
                        latest_messages,
                        ordered_messages,
                        retained: vec![retained_video, retained_audio],
                    });
                }
            }
            tokio::select! {
                _ = &mut completed, if proof.is_some() => break,
                _ = tick.tick() => {
                    // Keep the peer's latest state and media live until both
                    // receivers have completed, not only this receiver.
                    send_transport_topic(runtime, latest.clone(), format!("latest-{round}").into_bytes()).await?;
                    if !video_diagnosed && started.elapsed() >= Duration::from_secs(1) {
                        let observed = video_probe.state.lock().unwrap();
                        let error = observed.video.require(&observed.video_baseline, generation, (320, 180), 2, Instant::now()).err();
                        let native_rtp = native_rtp_report();
                        tracing::warn!(round, participant = ?agent.snapshot().participant_id, ?error, frames = observed.blocks, submitted = *frame_number, ?native_rtp, native_video = ?observed.video, "native media boundary observation");
                        video_diagnosed = true;
                    }
                    // Genuine first-IDR corpus inputs, held for 200ms so native
                    // framerate adaptation cannot sample only one alternating scene.
                    let source = if (*frame_number / 10).is_multiple_of(2) {
                        pulsebeam_testdata::QualityVideoSource::Zero
                    } else {
                        pulsebeam_testdata::QualityVideoSource::One
                    };
                    video.submit(video_frame_for(video.capture_time_us().await?, source)).await?;
                    let fixtures = stock_opus_fixture();
                    let index = usize::try_from(*frame_number).unwrap() % fixtures.len();
                    let mut frame = fixtures[index].clone();
                    let cycle = *frame_number / u64::try_from(fixtures.len()).unwrap();
                    frame.timestamp = frame.timestamp.checked_add(u32::try_from(cycle.checked_mul(48_000).unwrap()).unwrap()).unwrap();
                    audio.submit(InputFrame::Opus { frame }).await.inspect_err(|error| {
                        let native_rtp = native_rtp_report();
                        tracing::error!(round, generation, submitted = *frame_number, ?error, ?native_rtp, "native Opus producer admission failed");
                    })?;
                    *frame_number = frame_number.checked_add(1).unwrap();
                }
            }
        }
        anyhow::Ok(())
    })
    .await
    .map_err(|_| {
            let observed = video_probe.state.lock().unwrap();
            let video_error = observed.video.require(&observed.video_baseline, generation, (320, 180), 2, Instant::now()).err();
            let native_rtp = native_rtp_report();
            tracing::warn!(round, generation, participant = ?agent.snapshot().participant_id,
                frames = observed.blocks, submitted = *frame_number, ?native_rtp,
                native_video = ?observed.video, "native media boundary at final timeout");
            anyhow::anyhow!(
                "native media/topic exchange {round} timed out: participant={:?} video_ready={video_ready} audio_ready={audio_ready} latest={latest_messages} ordered={ordered_messages} video_oracle={video_error:?}",
                agent.snapshot().participant_id,
            )
    })??;

    proof.ok_or_else(|| anyhow::anyhow!("native exchange completed without a media proof"))
}

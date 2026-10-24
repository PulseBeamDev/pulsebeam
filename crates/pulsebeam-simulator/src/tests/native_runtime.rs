use std::net::IpAddr;
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
        assert_eq!(report.ordered_messages, 4);
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
            u64::MAX,
        )?,
        topology: pulsebeam_agent_core::MediaTopology {
            local_video: 1,
            local_audio: 1,
            remote_video: 1,
            remote_audio: 1,
        },
        retry: pulsebeam_agent_core::RetryPolicy::default(),
        log_level: pulsebeam_agent_core::LogLevel::default(),
    };
    let mut config = Config::new(session);
    config.media.audio_input = AudioInput::EncodedOpus { channels: 2 };
    config.media.video_input = VideoInput::EncodedH264;
    let host = NativeHost::bind(ip)?;
    let (runtime, actor) =
        RuntimeAgent::managed_with_services(config, Host::new(create_http_client()), move || {
            host.runtime()
        })?;
    tokio::task::spawn_local(actor);
    let agent = Agent::from_runtime(runtime.clone());
    let latest = core_ffi::TopicPublisher {
        name: "latest-state".into(),
        mode: core_ffi::TopicMode::Latest,
    };
    let ordered = core_ffi::TopicPublisher {
        name: "ordered-events".into(),
        mode: core_ffi::TopicMode::Ordered,
    };
    let mut desired = core_ffi::DesiredState {
        connected: true,
        publications: vec![
            core_ffi::PublicationIntent {
                slot: "v0".into(),
                label: "camera".into(),
                active: true,
            },
            core_ffi::PublicationIntent {
                slot: "a0".into(),
                label: "microphone".into(),
                active: true,
            },
        ],
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
    agent.replace_desired(desired.clone()).await?;

    let connected = wait_connected(&agent, None).await?;
    let generation = connected
        .generation
        .ok_or_else(|| anyhow::anyhow!("connected without generation"))?;
    let mut video = agent.local_video("v0".into()).await?;
    let mut audio = agent.local_audio("a0".into()).await?;
    let mut remote_video = agent.remote_video(0, ReceiveMode::Decoded).await?;
    let mut remote_audio = agent.remote_audio(0, ReceiveMode::Decoded).await?;
    let mut video_probe =
        MediaProbe::start(remote_video.clone(), generation, core_ffi::MediaKind::Video);
    let mut audio_probe =
        MediaProbe::start(remote_audio.clone(), generation, core_ffi::MediaKind::Audio);
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
    let remote_video_id = initial
        .catalog
        .publications
        .iter()
        .find(|publication| {
            publication.participant_id != participant
                && publication.kind == core_ffi::MediaKind::Video
        })
        .map(|publication| publication.id.clone())
        .ok_or_else(|| anyhow::anyhow!("{name} did not discover remote video"))?;
    let remote_audio_id = initial
        .catalog
        .publications
        .iter()
        .find(|publication| {
            publication.participant_id != participant
                && publication.kind == core_ffi::MediaKind::Audio
        })
        .map(|publication| publication.id.clone())
        .ok_or_else(|| anyhow::anyhow!("{name} did not discover remote audio"))?;

    desired.video = vec![core_ffi::VideoDemand {
        slot: 0,
        publication_id: remote_video_id.clone(),
        selector: None,
        height: 720,
        min_height: 0,
        min_fps: 0,
        priority: 1,
        playout_delay: core_ffi::PlayoutDelay::Adaptive,
    }];
    desired.audio.pinned = vec![remote_audio_id.clone()];
    agent.replace_desired(desired.clone()).await?;
    wait_video_binding(&agent, &remote_video_id).await?;
    let events = TopicProbe::start(agent.events(), name, &latest, &ordered, 1);
    prime_ordered_topic(&agent, &ordered, &events, &barriers.before_media).await?;

    let first = exchange(
        &barriers.before_reconnect,
        &agent,
        &video,
        &audio,
        &video_probe,
        &audio_probe,
        &events,
        &latest,
        &ordered,
        1,
        &mut frame_cursor,
    )
    .await?;
    drop(events);

    let first_expected = first.retained.clone();
    let final_generation = if reconnect {
        agent.reconnect().await?;
        let connected = wait_connected(&agent, Some(first_generation)).await?;
        let generation = connected
            .generation
            .ok_or_else(|| anyhow::anyhow!("reconnected without generation"))?;
        anyhow::ensure!(
            video.is_closed() && audio.is_closed(),
            "retired sources stayed live"
        );
        anyhow::ensure!(
            video.submit(video_frame(0)).await.is_err(),
            "stale source admitted input"
        );
        anyhow::ensure!(
            audio
                .submit(InputFrame::Opus {
                    frame: stock_opus_fixture()[0].clone()
                })
                .await
                .is_err(),
            "stale audio source admitted input"
        );
        anyhow::ensure!(
            matches!(remote_video.next().await, MediaStreamUpdate::Closed),
            "retired video observer stayed open"
        );
        anyhow::ensure!(
            matches!(remote_audio.next().await, MediaStreamUpdate::Closed),
            "retired audio observer stayed open"
        );
        video = agent.local_video("v0".into()).await?;
        audio = agent.local_audio("a0".into()).await?;
        remote_video = agent.remote_video(0, ReceiveMode::Decoded).await?;
        video_probe.ready(first_generation)?;
        video_probe =
            MediaProbe::start(remote_video.clone(), generation, core_ffi::MediaKind::Video);
        audio_probe.ready(first_generation)?;
        remote_audio = agent.remote_audio(0, ReceiveMode::Decoded).await?;
        audio_probe =
            MediaProbe::start(remote_audio.clone(), generation, core_ffi::MediaKind::Audio);
        let replacement = wait_remote_media(&agent, &video, &mut frame_cursor).await?;
        if replacement.participant_id.as_deref() != Some(participant.as_str()) {
            anyhow::bail!("{name} participant identity changed across reconnect");
        }
        replacement
            .generation
            .ok_or_else(|| anyhow::anyhow!("{name} replacement has no generation"))?
    } else {
        first_generation
    };
    barriers.after_reconnect.wait().await;
    if !reconnect {
        desired.video.clear();
        desired.audio.pinned.clear();
        agent.replace_desired(desired.clone()).await?;
        let replacement = wait_remote_media(&agent, &video, &mut frame_cursor).await?;
        let replacement_video = replacement
            .catalog
            .publications
            .iter()
            .find(|publication| {
                publication.participant_id != participant
                    && publication.kind == core_ffi::MediaKind::Video
            })
            .map(|publication| publication.id.clone())
            .ok_or_else(|| anyhow::anyhow!("{name} did not rediscover remote video"))?;
        let replacement_audio = replacement
            .catalog
            .publications
            .iter()
            .find(|publication| {
                publication.participant_id != participant
                    && publication.kind == core_ffi::MediaKind::Audio
            })
            .map(|publication| publication.id.clone())
            .ok_or_else(|| anyhow::anyhow!("{name} did not rediscover remote audio"))?;
        desired.video = vec![core_ffi::VideoDemand {
            slot: 0,
            publication_id: replacement_video,
            selector: None,
            height: 720,
            min_height: 0,
            min_fps: 0,
            priority: 1,
            playout_delay: core_ffi::PlayoutDelay::Adaptive,
        }];
        desired.audio.pinned = vec![replacement_audio];
        agent.replace_desired(desired.clone()).await?;
    }
    wait_video_binding(&agent, &desired.video[0].publication_id).await?;
    barriers.after_reconnect.wait().await;
    let events = TopicProbe::start(agent.events(), name, &latest, &ordered, 2);
    prime_ordered_topic(&agent, &ordered, &events, &barriers.after_reconnect).await?;

    anyhow::ensure!(
        first.retained == first_expected,
        "returned frames changed across reconnect"
    );
    let second = exchange(
        &barriers.before_close,
        &agent,
        &video,
        &audio,
        &video_probe,
        &audio_probe,
        &events,
        &latest,
        &ordered,
        2,
        &mut frame_cursor,
    )
    .await?;
    drop(events);
    let second_expected = second.retained.clone();
    agent.close().await?;
    anyhow::ensure!(
        runtime.snapshot().connection == pulsebeam_agent_core::ConnectionState::Disconnected,
        "graceful close did not disconnect"
    );
    anyhow::ensure!(
        runtime.statistics().borrow().is_none(),
        "native graph not retired"
    );
    anyhow::ensure!(
        video.is_closed() && audio.is_closed(),
        "closed sources stayed live"
    );
    anyhow::ensure!(
        video.submit(video_frame(0)).await.is_err(),
        "closed video admitted input"
    );
    anyhow::ensure!(
        audio
            .submit(InputFrame::Opus {
                frame: stock_opus_fixture()[0].clone()
            })
            .await
            .is_err(),
        "closed audio admitted input"
    );
    anyhow::ensure!(
        matches!(remote_video.next().await, MediaStreamUpdate::Closed),
        "closed video observer stayed open"
    );
    anyhow::ensure!(
        matches!(remote_audio.next().await, MediaStreamUpdate::Closed),
        "closed audio observer stayed open"
    );
    anyhow::ensure!(
        first.retained == first_expected && second.retained == second_expected,
        "returned owning frames changed after close"
    );

    Ok(PeerReport {
        participant,
        reconnected: reconnect,
        first_generation,
        final_generation,
        video_frames: first.video_frames.saturating_add(second.video_frames),
        audio_frames: first.audio_frames.saturating_add(second.audio_frames),
        latest_messages: first.latest_messages.saturating_add(second.latest_messages),
        ordered_messages: first
            .ordered_messages
            .saturating_add(second.ordered_messages),
    })
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

async fn wait_video_binding(agent: &Agent, video_id: &str) -> anyhow::Result<core_ffi::Snapshot> {
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
            if video_ready {
                return Ok(snapshot);
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("native video binding did not become ready"))?
}

async fn prime_ordered_topic(
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
                    agent.send_topic(publisher.clone(), b"route-ready".to_vec()).await?;
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
    video: &pulsebeam_agent_native::ffi::LocalSource,
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
    .map_err(|_| anyhow::anyhow!("native agent did not discover remote media"))?
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
    video: &pulsebeam_agent_native::ffi::LocalSource,
    audio: &pulsebeam_agent_native::ffi::LocalSource,
    video_probe: &MediaProbe,
    audio_probe: &MediaProbe,
    events: &TopicProbe,
    latest: &core_ffi::TopicPublisher,
    ordered: &core_ffi::TopicPublisher,
    round: u64,
    frame_number: &mut u64,
) -> anyhow::Result<ExchangeReport> {
    agent
        .send_topic(latest.clone(), format!("latest-{round}-old").into_bytes())
        .await?;
    agent
        .send_topic(latest.clone(), format!("latest-{round}").into_bytes())
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
    agent
        .send_topic(ordered.clone(), format!("ordered-{round}-0").into_bytes())
        .await?;
    wait_ordered_admitted(agent, ordered, next_sequence).await?;
    agent
        .send_topic(ordered.clone(), format!("ordered-{round}-1").into_bytes())
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
                    agent
                        .send_topic(latest.clone(), format!("latest-{round}").into_bytes())
                        .await?;
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

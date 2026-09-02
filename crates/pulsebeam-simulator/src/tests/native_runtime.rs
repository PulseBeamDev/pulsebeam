use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use pulsebeam_agent_core::ffi as core_ffi;
use pulsebeam_agent_native::ffi::{Agent, EventUpdate, MediaUpdate, NativeEvent, SnapshotUpdate};
use pulsebeam_agent_native::{Agent as RuntimeAgent, Config, Host};
use pulsebeam_core::net::UdpSocket;

use super::common::client::create_http_client;
use super::common::{
    DEFAULT_SIM_SHARDS, reserve_subnet, run_sim_or_timeout, start_sfu_node_with, subnet_ip,
    unspecified_addr,
};

const NATIVE_RUNTIME_SEED: u64 = 1;

const KEYFRAME: &[u8] = &[
    0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0, 0, 0, 1, 0x68, 0xce, 0x06, 0, 0, 0, 1, 0x65, 0x03, 0x04,
];

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

#[test]
fn native_agents_prove_media_topics_reconnect_and_close() {
    let seed = std::env::var("PULSEBEAM_SIM_SEED")
        .map(|value| value.parse().expect("simulation seed must be a u64"))
        .unwrap_or(NATIVE_RUNTIME_SEED);
    let _sim_clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    pulsebeam_runtime::net::shaper::seed_impairments(seed);

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
    let before_reconnect = Arc::new(tokio::sync::Barrier::new(2));
    let after_reconnect = Arc::new(tokio::sync::Barrier::new(2));
    let before_close = Arc::new(tokio::sync::Barrier::new(2));
    let alice_reports = reports_tx.clone();
    let alice_before = Arc::clone(&before_reconnect);
    let alice_after = Arc::clone(&after_reconnect);
    let alice_close = Arc::clone(&before_close);
    sim.client(alice_ip, async move {
        let report = run_peer(
            "alice",
            alice_ip,
            server_ip,
            false,
            alice_before,
            alice_after,
            alice_close,
        )
        .await?;
        alice_reports.send(report).await?;
        Ok(())
    });
    let bob_before = Arc::clone(&before_reconnect);
    let bob_after = Arc::clone(&after_reconnect);
    let bob_close = Arc::clone(&before_close);
    sim.client(bob_ip, async move {
        let report = run_peer(
            "bob", bob_ip, server_ip, true, bob_before, bob_after, bob_close,
        )
        .await?;
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
    let reports = reports.lock().unwrap();
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
        assert!(report.ordered_messages > 0);
    }
}

async fn run_peer(
    name: &str,
    ip: IpAddr,
    server_ip: IpAddr,
    reconnect: bool,
    before_reconnect: Arc<tokio::sync::Barrier>,
    after_reconnect: Arc<tokio::sync::Barrier>,
    before_close: Arc<tokio::sync::Barrier>,
) -> anyhow::Result<PeerReport> {
    let endpoint = format!("http://{server_ip}:7070");
    let session = pulsebeam_agent_core::AgentConfig {
        endpoint,
        room_id: "native-vertical".into(),
        request_headers: Vec::new(),
        topology: pulsebeam_agent_core::MediaTopology {
            local_video: vec!["camera".into()],
            local_audio: vec!["microphone".into()],
            remote_video: 1,
            remote_audio: 1,
        },
        manual_subscriptions: true,
        retry: pulsebeam_agent_core::RetryPolicy::default(),
    };
    let mut config = Config::new(session);
    config.local_ips.push(ip);
    let udp = UdpSocket::bind(unspecified_addr(ip, 0)).await?;
    let runtime = RuntimeAgent::spawn(config, Host::new(create_http_client(), udp)).await?;
    let agent = Agent::from_runtime(runtime);
    let video = agent.local_video("camera".into());
    let audio = agent.local_audio("microphone".into());
    let remote_video = agent.remote_video(0).await?;
    let remote_audio = agent.remote_audio(0).await?;
    let events = agent.events();

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
                slot: "camera".into(),
                active: true,
            },
            core_ffi::PublicationIntent {
                slot: "microphone".into(),
                active: true,
            },
        ],
        topic_publishers: vec![latest.clone(), ordered.clone()],
        topic_subscribers: vec![
            core_ffi::TopicSubscriber {
                name: latest.name.clone(),
                mode: latest.mode,
                publisher_id: None,
            },
            core_ffi::TopicSubscriber {
                name: ordered.name.clone(),
                mode: ordered.mode,
                publisher_id: None,
            },
        ],
        ..Default::default()
    };
    agent.replace_desired(desired.clone()).await?;

    let initial = wait_ready(&agent, None).await?;
    let participant = initial
        .participant_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("{name} connected without a participant ID"))?;
    let first_generation = initial
        .generation
        .ok_or_else(|| anyhow::anyhow!("{name} connected without a generation"))?;
    let remote_video_id = initial
        .publications
        .iter()
        .find(|publication| {
            publication.participant_id != participant
                && publication.kind == core_ffi::MediaKind::Video
        })
        .map(|publication| publication.id.clone())
        .ok_or_else(|| anyhow::anyhow!("{name} did not discover remote video"))?;
    let remote_audio_id = initial
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
        height: 720,
        min_height: 0,
        min_fps: 0,
        priority: 1,
    }];
    desired.audio.pinned = vec![remote_audio_id.clone()];
    agent.replace_desired(desired.clone()).await?;

    let first = exchange(
        &agent,
        &video,
        &audio,
        &remote_video,
        &remote_audio,
        &events,
        &latest,
        &ordered,
        1,
    )
    .await?;

    before_reconnect.wait().await;
    let final_generation = if reconnect {
        agent.reconnect().await?;
        let replacement = wait_ready(&agent, Some(first_generation)).await?;
        if replacement.participant_id.as_deref() != Some(participant.as_str()) {
            anyhow::bail!("{name} participant identity changed across reconnect");
        }
        replacement
            .generation
            .ok_or_else(|| anyhow::anyhow!("{name} replacement has no generation"))?
    } else {
        first_generation
    };
    after_reconnect.wait().await;
    if !reconnect {
        let replacement = wait_remote_replacement(&agent, &participant, &remote_video_id).await?;
        let replacement_video = replacement
            .publications
            .iter()
            .find(|publication| {
                publication.participant_id != participant
                    && publication.kind == core_ffi::MediaKind::Video
            })
            .map(|publication| publication.id.clone())
            .ok_or_else(|| anyhow::anyhow!("{name} did not rediscover remote video"))?;
        let replacement_audio = replacement
            .publications
            .iter()
            .find(|publication| {
                publication.participant_id != participant
                    && publication.kind == core_ffi::MediaKind::Audio
            })
            .map(|publication| publication.id.clone())
            .ok_or_else(|| anyhow::anyhow!("{name} did not rediscover remote audio"))?;
        desired.video[0].publication_id = replacement_video;
        desired.audio.pinned = vec![replacement_audio];
        agent.replace_desired(desired).await?;
    }

    let second = exchange(
        &agent,
        &video,
        &audio,
        &remote_video,
        &remote_audio,
        &events,
        &latest,
        &ordered,
        2,
    )
    .await?;
    before_close.wait().await;
    agent.close().await?;

    Ok(PeerReport {
        participant,
        reconnected: reconnect,
        first_generation,
        final_generation,
        video_frames: first.0.saturating_add(second.0),
        audio_frames: first.1.saturating_add(second.1),
        latest_messages: first.2.saturating_add(second.2),
        ordered_messages: first.3.saturating_add(second.3),
    })
}

async fn wait_remote_replacement(
    agent: &Agent,
    participant: &str,
    previous_video: &str,
) -> anyhow::Result<core_ffi::Snapshot> {
    let snapshots = agent.snapshots();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let SnapshotUpdate::Snapshot { snapshot } = snapshots.next().await else {
                anyhow::bail!("native snapshot stream closed")
            };
            let has_replacement_video = snapshot.publications.iter().any(|publication| {
                publication.participant_id != participant
                    && publication.kind == core_ffi::MediaKind::Video
                    && publication.id != previous_video
            });
            let has_remote_audio = snapshot.publications.iter().any(|publication| {
                publication.participant_id != participant
                    && publication.kind == core_ffi::MediaKind::Audio
            });
            if has_replacement_video && has_remote_audio {
                return Ok(snapshot);
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("native peer did not observe replacement publications"))?
}

async fn wait_ready(agent: &Agent, previous: Option<u64>) -> anyhow::Result<core_ffi::Snapshot> {
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
                .all(|publisher| publisher.connected)
                && snapshot
                    .topics
                    .subscribers
                    .iter()
                    .all(|subscriber| subscriber.connected);
            let remote_media = snapshot.participant_id.as_ref().is_some_and(|participant| {
                let remote: Vec<_> = snapshot
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
            if snapshot.connection == core_ffi::ConnectionState::Connected
                && generation_ready
                && topics_ready
                && remote_media
            {
                return Ok(snapshot);
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("native agent did not become ready"))?
}

#[allow(clippy::too_many_arguments)]
async fn exchange(
    agent: &Agent,
    video: &pulsebeam_agent_native::ffi::LocalMediaSender,
    audio: &pulsebeam_agent_native::ffi::LocalMediaSender,
    remote_video: &pulsebeam_agent_native::ffi::RemoteMediaReceiver,
    remote_audio: &pulsebeam_agent_native::ffi::RemoteMediaReceiver,
    events: &pulsebeam_agent_native::ffi::EventStream,
    latest: &core_ffi::TopicPublisher,
    ordered: &core_ffi::TopicPublisher,
    round: u64,
) -> anyhow::Result<(u64, u64, u64, u64)> {
    agent
        .send_topic(latest.clone(), format!("latest-{round}-old").into_bytes())
        .await?;
    agent
        .send_topic(latest.clone(), format!("latest-{round}").into_bytes())
        .await?;
    for sequence in 0..2 {
        agent
            .send_topic(
                ordered.clone(),
                format!("ordered-{round}-{sequence}").into_bytes(),
            )
            .await?;
    }

    let mut video_frames = 0u64;
    let mut audio_frames = 0u64;
    let mut latest_messages = 0u64;
    let mut ordered_messages = 0u64;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut frame_number = round.saturating_mul(10_000);

    tokio::time::timeout(Duration::from_secs(20), async {
        while video_frames == 0
            || audio_frames == 0
            || latest_messages == 0
            || ordered_messages < 2
        {
            tokio::select! {
                _ = tick.tick() => {
                    if latest_messages == 0 {
                        agent
                            .send_topic(latest.clone(), format!("latest-{round}").into_bytes())
                            .await?;
                    }
                    let video_frame = core_ffi::MediaFrame {
                        timestamp: frame_number.saturating_mul(3_000),
                        clock_rate: 90_000,
                        data: KEYFRAME.to_vec(),
                        absolute_capture_time_unix_us: unix_micros(pulsebeam_agent_native::clock::capture_wallclock()),
                        contiguous: true,
                        keyframe: true,
                        audio_level_dbov: None,
                        voice_activity: None,
                        target_bitrate_bps: Some(250_000),
                        width: Some(320),
                        height: Some(180),
                        frames_per_second: Some(30),
                        dependency_descriptor: None,
                        temporal_layers: None,
                    };
                    video.send(video_frame).await?;
                    let audio_frame = core_ffi::MediaFrame {
                        timestamp: frame_number.saturating_mul(960),
                        clock_rate: 48_000,
                        data: vec![0xf8, 0xff, 0xfe],
                        absolute_capture_time_unix_us: unix_micros(pulsebeam_agent_native::clock::capture_wallclock()),
                        contiguous: true,
                        keyframe: false,
                        audio_level_dbov: Some(-30),
                        voice_activity: Some(true),
                        target_bitrate_bps: None,
                        width: None,
                        height: None,
                        frames_per_second: None,
                        dependency_descriptor: None,
                        temporal_layers: None,
                    };
                    audio.send(audio_frame).await?;
                    frame_number = frame_number.saturating_add(1);
                }
                update = remote_video.next() => {
                    match update {
                        MediaUpdate::Frame { .. } => {
                            video_frames = video_frames.saturating_add(1);
                        }
                        MediaUpdate::Lagged { skipped } => {
                            anyhow::bail!("native video receiver dropped {skipped} frames")
                        }
                        MediaUpdate::Closed => anyhow::bail!("native video receiver closed"),
                    }
                }
                update = remote_audio.next() => {
                    match update {
                        MediaUpdate::Frame { .. } => {
                            audio_frames = audio_frames.saturating_add(1);
                        }
                        MediaUpdate::Lagged { skipped } => {
                            anyhow::bail!("native audio receiver dropped {skipped} frames")
                        }
                        MediaUpdate::Closed => anyhow::bail!("native audio receiver closed"),
                    }
                }
                event = events.next() => {
                    match event {
                        EventUpdate::Event { event: NativeEvent::Core {
                            notification: core_ffi::Notification::Topic {
                                notification: core_ffi::TopicNotification::Message { message }
                            }
                        }} => match message {
                            core_ffi::TopicMessage::Latest { .. } => {
                                latest_messages = latest_messages.saturating_add(1);
                            }
                            core_ffi::TopicMessage::Ordered { .. } => {
                                ordered_messages = ordered_messages.saturating_add(1);
                            }
                        },
                        EventUpdate::Lagged { skipped } => {
                            anyhow::bail!("native event receiver dropped {skipped} events")
                        }
                        EventUpdate::Closed => anyhow::bail!("native event receiver closed"),
                        EventUpdate::Event { .. } => {}
                    }
                }
            }
        }
        anyhow::Ok(())
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "native media/topic exchange timed out: video={video_frames} audio={audio_frames} latest={latest_messages} ordered={ordered_messages}"
        )
    })??;

    Ok((
        video_frames,
        audio_frames,
        latest_messages,
        ordered_messages,
    ))
}

fn unix_micros(value: std::time::SystemTime) -> Option<u64> {
    let micros = value
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_micros();
    u64::try_from(micros).ok()
}

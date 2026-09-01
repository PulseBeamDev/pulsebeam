use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use pulsebeam_agent_native::str0m::media::{Frequency, MediaTime};
use pulsebeam_agent_native::{Agent, Config, Host, MediaFrame};
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
    let agent = Agent::spawn(config, Host::new(create_http_client(), udp)).await?;
    let video = agent.local_media("camera");
    let audio = agent.local_audio("microphone");
    let mut remote_video = agent.remote_video(0).await?;
    let mut remote_audio = agent.remote_audio(0).await?;
    let mut events = agent.events();

    let latest = pulsebeam_agent_core::TopicPublisher {
        topic: "latest-state".into(),
        mode: pulsebeam_agent_core::TopicMode::Latest,
    };
    let ordered = pulsebeam_agent_core::TopicPublisher {
        topic: "ordered-events".into(),
        mode: pulsebeam_agent_core::TopicMode::Ordered,
    };
    let mut desired = pulsebeam_agent_core::DesiredState::default();
    desired.revision = 1;
    desired.connected = true;
    desired.publications = vec![
        pulsebeam_agent_core::PublicationIntent {
            slot: "camera".into(),
            active: true,
        },
        pulsebeam_agent_core::PublicationIntent {
            slot: "microphone".into(),
            active: true,
        },
    ];
    desired.topics.publishers = vec![latest.clone(), ordered.clone()];
    desired.topics.subscribers = vec![
        pulsebeam_agent_core::TopicSubscriber {
            topic: latest.topic.clone(),
            mode: latest.mode,
            publisher_id: None,
        },
        pulsebeam_agent_core::TopicSubscriber {
            topic: ordered.topic.clone(),
            mode: ordered.mode,
            publisher_id: None,
        },
    ];
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
        .values()
        .find(|publication| {
            publication.participant_id != participant
                && publication.kind == pulsebeam_agent_core::MediaKind::Video
        })
        .map(|publication| publication.id.clone())
        .ok_or_else(|| anyhow::anyhow!("{name} did not discover remote video"))?;
    let remote_audio_id = initial
        .publications
        .values()
        .find(|publication| {
            publication.participant_id != participant
                && publication.kind == pulsebeam_agent_core::MediaKind::Audio
        })
        .map(|publication| publication.id.clone())
        .ok_or_else(|| anyhow::anyhow!("{name} did not discover remote audio"))?;

    desired.revision = 2;
    desired.video = vec![pulsebeam_agent_core::VideoSubscription {
        slot: 0,
        track_id: remote_video_id.clone(),
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
        &mut remote_video,
        &mut remote_audio,
        &mut events,
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
            .values()
            .find(|publication| {
                publication.participant_id != participant
                    && publication.kind == pulsebeam_agent_core::MediaKind::Video
            })
            .map(|publication| publication.id.clone())
            .ok_or_else(|| anyhow::anyhow!("{name} did not rediscover remote video"))?;
        let replacement_audio = replacement
            .publications
            .values()
            .find(|publication| {
                publication.participant_id != participant
                    && publication.kind == pulsebeam_agent_core::MediaKind::Audio
            })
            .map(|publication| publication.id.clone())
            .ok_or_else(|| anyhow::anyhow!("{name} did not rediscover remote audio"))?;
        desired.revision = 3;
        desired.video[0].track_id = replacement_video;
        desired.audio.pinned = vec![replacement_audio];
        agent.replace_desired(desired).await?;
    }

    let second = exchange(
        &agent,
        &video,
        &audio,
        &mut remote_video,
        &mut remote_audio,
        &mut events,
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
        first_generation: first_generation.get(),
        final_generation: final_generation.get(),
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
) -> anyhow::Result<pulsebeam_agent_core::Snapshot> {
    let mut snapshots = agent.snapshots();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let snapshot = snapshots.borrow().clone();
            let has_replacement_video = snapshot.publications.values().any(|publication| {
                publication.participant_id != participant
                    && publication.kind == pulsebeam_agent_core::MediaKind::Video
                    && publication.id != previous_video
            });
            let has_remote_audio = snapshot.publications.values().any(|publication| {
                publication.participant_id != participant
                    && publication.kind == pulsebeam_agent_core::MediaKind::Audio
            });
            if has_replacement_video && has_remote_audio {
                return Ok(snapshot);
            }
            snapshots.changed().await?;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("native peer did not observe replacement publications"))?
}

async fn wait_ready(
    agent: &Agent,
    previous: Option<pulsebeam_agent_core::Generation>,
) -> anyhow::Result<pulsebeam_agent_core::Snapshot> {
    let mut snapshots = agent.snapshots();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let snapshot = snapshots.borrow().clone();
            let generation_ready = snapshot
                .generation
                .is_some_and(|generation| Some(generation) != previous);
            let topics_ready = snapshot
                .topics
                .publishers
                .iter()
                .all(|publisher| publisher.channel.is_some())
                && snapshot
                    .topics
                    .subscribers
                    .iter()
                    .all(|subscriber| subscriber.channel.is_some());
            let remote_media = snapshot.participant_id.as_ref().is_some_and(|participant| {
                let remote: Vec<_> = snapshot
                    .publications
                    .values()
                    .filter(|publication| &publication.participant_id != participant)
                    .collect();
                remote
                    .iter()
                    .any(|publication| publication.kind == pulsebeam_agent_core::MediaKind::Video)
                    && remote.iter().any(|publication| {
                        publication.kind == pulsebeam_agent_core::MediaKind::Audio
                    })
            });
            if snapshot.connection == pulsebeam_agent_core::ConnectionState::Connected
                && generation_ready
                && topics_ready
                && remote_media
            {
                return Ok(snapshot);
            }
            snapshots.changed().await?;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("native agent did not become ready"))?
}

#[allow(clippy::too_many_arguments)]
async fn exchange(
    agent: &Agent,
    video: &pulsebeam_agent_native::LocalMedia,
    audio: &pulsebeam_agent_native::LocalMedia,
    remote_video: &mut pulsebeam_agent_native::RemoteMedia,
    remote_audio: &mut pulsebeam_agent_native::RemoteMedia,
    events: &mut tokio::sync::broadcast::Receiver<pulsebeam_agent_native::AgentEvent>,
    latest: &pulsebeam_agent_core::TopicPublisher,
    ordered: &pulsebeam_agent_core::TopicPublisher,
    round: u64,
) -> anyhow::Result<(u64, u64, u64, u64)> {
    agent
        .send_topic(pulsebeam_agent_core::TopicSend {
            publisher: latest.clone(),
            payload: format!("latest-{round}-old").into_bytes(),
        })
        .await?;
    agent
        .send_topic(pulsebeam_agent_core::TopicSend {
            publisher: latest.clone(),
            payload: format!("latest-{round}").into_bytes(),
        })
        .await?;
    for sequence in 0..2 {
        agent
            .send_topic(pulsebeam_agent_core::TopicSend {
                publisher: ordered.clone(),
                payload: format!("ordered-{round}-{sequence}").into_bytes(),
            })
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
                capture_time = tick.tick() => {
                    if latest_messages == 0 {
                        agent
                            .send_topic(pulsebeam_agent_core::TopicSend {
                                publisher: latest.clone(),
                                payload: format!("latest-{round}").into_bytes(),
                            })
                            .await?;
                    }
                    let video_frame = MediaFrame {
                        ts: MediaTime::from_90khz(frame_number.saturating_mul(3_000)),
                        data: Arc::from(KEYFRAME),
                        capture_time,
                        abs_capture_time: Some(pulsebeam_agent_native::clock::capture_wallclock()),
                        contiguous: true,
                        is_keyframe: true,
                        audio_level: None,
                        voice_activity: None,
                        target_bitrate_bps: Some(250_000),
                        resolution: Some((320, 180, 30)),
                        dependency_descriptor: None,
                        temporal_layers: None,
                    };
                    video.send(video_frame).await?;
                    let audio_frame = MediaFrame {
                        ts: MediaTime::new(
                            frame_number.saturating_mul(960),
                            Frequency::FORTY_EIGHT_KHZ,
                        ),
                        data: Arc::from([0xf8, 0xff, 0xfe]),
                        capture_time,
                        abs_capture_time: Some(pulsebeam_agent_native::clock::capture_wallclock()),
                        contiguous: true,
                        is_keyframe: false,
                        audio_level: Some(-30),
                        voice_activity: Some(true),
                        target_bitrate_bps: None,
                        resolution: None,
                        dependency_descriptor: None,
                        temporal_layers: None,
                    };
                    audio.send(audio_frame).await?;
                    frame_number = frame_number.saturating_add(1);
                }
                packet = remote_video.recv_packet() => {
                    let _ = packet?;
                    video_frames = video_frames.saturating_add(1);
                }
                packet = remote_audio.recv_packet() => {
                    let _ = packet?;
                    audio_frames = audio_frames.saturating_add(1);
                }
                event = events.recv() => {
                    if let pulsebeam_agent_native::AgentEvent::Core(
                        pulsebeam_agent_core::Notification::Topic(
                            pulsebeam_agent_core::TopicNotification::Message(message)
                        )
                    ) = event? {
                        match message {
                            pulsebeam_agent_core::TopicMessage::Latest { .. } => {
                                latest_messages = latest_messages.saturating_add(1);
                            }
                            pulsebeam_agent_core::TopicMessage::Ordered { .. } => {
                                ordered_messages = ordered_messages.saturating_add(1);
                            }
                        }
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

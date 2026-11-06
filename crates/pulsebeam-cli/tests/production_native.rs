//! One production constructor/HTTP/kernel-socket witness, not a media matrix.
use anyhow::{Context, Result, anyhow, bail, ensure};
use pulsebeam_agent_native::agent_core::{
    AgentConfig, ConnectionState, DesiredState, Generation, Notification, TopicMode,
    TopicNotification, TopicPublisher, TopicRegistrations, TopicSend, TopicSubscriber,
    TrackSelector, TrackSpec, VideoObservation,
};
use pulsebeam_agent_native::{
    Agent, AgentEvent, Config, Host, I420Frame, InputFrame, MediaStreamUpdate, OutputFrame,
    PublicationInput, ReceiveMode, StreamUpdate,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// Reuse the real loopback node fixture already owned by browser integration.
mod support {
    pub type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
}
#[path = "../../../agents/pulsebeam-agent-web/tests/support/destination.rs"]
mod destination;

const WIDTH: u32 = 160;
const HEIGHT: u32 = 96;
const LUMA: usize = 160 * 96;
const CHROMA: usize = 80 * 48;

fn config(endpoint: &str, participant: &str) -> Result<Config> {
    let expiry = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .saturating_add(120);
    Ok(Config::new(AgentConfig {
        endpoint: endpoint.into(),
        token: pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key()?,
            "native-production-boundary",
            participant,
            expiry,
        )?,
        retry: Default::default(),
        log_level: Default::default(),
    }))
}

async fn connected(agent: &Agent) -> Result<()> {
    let mut snapshots = agent.snapshots();
    loop {
        let snapshot = snapshots.borrow().clone();
        if snapshot.connection == ConnectionState::Connected && snapshot.participant_id.is_some() {
            return Ok(());
        }
        ensure!(
            snapshot.connection != ConnectionState::TerminalFailure,
            "production connection failed: {snapshot:?}"
        );
        snapshots.changed().await?;
    }
}

async fn produce_i420(camera: PublicationInput) -> Result<()> {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tick.tick().await;
        let mut data = vec![180; LUMA];
        data.extend(vec![90; CHROMA]);
        data.extend(vec![180; CHROMA]);
        camera
            .submit(InputFrame::I420 {
                frame: I420Frame {
                    width: WIDTH,
                    height: HEIGHT,
                    timestamp_us: camera.capture_time_us().await?,
                    data,
                    rotation_degrees: 0,
                },
            })
            .await?;
    }
}

#[tokio::test]
async fn production_native_i420_crosses_real_sfu_and_closes() -> Result<()> {
    if let Some(root) = std::env::var_os("PULSEBEAM_BASELINE_ROOT") {
        let release = std::fs::read_to_string(std::path::Path::new(&root).join("etc/os-release"))?;
        ensure!(
            release.lines().any(|line| line == "ID=ubuntu"),
            "baseline is not Ubuntu"
        );
        ensure!(
            release.lines().any(|line| line == "VERSION_ID=\"24.04\""),
            "baseline is not Ubuntu 24.04"
        );
        let output = std::process::Command::new("/usr/bin/bwrap")
            .arg("--ro-bind")
            .arg(&root)
            .arg("/")
            .args([
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--",
                "/usr/bin/getconf",
                "GNU_LIBC_VERSION",
            ])
            .output()?;
        ensure!(
            output.status.success() && output.stdout == b"glibc 2.39\n",
            "baseline is not glibc 2.39"
        );
    }
    let server = destination::DestinationServer::start()
        .await
        .map_err(|error| anyhow!(error.to_string()))?;
    ensure!(
        server.address().ip().is_loopback(),
        "nonlocal test listener"
    );
    let endpoint = server.endpoint();
    // This constructor selects ThreadedRuntime, production reqwest and native
    // Linux sockets. No ControlledWorld or simulator clock is present.
    let publisher = Agent::spawn(
        config(&endpoint, "publisher")?,
        Host::new(Box::new(reqwest::Client::new())),
    )
    .await?;
    let subscriber = Agent::spawn(
        config(&endpoint, "subscriber")?,
        Host::new(Box::new(reqwest::Client::new())),
    )
    .await?;
    let publication = publisher
        .publication(TrackSpec::Video {
            label: "production-camera".into(),
        })
        .await?;
    let camera = publication.input().await?;
    let subscription = subscriber
        .video_subscription(
            TrackSelector {
                participant_external_id: "publisher".into(),
                label: "production-camera".into(),
            },
            VideoObservation {
                visible: true,
                height: HEIGHT,
                priority: 1,
                ..Default::default()
            },
        )
        .await?;
    publisher
        .replace_desired(DesiredState {
            revision: 1,
            connected: true,
            ..Default::default()
        })
        .await?;
    subscriber
        .replace_desired(DesiredState {
            revision: 1,
            connected: true,
            ..Default::default()
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        tokio::try_join!(connected(&publisher), connected(&subscriber))?;
        anyhow::Ok(())
    })
    .await
    .with_context(|| {
        format!(
            "production agents did not connect: publisher={:?}, subscriber={:?}",
            publisher.snapshot().connection,
            subscriber.snapshot().connection,
        )
    })??;

    camera.ready().await?;
    let mut producer = tokio::spawn(produce_i420(camera));
    let mut observation = None;
    let media = tokio::time::timeout(Duration::from_secs(20), async {
        let remote = observation.insert(subscription.observe(ReceiveMode::Decoded).await?);
        ensure!(remote.source_track_id().is_some_and(|id| subscriber.snapshot().publications.get(id).is_some_and(|source| source.label == "production-camera" && Some(source.participant_id.as_str()) == publisher.snapshot().participant_id.as_deref())), "observation is not bound to the actual remote publication ID");
        loop {
            tokio::select! {
                result = &mut producer => bail!("production input stopped: {result:?}"),
                update = remote.next() => match update {
                    MediaStreamUpdate::Frame { frame } => {
                        ensure!(Some(frame.track.generation) == subscriber.snapshot().generation.map(Generation::get), "wrong production generation");
                        let OutputFrame::I420 { frame: pixels } = frame.frame else { bail!("native production video did not decode"); };
                        ensure!(pixels.width == WIDTH && pixels.height == HEIGHT, "unexpected native decoded dimensions");
                        ensure!(pixels.data.len() == LUMA + CHROMA + CHROMA, "invalid native I420 layout");
                        let luma = pixels.data.get(..LUMA).ok_or_else(|| anyhow!("missing native luma"))?;
                        ensure!(luma.iter().all(|sample| sample.abs_diff(180) <= 12), "native decoded pixels do not match submitted content");
                        break anyhow::Ok(pixels);
                    }
                    MediaStreamUpdate::Track { .. } | MediaStreamUpdate::NativeDropped { .. } | MediaStreamUpdate::NativeDecodedDropped { .. } => {},
                    MediaStreamUpdate::Lagged { skipped } => bail!("production observation lagged by {skipped} updates"),
                    MediaStreamUpdate::Closed => bail!("production video closed before delivery"),
                }
            }
        }
    }).await.with_context(|| format!("production decoded-media deadline: publisher={:?}, subscriber={:?}", publisher.snapshot(), subscriber.snapshot()));
    let retained = media
        .as_ref()
        .ok()
        .and_then(|result| result.as_ref().ok())
        .cloned();
    let close = tokio::time::timeout(Duration::from_secs(5), subscriber.close()).await;
    if !matches!(&close, Ok(Ok(()))) {
        let _ = subscriber.abort().await;
    }
    let pixels = media.context("production decoded media did not arrive")??;
    close.context("production agents did not close")??;
    let observation = observation.ok_or_else(|| anyhow!("missing production observation"))?;
    ensure!(
        matches!(observation.next().await, MediaStreamUpdate::Closed),
        "retired physical observation remained open"
    );
    ensure!(
        !subscription.is_closed(),
        "disconnect retired the named subscription"
    );
    subscription.close().await?;
    ensure!(
        subscription.is_closed(),
        "named subscription release did not close its owner"
    );
    ensure!(
        subscriber.snapshot().connection == ConnectionState::Disconnected,
        "production graceful close did not disconnect"
    );
    ensure!(
        subscriber.statistics().borrow().is_none(),
        "production native graph not retired"
    );
    ensure!(
        Some(&pixels) == retained.as_ref(),
        "owning production pixels changed after close"
    );
    println!(
        "production native raw-I420 encode/real HTTP+SFU+Linux sockets/native decode/owned-close PASS"
    );
    if std::env::var_os("PULSEBEAM_FOREIGN_SMOKE").is_none() {
        producer.abort();
        let _ = producer.await;
        tokio::time::timeout(Duration::from_secs(5), publisher.close()).await??;
        ensure!(
            publisher.statistics().borrow().is_none(),
            "publisher native graph not retired"
        );
        return Ok(());
    }
    let topic_publisher = TopicPublisher {
        topic: "foreign-exchange".into(),
        mode: TopicMode::Ordered,
    };
    let events = publisher.events();
    publisher
        .replace_desired(DesiredState {
            revision: 2,
            connected: true,
            topics: TopicRegistrations {
                publishers: vec![topic_publisher.clone()],
                subscribers: vec![TopicSubscriber {
                    topic: "foreign-exchange".into(),
                    mode: TopicMode::Ordered,
                }],
            },
            ..Default::default()
        })
        .await?;
    let peer = publisher.clone();
    let (received_tx, mut received_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut responder = tokio::spawn(async move {
        while !received_tx.is_closed() {
            match events.next().await {
                StreamUpdate::Value(AgentEvent::Core(Notification::Topic(
                    TopicNotification::Message(message),
                ))) if message.topic == "foreign-exchange" => {
                    if message.payload != b"python-bytes-\0\xff"
                        && message.payload != b"node-bytes-\0\xff"
                    {
                        continue;
                    }
                    received_tx.send(message.payload.clone())?;
                    let mut snapshots = peer.snapshots();
                    loop {
                        let ready = snapshots.borrow().topics.publishers.iter().any(|status| {
                            status.registration == topic_publisher
                                && status.channel.is_some()
                                && status.data_track_id.is_some()
                        });
                        if ready {
                            break;
                        }
                        snapshots.changed().await?;
                    }
                    let mut payload = b"rust-ack:".to_vec();
                    payload.extend(message.payload);
                    peer.send_topic(TopicSend {
                        publisher: topic_publisher.clone(),
                        payload,
                    })
                    .await?;
                }
                StreamUpdate::Lagged { skipped } => bail!("topic event stream lagged by {skipped}"),
                StreamUpdate::Closed => bail!("topic event stream closed before exchange"),
                other => {
                    if let StreamUpdate::Value(AgentEvent::Core(Notification::Topic(event))) = other
                    {
                        eprintln!("Rust peer topic event: {event:?}");
                    }
                }
            }
        }
        anyhow::Ok(())
    });

    // Reuse the bounded server and live Rust publisher for both installed consumers.
    let foreign = config(&endpoint, "foreign")?;
    let mut command = tokio::process::Command::new(std::env::var("PULSEBEAM_FOREIGN_SMOKE")?);
    command
        .arg(&endpoint)
        .arg(&foreign.session.token)
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("LD_PRELOAD")
        .kill_on_drop(true);
    let output = tokio::select! {
        result = &mut responder => bail!("topic responder stopped before Python completed: {result:?}"),
        output = tokio::time::timeout(Duration::from_secs(35), command.output()) => output.context("generated foreign RTC witness did not finish")??,
    };
    ensure!(
        output.status.success(),
        "generated foreign RTC witness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));

    let node = config(&endpoint, "foreign-node")?;
    let mut command = tokio::process::Command::new(std::env::var("PULSEBEAM_NODE_WITNESS")?);
    command
        .arg(&endpoint)
        .arg(&node.session.token)
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("LD_PRELOAD")
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    let status = tokio::select! {
        result = &mut responder => bail!("topic responder stopped before Node completed: {result:?}"),
        status = tokio::time::timeout(Duration::from_secs(60), command.status()) => status.context("installed Node RTC witness did not finish")??,
    };
    ensure!(
        status.success(),
        "installed Node RTC witness failed: {status}"
    );
    responder.abort();
    let _ = responder.await;
    let mut received = std::collections::BTreeSet::new();
    while let Ok(payload) = received_rx.try_recv() {
        received.insert(payload);
    }
    ensure!(
        received.contains(b"python-bytes-\0\xff".as_slice()),
        "Rust peer missed Python topic bytes"
    );
    ensure!(
        received.contains(b"node-bytes-\0\xff".as_slice()),
        "Rust peer missed Node topic bytes"
    );
    producer.abort();
    let _ = producer.await;
    tokio::time::timeout(Duration::from_secs(5), publisher.close()).await??;
    ensure!(
        publisher.statistics().borrow().is_none(),
        "publisher native graph not retired"
    );
    Ok(())
}

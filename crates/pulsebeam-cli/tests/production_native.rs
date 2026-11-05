//! One production constructor/HTTP/kernel-socket witness, not a media matrix.
use anyhow::{Context, Result, anyhow, bail, ensure};
use pulsebeam_agent_native::agent_core::{
    AgentConfig, ConnectionState, DataEvent, DataUpdate, Generation, TopicMode, VideoObservation,
};
use pulsebeam_agent_native::{
    Agent, Config, Host, I420Frame, InputFrame, MediaStreamUpdate, OutputFrame, ProducerInput,
    ReceiveMode,
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

fn native_statistics(agent: &Agent) -> String {
    agent
        .statistics()
        .borrow()
        .as_ref()
        .map(|stats| String::from_utf8_lossy(&stats.report_json).into_owned())
        .unwrap_or_else(|| "not reported".into())
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

fn i420_frame(timestamp_us: i64) -> InputFrame {
    let mut data = vec![180; LUMA];
    data.extend(vec![90; CHROMA]);
    data.extend(vec![180; CHROMA]);
    InputFrame::I420 {
        frame: I420Frame {
            width: WIDTH,
            height: HEIGHT,
            timestamp_us,
            data,
            rotation_degrees: 0,
        },
    }
}

async fn produce_i420(camera: ProducerInput) -> Result<()> {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tick.tick().await;
        camera
            .submit(i420_frame(camera.capture_time_us().await?))
            .await?;
    }
}

async fn foreign_witness(
    publisher: &Agent,
    participant: &str,
    expected: &[u8],
    command: &mut tokio::process::Command,
    timeout: Duration,
) -> Result<String> {
    let exchange = publisher.data("foreign-exchange".into(), TopicMode::Ordered)?;
    let messages = publisher
        .participant(participant.into())?
        .data("foreign-exchange".into(), TopicMode::Ordered)?
        .receive()
        .await?;
    let expected = expected.to_vec();
    let (received_tx, mut received_rx) = tokio::sync::oneshot::channel();
    let responder = tokio::spawn(async move {
        let mut received_tx = Some(received_tx);
        loop {
            match messages.next().await {
                // A terminal read cannot be reused by the next installed consumer.
                Err(error) => return anyhow::Ok(Some(error)),
                Ok(DataUpdate::Closed) => return Ok(None),
                Ok(DataUpdate::Event {
                    event:
                        DataEvent::Message {
                            data,
                            participant_id,
                            track_id,
                            ..
                        },
                }) => {
                    ensure!(
                        data == expected,
                        "unexpected installed consumer bytes from {participant_id}/{track_id}: {data:?}; expected {expected:?}"
                    );
                    let mut payload = b"rust-ack:".to_vec();
                    payload.extend(data);
                    exchange.send(payload).await?;
                    if let Some(received_tx) = received_tx.take() {
                        let _ = received_tx.send(());
                    }
                }
                Ok(DataUpdate::Event {
                    event: DataEvent::Gap { from, to, .. },
                }) => bail!("owned data stream gap: {from}..{to}"),
            }
        }
    });
    let output = tokio::time::timeout(timeout, command.output()).await;
    responder.abort();
    let response = responder.await;
    let output = output.context("installed consumer did not finish")??;
    ensure!(
        output.status.success(),
        "installed consumer {participant} failed: {}\n{}; responder: {response:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    match response {
        Ok(Err(error)) => return Err(error),
        Err(error) if !error.is_cancelled() => return Err(error.into()),
        _ => {}
    }
    received_rx
        .try_recv()
        .context("Rust peer missed installed consumer bytes and acknowledgement")?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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
    let camera = publisher.create_video_input(Default::default()).await?;
    let publication = publisher
        .video("production-camera".into())?
        .publish(&camera, Default::default())
        .await?;
    let subscription = subscriber
        .participant("publisher".into())?
        .video("production-camera".into())?
        .subscribe(VideoObservation {
            visible: true,
            height: HEIGHT,
            priority: 1,
            ..Default::default()
        })
        .await?;
    publisher.connect().await?;
    subscriber.connect().await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        tokio::try_join!(connected(&publisher), connected(&subscriber))?;
        anyhow::Ok(())
    })
    .await
    .with_context(|| {
        format!(
            "production connection deadline: publisher={:?}, stats={}; subscriber={:?}, stats={}",
            publisher.snapshot(),
            native_statistics(&publisher),
            subscriber.snapshot(),
            native_statistics(&subscriber)
        )
    })??;

    publication.ready().await?;
    let mut producer = tokio::spawn(produce_i420(camera.clone()));
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
    let close = tokio::time::timeout(Duration::from_secs(5), subscriber.disconnect()).await;
    if !matches!(&close, Ok(Ok(()))) {
        let _ = subscriber.abort().await;
    }
    let pixels = media.context("production decoded media did not arrive")??;
    close.context("production agents did not close")??;
    ensure!(
        !publisher.is_closed(),
        "publisher retired before installed consumers"
    );
    ensure!(
        !subscriber.is_closed(),
        "subscriber disconnect was terminal"
    );
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
    if std::env::var_os("PULSEBEAM_FOREIGN_SMOKE").is_some() {
        // Reuse the bounded server and live Rust publisher for both installed consumers.
        let foreign = config(&endpoint, "foreign")?;
        let foreign_reader = config(&endpoint, "foreign-reader")?;
        let mut command = tokio::process::Command::new(std::env::var("PULSEBEAM_FOREIGN_SMOKE")?);
        command
            .arg(&endpoint)
            .arg(&foreign.session.token)
            .arg(&foreign_reader.session.token)
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("LD_PRELOAD")
            .kill_on_drop(true);
        let output = foreign_witness(
            &publisher,
            "foreign",
            b"python-bytes-\0\xff",
            &mut command,
            Duration::from_secs(35),
        )
        .await?;
        print!("{output}");

        let node = config(&endpoint, "foreign-node")?;
        let mut command = tokio::process::Command::new(std::env::var("PULSEBEAM_NODE_WITNESS")?);
        command
            .arg(&endpoint)
            .arg(&node.session.token)
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("LD_PRELOAD")
            .kill_on_drop(true);
        let output = foreign_witness(
            &publisher,
            "foreign-node",
            b"node-bytes-\0\xff",
            &mut command,
            Duration::from_secs(60),
        )
        .await?;
        print!("{output}");
    }
    producer.abort();
    let _ = producer.await;
    let close = tokio::time::timeout(Duration::from_secs(5), async {
        publication.close().await?;
        ensure!(
            !camera.is_closed(),
            "publication close stopped the independent producer"
        );
        camera
            .submit(i420_frame(camera.capture_time_us().await?))
            .await
            .context("publication close prevented detached producer admission")?;
        publisher.close().await?;
        anyhow::Ok(())
    })
    .await;
    if !matches!(&close, Ok(Ok(()))) {
        let _ = publisher.abort().await;
    }
    close.context("publisher did not close")??;
    ensure!(
        publisher.is_closed(),
        "terminal publisher close was not latched"
    );
    ensure!(
        publisher.statistics().borrow().is_none(),
        "publisher native graph not retired"
    );
    subscriber.close().await?;
    Ok(())
}

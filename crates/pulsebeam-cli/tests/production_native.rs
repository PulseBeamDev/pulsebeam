//! One production constructor/HTTP/kernel-socket witness, not a media matrix.
use anyhow::{Context, Result, anyhow, bail, ensure};
use pulsebeam_agent_native::agent_core::{
    AgentConfig, ConnectionState, DesiredState, Generation, TrackSelector, TrackSpec,
    VideoObservation,
};
use pulsebeam_agent_native::{
    Agent, Config, Host, I420Frame, InputFrame, MediaStreamUpdate, OutputFrame, PublicationInput,
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
                    MediaStreamUpdate::Track { .. } => {},
                    MediaStreamUpdate::Lagged { skipped } | MediaStreamUpdate::NativeDropped { skipped } | MediaStreamUpdate::NativeDecodedDropped { skipped } => bail!("production video observation lost {skipped} frames"),
                    MediaStreamUpdate::Closed => bail!("production video closed before delivery"),
                }
            }
        }
    }).await.with_context(|| format!("production decoded-media deadline: publisher={:?}, subscriber={:?}", publisher.snapshot(), subscriber.snapshot()));
    producer.abort();
    let _ = producer.await;
    let retained = media
        .as_ref()
        .ok()
        .and_then(|result| result.as_ref().ok())
        .cloned();
    let close = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::try_join!(publisher.close(), subscriber.close())?;
        anyhow::Ok(())
    })
    .await;
    if !matches!(&close, Ok(Ok(()))) {
        let _ = tokio::join!(publisher.abort(), subscriber.abort());
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
    for agent in [&publisher, &subscriber] {
        ensure!(
            agent.snapshot().connection == ConnectionState::Disconnected,
            "production graceful close did not disconnect"
        );
        ensure!(
            agent.statistics().borrow().is_none(),
            "production native graph not retired"
        );
    }
    ensure!(
        Some(&pixels) == retained.as_ref(),
        "owning production pixels changed after close"
    );
    println!(
        "production native raw-I420 encode/real HTTP+SFU+Linux sockets/native decode/owned-close PASS"
    );
    // The same bounded server also proves the emitted Python API and cdylib,
    // without adding a second server fixture or claiming another media matrix.
    let foreign = config(&endpoint, "foreign")?;
    let mut command = tokio::process::Command::new(std::env::var("PULSEBEAM_FOREIGN_SMOKE")?);
    command
        .arg(&endpoint)
        .arg(&foreign.session.token)
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(20), command.output())
        .await
        .context("generated foreign RTC witness did not finish")??;
    ensure!(
        output.status.success(),
        "generated foreign RTC witness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

//! One production constructor/HTTP/kernel-socket witness, not a media matrix.
use anyhow::{Result, anyhow, bail, ensure};
use pulsebeam_agent_native::agent_core::{
    AgentConfig, ConnectionState, DesiredState, Generation, MediaKind, MediaSlot, MediaTopology,
    PublicationIntent, VideoSubscription,
};
use pulsebeam_agent_native::{
    Agent, Config, Host, I420Frame, InputFrame, LocalSource, MediaStreamUpdate, OutputFrame,
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

fn config(endpoint: &str, participant: &str, publisher: bool) -> Result<Config> {
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
        topology: MediaTopology {
            local_video: u8::from(publisher),
            local_audio: 0,
            remote_video: u8::from(!publisher),
            remote_audio: 0,
        },
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

async fn produce_i420(camera: LocalSource) -> Result<()> {
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
        config(&endpoint, "publisher", true)?,
        Host::new(Box::new(reqwest::Client::new())),
    )
    .await?;
    let subscriber = Agent::spawn(
        config(&endpoint, "subscriber", false)?,
        Host::new(Box::new(reqwest::Client::new())),
    )
    .await?;
    publisher
        .replace_desired(DesiredState {
            revision: 1,
            connected: true,
            publications: vec![PublicationIntent {
                slot: "v0".into(),
                label: "production-camera".into(),
                active: true,
            }],
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
    .await??;

    let camera = publisher.source(MediaSlot::LocalVideo("v0".into())).await?;
    let remote = subscriber
        .remote(MediaSlot::RemoteVideo(0), ReceiveMode::Decoded)
        .await?;
    let mut producer = tokio::spawn(produce_i420(camera));
    let media = tokio::time::timeout(Duration::from_secs(20), async {
        let mut snapshots = subscriber.snapshots();
        let track_id = loop {
            let snapshot = snapshots.borrow().clone();
            if let Some(publication) = snapshot.publications.values().find(|publication| {
                publication.kind == MediaKind::Video
                    && snapshot.participant_id.as_deref() != Some(&publication.participant_id)
            }) {
                break publication.id.clone();
            }
            snapshots.changed().await?;
        };
        subscriber
            .replace_desired(DesiredState {
                revision: 2,
                connected: true,
                video: vec![VideoSubscription {
                    slot: 0,
                    track_id,
                    selector: None,
                    height: HEIGHT,
                    min_height: 0,
                    min_fps: 0,
                    priority: 1,
                    playout_delay: Default::default(),
                }],
                ..Default::default()
            })
            .await?;
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
    }).await;
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
    let pixels = media??;
    close??;
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
    let foreign = config(&endpoint, "foreign", true)?;
    let mut command = tokio::process::Command::new(std::env::var("PULSEBEAM_FOREIGN_SMOKE")?);
    command
        .arg(&endpoint)
        .arg(&foreign.session.token)
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(20), command.output()).await??;
    ensure!(
        output.status.success(),
        "generated foreign RTC witness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

use std::{io, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use pulsebeam_agent_native::agent_core::{
    AgentConfig, ConnectionState, LogLevel, VideoObservation,
};
use pulsebeam_agent_native::{
    Agent, Config, Host, I420Frame, InputFrame, MediaStreamUpdate, OutputFrame, ReceiveMode,
    RemoteMedia,
};
use serde_json::{Value, json};
use tokio::{task::JoinSet, time::Instant};

const ROOM: &str = "release-roster";
const CAMERA: &str = "camera";
const WIDTH: u32 = 160;
const HEIGHT: u32 = 96;
const LUMA: usize = (WIDTH * HEIGHT) as usize;
const DECODE_GAP: Duration = Duration::from_millis(350);
const FIRST_DECODE_BUDGET: Duration = Duration::from_secs(35);

#[derive(serde::Deserialize)]
struct Clients {
    tokens: Vec<String>,
    capacity_probe_token: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let [source, destination, metrics, seconds, publisher_id]: [String; 5] = std::env::args()
        .skip(1)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| {
            anyhow::anyhow!("expected three endpoints, sample seconds, and a publisher ID")
        })?;
    ensure!(source != destination, "expected distinct server endpoints");
    let sample = Duration::from_secs(seconds.parse()?);
    let clients: Clients = serde_json::from_reader(io::stdin().lock())?;
    let probe = clients
        .capacity_probe_token
        .as_deref()
        .map(|token| (destination.as_str(), token));
    let mut tokens = clients.tokens.into_iter();
    let publisher = agent(&source, tokens.next().context("missing publisher token")?).await?;
    let mut viewers = Vec::new();
    for token in tokens {
        viewers.push(agent(&destination, token).await?);
    }
    ensure!(!viewers.is_empty(), "missing viewer tokens");
    let report = tokio::time::timeout(
        FIRST_DECODE_BUDGET + sample + Duration::from_secs(3),
        decoded(&publisher, &viewers, &publisher_id, &metrics, sample, probe),
    )
    .await
    .context("cross-node decode deadline")??;
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut disconnects = JoinSet::new();
        for agent in viewers.into_iter().chain([publisher]) {
            disconnects.spawn(async move { agent.disconnect().await });
        }
        while let Some(result) = disconnects.join_next().await {
            result??;
        }
        anyhow::Ok(())
    })
    .await
    .context("clients did not disconnect")??;
    let successful = report.iter().all(|viewer| viewer["outcome"] == "decoded");
    println!(
        "{}",
        json!({"viewers": report, "sample_seconds": sample.as_secs(), "capacity_rejected": probe.is_some()})
    );
    ensure!(
        successful,
        "not all viewers received and sustained decoded output"
    );
    Ok(())
}

async fn agent(endpoint: &str, token: String) -> Result<Agent> {
    let config = Config::new(AgentConfig {
        endpoint: endpoint.into(),
        token,
        retry: Default::default(),
        log_level: LogLevel::Warn,
    });
    let http = reqwest::Client::builder().no_proxy().build()?;
    Ok(Agent::spawn(config, Host::new(Box::new(http))).await?)
}

async fn joined(agent: &Agent) -> Result<()> {
    let mut snapshots = agent.snapshots();
    loop {
        {
            let snapshot = snapshots.borrow();
            ensure!(
                snapshot.connection != ConnectionState::TerminalFailure,
                "native connection failed"
            );
            if snapshot.connection == ConnectionState::Connected {
                ensure!(
                    snapshot.room_id.as_deref() == Some(ROOM),
                    "unexpected room scope"
                );
                return Ok(());
            }
        }
        snapshots.changed().await?;
    }
}

async fn decoded(
    publisher: &Agent,
    viewers: &[Agent],
    publisher_id: &str,
    metrics: &str,
    sample: Duration,
    capacity_probe: Option<(&str, &str)>,
) -> Result<Vec<Value>> {
    let camera = publisher.create_video_input(Default::default()).await?;
    let publication = publisher
        .video(CAMERA.into())?
        .publish(Default::default())?;
    let _pipe = camera.pipe_to(&publication)?;
    let start = Instant::now();
    publisher.join(ROOM.into())?;
    let mut playback = JoinSet::new();
    for (index, viewer) in viewers.iter().enumerate() {
        let subscription = viewer
            .room(ROOM.into())?
            .participant(publisher_id.into())?
            .video(CAMERA.into())?
            .subscribe(VideoObservation {
                visible: true,
                priority: 1,
                ..Default::default()
            })?;
        viewer.join(ROOM.into())?;
        let snapshots = viewer.snapshots();
        playback.spawn(async move {
            let attach = async {
                let remote = subscription.observe(ReceiveMode::Decoded).await?;
                next_frame(&remote).await?;
                anyhow::Ok(remote)
            };
            let mut report =
                match tokio::time::timeout_at(start + FIRST_DECODE_BUDGET, attach).await {
                    Ok(Ok(remote)) => observe(remote, start, sample).await,
                    Ok(Err(error)) => json!({
                        "outcome": "failed", "error": error.to_string(),
                        "failed_after_ms": start.elapsed().as_millis(),
                    }),
                    Err(_) => json!({"outcome": "first_decode_timeout"}),
                };
            report["viewer"] = json!(index);
            report["connection"] = json!(
                snapshots
                    .borrow()
                    .connection_id
                    .as_ref()
                    .map(|id| id.as_str())
            );
            (report, subscription)
        });
    }
    joined(publisher).await?;
    publication.ready().await?;
    let frames = async {
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        loop {
            ticker.tick().await;
            let mut data = vec![180; LUMA];
            data.resize(LUMA * 5 / 4, 90);
            data.resize(LUMA * 3 / 2, 180);
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
    };
    let collect = async {
        let mut reports = Vec::with_capacity(viewers.len());
        let mut subscriptions = Vec::with_capacity(viewers.len());
        while let Some(result) = playback.join_next().await {
            let (report, subscription) = result?;
            reports.push(report);
            subscriptions.push(subscription);
        }
        check_unprotected(metrics).await?;
        anyhow::Ok(reports)
    };
    let reject_excess = async {
        if let Some((endpoint, token)) = capacity_probe {
            for viewer in viewers {
                joined(viewer).await?;
            }
            check_capacity(endpoint, token).await?;
        }
        anyhow::Ok(())
    };
    tokio::select! {
        result = frames => result,
        result = async { Ok(tokio::try_join!(collect, reject_excess)?.0) } => result,
    }
}

async fn check_capacity(endpoint: &str, token: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()?;
    let proof = client
        .post(format!("{endpoint}/api/v1/native/admissions"))
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .body(json!({"room": ROOM}).to_string())
        .send()
        .await?
        .error_for_status()?;
    let proof: Value = serde_json::from_slice(&proof.bytes().await?)?;
    let owner = proof["owner"].as_str().context("missing owner proof")?;
    // At capacity admission must stop before SDP negotiation.
    let rejected = client
        .post(format!("{endpoint}/api/v1/native"))
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .header("PulseBeam-Owner", owner)
        .header("PulseBeam-Attempt", "1")
        .body(json!({"profile": "pulsebeam.media.v1", "room": ROOM, "offer": ""}).to_string())
        .send()
        .await?;
    ensure!(
        rejected.status() == 503,
        "expected HTTP 503 at the node connection limit, got {}",
        rejected.status()
    );
    let problem: Value = serde_json::from_slice(&rejected.bytes().await?)?;
    ensure!(
        problem["detail"] == "join failed: node connection limit reached",
        "unexpected capacity response: {problem}"
    );
    Ok(())
}

async fn observe(remote: RemoteMedia, start: Instant, sample: Duration) -> Value {
    let first = Instant::now();
    let mut error = None;
    let stop = first + sample;
    let mut last = first;
    let (mut frames, mut late_gaps, mut peak_gap) = (1, 0, Duration::ZERO);
    while Instant::now() < stop {
        let update = tokio::time::timeout_at(stop, next_frame(&remote)).await;
        let gap = Instant::now() - last;
        peak_gap = peak_gap.max(gap);
        late_gaps += u64::from(gap > DECODE_GAP);
        match update {
            Ok(Ok(())) => {}
            Ok(Err(failed)) => {
                error = Some(failed.to_string());
                break;
            }
            Err(_) => break,
        }
        frames += 1;
        last = Instant::now();
    }
    json!({
        "outcome": if error.is_some() { "failed" } else if late_gaps > 0 { "deadline_miss" } else { "decoded" },
        "error": error,
        "first_decode_ms": (first - start).as_millis(),
        "decoded_frames": frames,
        "gaps_over_350_ms": late_gaps,
        "peak_gap_ms": peak_gap.as_millis(),
    })
}

async fn next_frame(remote: &RemoteMedia) -> Result<()> {
    loop {
        match remote.next().await {
            MediaStreamUpdate::Frame { frame } => {
                let OutputFrame::I420 { frame } = frame.frame else {
                    bail!("native output was not decoded I420");
                };
                ensure!(
                    (frame.width, frame.height) == (WIDTH, HEIGHT),
                    "wrong decoded size"
                );
                ensure!(
                    frame.data.len() == LUMA * 3 / 2,
                    "invalid decoded I420 layout"
                );
                ensure!(
                    frame.data[..LUMA]
                        .iter()
                        .all(|sample| sample.abs_diff(180) <= 12),
                    "decoded pixels did not match the publisher"
                );
                return Ok(());
            }
            MediaStreamUpdate::Closed => bail!("video closed before decode"),
            _ => {}
        }
    }
}

async fn check_unprotected(endpoint: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()?;
    let report = client
        .get(endpoint)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    for state in ["degraded", "protected"] {
        let expected = format!("regional_media_protection_sources{{state=\"{state}\"}} 0");
        ensure!(
            report.lines().any(|line| line == expected),
            "expected {expected} during live decode with only one route"
        );
    }
    Ok(())
}

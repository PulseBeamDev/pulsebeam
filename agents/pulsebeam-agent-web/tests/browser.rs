use std::env;
use std::error::Error;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use thirtyfour::bidi::BrowsingContextId;
use thirtyfour::bidi::modules::browsing_context::ReadinessState;
use thirtyfour::prelude::*;
use thirtyfour::testing::run_browser_test;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const PUBLIC_AGENT_CONTRACT: &str = r#"
(async () => {
  let invalidCode;
  try {
    await window.pulsebeam.createAgent({ endpoint: "relative", roomId: "room", topology: {} });
  } catch (error) {
    invalidCode = error.code;
  }

  const sourceSlots = ["camera"];
  const agent = await window.pulsebeam.createAgent({
    endpoint: window.location.origin,
    roomId: "sender-stats",
    topology: { localVideo: sourceSlots, localAudio: ["microphone"], remoteVideo: 2, remoteAudio: 1 },
  });
  sourceSlots.push("mutated");
  let immutableCode;
  try {
    await agent.setLocalTrack("mutated", null);
  } catch (error) {
    immutableCode = error.code;
  }
  const initialIdentity = agent.getSnapshot() === agent.getSnapshot();
  const revisions = [agent.getSnapshot().desiredRevision];
  const order = [];
  let removeFirst;
  removeFirst = agent.subscribe(() => {
    order.push("first");
    removeFirst();
  });
  const removeSecond = agent.subscribe(() => order.push("second"));

  agent.setVideoDemand([
    { slot: 0, publicationId: "remote/video", height: 720, minHeight: 180, minFps: 15, priority: 4 },
  ]);
  revisions.push(agent.getSnapshot().desiredRevision);
  agent.setAudioDemand({ pinned: ["remote/audio"], automatic: false });
  revisions.push(agent.getSnapshot().desiredRevision);
  agent.setPlayoutDelay({ mode: "fixed", minMs: 80, maxMs: 160 });
  revisions.push(agent.getSnapshot().desiredRevision);
  removeSecond();

  let commandCode;
  try {
    agent.setVideoDemand([{ slot: 2, publicationId: "outside", height: 360 }]);
  } catch (error) {
    commandCode = error.code;
  }

  const topic = agent.openTopic({ name: "cursor", mode: "latest", publish: true, subscribe: true });
  const topicIdentity = topic === agent.openTopic({
    name: "cursor", mode: "latest", publish: true, subscribe: true,
  });
  revisions.push(agent.getSnapshot().desiredRevision);
  const registeredTopics = agent.getSnapshot().topics.publishers.length;
  const pendingTopic = topic[Symbol.asyncIterator]().next();
  topic.close();
  const topicIteratorClosed = (await pendingTopic).done;
  revisions.push(agent.getSnapshot().desiredRevision);
  const topicsAfterClose = agent.getSnapshot().topics.publishers.length;

  const canvas = document.createElement("canvas");
  canvas.width = 64;
  canvas.height = 64;
  const track = canvas.captureStream(5).getVideoTracks()[0];
  await agent.setLocalTrack("camera", track, "detail");
  const audioContext = new AudioContext();
  const audioTrack = audioContext.createMediaStreamDestination().stream.getAudioTracks()[0];
  await agent.setLocalTrack("microphone", audioTrack, "music");
  revisions.push(agent.getSnapshot().desiredRevision);
  const published = agent.getSnapshot().localPublications.find((item) => item.slot === "camera");
  const publishedAudio = agent.getSnapshot().localPublications.find((item) => item.slot === "microphone");
  await agent.setLocalMuted("camera", true);
  revisions.push(agent.getSnapshot().desiredRevision);
  const muted = agent.getSnapshot().localPublications.find((item) => item.slot === "camera");
  const mutedValue = muted.muted;
  const enabledWhenMuted = muted.track.enabled;
  await agent.setLocalMuted("camera", false);

  const joining = new Promise((resolve, reject) => {
    const deadline = setTimeout(() => reject(new Error("agent did not start signaling")), 5000);
    const remove = agent.subscribe(() => {
      if (agent.getSnapshot().connection !== "joining") return;
      clearTimeout(deadline);
      remove();
      resolve();
    });
  });
  agent.connect();
  await joining;
  const statistics = await agent.statistics();
  const sender = statistics.senders.find((item) => item.slot === "camera");
  const audioSender = statistics.senders.find((item) => item.slot === "microphone");
  const replacementCanvas = document.createElement("canvas");
  const replacementTrack = replacementCanvas.captureStream(5).getVideoTracks()[0];
  await agent.setLocalTrack("camera", replacementTrack, "motion");
  const replacementStatistics = await agent.statistics();
  const replacementSender = replacementStatistics.senders.find((item) => item.slot === "camera");
  const snapshotHasReplacement = agent.getSnapshot().localPublications
    .find((item) => item.slot === "camera").track === replacementTrack;
  await agent.setLocalTrack("camera", null);
  await agent.setLocalTrack("microphone", null);
  const removedTracks = agent.getSnapshot().localPublications.filter((item) => item.track).length;
  const closeNotifications = [];
  agent.subscribe(() => closeNotifications.push(agent.getSnapshot().connection));
  await agent.close();
  const closed = agent.getSnapshot();
  const notificationsAtClose = closeNotifications.length;
  agent.subscribe(() => closeNotifications.push("late"));
  track.stop();
  replacementTrack.stop();
  audioTrack.stop();
  await audioContext.close();

  return {
    invalidCode,
    commandCode,
    immutableCode,
    initialIdentity,
    order,
    revisions,
    topicIdentity,
    topicIteratorClosed,
    registeredTopics,
    topicsAfterClose,
    local: {
      sameTrack: published.track === track,
      policy: published.policy,
      contentHint: track.contentHint,
      muted: mutedValue,
      enabledWhenMuted,
    },
    audio: {
      sameTrack: publishedAudio.track === audioTrack,
      policy: publishedAudio.policy,
      contentHint: audioTrack.contentHint,
      maxBitrate: audioSender.encodings[0]?.maxBitrate,
      dtx: audioSender.encodings[0]?.dtx,
    },
    sender,
    replacement: {
      initialTrackId: sender.trackId,
      trackId: replacementSender.trackId,
      expectedTrackId: replacementTrack.id,
      snapshotHasReplacement,
      active: replacementSender.encodings.filter((encoding) => encoding.active).length,
      removedTracks,
    },
    closed: {
      connection: closed.connection,
      localTracks: closed.localPublications.filter((item) => item.track).length,
      remoteVideo: closed.remoteVideo.length,
      notificationsAtClose,
      notificationsAfterClose: closeNotifications.length,
    },
  };
})()
"#;

const SERVER_SLICE: &str = r#"
(async () => {
  const waitForSnapshot = (agent, predicate, label) => new Promise((resolve, reject) => {
    if (predicate(agent.getSnapshot())) {
      resolve(agent.getSnapshot());
      return;
    }
    const deadline = setTimeout(() => {
      remove();
      reject(new Error(`${label}: ${JSON.stringify(agent.getSnapshot(), bigintJson)}`));
    }, 15000);
    const remove = agent.subscribe(() => {
      const snapshot = agent.getSnapshot();
      if (!predicate(snapshot)) return;
      clearTimeout(deadline);
      remove();
      resolve(snapshot);
    });
  });
  const bigintJson = (_key, value) => typeof value === "bigint" ? value.toString() : value;
  const roomId = `web-${crypto.randomUUID().slice(0, 12)}`;
  const config = {
    endpoint: "http://127.0.0.1:7070",
    roomId,
    topology: { localVideo: ["camera"], localAudio: ["microphone"], remoteVideo: 1, remoteAudio: 1 },
  };
  const first = await window.pulsebeam.createAgent(config);
  const second = await window.pulsebeam.createAgent(config);
  const firstCursor = first.openTopic({ name: "cursor", mode: "latest", publish: true });
  const firstChat = first.openTopic({ name: "chat", mode: "ordered", publish: true });
  const secondCursor = second.openTopic({ name: "cursor", mode: "latest", subscribe: true });
  const secondChat = second.openTopic({ name: "chat", mode: "ordered", subscribe: true });
  const messages = [];
  let resynchronizations = 0;
  secondCursor.subscribe((delivery) => {
    if (delivery.type === "message") messages.push({ mode: delivery.mode, text: new TextDecoder().decode(delivery.payload) });
  });
  secondChat.subscribe((delivery) => {
    if (delivery.type === "message") {
      messages.push({ mode: delivery.mode, text: new TextDecoder().decode(delivery.payload) });
    } else {
      resynchronizations += 1;
    }
  });
  const received = new Promise((resolve, reject) => {
    const deadline = setTimeout(() => reject(new Error("topic messages timed out")), 15000);
    const check = () => {
      if (messages.length !== 2) return;
      clearTimeout(deadline);
      resolve();
    };
    secondCursor.subscribe(check);
    secondChat.subscribe(check);
  });

  const firstCanvas = document.createElement("canvas");
  const secondCanvas = document.createElement("canvas");
  firstCanvas.width = secondCanvas.width = 64;
  firstCanvas.height = secondCanvas.height = 64;
  const firstVideo = firstCanvas.captureStream(5).getVideoTracks()[0];
  const secondVideo = secondCanvas.captureStream(5).getVideoTracks()[0];
  const firstAudioContext = new AudioContext();
  const secondAudioContext = new AudioContext();
  const firstAudio = firstAudioContext.createMediaStreamDestination().stream.getAudioTracks()[0];
  const secondAudio = secondAudioContext.createMediaStreamDestination().stream.getAudioTracks()[0];
  await first.setLocalTrack("camera", firstVideo, "motion");
  await first.setLocalTrack("microphone", firstAudio, "speech");
  await second.setLocalTrack("camera", secondVideo, "detail");
  await second.setLocalTrack("microphone", secondAudio, "music");
  second.connect();
  await waitForSnapshot(second, (snapshot) => snapshot.connection === "connected", "second connect");
  first.connect();
  await waitForSnapshot(first, (snapshot) => snapshot.connection === "connected", "first connect");
  const secondDiscovered = await waitForSnapshot(
    second,
    (snapshot) => snapshot.publications.some((publication) => publication.kind === "video")
      && snapshot.publications.some((publication) => publication.kind === "audio"),
    "second publication discovery",
  );
  const firstDiscovered = await waitForSnapshot(
    first,
    (snapshot) => snapshot.publications.some((publication) => publication.kind === "video")
      && snapshot.publications.some((publication) => publication.kind === "audio"),
    "first publication discovery",
  );
  const firstPublicationId = secondDiscovered.publications.find((publication) => publication.kind === "video").id;
  const secondPublicationId = firstDiscovered.publications.find((publication) => publication.kind === "video").id;
  const firstAudioId = secondDiscovered.publications.find((publication) => publication.kind === "audio").id;
  second.setVideoDemand([{ slot: 0, publicationId: firstPublicationId, height: 360, minHeight: 180, minFps: 7, priority: 2 }]);
  first.setVideoDemand([{ slot: 0, publicationId: secondPublicationId, height: 540, minHeight: 180, minFps: 7, priority: 1 }]);
  second.setAudioDemand({ pinned: [firstAudioId], automatic: false });
  first.setAudioDemand({ automatic: true });
  first.setPlayoutDelay({ mode: "fixed", minMs: 80, maxMs: 160 });
  second.setPlayoutDelay({ mode: "fixed", minMs: 80, maxMs: 160 });
  const secondBound = await waitForSnapshot(
    second,
    (snapshot) => snapshot.remoteVideo.some((video) => video.publicationId === firstPublicationId && video.bound && video.track)
      && snapshot.remoteAudio.some((audio) => audio.bound && audio.track),
    "second media binding",
  );
  await waitForSnapshot(
    first,
    (snapshot) => snapshot.remoteVideo.some((video) => video.publicationId === secondPublicationId && video.bound && video.track)
      && snapshot.remoteAudio.some((audio) => audio.bound && audio.track),
    "first media binding",
  );
  const before = secondBound.remoteVideo.find((video) => video.publicationId === firstPublicationId);

  firstCursor.send(new TextEncoder().encode("latest"));
  firstChat.send(new TextEncoder().encode("ordered"));
  await received;
  const participantId = first.getSnapshot().participantId;
  const generation = first.getSnapshot().generation;
  first.reconnect();
  const replacement = await waitForSnapshot(
    first,
    (snapshot) => snapshot.connection === "connected" && snapshot.generation !== generation,
    "transport replacement",
  );
  const rebound = await waitForSnapshot(
    second,
    (snapshot) => snapshot.remoteVideo.some((video) => video.publicationId === firstPublicationId && video.bound && video.track),
    "video rebound",
  );
  const after = rebound.remoteVideo.find((video) => video.publicationId === firstPublicationId);
  const recovered = new Promise((resolve, reject) => {
    const deadline = setTimeout(() => reject(new Error("ordered recovery timed out")), 15000);
    const remove = secondChat.subscribe((delivery) => {
      if (delivery.type !== "message" || new TextDecoder().decode(delivery.payload) !== "recovered") return;
      clearTimeout(deadline);
      remove();
      resolve();
    });
  });
  firstChat.send(new TextEncoder().encode("recovered"));
  await recovered;
  const statistics = await first.statistics();
  await first.close();
  await second.close();
  firstVideo.stop();
  secondVideo.stop();
  firstAudio.stop();
  secondAudio.stop();
  await firstAudioContext.close();
  await secondAudioContext.close();
  return {
    messages: messages.sort((left, right) => left.text.localeCompare(right.text)),
    resynchronizations,
    participantId,
    replacementParticipantId: replacement.participantId,
    stableStream: before.stream === after.stream,
    senderCount: statistics.senders.length,
    firstConnection: first.getSnapshot().connection,
    secondConnection: second.getSnapshot().connection,
  };
})()
"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContractResult {
    invalid_code: String,
    command_code: String,
    immutable_code: String,
    initial_identity: bool,
    order: Vec<String>,
    revisions: Vec<String>,
    topic_identity: bool,
    topic_iterator_closed: bool,
    registered_topics: usize,
    topics_after_close: usize,
    local: LocalResult,
    audio: AudioResult,
    sender: SenderResult,
    replacement: ReplacementResult,
    closed: ClosedResult,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalResult {
    same_track: bool,
    policy: String,
    content_hint: String,
    muted: bool,
    enabled_when_muted: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AudioResult {
    same_track: bool,
    policy: String,
    content_hint: String,
    max_bitrate: u32,
    dtx: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SenderResult {
    track_id: String,
    encodings: Vec<EncodingResult>,
}

#[derive(Debug, Deserialize)]
struct EncodingResult {
    rid: Option<String>,
    active: bool,
    #[serde(rename = "scalabilityMode")]
    scalability_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplacementResult {
    initial_track_id: String,
    track_id: String,
    expected_track_id: String,
    snapshot_has_replacement: bool,
    active: usize,
    removed_tracks: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClosedResult {
    connection: String,
    local_tracks: usize,
    remote_video: usize,
    notifications_at_close: usize,
    notifications_after_close: usize,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct TopicMessage {
    mode: String,
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerResult {
    messages: Vec<TopicMessage>,
    resynchronizations: usize,
    participant_id: String,
    replacement_participant_id: String,
    stable_stream: bool,
    sender_count: usize,
    first_connection: String,
    second_connection: String,
}

struct StaticServer {
    address: String,
    task: JoinHandle<()>,
}

impl StaticServer {
    async fn start() -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let root = root.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, &root).await;
                });
            }
        });
        Ok(Self { address, task })
    }

    fn fixture_url(&self) -> String {
        format!("http://{}/tests/fixture.html", self.address)
    }
}

impl Drop for StaticServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn public_agent_contract_runs_through_bidi() -> TestResult<()> {
    let server = StaticServer::start().await?;
    let fixture_url = server.fixture_url();
    let mut capabilities = DesiredCapabilities::chrome();
    capabilities.set_headless()?;
    capabilities.set_no_sandbox()?;
    capabilities.set_disable_gpu()?;
    capabilities.enable_bidi()?;
    if let Some(binary) = env::var_os("PULSEBEAM_BROWSER_BINARY") {
        capabilities.set_binary(&binary.to_string_lossy())?;
    }

    let result = run_browser_test(WebDriver::managed(capabilities), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        load_fixture(&bidi, &context, &fixture_url).await?;
        let contract: ContractResult =
            evaluate_json(&bidi, &context, PUBLIC_AGENT_CONTRACT).await?;
        assert_contract(contract);

        if env::var_os("PULSEBEAM_BROWSER_SERVER").is_some() {
            load_fixture(&bidi, &context, &fixture_url).await?;
            let slice: ServerResult = evaluate_json(&bidi, &context, SERVER_SLICE).await?;
            assert_eq!(
                slice.messages,
                [
                    TopicMessage {
                        mode: "latest".into(),
                        text: "latest".into()
                    },
                    TopicMessage {
                        mode: "ordered".into(),
                        text: "ordered".into()
                    },
                    TopicMessage {
                        mode: "ordered".into(),
                        text: "recovered".into()
                    },
                ]
            );
            assert_eq!(slice.resynchronizations, 1);
            assert_eq!(slice.replacement_participant_id, slice.participant_id);
            assert!(slice.stable_stream);
            assert_eq!(slice.sender_count, 2);
            assert_eq!(slice.first_connection, "closed");
            assert_eq!(slice.second_connection, "closed");
        }

        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await;

    result.map_err(|error| format!("browser test failed: {error}").into())
}

fn assert_contract(contract: ContractResult) {
    assert_eq!(contract.invalid_code, "invalid-config");
    assert_eq!(contract.command_code, "invalid-command");
    assert_eq!(contract.immutable_code, "invalid-command");
    assert!(contract.initial_identity);
    assert_eq!(contract.order, ["first", "second", "second", "second"]);
    assert!(contract.revisions.windows(2).all(|pair| {
        let left = pair
            .first()
            .and_then(|revision| revision.parse::<u64>().ok());
        let right = pair
            .get(1)
            .and_then(|revision| revision.parse::<u64>().ok());
        left.zip(right).is_some_and(|(left, right)| left < right)
    }));
    assert!(contract.topic_identity);
    assert!(contract.topic_iterator_closed);
    assert_eq!(contract.registered_topics, 1);
    assert_eq!(contract.topics_after_close, 0);
    assert!(contract.local.same_track);
    assert_eq!(contract.local.policy, "detail");
    assert_eq!(contract.local.content_hint, "detail");
    assert!(contract.local.muted);
    assert!(!contract.local.enabled_when_muted);
    assert!(contract.audio.same_track);
    assert_eq!(contract.audio.policy, "music");
    assert_eq!(contract.audio.content_hint, "music");
    assert_eq!(contract.audio.max_bitrate, 128_000);
    assert!(
        contract
            .audio
            .dtx
            .as_deref()
            .is_none_or(|dtx| dtx == "disabled")
    );
    assert!(!contract.sender.track_id.is_empty());
    assert_eq!(contract.sender.encodings.len(), 3);
    let active = contract
        .sender
        .encodings
        .iter()
        .filter(|encoding| encoding.active)
        .count();
    assert_eq!(active, 1);
    assert_eq!(
        contract
            .sender
            .encodings
            .iter()
            .filter_map(|encoding| encoding.rid.as_deref())
            .collect::<Vec<_>>(),
        ["q", "h", "f"]
    );
    assert!(contract.sender.encodings.iter().all(|encoding| {
        matches!(
            encoding.scalability_mode.as_deref(),
            Some("L1T2") | Some("L1T3")
        )
    }));
    assert_ne!(
        contract.replacement.track_id,
        contract.replacement.initial_track_id
    );
    assert!(!contract.replacement.expected_track_id.is_empty());
    assert!(contract.replacement.snapshot_has_replacement);
    assert_eq!(contract.replacement.active, 3);
    assert_eq!(contract.replacement.removed_tracks, 0);
    assert_eq!(contract.closed.connection, "closed");
    assert_eq!(contract.closed.local_tracks, 0);
    assert_eq!(contract.closed.remote_video, 0);
    assert_eq!(
        contract.closed.notifications_at_close,
        contract.closed.notifications_after_close
    );
}

async fn load_fixture(
    bidi: &thirtyfour::bidi::BiDi,
    context: &BrowsingContextId,
    fixture_url: &str,
) -> TestResult<()> {
    bidi.browsing_context()
        .navigate(context.clone(), fixture_url, Some(ReadinessState::Complete))
        .await?;
    evaluate_json::<()>(
        bidi,
        context,
        "import('/dist/index.js').then((module) => { window.pulsebeam = module; return null; })",
    )
    .await
}

async fn evaluate_json<T: DeserializeOwned>(
    bidi: &thirtyfour::bidi::BiDi,
    context: &BrowsingContextId,
    expression: &str,
) -> TestResult<T> {
    let source = format!(
        "(async () => JSON.stringify(await ({expression}), (_key, value) => typeof value === 'bigint' ? value.toString() : value))()"
    );
    let result = bidi
        .script()
        .evaluate(context.clone(), source, true)
        .await?;
    let value = result
        .ok_value()
        .ok_or_else(|| format!("browser expression raised an exception: {result:?}"))?;
    let json = value
        .get("value")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("browser expression did not return a JSON string: {value}"))?;
    Ok(serde_json::from_str(json)?)
}

async fn serve(mut stream: TcpStream, root: &Path) -> io::Result<()> {
    let mut request = [0_u8; 16 * 1024];
    let length = stream.read(&mut request).await?;
    debug_assert!(length <= request.len());
    let received = request.get(..length).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "request exceeded read buffer")
    })?;
    let head = String::from_utf8_lossy(received);
    let mut parts = head.lines().next().unwrap_or_default().split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();

    if method != "GET" && method != "HEAD" {
        if target.contains("/rooms/sender-stats/") {
            let mut byte = [0_u8; 1];
            while stream.read(&mut byte).await? != 0 {}
            return Ok(());
        }
        return respond(
            &mut stream,
            503,
            "text/plain",
            b"test signaling unavailable",
            method,
        )
        .await;
    }

    let Some(path) = static_path(root, target) else {
        return respond(&mut stream, 404, "text/plain", b"not found", method).await;
    };
    match fs::read(&path).await {
        Ok(body) => respond(&mut stream, 200, content_type(&path), &body, method).await,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            respond(&mut stream, 404, "text/plain", b"not found", method).await
        }
        Err(error) => Err(error),
    }
}

fn static_path(root: &Path, target: &str) -> Option<PathBuf> {
    let path = target.split('?').next()?.trim_start_matches('/');
    let relative = Path::new(path);
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(root.join(relative))
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    method: &str,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    if method != "HEAD" {
        stream.write_all(body).await?;
    }
    stream.shutdown().await
}

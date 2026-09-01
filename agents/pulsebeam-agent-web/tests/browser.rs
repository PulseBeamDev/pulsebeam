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

const LISTENER_ABORT: &str = r#"
(async () => {
  const runtime = await window.pulsebeam.createRuntime({
    endpoint: window.location.origin,
    roomId: "listener-abort",
    topology: {},
  });
  runtime.subscribe(() => runtime.abort());
  runtime.connect();
  return runtime.diagnostics();
})()
"#;

const RETRY_ABORT: &str = r#"
(async () => {
  const runtime = await window.pulsebeam.createRuntime({
    endpoint: window.location.origin,
    roomId: "retry-abort",
    topology: {},
  });
  const originalSetTimeout = window.setTimeout;
  let resolveScheduled;
  const scheduled = new Promise((resolve) => {
    resolveScheduled = resolve;
  });
  window.setTimeout = (callback, delay, ...args) => {
    const timer = originalSetTimeout.call(window, callback, delay, ...args);
    queueMicrotask(() => resolveScheduled(runtime.diagnostics()));
    return timer;
  };
  const deadline = originalSetTimeout.call(window, () => resolveScheduled(null), 5000);
  runtime.connect();
  const failed = await scheduled;
  clearTimeout(deadline);
  window.setTimeout = originalSetTimeout;
  if (failed === null) throw new Error(`runtime retry timed out: ${JSON.stringify(runtime.diagnostics())}`);
  runtime.abort();
  return { failed, aborted: runtime.diagnostics() };
})()
"#;

const SERVER_SLICE: &str = r#"
(async () => {
  const roomId = `web-${crypto.randomUUID().slice(0, 12)}`;
  const config = {
    endpoint: "http://127.0.0.1:7070",
    roomId,
    topology: { remoteVideo: 1, remoteAudio: 1 },
    topics: [
      { name: "cursor", mode: "latest", publish: true, subscribe: true },
      { name: "chat", mode: "ordered", publish: true, subscribe: true },
    ],
  };
  const first = await window.pulsebeam.createRuntime(config);
  const second = await window.pulsebeam.createRuntime(config);

  const waitForSnapshot = (runtime, predicate, label) => new Promise((resolve, reject) => {
    if (predicate(runtime.getSnapshot())) {
      resolve(runtime.getSnapshot());
      return;
    }
    const timeout = setTimeout(() => {
      unsubscribe();
      reject(new Error(`${label}: ${JSON.stringify(runtime.getSnapshot(), bigintJson)}`));
    }, 15000);
    const unsubscribe = runtime.subscribe(() => {
      const snapshot = runtime.getSnapshot();
      if (!predicate(snapshot)) return;
      clearTimeout(timeout);
      unsubscribe();
      resolve(snapshot);
    });
  });
  const bigintJson = (_key, value) => typeof value === "bigint" ? value.toString() : value;
  const messages = [];
  const received = new Promise((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error("topic messages timed out")), 15000);
    second.onEvent((event) => {
      if (event.type !== "topic-message") return;
      messages.push({ mode: event.mode, text: new TextDecoder().decode(event.payload) });
      if (messages.length !== 2) return;
      clearTimeout(timeout);
      resolve();
    });
  });

  second.connect();
  await waitForSnapshot(second, (snapshot) => snapshot.connection === "connected", "second connect");
  first.connect();
  await waitForSnapshot(first, (snapshot) => snapshot.connection === "connected", "first connect");
  await waitForSnapshot(second, (snapshot) => snapshot.participants === 1, "participant discovery");

  first.sendTopic("cursor", "latest", new TextEncoder().encode("latest"));
  first.sendTopic("chat", "ordered", new TextEncoder().encode("ordered"));
  await received;

  const participantId = first.getSnapshot().participantId;
  const generation = first.getSnapshot().generation;
  first.forceReconnect();
  const replacement = await waitForSnapshot(
    first,
    (snapshot) => snapshot.connection === "connected" && snapshot.generation !== generation,
    "transport replacement",
  );

  first.close();
  await waitForSnapshot(first, (snapshot) => snapshot.connection === "disconnected", "first close");
  const closed = first.diagnostics();
  first.abort();
  second.close();
  await waitForSnapshot(second, (snapshot) => snapshot.connection === "disconnected", "second close");
  second.abort();
  return { messages, participantId, replacement, closed };
})()
"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Diagnostics {
    peers: usize,
    requests: usize,
    timers: usize,
    closed: bool,
}

#[derive(Debug, Deserialize)]
struct RetryResult {
    failed: Diagnostics,
    aborted: Diagnostics,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct TopicMessage {
    mode: String,
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    participant_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerResult {
    messages: Vec<TopicMessage>,
    participant_id: Option<String>,
    replacement: Snapshot,
    closed: Diagnostics,
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
async fn browser_runtime_uses_bidi_for_its_host_boundary() -> TestResult<()> {
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
        let diagnostics: Diagnostics = evaluate_json(&bidi, &context, LISTENER_ABORT).await?;
        assert_released(&diagnostics);

        load_fixture(&bidi, &context, &fixture_url).await?;
        let retry: RetryResult = evaluate_json(&bidi, &context, RETRY_ABORT).await?;
        assert_eq!(retry.failed.peers, 0);
        assert_eq!(retry.failed.requests, 0);
        assert_eq!(retry.failed.timers, 1);
        assert!(!retry.failed.closed);
        assert_released(&retry.aborted);

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
                ]
            );
            assert_eq!(slice.replacement.participant_id, slice.participant_id);
            assert_eq!(slice.closed.peers, 0);
        }

        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await;

    result.map_err(|error| format!("browser test failed: {error}").into())
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

fn assert_released(diagnostics: &Diagnostics) {
    assert_eq!(diagnostics.peers, 0);
    assert_eq!(diagnostics.requests, 0);
    assert_eq!(diagnostics.timers, 0);
    assert!(diagnostics.closed);
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

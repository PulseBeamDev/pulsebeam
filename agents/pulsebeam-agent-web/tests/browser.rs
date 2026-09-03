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

const PUBLIC_AGENT_CONTRACT: &str = include_str!("contracts/public-agent-contract.js");
const UNIFFI_MEDIA_CONTRACT: &str = include_str!("contracts/uniffi-media-contract.js");
const SERVER_SLICE: &str = include_str!("contracts/server-slice-contract.js");

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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UniFfiMediaResult {
    track_identity: bool,
    stream_identity: bool,
    foreign_rejected: bool,
    stale_rejected: bool,
    exhaustion_rejected: bool,
    retained_before_track_release: String,
    retained_after_track_release: String,
    retained_before_stream_release: String,
    retained_after_stream_release: String,
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

    fn uniffi_fixture_url(&self) -> String {
        format!("http://{}/tests/uniffi-fixture.html", self.address)
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

#[tokio::test(flavor = "multi_thread")]
async fn generated_media_types_run_through_bidi() -> TestResult<()> {
    let server = StaticServer::start().await?;
    let fixture_url = server.uniffi_fixture_url();
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
        bidi.browsing_context()
            .navigate(context.clone(), fixture_url, Some(ReadinessState::Complete))
            .await?;
        let result: UniFfiMediaResult =
            evaluate_json(&bidi, &context, UNIFFI_MEDIA_CONTRACT).await?;
        assert!(result.track_identity);
        assert!(result.stream_identity);
        assert!(result.foreign_rejected);
        assert!(result.stale_rejected);
        assert!(result.exhaustion_rejected);
        assert_eq!(result.retained_before_track_release, "1");
        assert_eq!(result.retained_after_track_release, "0");
        assert_eq!(result.retained_before_stream_release, "1");
        assert_eq!(result.retained_after_stream_release, "0");
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await;

    result.map_err(|error| format!("generated binding browser test failed: {error}").into())
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

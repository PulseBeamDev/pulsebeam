use std::env;
use std::error::Error;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
const LIVE_AGENT_CONTRACT: &str = include_str!("contracts/live-agent-contract.js");
const UNIFFI_MEDIA_CONTRACT: &str = include_str!("contracts/uniffi-media-contract.js");
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContractResult {
    exports: Vec<String>,
    independent: bool,
    config_copied: bool,
    initial_stable: bool,
    initial_frozen: bool,
    initial: String,
    latest_only: bool,
    close_before_settlement: bool,
    local_operations: bool,
    validation_rejected: bool,
    failure_event: bool,
    caller_owns_track: bool,
    no_removed_listener_calls: bool,
    closed: String,
    post_close: bool,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LiveAgentResult {
    connected: bool,
    discovered: bool,
    delivered: bool,
    reconnected: bool,
    topic_metadata: bool,
    caller_owns_track: bool,
}

struct DestinationServer {
    child: Child,
}

impl DestinationServer {
    fn start() -> TestResult<Self> {
        let package = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let root = package
            .parent()
            .and_then(Path::parent)
            .ok_or("web package must be inside the workspace")?;
        let mut child = Command::new("cargo")
            .args(["run", "--release", "-p", "pulsebeam", "--", "--dev"])
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        for _ in 0..300 {
            if std::net::TcpStream::connect("127.0.0.1:7070").is_ok() {
                return Ok(Self { child });
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
        Err("PulseBeam development server did not listen on port 7070".into())
    }
}

impl Drop for DestinationServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct StaticServer {
    address: String,
    task: JoinHandle<()>,
    wasm_requests: Arc<AtomicUsize>,
}

impl StaticServer {
    async fn start() -> io::Result<Self> {
        Self::start_with_wasm_failure(false).await
    }

    async fn start_with_wasm_failure(fail_wasm: bool) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let wasm_requests = Arc::new(AtomicUsize::new(0));
        let request_count = Arc::clone(&wasm_requests);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let root = root.clone();
                let request_count = Arc::clone(&request_count);
                tokio::spawn(async move {
                    let _ = serve(stream, &root, fail_wasm, &request_count).await;
                });
            }
        });
        Ok(Self {
            address,
            task,
            wasm_requests,
        })
    }

    fn fixture_url(&self) -> String {
        format!("http://{}/tests/fixture.html", self.address)
    }

    fn uniffi_fixture_url(&self) -> String {
        format!("http://{}/tests/uniffi-fixture.html", self.address)
    }

    fn wasm_requests(&self) -> usize {
        self.wasm_requests.load(Ordering::Relaxed)
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

        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await;

    result.map_err(|error| -> Box<dyn Error + Send + Sync> {
        format!("browser test failed: {error}").into()
    })?;
    assert_eq!(server.wasm_requests(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn public_agent_connects_and_delivers_remote_media() -> TestResult<()> {
    let _destination = DestinationServer::start()?;
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

    run_browser_test(WebDriver::managed(capabilities), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        load_fixture(&bidi, &context, &fixture_url).await?;
        let result: LiveAgentResult = evaluate_json(&bidi, &context, LIVE_AGENT_CONTRACT).await?;
        assert!(result.connected);
        assert!(result.discovered);
        assert!(result.delivered);
        assert!(result.reconnected);
        assert!(result.topic_metadata);
        assert!(result.caller_owns_track);
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|error| format!("live browser agent test failed: {error}").into())
}

#[tokio::test(flavor = "multi_thread")]
async fn initialization_failure_is_private_and_deterministic() -> TestResult<()> {
    let server = StaticServer::start_with_wasm_failure(true).await?;
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
        let connection: String = evaluate_json(
            &bidi,
            &context,
            r#"new Promise((resolve, reject) => {
                const agent = window.pulsebeam.createAgent({
                    endpoint: location.origin,
                    roomId: "room",
                    topology: {},
                });
                const timeout = setTimeout(() => reject(new Error("initialization did not fail")), 5000);
                agent.subscribe(() => {
                    if (agent.getSnapshot().connection !== "terminal-failure") return;
                    clearTimeout(timeout);
                    resolve("terminal-failure");
                });
                agent.setState({ connected: true });
            })"#,
        )
        .await?;
        assert_eq!(connection, "terminal-failure");
        let unhandled_rejections: usize = evaluate_json(
            &bidi,
            &context,
            r#"new Promise((resolve) => setTimeout(() => resolve(globalThis.__pulsebeamUnhandledRejections.length), 20))"#,
        )
        .await?;
        assert_eq!(unhandled_rejections, 0);
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await;

    result.map_err(|error| -> Box<dyn Error + Send + Sync> {
        format!("browser failure test failed: {error}").into()
    })?;
    assert_eq!(server.wasm_requests(), 1);
    Ok(())
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
    assert_eq!(contract.exports, ["createAgent"]);
    assert!(contract.independent);
    assert!(contract.config_copied);
    assert!(contract.initial_stable);
    assert!(contract.initial_frozen);
    assert_eq!(contract.initial, "disconnected");
    assert!(contract.latest_only);
    assert!(contract.close_before_settlement);
    assert!(contract.local_operations);
    assert!(contract.validation_rejected);
    assert!(contract.failure_event);
    assert!(contract.caller_owns_track);
    assert!(contract.no_removed_listener_calls);
    assert_eq!(contract.closed, "disconnected");
    assert!(contract.post_close);
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
        r#"(globalThis.__pulsebeamUnhandledRejections = [], addEventListener("unhandledrejection", (event) => globalThis.__pulsebeamUnhandledRejections.push(event.reason)), null)"#,
    )
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

async fn serve(
    mut stream: TcpStream,
    root: &Path,
    fail_wasm: bool,
    wasm_requests: &AtomicUsize,
) -> io::Result<()> {
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

    if target.ends_with(".wasm") {
        wasm_requests.fetch_add(1, Ordering::Relaxed);
        if fail_wasm {
            return respond(
                &mut stream,
                503,
                "text/plain",
                b"test wasm unavailable",
                method,
            )
            .await;
        }
    }

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

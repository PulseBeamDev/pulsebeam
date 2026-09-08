use std::env;
use std::error::Error;
use std::io;
use std::path::{Component, Path, PathBuf};
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
const UNIFFI_MEDIA_CONTRACT: &str = include_str!("contracts/uniffi-media-contract.js");
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContractResult {
    exports: Vec<String>,
    independent: bool,
    initial_stable: bool,
    initial_frozen: bool,
    initial: String,
    connecting: String,
    connecting_stable: bool,
    null_cancelled: bool,
    defensive_copies: bool,
    close_before_settlement: bool,
    failed: String,
    calls: Vec<String>,
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
                const agent = window.pulsebeam.createAgent();
                const timeout = setTimeout(() => reject(new Error("initialization did not fail")), 5000);
                agent.subscribe(() => {
                    if (agent.getSnapshot().connection !== "failed") return;
                    clearTimeout(timeout);
                    resolve("failed");
                });
                agent.setState({ connection: { roomId: "room", token: "token" } });
            })"#,
        )
        .await?;
        assert_eq!(connection, "failed");
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
    assert!(contract.initial_stable);
    assert!(contract.initial_frozen);
    assert_eq!(contract.initial, "disconnected");
    assert_eq!(contract.connecting, "connecting");
    assert!(contract.connecting_stable);
    assert!(contract.null_cancelled);
    assert!(contract.defensive_copies);
    assert!(contract.close_before_settlement);
    assert_eq!(contract.failed, "failed");
    assert_eq!(
        contract.calls,
        [
            "first", "second", "second", "late", "second", "late", "second", "late", "late"
        ]
    );
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

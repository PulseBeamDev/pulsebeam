#[allow(
    clippy::disallowed_types,
    reason = "test-only UDP observations are shared with owned HTTP handlers, never with production shards"
)]
mod rtp_relay;

use std::env;
use std::error::Error;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
#[allow(
    clippy::disallowed_types,
    reason = "owned live HTTP fixture shares its state across handler tasks"
)]
use std::sync::Arc;
#[allow(
    clippy::disallowed_types,
    reason = "independent fixture WASM request counter, not a multi-atomic snapshot"
)]
use std::sync::atomic::{AtomicUsize, Ordering};

use rtp_relay::Relays;
use serde::de::DeserializeOwned;
use thirtyfour::ChromeCapabilities;
use thirtyfour::bidi::BrowsingContextId;
use thirtyfour::bidi::modules::browsing_context::ReadinessState;
use thirtyfour::prelude::*;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::{JoinHandle, JoinSet};

pub type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

pub struct DestinationServer {
    child: Child,
}
impl DestinationServer {
    #[allow(
        clippy::disallowed_methods,
        reason = "synchronous owned-process startup polling before browser traffic, never a production shard"
    )]
    pub fn start() -> TestResult<Self> {
        if std::net::TcpStream::connect("127.0.0.1:7070").is_ok() {
            return Err("refusing to use an existing PulseBeam listener on 127.0.0.1:7070; browser contracts require an owned destination server".into());
        }
        let binary = env::var_os("PULSEBEAM_SERVER_BINARY").ok_or(
            "missing PULSEBEAM_SERVER_BINARY; run the SDK browser target through ./bazel test",
        )?;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(
            PathBuf::from(
                env::var_os("TEST_UNDECLARED_OUTPUTS_DIR")
                    .ok_or("missing Bazel TEST_UNDECLARED_OUTPUTS_DIR")?,
            )
            .join("destination-server.log"),
        )?;
        let mut child = Command::new(Path::new(&binary).canonicalize()?)
            .arg("--dev")
            .current_dir(env::var_os("TEST_TMPDIR").ok_or("missing Bazel TEST_TMPDIR")?)
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()?;
        for _ in 0..300 {
            if let Some(status) = child.try_wait()? {
                return Err(format!(
                    "owned PulseBeam development server exited before readiness with {status}"
                )
                .into());
            }
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

#[allow(
    clippy::disallowed_types,
    reason = "owned HTTP fixture shares relay state and one independent request counter across handler tasks"
)]
pub struct StaticServer {
    address: String,
    task: JoinHandle<()>,
    wasm_requests: Arc<AtomicUsize>,
}
#[allow(
    clippy::disallowed_types,
    reason = "owned HTTP fixture shares relay state and one independent request counter across handler tasks"
)]
impl StaticServer {
    pub async fn start(root: impl Into<PathBuf>) -> io::Result<Self> {
        Self::start_with_wasm_failure(root, false).await
    }
    pub async fn start_with_wasm_failure(
        root: impl Into<PathBuf>,
        fail_wasm: bool,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let root = root.into();
        let wasm_requests = Arc::new(AtomicUsize::new(0));
        let request_count = Arc::clone(&wasm_requests);
        let relays = Arc::new(Mutex::new(Relays::default()));
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let root = root.clone();
                        let request_count = Arc::clone(&request_count);
                        let relays = Arc::clone(&relays);
                        connections.spawn(async move {
                            let _ = serve(stream, &root, fail_wasm, &request_count, &relays).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self {
            address,
            task,
            wasm_requests,
        })
    }
    pub fn url(&self, path: &str) -> String {
        format!("http://{}/{path}", self.address)
    }
    pub fn wasm_requests(&self) -> usize {
        self.wasm_requests.load(Ordering::Relaxed)
    }
}
impl Drop for StaticServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn capabilities() -> TestResult<ChromeCapabilities> {
    if let Some(binary) = env::var_os("PULSEBEAM_BROWSER_BINARY") {
        if !Path::new(&binary).is_file() {
            return Err(format!(
                "PULSEBEAM_BROWSER_BINARY does not name a readable Chrome/Chromium executable: {}",
                binary.to_string_lossy()
            )
            .into());
        }
    } else {
        return Err(
            "missing PULSEBEAM_BROWSER_BINARY; run the SDK browser target through ./bazel test"
                .into(),
        );
    }
    let mut capabilities = DesiredCapabilities::chrome();
    capabilities.set_headless()?;
    capabilities.set_no_sandbox()?;
    capabilities.set_disable_gpu()?;
    capabilities.add_arg("--enable-logging")?;
    if let Some(outputs) = env::var_os("TEST_UNDECLARED_OUTPUTS_DIR") {
        let thread = std::thread::current();
        let test_name = thread.name().unwrap_or("browser");
        let log_path = Path::new(&outputs).join(format!("chrome-{test_name}.log"));
        capabilities.set_browser_option("logPath", log_path.to_string_lossy())?;
    }
    capabilities.add_arg("--use-fake-device-for-media-stream")?;
    capabilities.add_arg("--use-fake-ui-for-media-stream")?;
    capabilities.enable_bidi()?;
    if let Some(binary) = env::var_os("PULSEBEAM_BROWSER_BINARY") {
        capabilities.set_binary(&Path::new(&binary).canonicalize()?.to_string_lossy())?;
    }
    Ok(capabilities)
}
pub async fn managed_driver(
    capabilities: ChromeCapabilities,
) -> thirtyfour::error::WebDriverResult<WebDriver> {
    let binary = env::var_os("PULSEBEAM_DRIVER_BINARY").ok_or_else(|| {
        thirtyfour::error::WebDriverError::ParseError(
            "missing PULSEBEAM_DRIVER_BINARY; run the SDK browser target through ./bazel test"
                .into(),
        )
    })?;
    let binary = Path::new(&binary)
        .canonicalize()
        .map_err(|error| thirtyfour::error::WebDriverError::ParseError(error.to_string()))?;
    thirtyfour::manager::WebDriverManager::builder()
        .driver_binary(thirtyfour::manager::BrowserKind::Chrome, binary)
        .stdio(thirtyfour::manager::StdioMode::Inherit)
        .offline()
        .build()
        .launch(capabilities)
        .await
}

pub async fn navigate(
    bidi: &thirtyfour::bidi::BiDi,
    context: &BrowsingContextId,
    url: String,
) -> TestResult<()> {
    bidi.browsing_context()
        .navigate(context.clone(), url, Some(ReadinessState::Complete))
        .await?;
    Ok(())
}
pub async fn evaluate_json<T: DeserializeOwned>(
    bidi: &thirtyfour::bidi::BiDi,
    context: &BrowsingContextId,
    expression: &str,
) -> TestResult<T> {
    let result = bidi
        .script()
        .evaluate(context.clone(), expression.to_owned(), true)
        .await?;
    let value = result
        .ok_value()
        .ok_or_else(|| format!("browser expression raised an exception: {result:?}"))?;
    Ok(serde_json::from_value(remote_value(value))?)
}

fn remote_value(value: &serde_json::Value) -> serde_json::Value {
    let Some(kind) = value.get("type").and_then(serde_json::Value::as_str) else {
        return value.clone();
    };
    let inner = value.get("value").unwrap_or(&serde_json::Value::Null);
    match kind {
        "object" => inner.as_array().map_or_else(
            || inner.clone(),
            |entries| {
                serde_json::Value::Object(
                    entries
                        .iter()
                        .filter_map(|entry| {
                            let pair = entry.as_array()?;
                            Some((
                                pair.first()?.as_str()?.to_owned(),
                                remote_value(pair.get(1)?),
                            ))
                        })
                        .collect(),
                )
            },
        ),
        "array" => inner.as_array().map_or_else(
            || inner.clone(),
            |items| serde_json::Value::Array(items.iter().map(remote_value).collect()),
        ),
        "null" | "undefined" => serde_json::Value::Null,
        _ => inner.clone(),
    }
}
#[allow(
    clippy::disallowed_types,
    reason = "borrows the fixture's independent WASM request counter"
)]
async fn serve(
    mut stream: TcpStream,
    root: &Path,
    fail_wasm: bool,
    wasm_requests: &AtomicUsize,
    relays: &Mutex<Relays>,
) -> io::Result<()> {
    let mut request = [0_u8; 16 * 1024];
    let length = stream.read(&mut request).await?;
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
    if let Some(query) = target.strip_prefix("/__test/rtp-relay?") {
        let fields: std::collections::HashMap<_, _> = query
            .split('&')
            .filter_map(|field| field.split_once('='))
            .collect();
        let destination = fields
            .get("destination")
            .and_then(|value| value.parse().ok());
        let extension = fields
            .get("extension")
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|value| *value > 0);
        let (Some(destination), Some(extension)) = (destination, extension) else {
            return respond(
                &mut stream,
                503,
                "text/plain",
                b"invalid relay destination or extension",
                method,
            )
            .await;
        };
        let result = relays.lock().await.allocate(destination, extension).await;
        return respond_relay(&mut stream, result, method).await;
    }
    if let Some(path) = target.strip_prefix("/__test/rtp-relay/") {
        let keys = path
            .split_once('/')
            .and_then(|(id, ssrc)| Some((id.parse::<usize>().ok()?, ssrc.parse::<u32>().ok()?)));
        let Some((id, ssrc)) = keys else {
            return respond(
                &mut stream,
                503,
                "text/plain",
                b"invalid relay observation key",
                method,
            )
            .await;
        };
        let result = relays.lock().await.observation(id, ssrc).await;
        return respond_relay(&mut stream, result, method).await;
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
async fn respond_relay(
    stream: &mut TcpStream,
    result: io::Result<Vec<u8>>,
    method: &str,
) -> io::Result<()> {
    match result {
        Ok(body) => respond(stream, 200, "application/json", &body, method).await,
        Err(error) => {
            respond(
                stream,
                503,
                "text/plain",
                error.to_string().as_bytes(),
                method,
            )
            .await
        }
    }
}

fn static_path(root: &Path, target: &str) -> Option<PathBuf> {
    let relative = Path::new(target.split('?').next()?.trim_start_matches('/'));
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(root.join(if relative.as_os_str().is_empty() {
        Path::new("index.html")
    } else {
        relative
    }))
}
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
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
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    if method != "HEAD" {
        stream.write_all(body).await?;
    }
    stream.shutdown().await
}

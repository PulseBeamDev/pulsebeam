use std::env;
use std::error::Error;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::de::DeserializeOwned;
use thirtyfour::ChromeCapabilities;
use thirtyfour::bidi::BrowsingContextId;
use thirtyfour::bidi::modules::browsing_context::ReadinessState;
use thirtyfour::prelude::*;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

pub type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

pub struct DestinationServer {
    child: Child,
}
impl DestinationServer {
    pub fn start() -> TestResult<Self> {
        if std::net::TcpStream::connect("127.0.0.1:7070").is_ok() {
            return Err("refusing to use an existing PulseBeam listener on 127.0.0.1:7070; browser contracts require an owned destination server".into());
        }
        let package = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let root = package
            .parent()
            .and_then(Path::parent)
            .ok_or("web package must be inside the workspace")?;
        let build = Command::new("cargo")
            .args(["build", "--release", "-p", "pulsebeam"])
            .current_dir(root)
            .output()?;
        if !build.status.success() {
            return Err(format!(
                "failed to build the owned PulseBeam development server:\n{}",
                String::from_utf8_lossy(&build.stderr)
            )
            .into());
        }
        let mut child = Command::new("cargo")
            .args(["run", "--release", "-p", "pulsebeam", "--", "--dev"])
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
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

pub struct StaticServer {
    address: String,
    task: JoinHandle<()>,
    wasm_requests: Arc<AtomicUsize>,
}
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
    } else if !["google-chrome", "chromium", "chromium-browser"]
        .iter()
        .any(|name| {
            Command::new(name)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok()
        })
    {
        return Err("no compatible Chrome/Chromium was found; install one or set PULSEBEAM_BROWSER_BINARY=/path/to/chrome".into());
    }
    let mut capabilities = DesiredCapabilities::chrome();
    capabilities.set_headless()?;
    capabilities.set_no_sandbox()?;
    capabilities.set_disable_gpu()?;
    capabilities.add_arg("--use-fake-device-for-media-stream")?;
    capabilities.add_arg("--use-fake-ui-for-media-stream")?;
    capabilities.enable_bidi()?;
    if let Some(binary) = env::var_os("PULSEBEAM_BROWSER_BINARY") {
        capabilities.set_binary(&binary.to_string_lossy())?;
    }
    Ok(capabilities)
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
async fn serve(
    mut stream: TcpStream,
    root: &Path,
    fail_wasm: bool,
    wasm_requests: &AtomicUsize,
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
    let relative = Path::new(target.split('?').next()?.trim_start_matches('/'));
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
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    if method != "HEAD" {
        stream.write_all(body).await?;
    }
    stream.shutdown().await
}

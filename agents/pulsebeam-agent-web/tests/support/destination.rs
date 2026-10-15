use super::support::TestResult;
use std::{
    env,
    error::Error,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub struct ExecutableServer {
    child: Child,
    _startup_lock: std::fs::File,
}
impl ExecutableServer {
    #[allow(
        clippy::disallowed_methods,
        reason = "synchronous owned-process startup polling before browser traffic, never a production shard"
    )]
    pub fn start() -> TestResult<Self> {
        // Only the production executable has fixed ports. Other owners use bound :0 listeners.
        let startup_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open("/tmp/pulsebeam-production-browser-startup.lock")?;
        startup_lock.lock()?;
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
        let child = Command::new(Path::new(&binary).canonicalize()?)
            .arg("--dev")
            .current_dir(env::var_os("TEST_TMPDIR").ok_or("missing Bazel TEST_TMPDIR")?)
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()?;
        let mut server = Self {
            child,
            _startup_lock: startup_lock,
        };
        for _ in 0..300 {
            if let Some(status) = server.child.try_wait()? {
                return Err(format!(
                    "owned PulseBeam development server exited before readiness with {status}"
                )
                .into());
            }
            if std::net::TcpStream::connect("127.0.0.1:7070").is_ok() {
                return Ok(server);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Err("PulseBeam development server did not listen on port 7070".into())
    }
}
impl Drop for ExecutableServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct DestinationServer {
    address: std::net::SocketAddr,
    shutdown: tokio_util::sync::CancellationToken,
    thread: Option<std::thread::JoinHandle<TestResult<()>>>,
}

impl DestinationServer {
    pub async fn start() -> TestResult<Self> {
        Self::bind("127.0.0.1:0".parse()?).await
    }

    pub async fn bind(address: std::net::SocketAddr) -> TestResult<Self> {
        use pulsebeam_core::auth::{
            DEVELOPMENT_API_KEY_ID, DEVELOPMENT_API_PUBLIC_KEY, DEVELOPMENT_PROJECT_ID,
            KeyRegistry, KeyRegistryEntry,
        };
        let socket = tokio::net::TcpSocket::new_v4()?;
        socket.set_reuseaddr(true)?;
        socket.bind(address)?;
        let listener = socket.listen(128)?;
        let address = listener.local_addr()?;
        let listener = listener.into_std()?;
        let shutdown = tokio_util::sync::CancellationToken::new();
        let cancellation = shutdown.clone();
        let thread = std::thread::spawn(move || -> TestResult<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            tokio::task::LocalSet::new().block_on(&runtime, async move {
                let listener = TcpListener::from_std(listener)?;
                // Metrics installs a process-global recorder; the executable-startup
                // contract owns that boundary, not these restartable in-process nodes.
                pulsebeam::node::NodeBuilder::new()
                    .workers(1)
                    .work_stealing(1)
                    .local_addr("127.0.0.1:0".parse()?)
                    .advertise_bound_udp()
                    .rng(pulsebeam_runtime::rand::os_rng())
                    .with_http_api_listener(listener)
                    .with_key_registry(KeyRegistry::new(
                        *DEVELOPMENT_PROJECT_ID,
                        vec![KeyRegistryEntry {
                            key_id: *DEVELOPMENT_API_KEY_ID,
                            public_key: *DEVELOPMENT_API_PUBLIC_KEY,
                        }],
                    )?)
                    .run(cancellation)
                    .await?;
                Ok(())
            })
        });
        let mut server = Self {
            address,
            shutdown,
            thread: Some(thread),
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if let Some(thread) = server.thread.take_if(|thread| thread.is_finished()) {
                    return match thread.join() {
                        Ok(Err(error)) => Err(error),
                        _ => Err("owned node exited before HTTP readiness".into()),
                    };
                }
                if let Ok(mut stream) = TcpStream::connect(address).await {
                    let ready = async {
                        use tokio::io::AsyncBufReadExt;
                        stream
                            .write_all(
                                b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                            )
                            .await?;
                        let mut status = String::new();
                        tokio::io::BufReader::new(stream).read_line(&mut status).await?;
                        Ok::<_, std::io::Error>(status.starts_with("HTTP/1."))
                    };
                    if matches!(tokio::time::timeout(std::time::Duration::from_secs(1), ready).await, Ok(Ok(true))) {
                        return Ok::<_, Box<dyn Error + Send + Sync>>(());
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await??;
        Ok(server)
    }

    pub fn address(&self) -> std::net::SocketAddr {
        self.address
    }
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for DestinationServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

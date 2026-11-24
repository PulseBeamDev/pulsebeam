use super::support::TestResult;
use std::{error::Error, net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub struct DestinationServer {
    address: SocketAddr,
    metrics_address: Option<SocketAddr>,
    shutdown: tokio_util::sync::CancellationToken,
    thread: Option<std::thread::JoinHandle<TestResult<()>>>,
}

impl DestinationServer {
    pub async fn start() -> TestResult<Self> {
        Self::bind("127.0.0.1:0".parse()?).await
    }

    pub async fn start_observed(
        configure: impl FnOnce(
            pulsebeam::node::NodeBuilder,
            SocketAddr,
        ) -> TestResult<pulsebeam::node::NodeBuilder>
        + Send
        + 'static,
    ) -> TestResult<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        Self::bind_with_metrics("127.0.0.1:0".parse()?, Some(listener), configure).await
    }

    pub async fn bind(address: SocketAddr) -> TestResult<Self> {
        Self::bind_with_metrics(address, None, |node, _| Ok(node)).await
    }

    async fn bind_with_metrics(
        address: SocketAddr,
        metrics: Option<std::net::TcpListener>,
        configure: impl FnOnce(
            pulsebeam::node::NodeBuilder,
            SocketAddr,
        ) -> TestResult<pulsebeam::node::NodeBuilder>
        + Send
        + 'static,
    ) -> TestResult<Self> {
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
        let metrics_address = metrics
            .as_ref()
            .map(std::net::TcpListener::local_addr)
            .transpose()?;
        let shutdown = tokio_util::sync::CancellationToken::new();
        let cancellation = shutdown.clone();
        let utc_policy = std::env::var_os("PULSEBEAM_TEST_UTC_POLICY").map_or_else(
            || std::path::Path::new(file!()).with_file_name("leap-seconds.list"),
            std::path::PathBuf::from,
        );
        let thread = std::thread::spawn(move || -> TestResult<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            tokio::task::LocalSet::new().block_on(&runtime, async move {
                let listener = TcpListener::from_std(listener)?;
                let mut node = pulsebeam::node::NodeBuilder::new()
                    .workers(1)
                    .work_stealing(1)
                    .local_addr("127.0.0.1:0".parse()?)
                    .advertise_bound_udp()
                    .host_utc_policy(utc_policy, false)
                    .rng(pulsebeam_runtime::rand::os_rng())
                    .with_http_api_listener(listener)
                    .with_key_registry(KeyRegistry::new(
                        *DEVELOPMENT_PROJECT_ID,
                        vec![KeyRegistryEntry {
                            key_id: *DEVELOPMENT_API_KEY_ID,
                            public_key: *DEVELOPMENT_API_PUBLIC_KEY,
                        }],
                    )?);
                // Only the admission witness opens this listener. Restartable nodes
                // do not install the process-global exporter again.
                if let Some(metrics) = metrics {
                    node = node.with_internal_metrics_listener(TcpListener::from_std(metrics)?);
                }
                configure(node, address)?.run(cancellation).await?;
                Ok(())
            })
        });
        let mut server = Self {
            address,
            metrics_address,
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

    pub async fn wait_for_fabric_rtp_admissions(&self) -> TestResult<()> {
        let address = self
            .metrics_address
            .ok_or("no destination metrics listener")?;
        tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                let mut stream = TcpStream::connect(address).await?;
                stream
                    .write_all(b"GET /metrics HTTP/1.0\r\nHost: localhost\r\n\r\n")
                    .await?;
                let mut response = String::new();
                stream.read_to_string(&mut response).await?;
                let admitted = |prefix: &str| {
                    response.lines().any(|line| {
                        line.strip_prefix(prefix)
                            .and_then(|value| value.trim().parse::<u64>().ok())
                            .is_some_and(|count| count > 0)
                    })
                };
                if admitted("pulsebeam_shard_fabric_rtp_branch_admissions{profile=\"h264\"}")
                    && admitted("pulsebeam_shard_fabric_rtp_branch_admissions{profile=\"opus\"}")
                {
                    return Ok::<_, Box<dyn Error + Send + Sync>>(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(
            |_| "no committed H.264 and Opus Fabric RTP branches in the connected browser smoke",
        )??;
        Ok(())
    }

    pub fn address(&self) -> SocketAddr {
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

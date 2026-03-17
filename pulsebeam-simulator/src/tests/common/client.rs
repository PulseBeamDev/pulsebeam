use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use pulsebeam_agent::actor::{Agent, AgentBuilder, AgentEvent, AgentStats};
use pulsebeam_agent::api::HttpApiClient;
use pulsebeam_agent::media::H264Looper;
use pulsebeam_agent::{MediaKind, SimulcastLayer, TransceiverDirection};
use pulsebeam_core::net::UdpSocket;
use pulsebeam_core::net::{AsyncHttpClient, HttpError, HttpRequest, HttpResult};
use std::net::IpAddr;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub struct SimClientBuilder {
    ip: IpAddr,
    agent_builder: AgentBuilder,
}

impl SimClientBuilder {
    pub async fn bind(ip: IpAddr, server_ip: IpAddr) -> anyhow::Result<Self> {
        let client = create_http_client();
        let server_base_uri = format!("http://{}:3000", server_ip);
        let api = HttpApiClient::new(client, &server_base_uri)?;

        let socket = UdpSocket::bind("0.0.0.0:0").await?;

        Ok(Self {
            ip,
            agent_builder: AgentBuilder::new(api, socket).with_local_ip(ip),
        })
    }

    pub fn with_track(
        mut self,
        kind: MediaKind,
        dir: TransceiverDirection,
        simulcast_layers: Option<Vec<SimulcastLayer>>,
    ) -> Self {
        self.agent_builder = self.agent_builder.with_track(kind, dir, simulcast_layers);
        self
    }

    pub async fn connect(self, room: &str) -> anyhow::Result<SimClient> {
        let agent = self.agent_builder.connect(room).await?;
        tracing::info!("connected to {room}");
        Ok(SimClient {
            ip: self.ip,
            participant_id: agent.participant_id().to_string(),
            agent,
            local_mids: Vec::new(),
            discovered_tracks: Vec::new(),
            remote_tracks: Vec::new(),
            join_set: JoinSet::new(),
        })
    }
}

pub struct SimClient {
    pub ip: IpAddr,
    pub agent: Agent,
    /// The participant ID assigned by the server for this client.
    pub participant_id: String,
    /// Local track mids (as reported by LocalTrackAdded events).
    pub local_mids: Vec<pulsebeam_agent::str0m::media::Mid>,
    /// Remote track IDs that have been discovered from signaling updates.
    pub discovered_tracks: Vec<String>,
    /// Remote tracks that have been assigned to a slot and are actively streaming.
    pub remote_tracks: Vec<(pulsebeam_agent::str0m::media::Mid, String)>,
    join_set: JoinSet<()>,
}

impl SimClient {
    pub async fn drive(&mut self, token: CancellationToken) -> anyhow::Result<AgentStats> {
        self.drive_until_cancelled(token, |_| false).await
    }

    pub async fn drive_until_cancelled<F>(
        &mut self,
        token: CancellationToken,
        mut predicate: F,
    ) -> anyhow::Result<AgentStats>
    where
        F: FnMut(&AgentStats) -> bool,
    {
        let mut check_interval = tokio::time::interval(Duration::from_millis(200));
        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    let stats = self.agent.get_stats().await.unwrap_or_default();
                    return Ok(stats);
                }
                Some(event) = self.agent.next_event() => {
                    match event {
                        AgentEvent::LocalTrackAdded(sender) => {
                            tracing::info!("{} starting publisher for mid: {:?} rid: {:?}", self.ip, sender.mid, sender.rid);
                            self.local_mids.push(sender.mid);
                            let looper = create_h264_looper_for_rid(sender.rid.as_ref().map(|r| r.as_ref()));
                            self.join_set.spawn(looper.run(sender));
                        }
                        AgentEvent::RemoteTrackDiscovered(track) => {
                            tracing::info!("{} discovered remote track: {:?}", self.ip, track.id);
                            if !self.discovered_tracks.contains(&track.id) {
                                self.discovered_tracks.push(track.id.clone());
                            }
                        }
                        AgentEvent::RemoteTrackAdded(recv) => {
                            tracing::info!("{} subscribed to remote track: {:?}", self.ip, recv.track.id);
                            self.remote_tracks.push((recv.mid, recv.track.id.clone()));
                        }
                        _ => {}
                    }
                }
                _ = check_interval.tick() => {
                    if let Some(stats) = self.agent.get_stats().await && predicate(&stats) {
                        return Ok(stats);
                    }
                }
            }
        }
    }

    pub async fn drive_until<F>(
        &mut self,
        timeout: Duration,
        predicate: F,
    ) -> anyhow::Result<AgentStats>
    where
        F: FnMut(&AgentStats) -> bool,
    {
        let token = CancellationToken::new();
        let _guard = token.clone().drop_guard();
        tokio::select! {
            _ = tokio::time::sleep(timeout) => {
                let stats = self.agent.get_stats().await.unwrap_or_default();
                anyhow::bail!(
                    "Client {} timed out. Final Stats:\n{:?}\nDiscovered: {:?}\nRemoteTracks: {:?}",
                    self.ip,
                    stats,
                    self.discovered_tracks,
                    self.remote_tracks
                );
            }
            result = self.drive_until_cancelled(token, predicate) => result
        }
    }

    pub async fn wait_for_remote_tracks(
        &mut self,
        count: usize,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        // Use wall-clock time so this timeout isn't affected by simulated tokio time.
        let start = std::time::Instant::now();
        let mut check_interval = tokio::time::interval(Duration::from_millis(200));

        loop {
            if self.remote_tracks.len() >= count {
                return Ok(());
            }

            // As a fallback, consider a track "received" if we see actual rx bytes in stats.
            // This makes the test more robust to transient event delivery or ordering issues.
            if let Some(stats) = self.agent.get_stats().await {
                let rx_tracks = stats
                    .tracks
                    .values()
                    .filter(|t| t.rx_layers.values().any(|l| l.bytes > 0))
                    .count();
                if rx_tracks >= count {
                    tracing::info!(
                        "{} observed {} tracks with inbound bytes (fallback)",
                        self.ip,
                        rx_tracks
                    );
                    return Ok(());
                }
            }

            if start.elapsed() > timeout {
                let stats = self.agent.get_stats().await.unwrap_or_default();
                anyhow::bail!(
                    "Client {} timed out waiting for {} remote tracks. Got {:?} / {:?}. Final Stats:\n{:?}",
                    self.ip,
                    count,
                    self.discovered_tracks,
                    self.remote_tracks,
                    stats
                );
            }

            tokio::select! {
                Some(event) = self.agent.next_event() => {
                    match event {
                        AgentEvent::LocalTrackAdded(sender) => {
                            tracing::info!("{} starting publisher for mid: {:?} rid: {:?}", self.ip, sender.mid, sender.rid);
                            let looper = create_h264_looper_for_rid(sender.rid.as_ref().map(|r| r.as_ref()));
                            self.join_set.spawn(looper.run(sender));
                        }
                        AgentEvent::RemoteTrackDiscovered(track) => {
                            tracing::info!("{} discovered remote track: {:?}", self.ip, track.id);
                            if !self.discovered_tracks.contains(&track.id) {
                                self.discovered_tracks.push(track.id.clone());
                            }
                        }
                        AgentEvent::RemoteTrackAdded(recv) => {
                            tracing::info!("{} subscribed to remote track: {:?}", self.ip, recv.track.id);
                            self.remote_tracks
                                .push((recv.mid, recv.track.id.clone()));
                        }
                        _ => {}
                    }
                }
                _ = check_interval.tick() => {}
            }
        }
    }

    pub async fn wait_for_discovered_tracks(
        &mut self,
        count: usize,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let start = std::time::Instant::now();
        let mut check_interval = tokio::time::interval(Duration::from_millis(200));

        loop {
            if self.discovered_tracks.len() >= count {
                return Ok(());
            }

            if start.elapsed() > timeout {
                let stats = self.agent.get_stats().await.unwrap_or_default();
                anyhow::bail!(
                    "Client {} timed out waiting for {} discovered tracks. Got {:?}. Final Stats:\n{:?}",
                    self.ip,
                    count,
                    self.discovered_tracks,
                    stats
                );
            }

            tokio::select! {
                Some(event) = self.agent.next_event() => {
                    match event {
                        AgentEvent::LocalTrackAdded(sender) => {
                            tracing::info!("{} starting publisher for mid: {:?} rid: {:?}", self.ip, sender.mid, sender.rid);
                            self.local_mids.push(sender.mid);
                            let looper = create_h264_looper_for_rid(sender.rid.as_ref().map(|r| r.as_ref()));
                            self.join_set.spawn(looper.run(sender));
                        }
                        AgentEvent::RemoteTrackDiscovered(track) => {
                            tracing::info!("{} discovered remote track: {:?}", self.ip, track.id);
                            if !self.discovered_tracks.contains(&track.id) {
                                self.discovered_tracks.push(track.id.clone());
                            }
                        }
                        AgentEvent::RemoteTrackAdded(recv) => {
                            tracing::info!("{} subscribed to remote track: {:?}", self.ip, recv.track.id);
                            self.remote_tracks
                                .push((recv.mid, recv.track.id.clone()));
                        }
                        _ => {}
                    }
                }
                _ = check_interval.tick() => {}
            }
        }
    }
}

pub fn create_http_client() -> Box<dyn AsyncHttpClient> {
    let client = Client::builder(TokioExecutor::new()).build(connector::connector());
    let client = HyperClientWrapper(client);
    Box::new(client)
}

pub fn create_h264_looper_for_rid(rid: Option<&str>) -> H264Looper {
    let data = match rid {
        Some("f") => pulsebeam_testdata::RAW_H264_FULL,
        Some("h") => pulsebeam_testdata::RAW_H264_HALF,
        Some("q") | _ => pulsebeam_testdata::RAW_H264_QUARTER,
    };
    H264Looper::new(data, 30)
}

pub struct HyperClientWrapper<C>(pub Client<C, Full<Bytes>>);

impl<C> AsyncHttpClient for HyperClientWrapper<C>
where
    // These bounds are required for Hyper to actually send a request
    C: tower::Service<http::Uri> + Clone + Send + Sync + 'static,
    C::Response: hyper::rt::Read
        + hyper::rt::Write
        + hyper_util::client::legacy::connect::Connection
        + Send
        + Unpin,
    C::Future: Send + Unpin,
    C::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    fn execute(&self, req: HttpRequest) -> HttpResult<'_> {
        let client = self.0.clone();

        Box::pin(async move {
            // 1. Convert http::Request<Vec<u8>> -> http::Request<Full<Bytes>>
            let (parts, body) = req.into_parts();
            let hyper_req = http::Request::from_parts(parts, Full::new(Bytes::from(body)));

            // 2. Execute via Hyper
            let res = client
                .request(hyper_req)
                .await
                .map_err(|e| Box::new(e) as HttpError)?;

            // 3. Buffer the streaming body back into a Vec<u8>
            let (parts, res_body) = res.into_parts();
            let bytes = res_body
                .collect()
                .await
                .map_err(|e| Box::new(e) as HttpError)?
                .to_bytes();

            Ok(http::Response::from_parts(parts, bytes.to_vec()))
        })
    }
}

mod connector {
    use hyper::Uri;
    use pin_project_lite::pin_project;
    use std::{future::Future, io::Error, pin::Pin};
    use tokio::io::AsyncWrite;
    use tower::Service;
    use turmoil::net::TcpStream;

    type Fut = Pin<Box<dyn Future<Output = Result<TurmoilConnection, Error>> + Send>>;

    pub fn connector()
    -> impl Service<Uri, Response = TurmoilConnection, Error = Error, Future = Fut> + Clone {
        tower::service_fn(|uri: Uri| {
            Box::pin(async move {
                let conn = TcpStream::connect(uri.authority().unwrap().as_str()).await?;
                Ok::<_, Error>(TurmoilConnection { fut: conn })
            }) as Fut
        })
    }

    pin_project! {
        pub struct TurmoilConnection{
            #[pin]
            fut: turmoil::net::TcpStream
        }
    }

    impl hyper::rt::Read for TurmoilConnection {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            mut buf: hyper::rt::ReadBufCursor<'_>,
        ) -> std::task::Poll<Result<(), Error>> {
            let n = unsafe {
                let mut tbuf = tokio::io::ReadBuf::uninit(buf.as_mut());
                let result = tokio::io::AsyncRead::poll_read(self.project().fut, cx, &mut tbuf);
                match result {
                    std::task::Poll::Ready(Ok(())) => tbuf.filled().len(),
                    other => return other,
                }
            };

            unsafe {
                buf.advance(n);
            }
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl hyper::rt::Write for TurmoilConnection {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<Result<usize, Error>> {
            Pin::new(&mut self.fut).poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Error>> {
            Pin::new(&mut self.fut).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Error>> {
            Pin::new(&mut self.fut).poll_shutdown(cx)
        }
    }

    impl hyper_util::client::legacy::connect::Connection for TurmoilConnection {
        fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
            hyper_util::client::legacy::connect::Connected::new()
        }
    }
}

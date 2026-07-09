use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use pulsebeam_agent::actor::{AgentBuilder, AgentEvent};
use pulsebeam_agent::api::HttpApiClient;
use pulsebeam_agent::media::H264Looper;
use pulsebeam_agent::{AgentDriver, MediaKind, SimulcastLayer, TransceiverDirection};
use pulsebeam_core::net::UdpSocket;
use pulsebeam_core::net::{AsyncHttpClient, HttpError, HttpRequest, HttpResult};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::net::IpAddr;
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

pub struct SimClientBuilder {
    ip: IpAddr,
    agent_builder: AgentBuilder,
}

impl SimClientBuilder {
    pub async fn bind(ip: IpAddr, server_ip: IpAddr) -> anyhow::Result<Self> {
        let client = create_http_client();
        let server_base_uri = format!("http://{}:7070", server_ip);
        let api = HttpApiClient::new(client, &server_base_uri)?;

        let socket = UdpSocket::bind("0.0.0.0:0").await?;

        Ok(Self {
            ip,
            agent_builder: AgentBuilder::new(api, socket).with_local_ip(ip),
        })
    }

    /// Like `bind` but also configures a TCP active stream to the server's ICE
    /// port (3478).  Use with `start_sfu_node_tcp_only` to test TCP connectivity.
    pub async fn bind_tcp(ip: IpAddr, server_ip: IpAddr) -> anyhow::Result<Self> {
        let client = create_http_client();
        let server_base_uri = format!("http://{}:7070", server_ip);
        let api = HttpApiClient::new(client, &server_base_uri)?;

        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let server_tcp_addr: std::net::SocketAddr = format!("{}:3478", server_ip).parse()?;

        Ok(Self {
            ip,
            agent_builder: AgentBuilder::new(api, socket)
                .with_local_ip(ip)
                .with_tcp_server_addr(server_tcp_addr),
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
        let driver = self.agent_builder.connect(room).await?;
        tracing::info!("connected to {room}");
        let ctx = ClientContext {
            ip: self.ip,
            driver,
            local_mids: HashSet::new(),
            discovered_tracks: HashSet::new(),
            remote_tracks: HashMap::new(),
        };
        Ok(SimClient {
            ctx,
            join_set: JoinSet::new(),
        })
    }
}

pub struct ClientContext {
    pub ip: IpAddr,
    pub driver: AgentDriver,

    /// Local track mids (as reported by LocalTrackAdded events).
    pub local_mids: HashSet<pulsebeam_agent::str0m::media::Mid>,
    /// Remote track IDs that have been discovered from signaling updates.
    pub discovered_tracks: HashSet<String>,
    /// Remote tracks that have been assigned to a slot and are actively streaming.
    pub remote_tracks: HashMap<pulsebeam_agent::str0m::media::Mid, String>,
}

pub struct SimClient {
    pub ctx: ClientContext,
    join_set: JoinSet<()>,
}

impl SimClient {
    pub async fn drive(&mut self, token: CancellationToken) -> anyhow::Result<()> {
        self.drive_until_cancelled(token, |_| false).await
    }

    pub async fn drive_for(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let token = CancellationToken::new();
        let mut driver = Box::pin(self.drive_until_cancelled(token.clone(), |_| false));

        tokio::select! {
            _ = tokio::time::sleep(timeout) => {
                token.cancel();
            }
            res = &mut driver => {
                return res;
            }
        }

        driver.await
    }

    pub async fn drive_until<F>(&mut self, timeout: Duration, predicate: F) -> anyhow::Result<()>
    where
        F: FnMut(&ClientContext) -> bool,
    {
        let token = CancellationToken::new();
        let _guard = token.clone().drop_guard();
        tokio::select! {
            _ = tokio::time::sleep(timeout) => {
                let stats = self.ctx.driver.stats();
                anyhow::bail!(
                    "Client {} timed out ({:?}). Final Stats:\n{:?}\nDiscovered: {:?}\nRemoteTracks: {:?}",
                    self.ctx.ip,
                    timeout,
                    stats,
                    self.ctx.discovered_tracks,
                    self.ctx.remote_tracks
                );
            }
            result = self.drive_until_cancelled(token, predicate) => result
        }
    }

    pub async fn drive_with<F>(&mut self, predicate: F) -> anyhow::Result<()>
    where
        F: FnMut(&ClientContext) -> bool,
    {
        self.drive_until_cancelled(CancellationToken::new(), predicate)
            .await
    }

    pub async fn drive_until_cancelled<F>(
        &mut self,
        token: CancellationToken,
        mut predicate: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(&ClientContext) -> bool,
    {
        let span = tracing::info_span!("drive_until_cancelled", ip = %self.ctx.ip, participant_id = %self.ctx.driver.participant_id());
        async move {
            let mut check_interval = tokio::time::interval(Duration::from_millis(200));
            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        return Ok(());
                    }
                    Some(event) = self.ctx.driver.poll() => {
                        match event {
                            AgentEvent::LocalTrackAdded(sender) => {
                                tracing::info!("{} starting publisher for mid: {:?} rid: {:?}", self.ctx.ip, sender.mid, sender.rid);
                                self.ctx.local_mids.insert(sender.mid);
                                let looper = create_h264_looper_for_rid(sender.rid.as_ref().map(|r| r.as_ref()));
                                self.join_set.spawn(looper.run(sender));
                            }
                            AgentEvent::RemoteTrackDiscovered(track) => {
                                tracing::info!("{} discovered remote track: {:?}", self.ctx.ip, track.id);
                                if !self.ctx.discovered_tracks.contains(&track.id) {
                                    self.ctx.discovered_tracks.insert(track.id.clone());
                                }
                            }
                            AgentEvent::RemoteTrackAdded {mid, track} => {
                                tracing::info!("{} subscribed to remote track: {:?}", self.ctx.ip, track.id);
                                self.ctx.remote_tracks.insert(mid, track.id.clone());
                            }
                            _ => {}
                        }

                        // Re-check the predicate after processing an event, since a new
                        // event may indicate the desired state has been reached.
                        if predicate(&self.ctx) {
                            return Ok(());
                        }
                    }
                    _ = check_interval.tick() => {
                        if predicate(&self.ctx) {
                            return Ok(());
                        }
                    }
                }
            }
        }.instrument(span).await
    }
}

pub fn create_http_client() -> Box<dyn AsyncHttpClient> {
    let client = Client::builder(TokioExecutor::new()).build(connector::connector());
    let client = HyperClientWrapper(client);
    Box::new(client)
}

pub fn create_h264_looper_for_rid(rid: Option<&str>) -> H264Looper {
    let data = match rid {
        Some("f") => pulsebeam_testdata::RAW_H264_FULL_CBR,
        Some("h") => pulsebeam_testdata::RAW_H264_HALF_CBR,
        Some("q") | _ => pulsebeam_testdata::RAW_H264_QUARTER_CBR,
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
            // Use a stack buffer for reads to avoid unsafe operations on the
            // underlying `ReadBufCursor`. This avoids UB while allowing compatibility
            // with Hyper's legacy runtime traits.
            let mut temp = [0u8; 8192];
            let mut tbuf = tokio::io::ReadBuf::new(&mut temp);

            match tokio::io::AsyncRead::poll_read(self.project().fut, cx, &mut tbuf) {
                std::task::Poll::Ready(Ok(())) => {
                    let n = tbuf.filled().len();
                    if n > 0 {
                        buf.put_slice(tbuf.filled());
                    }
                    std::task::Poll::Ready(Ok(()))
                }
                other => other,
            }
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

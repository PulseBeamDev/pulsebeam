use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use pulsebeam_agent::actor::{Agent, AgentBuilder, AgentEvent, AgentStats};
use pulsebeam_agent::api::HttpApiClient;
use pulsebeam_agent::media::H264Looper;
use pulsebeam_agent::{MediaKind, TransceiverDirection};
use pulsebeam_core::net::UdpSocket;
use pulsebeam_core::net::{AsyncHttpClient, HttpError, HttpRequest, HttpResult};
use std::net::IpAddr;
use std::time::Duration;
use tokio::task::JoinSet;

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

    pub fn with_track(mut self, kind: MediaKind, dir: TransceiverDirection) -> Self {
        self.agent_builder = self.agent_builder.with_track(kind, dir, None);
        self
    }

    pub async fn connect(self, room: &str) -> anyhow::Result<SimClient> {
        let agent = self.agent_builder.connect(room).await?;
        tracing::info!("connected to {room}");
        Ok(SimClient {
            ip: self.ip,
            agent,
            join_set: JoinSet::new(),
        })
    }
}

pub struct SimClient {
    pub ip: IpAddr,
    pub agent: Agent,
    join_set: JoinSet<()>,
}

impl SimClient {
    pub async fn drive_until<F>(
        &mut self,
        timeout: Duration,
        mut predicate: F,
    ) -> anyhow::Result<AgentStats>
    where
        F: FnMut(&AgentStats) -> bool,
    {
        let start = tokio::time::Instant::now();
        let mut check_interval = tokio::time::interval(Duration::from_millis(200));

        loop {
            if start.elapsed() > timeout {
                let stats = self.agent.get_stats().await.unwrap_or_default();
                anyhow::bail!("Client {} timed out. Final Stats:\n{:?}", self.ip, stats);
            }

            tokio::select! {
                Some(event) = self.agent.next_event() => {
                    match event {
                        AgentEvent::LocalTrackAdded(sender) => {
                            tracing::info!("{} starting publisher for mid: {:?}", self.ip, sender.mid);
                            let looper = create_h264_looper(30);
                            self.join_set.spawn(looper.run(sender));
                        }
                        AgentEvent::RemoteTrackAdded(recv) => {
                            tracing::info!("{} subscribed to remote track: {:?}", self.ip, recv.track.id);
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
}

pub fn create_http_client() -> Box<dyn AsyncHttpClient> {
    let client = Client::builder(TokioExecutor::new()).build(connector::connector());
    let client = HyperClientWrapper(client);
    Box::new(client)
}

pub fn create_h264_looper(fps: u32) -> H264Looper {
    H264Looper::new(pulsebeam_testdata::RAW_H264_QUARTER, fps)
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

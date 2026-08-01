use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use pulsebeam_agent::actor::AgentBuilder;
use pulsebeam_agent::agent::{
    DataPublisher, DataSubscriber, OrderedTopicPublisher, OrderedTopicSubscriber,
};
use pulsebeam_agent::api::HttpApiClient;
use pulsebeam_agent::media::{H264Looper, VbrLooper, VbrProfile};
use pulsebeam_agent::{
    Agent, LocalTrack, ParticipantChange, Participants, RemoteTrack, SimulcastLayer,
};
use pulsebeam_core::net::UdpSocket;
use pulsebeam_core::net::{AsyncHttpClient, HttpError, HttpRequest, HttpResult};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

pub struct SimClientBuilder {
    ip: IpAddr,
    agent_builder: AgentBuilder,
    video_rx: Option<Arc<Mutex<VideoReceiveLog>>>,
    publishes_video: bool,
    /// When set, publish with a variable-bitrate source instead of the constant-rate looper.
    vbr_profile: Option<VbrProfile>,
}

fn http_base_uri(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("http://{}:{}", v4, port),
        IpAddr::V6(v6) => format!("http://[{}]:{}", v6, port),
    }
}

impl SimClientBuilder {
    pub async fn bind(ip: IpAddr, server_ip: IpAddr) -> anyhow::Result<Self> {
        let client = create_http_client();
        let server_base_uri = http_base_uri(server_ip, 7070);
        let api = HttpApiClient::new(client, &server_base_uri)?;

        let socket = UdpSocket::bind("0.0.0.0:0").await?;

        Ok(Self {
            ip,
            agent_builder: AgentBuilder::new(api, socket).with_local_ip(ip),
            video_rx: None,
            publishes_video: false,
            vbr_profile: None,
        })
    }

    /// Like `bind` but also configures a TCP active stream to the server's ICE
    /// port (3478).  Use with `start_sfu_node_tcp_only` to test TCP connectivity.
    pub async fn bind_tcp(ip: IpAddr, server_ip: IpAddr) -> anyhow::Result<Self> {
        let client = create_http_client();
        let server_base_uri = http_base_uri(server_ip, 7070);
        let api = HttpApiClient::new(client, &server_base_uri)?;

        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let server_tcp_addr = std::net::SocketAddr::new(server_ip, 3478);

        Ok(Self {
            ip,
            agent_builder: AgentBuilder::new(api, socket)
                .with_local_ip(ip)
                .with_tcp_server_addr(server_tcp_addr),
            video_rx: None,
            publishes_video: false,
            vbr_profile: None,
        })
    }

    pub fn publish_video(mut self, simulcast_layers: Option<Vec<SimulcastLayer>>) -> Self {
        self.agent_builder = self.agent_builder.video_upstream_slots(1, simulcast_layers);
        self.publishes_video = true;
        self
    }

    /// Publish with a VBR source (see [`VbrProfile`]) rather than the constant-rate looper.
    pub fn with_vbr(mut self, profile: VbrProfile) -> Self {
        self.vbr_profile = Some(profile);
        self
    }

    pub fn receive_video(mut self, capacity: usize) -> Self {
        self.agent_builder = self.agent_builder.video_downstream_slots(capacity);
        self
    }

    /// Inject a shared `VideoReceiveLog` so the harness can read it externally.
    /// If not called, `connect()` allocates a private one.
    pub fn with_video_rx(mut self, rx: Arc<Mutex<VideoReceiveLog>>) -> Self {
        self.video_rx = Some(rx);
        self
    }

    pub async fn connect(self, room: &str) -> anyhow::Result<SimClient> {
        let (agent, runner) = self.agent_builder.connect_unmanaged(room).await?;
        let mut join_set = JoinSet::new();
        join_set.spawn(async move {
            runner.run().await.expect("agent runner failed");
        });
        let local_video = if self.publishes_video {
            Some(agent.media().publish_video().await?)
        } else {
            None
        };
        let (incoming_track_tx, incoming_tracks) = tokio::sync::mpsc::channel(32);
        let participants = agent.participants();
        tracing::info!("connected to {room}");
        let video_rx = self
            .video_rx
            .unwrap_or_else(|| Arc::new(Mutex::new(VideoReceiveLog::default())));
        let ctx = ClientContext {
            ip: self.ip,
            agent,
            incoming_tracks,
            incoming_track_tx,
            participants,
            discovered_tracks: HashSet::new(),
            published_topics: Arc::new(Mutex::new(HashMap::new())),
            subscribed_topics: Arc::new(Mutex::new(HashMap::new())),
            ordered_publishers: Arc::new(Mutex::new(HashMap::new())),
            ordered_subscribers: Arc::new(Mutex::new(HashMap::new())),
            remote_tracks: HashMap::new(),
            requested_tracks: HashSet::new(),
            received_data: Vec::new(),
            video_rx,
            local_publications: local_video.into_iter().collect(),
        };
        for publication in &ctx.local_publications {
            for sender in publication.encodings().iter().cloned() {
                let rid = sender.rid();
                match self.vbr_profile {
                    Some(profile) => {
                        let looper = create_vbr_looper_for_rid(rid, profile);
                        join_set.spawn(looper.run(sender));
                    }
                    None => {
                        let looper = create_h264_looper_for_rid(rid);
                        join_set.spawn(looper.run(sender));
                    }
                }
            }
        }
        Ok(SimClient { ctx, join_set })
    }
}

/// What the subscriber's depacketizer made of the stream the SFU sent it.
///
/// `contiguous` is str0m's own reassembly verdict: it is false whenever a frame
/// was preceded by a sequence-number hole, which is exactly what a botched
/// switch produces. `is_keyframe` lets a test assert that each switch actually
/// delivered a decodable entry point.
#[derive(Default, Debug, Clone)]
pub struct VideoReceiveLog {
    pub frames: u64,
    pub keyframes: u64,
    pub non_contiguous: u64,
    /// Frames whose RTP timestamp had already been used by an earlier frame.
    /// str0m re-emits a frame when a retransmission for it lands after it was
    /// already delivered, so this is bounded by the NACK count rather than zero.
    pub duplicate_ts_frames: u64,
    /// Number of backwards RTP-timestamp jumps seen. Ordinary reordering may
    /// cause one; a broken simulcast switch causes one per botched transition.
    pub ts_regression_count: u64,
    /// Largest backwards jump in RTP time, in 90kHz ticks.
    pub max_ts_regression: u64,
    /// Keyframes that arrived without SPS+PPS in the same picture. The decoder
    /// cannot render these: the SFU keeps one egress SSRC across switches while
    /// every simulcast layer has its own SPS.
    pub missing_parameter_sets: u64,
    pub bytes: u64,
    last_ts: Option<u64>,
    seen_ts: HashSet<u64>,
}

/// Scans an Annex-B frame for the H.264 NAL unit types it contains, using the
/// same `pulsebeam_core::h264::classify()` classifier as the production SFU forwarder.
fn annexb_nalu_types(data: &[u8]) -> Vec<pulsebeam_core::h264::NalFlags> {
    let mut flags = Vec::new();
    let mut i = 0usize;
    while i + 3 < data.len() {
        let short = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        let long = i + 3 < data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1;
        if short || long {
            let start = i + if short { 3 } else { 4 };
            if start < data.len() {
                flags.push(pulsebeam_core::h264::classify(&data[start..]));
            }
            i = start + 1;
        } else {
            i += 1;
        }
    }
    flags
}

impl VideoReceiveLog {
    fn record(&mut self, frame: &pulsebeam_agent::MediaFrame) {
        self.frames += 1;
        self.bytes += frame.data.len() as u64;
        if frame.is_keyframe {
            self.keyframes += 1;
            let nalus = annexb_nalu_types(&frame.data);
            let has_sps = nalus.iter().any(|f| f.sps());
            let has_pps = nalus.iter().any(|f| f.pps());
            if !has_sps || !has_pps {
                self.missing_parameter_sets += 1;
            }
        }
        if !frame.contiguous {
            self.non_contiguous += 1;
        }
        let ts = frame.ts.numer();
        if !self.seen_ts.insert(ts) {
            self.duplicate_ts_frames += 1;
        }
        if let Some(prev) = self.last_ts
            && ts < prev
        {
            let delta = prev - ts;
            self.max_ts_regression = self.max_ts_regression.max(delta);
            // Only count as a "switch regression" if the jump is small (< 1s at 90kHz).
            // Video loops in test data cause large backwards jumps (~entire clip duration)
            // which are not stream quality issues.
            if delta < 90_000 {
                self.ts_regression_count += 1;
            }
        }
        self.last_ts = Some(ts);
    }
}

pub struct ClientContext {
    pub ip: IpAddr,
    pub agent: Agent,
    incoming_tracks: tokio::sync::mpsc::Receiver<RemoteTrack>,
    pub(crate) incoming_track_tx: tokio::sync::mpsc::Sender<RemoteTrack>,
    participants: Participants,
    /// Aggregated decode-side view of every remote video track.
    pub video_rx: Arc<Mutex<VideoReceiveLog>>,
    local_publications: Vec<LocalTrack>,

    /// Remote track IDs that have been discovered from signaling updates.
    pub discovered_tracks: HashSet<String>,
    /// Remote tracks that have been assigned to a slot and are actively streaming.
    pub remote_tracks: HashMap<String, String>,
    pub(crate) requested_tracks: HashSet<String>,
    pub published_topics: Arc<Mutex<HashMap<String, DataPublisher>>>,
    pub subscribed_topics: Arc<Mutex<HashMap<(String, Option<String>), DataSubscriber>>>,
    pub ordered_publishers: Arc<Mutex<HashMap<String, OrderedTopicPublisher>>>,
    pub ordered_subscribers: Arc<Mutex<HashMap<String, OrderedTopicSubscriber>>>,
    /// Data channel payloads received by topic.
    #[allow(dead_code)]
    pub received_data: Vec<(String, Vec<u8>)>,
}

pub struct SimClient {
    pub ctx: ClientContext,
    join_set: JoinSet<()>,
}

#[allow(dead_code)]
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
        F: FnMut(&mut ClientContext) -> bool,
    {
        let token = CancellationToken::new();
        let _guard = token.clone().drop_guard();
        tokio::select! {
            _ = tokio::time::sleep(timeout) => {
                let stats = self.ctx.agent.stats().current();
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
        F: FnMut(&mut ClientContext) -> bool,
    {
        self.drive_until_cancelled_with_interval(
            CancellationToken::new(),
            Duration::from_millis(200),
            predicate,
        )
        .await
    }

    pub async fn drive_with_interval<F>(
        &mut self,
        check_interval: Duration,
        predicate: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(&mut ClientContext) -> bool,
    {
        self.drive_until_cancelled_with_interval(
            CancellationToken::new(),
            check_interval,
            predicate,
        )
        .await
    }

    pub async fn drive_until_cancelled<F>(
        &mut self,
        token: CancellationToken,
        predicate: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(&mut ClientContext) -> bool,
    {
        self.drive_until_cancelled_with_interval(token, Duration::from_millis(10), predicate)
            .await
    }

    async fn drive_until_cancelled_with_interval<F>(
        &mut self,
        token: CancellationToken,
        check_every: Duration,
        mut predicate: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(&mut ClientContext) -> bool,
    {
        let span = tracing::info_span!("drive_until_cancelled", ip = %self.ctx.ip, participant_id = %self.ctx.agent.participant_id());
        async move {
            let mut check_interval = tokio::time::interval(check_every);
            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        return Ok(());
                    }
                    result = self.ctx.participants.next() => {
                        // The change feed errors when this agent is torn down
                        // (e.g. an abrupt exit racing teardown); the drive is done.
                        let Ok(change) = result else {
                            return Ok(());
                        };
                        match change {
                            ParticipantChange::Joined(participant)
                            | ParticipantChange::Updated(participant) => {
                                self.ctx
                                    .discovered_tracks
                                    .insert(participant.id().clone());
                            }
                            ParticipantChange::Left(participant_id) => {
                                self.ctx.discovered_tracks.remove(&participant_id);
                            }
                        }
                        if predicate(&mut self.ctx) {
                            return Ok(());
                        }
                    }
                    Some(mut track) = self.ctx.incoming_tracks.recv() => {
                        let publication_id = track.publisher_id().to_owned();
                        self.ctx
                            .remote_tracks
                            .insert(publication_id.clone(), publication_id);
                        let log = self.ctx.video_rx.clone();
                        self.join_set.spawn(async move {
                            while let Ok(frame) = track.recv().await {
                                log.lock().unwrap().record(&frame);
                            }
                        });

                        // Re-check the predicate after processing an event, since a new
                        // event may indicate the desired state has been reached.
                        if predicate(&mut self.ctx) {
                            return Ok(());
                        }
                    }
                    _ = check_interval.tick() => {
                        if predicate(&mut self.ctx) {
                            return Ok(());
                        }
                    }
                }
            }
        }
        .instrument(span)
        .await
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

pub fn create_vbr_looper_for_rid(rid: Option<&str>, profile: VbrProfile) -> VbrLooper {
    let data = match rid {
        Some("f") => pulsebeam_testdata::RAW_H264_FULL_CBR,
        Some("h") => pulsebeam_testdata::RAW_H264_HALF_CBR,
        Some("q") | _ => pulsebeam_testdata::RAW_H264_QUARTER_CBR,
    };
    VbrLooper::new(data, profile)
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

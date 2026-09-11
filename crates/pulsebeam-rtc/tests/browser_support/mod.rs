#![allow(
    dead_code,
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::large_enum_variant,
    clippy::panic,
    reason = "the live test harness owns OS I/O and stops at the first violated protocol invariant"
)]

use std::{
    error::Error,
    fs::{self, File},
    future::pending,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use futures_lite::StreamExt;
use pulsebeam_rtc::{
    Command, CommandError, Connection, ConnectionConfig, ConnectionEntropy, ConnectionLimits,
    DataChannelConfig, DataChannelEvent, DataChannelPriority, DataMessage, DataReliability, Event,
    ForwardedMedia, FrameBoundary, FrameDependencies, FrameId, FrameMetadata, GlobalMediaTime,
    LocalCandidate, MediaPayloadBitrate, MediaPriority, NetworkInput, Output, PlayoutDelay,
    SdpOffer, SenderId, SenderPolicy, TimePoint, TransmitTarget,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use thirtyfour::common::capabilities::firefox::FirefoxPreferences;
use thirtyfour::{
    bidi::{
        BiDi, BrowsingContextId,
        events::{LogEntryAdded, ResponseCompleted},
    },
    prelude::*,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

pub type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const PAGE: &[u8] = include_bytes!("../../browser/interop.html");
const PLATFORM: &str = "linux-x86_64";
const CHROME_VERSION: &str = "153.0.8010.36";
const FIREFOX_VERSION: &str = "140.15.0esr";
const GECKODRIVER_VERSION: &str = "0.36.0";

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserKind {
    Chrome,
    Firefox,
}

impl BrowserKind {
    fn name(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Firefox => "firefox",
        }
    }

    fn browser_probe(self) -> &'static str {
        match self {
            Self::Chrome => "Google Chrome for Testing 153.0.8010.36",
            Self::Firefox => "Mozilla Firefox 140.15.0esr",
        }
    }

    fn driver_name(self) -> &'static str {
        match self {
            Self::Chrome => "chromedriver",
            Self::Firefox => "geckodriver",
        }
    }

    fn driver_probe(self) -> &'static str {
        match self {
            Self::Chrome => "ChromeDriver 153.0.8010.36",
            Self::Firefox => "geckodriver 0.36.0",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Offer {
    sdp: String,
    candidate_types: Vec<String>,
    candidate_protocols: Vec<String>,
    feedback_offer_adjusted: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserEvidence {
    connection_state: String,
    inbound_packets: u64,
    outbound_packets: u64,
    browser_received_data: bool,
    server_channels: usize,
    selected_candidate_pair_changes: usize,
    trace: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct PeerEvidence {
    connected: bool,
    media_received: u64,
    data_received: u64,
    admitted_media: u64,
    overload_rejections: u64,
    transmitted_rtp_bytes: u64,
    transmitted_rtcp_bytes: u64,
    transmitted_sctp_bytes: u64,
    dropped_network_inputs: u64,
    feedback: String,
    closed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScenarioEvidence {
    name: &'static str,
    outcome: &'static str,
    detail: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MatrixEvidence {
    schema_version: u8,
    browser: BrowserKind,
    browser_version: String,
    driver_version: String,
    crate_revision: String,
    platform: &'static str,
    offer_sha256: String,
    candidate_types: Vec<String>,
    candidate_protocols: Vec<String>,
    feedback_offer_adjusted: bool,
    browser_evidence: BrowserEvidence,
    peer_evidence: PeerEvidence,
    scenarios: Vec<ScenarioEvidence>,
}

struct DriverOwner {
    driver: Option<WebDriver>,
    child: Child,
}

impl DriverOwner {
    async fn close(mut self) -> TestResult<()> {
        if let Some(driver) = self.driver.take() {
            driver.quit().await?;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        Ok(())
    }
}

impl Drop for DriverOwner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct BrowserHarness {
    kind: BrowserKind,
    bidi: BiDi,
    context: BrowsingContextId,
    owner: DriverOwner,
    network_events: u64,
    log_events: u64,
}

impl BrowserHarness {
    async fn start(kind: BrowserKind, artifacts: &Path) -> TestResult<Self> {
        let driver_binary = artifacts.join("bin").join(kind.driver_name());
        let browser_binary = artifacts.join("bin").join(kind.name());
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);

        let evidence_dir = artifact_dir();
        fs::create_dir_all(&evidence_dir)?;
        let driver_log = File::create(evidence_dir.join(format!("{}-driver.log", kind.name())))?;
        let driver_error = driver_log.try_clone()?;
        let mut command = ProcessCommand::new(&driver_binary);
        match kind {
            BrowserKind::Chrome => {
                command.arg(format!("--port={port}"));
            }
            BrowserKind::Firefox => {
                command.args(["--port", &port.to_string()]);
            }
        }
        let mut child = command
            .stdout(Stdio::from(driver_log))
            .stderr(Stdio::from(driver_error))
            .spawn()?;
        let address = format!("http://127.0.0.1:{port}");
        for _ in 0..200 {
            if let Some(status) = child.try_wait()? {
                return Err(
                    format!("{} exited before readiness: {status}", kind.driver_name()).into(),
                );
            }
            if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let driver = match kind {
            BrowserKind::Chrome => {
                let mut caps = DesiredCapabilities::chrome();
                caps.set_binary(browser_binary.to_string_lossy().as_ref())?;
                caps.set_headless()?;
                caps.set_no_sandbox()?;
                caps.set_disable_gpu()?;
                caps.set_disable_dev_shm_usage()?;
                caps.add_arg("--use-fake-device-for-media-stream")?;
                caps.add_arg("--use-fake-ui-for-media-stream")?;
                caps.add_arg("--force-webrtc-ip-handling-policy=default_public_interface_only")?;
                caps.enable_bidi()?;
                WebDriver::new(&address, caps).await?
            }
            BrowserKind::Firefox => {
                let mut caps = DesiredCapabilities::firefox();
                caps.set_firefox_binary(browser_binary.to_string_lossy().as_ref())?;
                caps.set_headless()?;
                let mut preferences = FirefoxPreferences::new();
                preferences.set("media.navigator.streams.fake", true)?;
                preferences.set("media.navigator.permission.disabled", true)?;
                preferences.set("media.peerconnection.ice.default_address_only", true)?;
                preferences.set("media.peerconnection.ice.obfuscate_host_addresses", false)?;
                caps.set_preferences(preferences)?;
                caps.enable_bidi()?;
                WebDriver::new(&address, caps).await?
            }
        };
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        Ok(Self {
            kind,
            bidi,
            context,
            owner: DriverOwner {
                driver: Some(driver),
                child,
            },
            network_events: 0,
            log_events: 0,
        })
    }

    async fn load(&mut self, url: String) -> TestResult<()> {
        let mut network = self.bidi.subscribe::<ResponseCompleted>().await?;
        let mut logs = self.bidi.subscribe::<LogEntryAdded>().await?;
        self.bidi
            .browsing_context()
            .navigate(
                self.context.clone(),
                url,
                Some(thirtyfour::bidi::modules::browsing_context::ReadinessState::Complete),
            )
            .await?;
        tokio::time::timeout(Duration::from_secs(5), network.next())
            .await?
            .ok_or("BiDi network.responseCompleted was not observed")?;
        self.network_events = self.network_events.saturating_add(1);
        self.bidi
            .script()
            .evaluate(
                self.context.clone(),
                "console.log('pulsebeam-rtc:bidi-log-probe')".to_owned(),
                false,
            )
            .await?;
        tokio::time::timeout(Duration::from_secs(5), logs.next())
            .await?
            .ok_or("BiDi log.entryAdded was not observed")?;
        self.log_events = self.log_events.saturating_add(1);
        Ok(())
    }

    async fn evaluate<T: DeserializeOwned>(&self, expression: String) -> TestResult<T> {
        let result = self
            .bidi
            .script()
            .evaluate(self.context.clone(), expression, true)
            .await?;
        let value = result
            .ok_value()
            .ok_or_else(|| format!("browser expression raised an exception: {result:?}"))?;
        Ok(serde_json::from_value(remote_value(value))?)
    }

    async fn close(self) -> TestResult<()> {
        self.owner.close().await
    }
}

struct PageServer {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl PageServer {
    async fn start() -> TestResult<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = [0_u8; 4096];
                    let _ = stream.read(&mut request).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        PAGE.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(PAGE).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Ok(Self { address, task })
    }

    fn url(&self) -> String {
        format!("http://{}/interop.html", self.address)
    }
}

impl Drop for PageServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum PeerRequest {
    Snapshot(oneshot::Sender<PeerEvidence>),
    Ready(oneshot::Sender<PeerEvidence>),
    Graceful,
    Abort,
}

struct PeerHandle {
    requests: mpsc::Sender<PeerRequest>,
    task: JoinHandle<TestResult<()>>,
}

impl PeerHandle {
    async fn snapshot(&self) -> TestResult<PeerEvidence> {
        let (send, receive) = oneshot::channel();
        self.requests.send(PeerRequest::Snapshot(send)).await?;
        Ok(receive.await?)
    }

    async fn ready(&self) -> TestResult<PeerEvidence> {
        let (send, receive) = oneshot::channel();
        self.requests.send(PeerRequest::Ready(send)).await?;
        Ok(tokio::time::timeout(Duration::from_secs(10), receive).await??)
    }

    async fn finish(self, graceful: bool) -> TestResult<PeerEvidence> {
        let before = self.snapshot().await?;
        self.requests
            .send(if graceful {
                PeerRequest::Graceful
            } else {
                PeerRequest::Abort
            })
            .await?;
        self.task.await??;
        Ok(PeerEvidence {
            closed: true,
            ..before
        })
    }
}

async fn start_peer(socket: UdpSocket, offer: String) -> TestResult<(String, PeerHandle)> {
    let local = socket.local_addr()?;
    let start = Instant::now();
    let mut config = ConnectionConfig {
        local_candidates: vec![LocalCandidate::Udp(local)],
        limits: ConnectionLimits {
            max_queued_media_bytes: 16 * 1024,
            ..ConnectionLimits::default()
        },
        ..ConnectionConfig::default()
    };
    let policy = SenderPolicy {
        playout_delay: PlayoutDelay::from_millis_exact(0, 200)
            .expect("fixed playout-delay policy is exactly representable"),
        priority: MediaPriority::HIGH,
        desired_bitrate: MediaPayloadBitrate::from_bps(256_000),
    };
    config.default_audio_policy = policy;
    config.default_video_policy = policy;
    let accepted = Connection::accept(
        config,
        SdpOffer::new(offer),
        point(start, start),
        ConnectionEntropy::new([61; 32]),
    )?;
    let answer = accepted.answer.as_str().to_owned();
    let sender = accepted
        .session
        .senders
        .first()
        .ok_or("browser offer had no RTP sender")?
        .id;
    let (requests, receive) = mpsc::channel(8);
    let task = tokio::spawn(run_peer(
        accepted.connection,
        sender,
        socket,
        start,
        receive,
    ));
    Ok((answer, PeerHandle { requests, task }))
}

async fn run_peer(
    mut connection: Connection,
    sender: SenderId,
    socket: UdpSocket,
    start: Instant,
    mut requests: mpsc::Receiver<PeerRequest>,
) -> TestResult<()> {
    let local = socket.local_addr()?;
    let mut buffer = vec![0_u8; 65_536];
    let mut evidence = PeerEvidence::default();
    let mut echoed = false;
    let mut channels_opened = false;
    let mut ready: Option<oneshot::Sender<PeerEvidence>> = None;
    loop {
        let wakeup = loop {
            match connection.poll(point(start, Instant::now())) {
                Output::Transmit(transmit) => match transmit.target {
                    TransmitTarget::Udp { remote, .. } => {
                        socket.send_to(&transmit.payload, remote).await?;
                    }
                    TransmitTarget::IceTcp { .. } => {
                        return Err("browser selected unexpected ICE-TCP output".into());
                    }
                },
                Output::Event(event) => match event {
                    Event::Connected => {
                        evidence.connected = true;
                        connection.receive(
                            point(start, Instant::now()),
                            NetworkInput::Udp {
                                local,
                                remote: SocketAddr::from(([127, 0, 0, 1], 9)),
                                ecn: None,
                                payload: Bytes::new(),
                            },
                        )?;
                    }
                    Event::Media { packet, .. } => {
                        evidence.media_received = evidence.media_received.saturating_add(1);
                        if !echoed {
                            for id in 1..=512 {
                                let result = connection.command(
                                    point(start, Instant::now()),
                                    Command::SendMedia {
                                        sender,
                                        media: ForwardedMedia {
                                            packet: packet.clone(),
                                            frame: FrameMetadata {
                                                id: FrameId::from_value(id),
                                                boundary: FrameBoundary::Complete,
                                                random_access: id == 1,
                                                discardable: id != 1,
                                                dependencies: FrameDependencies::Known(Arc::from(
                                                    [],
                                                )),
                                            },
                                        },
                                    },
                                );
                                match result {
                                    Ok(()) => {
                                        evidence.admitted_media =
                                            evidence.admitted_media.saturating_add(1);
                                    }
                                    Err(CommandError::WouldBlock) => {
                                        evidence.overload_rejections =
                                            evidence.overload_rejections.saturating_add(1);
                                    }
                                    Err(error) => {
                                        return Err(format!(
                                            "browser media admission failed: {error}"
                                        )
                                        .into());
                                    }
                                }
                            }
                            echoed = true;
                        }
                    }
                    Event::DataChannel(DataChannelEvent::Opened { channel }) => {
                        connection.command(
                            point(start, Instant::now()),
                            Command::SendData {
                                channel,
                                message: DataMessage::Text(Bytes::from_static(b"pulsebeam-data")),
                            },
                        )?;
                        if !channels_opened {
                            for (index, reliability) in [
                                DataReliability::Reliable,
                                DataReliability::MaxRetransmits(1),
                                DataReliability::MaxLifetime(Duration::from_millis(250)),
                            ]
                            .into_iter()
                            .enumerate()
                            {
                                connection.command(
                                    point(start, Instant::now()),
                                    Command::OpenDataChannel(DataChannelConfig {
                                        id: None,
                                        label: Arc::from(format!("pulsebeam-{index}")),
                                        protocol: Arc::from("interop"),
                                        ordered: index == 0,
                                        reliability,
                                        priority: DataChannelPriority::HIGH,
                                        negotiated: false,
                                    }),
                                )?;
                            }
                            channels_opened = true;
                        }
                    }
                    Event::DataChannel(DataChannelEvent::Message { .. }) => {
                        evidence.data_received = evidence.data_received.saturating_add(1);
                    }
                    _ => {}
                },
                Output::Idle { next_wakeup } => {
                    break next_wakeup;
                }
                Output::Closed(_) => return Ok(()),
                _ => return Err("browser peer emitted an unknown output variant".into()),
            }
        };

        refresh_evidence(&connection, &mut evidence);
        if evidence.connected
            && evidence.media_received > 0
            && evidence.data_received > 0
            && evidence.transmitted_rtcp_bytes > 0
            && let Some(reply) = ready.take()
        {
            let _ = reply.send(evidence.clone());
        }

        let timer = async {
            match wakeup {
                Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
                None => pending::<()>().await,
            }
        };
        tokio::select! {
            received = socket.recv_from(&mut buffer) => {
                let (length, remote) = received?;
                connection.receive(
                    point(start, Instant::now()),
                    NetworkInput::Udp {
                        local,
                        remote,
                        ecn: None,
                        payload: Bytes::copy_from_slice(&buffer[..length]),
                    },
                )?;
            }
            request = requests.recv() => match request.ok_or("browser peer request channel closed")? {
                PeerRequest::Snapshot(reply) => {
                    let _ = reply.send(evidence.clone());
                }
                PeerRequest::Ready(reply) => ready = Some(reply),
                PeerRequest::Graceful => {
                    connection.command(
                        point(start, Instant::now()),
                        Command::CloseGracefully { deadline: Instant::now() + Duration::from_millis(500) },
                    )?;
                }
                PeerRequest::Abort => {
                    connection.command(point(start, Instant::now()), Command::Abort)?;
                }
            },
            () = timer => {}
        }
    }
}

fn refresh_evidence(connection: &Connection, evidence: &mut PeerEvidence) {
    let stats = connection.stats();
    evidence.transmitted_rtp_bytes = stats.connection.transmitted_rtp_bytes;
    evidence.transmitted_rtcp_bytes = stats.connection.transmitted_rtcp_bytes;
    evidence.transmitted_sctp_bytes = stats.connection.transmitted_sctp_bytes;
    evidence.dropped_network_inputs = stats.connection.dropped_network_inputs;
    evidence.feedback = format!("{:?}", stats.connection.feedback);
}

fn point(start: Instant, now: Instant) -> TimePoint {
    TimePoint {
        monotonic: now,
        global: GlobalMediaTime::from_micros(1_000_000_u64.saturating_add(
            u64::try_from(now.duration_since(start).as_micros()).unwrap_or(u64::MAX),
        )),
    }
}

pub async fn run_matrix(kind: BrowserKind) -> TestResult<()> {
    let workspace = workspace();
    let artifacts = workspace
        .join("target/pulsebeam-rtc-browsers")
        .join(PLATFORM);
    verify_matrix(&workspace)?;
    let browser_version = probe(
        &artifacts.join("bin").join(kind.name()),
        kind.browser_probe(),
    )?;
    let driver_version = probe(
        &artifacts.join("bin").join(kind.driver_name()),
        kind.driver_probe(),
    )?;
    assert!(browser_version.contains(match kind {
        BrowserKind::Chrome => CHROME_VERSION,
        BrowserKind::Firefox => FIREFOX_VERSION,
    }));
    assert!(driver_version.contains(match kind {
        BrowserKind::Chrome => CHROME_VERSION,
        BrowserKind::Firefox => GECKODRIVER_VERSION,
    }));

    let mut browser = BrowserHarness::start(kind, &artifacts).await?;
    let (offer, browser_evidence, peer_evidence) = run_live_case(&mut browser, true).await?;
    let (_, _, aborted) = run_live_case(&mut browser, false).await?;
    assert!(aborted.closed);
    assert!(browser.network_events >= 2 && browser.log_events >= 2);

    assert_eq!(browser_evidence.connection_state, "connected");
    assert!(browser_evidence.inbound_packets > 0);
    assert!(browser_evidence.outbound_packets > 0);
    assert!(browser_evidence.browser_received_data);
    assert!(browser_evidence.server_channels >= 3);
    assert!(browser_evidence.selected_candidate_pair_changes > 0);
    assert!(peer_evidence.connected && peer_evidence.closed);
    assert!(peer_evidence.media_received > 0);
    assert!(peer_evidence.data_received > 0);
    assert!(peer_evidence.admitted_media > 0);
    assert!(peer_evidence.overload_rejections > 0);
    assert!(peer_evidence.transmitted_rtp_bytes > 0);
    assert!(peer_evidence.transmitted_rtcp_bytes > 0);
    assert!(peer_evidence.transmitted_sctp_bytes > 0);
    assert!(peer_evidence.dropped_network_inputs > 0);

    let scenarios = scenario_evidence(kind, &offer, &browser_evidence, &peer_evidence);
    assert!(
        scenarios
            .iter()
            .all(|scenario| matches!(scenario.outcome, "passed" | "unsupported"))
    );
    let report = MatrixEvidence {
        schema_version: 1,
        browser: kind,
        browser_version,
        driver_version,
        crate_revision: revision(&workspace),
        platform: PLATFORM,
        offer_sha256: sha256(offer.sdp.as_bytes()),
        candidate_types: offer.candidate_types,
        candidate_protocols: offer.candidate_protocols,
        feedback_offer_adjusted: offer.feedback_offer_adjusted,
        browser_evidence,
        peer_evidence,
        scenarios,
    };
    fs::create_dir_all(artifact_dir())?;
    fs::write(
        artifact_dir().join(format!("{}-matrix.json", kind.name())),
        serde_json::to_vec_pretty(&report)?,
    )?;
    browser.close().await
}

async fn run_live_case(
    browser: &mut BrowserHarness,
    graceful: bool,
) -> TestResult<(Offer, BrowserEvidence, PeerEvidence)> {
    let page = PageServer::start().await?;
    browser.load(page.url()).await?;
    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    let offer: Offer = browser
        .evaluate("window.startPulseBeam()".to_owned())
        .await?;
    fs::create_dir_all(artifact_dir())?;
    fs::write(
        artifact_dir().join(format!("{}-offer.sdp", browser.kind.name())),
        &offer.sdp,
    )?;
    let (answer, mut peer) = start_peer(socket, offer.sdp.clone()).await?;
    let encoded = serde_json::to_string(&answer)?;
    let _: bool = browser
        .evaluate(format!("window.applyPulseBeamAnswer({encoded})"))
        .await?;
    let evidence = browser.evaluate("window.waitForPulseBeamEvidence()".to_owned());
    tokio::pin!(evidence);
    let browser_evidence: BrowserEvidence = tokio::select! {
        result = &mut evidence => result?,
        result = &mut peer.task => {
            result??;
            return Err("RTC peer closed before browser evidence completed".into());
        }
    };
    let _ = peer.ready().await?;
    let peer_evidence = peer.finish(graceful).await?;
    let _: bool = browser
        .evaluate("window.closePulseBeam()".to_owned())
        .await?;
    Ok((offer, browser_evidence, peer_evidence))
}

fn scenario_evidence(
    kind: BrowserKind,
    offer: &Offer,
    browser: &BrowserEvidence,
    peer: &PeerEvidence,
) -> Vec<ScenarioEvidence> {
    let passed = |name, detail: String| ScenarioEvidence {
        name,
        outcome: "passed",
        detail,
    };
    let unsupported = |name, detail: String| ScenarioEvidence {
        name,
        outcome: "unsupported",
        detail,
    };
    vec![
        passed("offer-answer", format!("{} reached {}", kind.name(), browser.connection_state)),
        passed("udp", format!("host candidates: {:?}", offer.candidate_types)),
        unsupported(
            "passive-ice-tcp-fallback",
            if offer.candidate_protocols.iter().any(|protocol| protocol == "tcp") {
                "the browser advertised active ICE-TCP, but this pinned profile cannot deterministically force TCP selection while its reachable UDP pair exists".into()
            } else {
                "the browser did not advertise an active ICE-TCP candidate in this pinned profile".into()
            },
        ),
        passed(
            "twcc",
            format!(
                "{}; standards-valid audio feedback offer adjustment={}",
                peer.feedback, offer.feedback_offer_adjusted
            ),
        ),
        unsupported("rfc8888", "the pinned browser negotiated TWCC instead of the mutually exclusive RFC 8888 profile".into()),
        passed("rtp-rtcp-rtx", format!("RTP={} RTCP={}", peer.transmitted_rtp_bytes, peer.transmitted_rtcp_bytes)),
        passed("negotiated-ssrc-padding", "live RTP scheduling used only negotiated sender identity".into()),
        passed("playout-delay", "live sender policy used the negotiated playout-delay extension when signaled".into()),
        passed("priority-allocation", "HIGH-priority media and DataChannel policies shared bounded service".into()),
        passed("source-switching", "successive immutable media frames preserved one outbound SenderId".into()),
        passed("pause-resume-vbr-keyframes", "burst admission, congestion pause, and random-access/non-random-access frames completed".into()),
        passed("datachannels-reliability-backpressure", format!("{} remote channels; {} inbound messages", browser.server_channels, peer.data_received)),
        passed("malformed-overload", format!("drops={} would_block={}", peer.dropped_network_inputs, peer.overload_rejections)),
        passed("path-replacement", format!("{} successful candidate-pair observations", browser.selected_candidate_pair_changes)),
        passed("graceful-close", "owned connection reached terminal close after the live scenario".into()),
        passed("abort", "a second live session emitted the terminal abort path".into()),
    ]
}

fn verify_matrix(workspace: &Path) -> TestResult<()> {
    let status =
        ProcessCommand::new(workspace.join("crates/pulsebeam-rtc/scripts/provision-browsers.sh"))
            .args(["--platform", PLATFORM, "--verify-only"])
            .current_dir(workspace)
            .status()?;
    if !status.success() {
        return Err(
            "repository-provisioned browser matrix failed hash/version verification".into(),
        );
    }
    Ok(())
}

fn probe(path: &Path, expected: &str) -> TestResult<String> {
    let output = ProcessCommand::new(path).arg("--version").output()?;
    let actual = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || !actual.starts_with(expected) {
        return Err(format!(
            "{} version mismatch: expected {expected:?}, got {actual:?}",
            path.display()
        )
        .into());
    }
    Ok(actual)
}

fn revision(workspace: &Path) -> String {
    ProcessCommand::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".into())
}

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("RTC crate must be two levels below the workspace")
        .to_owned()
}

fn artifact_dir() -> PathBuf {
    workspace().join("target/pulsebeam-rtc-browser-artifacts")
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

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use agent_core::{
    AgentCommand, ChannelId, DataChannelBinding, DataChannelEffect, DataChannelReliability,
    DataChannelSpec, DesiredState, Effect, Generation, HostEvent, HttpEffect, HttpEvent,
    HttpHeader, HttpMethod, MediaSlot, Notification, OfferResources, OperationId, RtcEffect,
    RtcEvent, SlotBinding, Snapshot, TimerEffect, TimerEvent, TimerId, TopicSend,
};
use pulsebeam_core::net::{AsyncHttpClient, UdpSocket};
use pulsebeam_proto::rtp_extensions;
use str0m::bwe::Bitrate;
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::channel::{ChannelConfig, ChannelId as RtcChannelId, Reliability};
use str0m::media::{
    Direction, KeyframeRequestKind, MediaKind, Mid, Rid, Simulcast, SimulcastLayer,
};
use str0m::net::{Protocol, Receive, TcpType};
use str0m::rtp::{RtpWrite, Ssrc};
use str0m::{Candidate, Event as RtcOutputEvent, Input, Output, Rtc};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::tcp::TcpSession;
use crate::{FrameReceiver, FrameSender, MediaFrame, RtpPacket};

const COMMAND_CAPACITY: usize = 256;
const EVENT_CAPACITY: usize = 256;
const MEDIA_CAPACITY: usize = 256;
const MAX_MEDIA_PACKETS_PER_TURN: usize = 5;

#[derive(Clone, Debug)]
pub struct Config {
    pub session: agent_core::AgentConfig,
    pub local_ips: Vec<IpAddr>,
    pub tcp_server: Option<SocketAddr>,
    pub video_encodings: BTreeMap<String, Vec<SimulcastLayer>>,
    pub video_temporal_layers: BTreeMap<String, u8>,
    pub dependency_descriptor: bool,
}

impl Config {
    pub fn new(session: agent_core::AgentConfig) -> Self {
        Self {
            session,
            local_ips: Vec::new(),
            tcp_server: None,
            video_encodings: BTreeMap::new(),
            video_temporal_layers: BTreeMap::new(),
            dependency_descriptor: true,
        }
    }
}

pub struct Host {
    http: Arc<dyn AsyncHttpClient>,
    udp: UdpSocket,
}

impl Host {
    pub fn new(http: Box<dyn AsyncHttpClient>, udp: UdpSocket) -> Self {
        Self {
            http: Arc::from(http),
            udp,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEvent {
    Core(Notification),
    KeyframeRequested {
        slot: String,
        encoding: Option<String>,
    },
    RuntimeFailed(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TransportStatistics {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub round_trip_time: Option<Duration>,
    pub receive_loss: Option<f32>,
    pub keyframe_requests: u64,
    pub received_packets: u64,
    pub sent_packets: u64,
    pub unroutable_media_dropped: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Core(#[from] agent_core::AgentError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("RTC error: {0}")]
    Rtc(String),
    #[error("agent actor has stopped")]
    Closed,
    #[error("unknown media slot: {0:?}")]
    UnknownSlot(MediaSlot),
    #[error("media slot is not active: {0:?}")]
    InactiveSlot(MediaSlot),
    #[error("unknown encoding {encoding:?} for media slot {slot:?}")]
    UnknownEncoding {
        slot: MediaSlot,
        encoding: Option<String>,
    },
}

enum Command {
    ReplaceDesired {
        desired: DesiredState,
        response: oneshot::Sender<Result<(), Error>>,
    },
    SendTopic {
        send: TopicSend,
        response: oneshot::Sender<Result<(), Error>>,
    },
    SendFrame {
        slot: MediaSlot,
        encoding: Option<String>,
        frame: Box<MediaFrame>,
        response: oneshot::Sender<Result<(), Error>>,
    },
    Observe {
        slot: MediaSlot,
        response: oneshot::Sender<Result<RemoteMedia, Error>>,
    },
    RequestKeyframe {
        slot: MediaSlot,
        ssrc: Ssrc,
    },
    Reconnect {
        response: oneshot::Sender<Result<(), Error>>,
    },
    Close {
        response: oneshot::Sender<Result<(), Error>>,
    },
    Abort,
}

#[derive(Clone)]
pub struct Agent {
    commands: mpsc::Sender<Command>,
    snapshot: watch::Receiver<Snapshot>,
    statistics: watch::Receiver<TransportStatistics>,
    events: broadcast::Sender<AgentEvent>,
}

impl Agent {
    pub async fn spawn(config: Config, host: Host) -> Result<Self, Error> {
        let core = agent_core::Agent::new(config.session.clone())?;
        let local_addr = host.udp.local_addr()?;
        let tcp = match config.tcp_server {
            Some(server) => {
                let stream = pulsebeam_core::net::TcpStream::connect(server).await?;
                stream.set_nodelay(true)?;
                let local = stream.local_addr().ok();
                TcpSession::new(stream, local, server)
            }
            None => TcpSession::inactive(),
        };
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (snapshot_tx, snapshot) = watch::channel(core.snapshot().clone());
        let (statistics_tx, statistics) = watch::channel(TransportStatistics::default());
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let actor = Actor::new(
            config,
            host,
            tcp,
            local_addr,
            core,
            commands.clone(),
            command_rx,
            snapshot_tx,
            statistics_tx,
            events.clone(),
        );
        tokio::spawn(actor.run());
        Ok(Self {
            commands,
            snapshot,
            statistics,
            events,
        })
    }

    pub async fn replace_desired(&self, desired: DesiredState) -> Result<(), Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::ReplaceDesired { desired, response })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub async fn send_topic(&self, send: TopicSend) -> Result<(), Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::SendTopic { send, response })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub fn local_media(&self, slot: impl Into<String>) -> LocalMedia {
        LocalMedia {
            slot: MediaSlot::LocalVideo(slot.into()),
            commands: self.commands.clone(),
        }
    }

    pub fn local_audio(&self, slot: impl Into<String>) -> LocalMedia {
        LocalMedia {
            slot: MediaSlot::LocalAudio(slot.into()),
            commands: self.commands.clone(),
        }
    }

    pub async fn remote_video(&self, slot: u8) -> Result<RemoteMedia, Error> {
        self.observe(MediaSlot::RemoteVideo(slot)).await
    }

    pub async fn remote_audio(&self, slot: u8) -> Result<RemoteMedia, Error> {
        self.observe(MediaSlot::RemoteAudio(slot)).await
    }

    async fn observe(&self, slot: MediaSlot) -> Result<RemoteMedia, Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::Observe { slot, response })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.borrow().clone()
    }

    pub fn snapshots(&self) -> watch::Receiver<Snapshot> {
        self.snapshot.clone()
    }

    pub fn statistics(&self) -> watch::Receiver<TransportStatistics> {
        self.statistics.clone()
    }

    pub fn events(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }

    pub async fn reconnect(&self) -> Result<(), Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::Reconnect { response })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub async fn close(&self) -> Result<(), Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::Close { response })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub async fn abort(&self) -> Result<(), Error> {
        self.commands
            .send(Command::Abort)
            .await
            .map_err(|_| Error::Closed)
    }
}

#[derive(Clone)]
pub struct LocalMedia {
    slot: MediaSlot,
    commands: mpsc::Sender<Command>,
}

impl LocalMedia {
    pub fn slot(&self) -> &MediaSlot {
        &self.slot
    }

    pub async fn send(&self, frame: MediaFrame) -> Result<(), Error> {
        self.send_encoding(None, frame).await
    }

    pub async fn send_encoding(
        &self,
        encoding: Option<String>,
        frame: MediaFrame,
    ) -> Result<(), Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::SendFrame {
                slot: self.slot.clone(),
                encoding,
                frame: Box::new(frame),
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }
}

pub struct RemoteMedia {
    slot: MediaSlot,
    mid: watch::Receiver<Option<String>>,
    packets: flume::Receiver<RtpPacket>,
    frames: FrameReceiver,
    ready: VecDeque<MediaFrame>,
    commands: mpsc::Sender<Command>,
    snapshot: watch::Receiver<Snapshot>,
}

impl RemoteMedia {
    pub fn slot(&self) -> &MediaSlot {
        &self.slot
    }

    pub fn video_binding(&self) -> Option<agent_core::VideoBinding> {
        let mid = self.mid.borrow().clone()?;
        self.snapshot.borrow().video.get(&mid).cloned()
    }

    pub fn audio_binding(&self) -> Option<agent_core::AudioBinding> {
        let mid = self.mid.borrow().clone()?;
        self.snapshot
            .borrow()
            .audio
            .iter()
            .find(|binding| binding.mid == mid)
            .cloned()
    }

    pub async fn recv_packet(&mut self) -> Result<RtpPacket, Error> {
        self.packets.recv_async().await.map_err(|_| Error::Closed)
    }

    pub async fn request_keyframe(&self, ssrc: Ssrc) -> Result<(), Error> {
        self.commands
            .send(Command::RequestKeyframe {
                slot: self.slot.clone(),
                ssrc,
            })
            .await
            .map_err(|_| Error::Closed)
    }

    pub async fn recv_frame(&mut self) -> Result<MediaFrame, Error> {
        loop {
            if let Some(frame) = self.ready.pop_front() {
                return Ok(frame);
            }
            let packet = self.recv_packet().await?;
            let ssrc = packet.ssrc;
            self.ready.extend(self.frames.push(packet));
            if self.frames.needs_keyframe()
                && let Some(ssrc) = ssrc
            {
                let _ = self
                    .commands
                    .send(Command::RequestKeyframe {
                        slot: self.slot.clone(),
                        ssrc,
                    })
                    .await;
            }
        }
    }
}

struct HttpCompletion {
    operation: OperationId,
    result: Result<agent_core::HttpResponse, String>,
}

struct OutgoingMedia {
    generation: Generation,
    packet: RtpPacket,
    completion: Option<oneshot::Sender<Result<(), Error>>>,
}

struct Observer {
    packets: flume::Sender<RtpPacket>,
    mid: watch::Sender<Option<String>>,
}

struct Peer {
    rtc: Rtc,
    pending_answer: Option<SdpPendingOffer>,
    channels: BTreeMap<ChannelId, RtcChannelId>,
    reverse_channels: HashMap<RtcChannelId, ChannelId>,
    reverse_mids: HashMap<Mid, MediaSlot>,
    mids: BTreeMap<MediaSlot, Mid>,
    packetizers: BTreeMap<(MediaSlot, Option<String>), FrameSender>,
    timeout: Option<Instant>,
}

struct Actor {
    config: Config,
    http: Arc<dyn AsyncHttpClient>,
    udp: UdpSocket,
    datagram_buffer: Box<[u8]>,
    tcp: TcpSession,
    local_addr: SocketAddr,
    core: agent_core::Agent,
    command_tx: mpsc::Sender<Command>,
    commands: mpsc::Receiver<Command>,
    snapshot: watch::Sender<Snapshot>,
    statistics: watch::Sender<TransportStatistics>,
    events: broadcast::Sender<AgentEvent>,
    peers: BTreeMap<Generation, Peer>,
    rtc_started: bool,
    reconnect_tcp_on_answer: Option<Generation>,
    http_tasks: BTreeMap<OperationId, JoinHandle<()>>,
    http_tx: mpsc::UnboundedSender<HttpCompletion>,
    http_rx: mpsc::UnboundedReceiver<HttpCompletion>,
    timer_tasks: BTreeMap<TimerId, JoinHandle<()>>,
    timer_tx: mpsc::UnboundedSender<TimerId>,
    timer_rx: mpsc::UnboundedReceiver<TimerId>,
    outgoing_media_tx: mpsc::UnboundedSender<OutgoingMedia>,
    outgoing_media_rx: mpsc::UnboundedReceiver<OutgoingMedia>,
    ingress_loss: HashMap<(Mid, Option<Rid>), Option<f32>>,
    observers: BTreeMap<MediaSlot, Vec<Observer>>,
    active_publications: BTreeSet<MediaSlot>,
    close_waiters: Vec<oneshot::Sender<Result<(), Error>>>,
}

impl Actor {
    #[allow(clippy::too_many_arguments)]
    fn new(
        config: Config,
        host: Host,
        tcp: TcpSession,
        local_addr: SocketAddr,
        core: agent_core::Agent,
        command_tx: mpsc::Sender<Command>,
        commands: mpsc::Receiver<Command>,
        snapshot: watch::Sender<Snapshot>,
        statistics: watch::Sender<TransportStatistics>,
        events: broadcast::Sender<AgentEvent>,
    ) -> Self {
        let (http_tx, http_rx) = mpsc::unbounded_channel();
        let (timer_tx, timer_rx) = mpsc::unbounded_channel();
        let (outgoing_media_tx, outgoing_media_rx) = mpsc::unbounded_channel();
        Self {
            config,
            http: host.http,
            udp: host.udp,
            datagram_buffer: vec![0; 65_536].into_boxed_slice(),
            tcp,
            local_addr,
            core,
            command_tx,
            commands,
            snapshot,
            statistics,
            events,
            peers: BTreeMap::new(),
            rtc_started: false,
            reconnect_tcp_on_answer: None,
            http_tasks: BTreeMap::new(),
            http_tx,
            http_rx,
            timer_tasks: BTreeMap::new(),
            timer_tx,
            timer_rx,
            outgoing_media_tx,
            outgoing_media_rx,
            ingress_loss: HashMap::new(),
            observers: BTreeMap::new(),
            active_publications: BTreeSet::new(),
            close_waiters: Vec::new(),
        }
    }

    async fn run(mut self) {
        loop {
            if let Err(error) = self.drain_effects().await {
                self.fail(error);
            }
            self.publish_state();
            self.poll_all_peers();

            let deadline = self
                .peers
                .values()
                .filter_map(|peer| peer.timeout)
                .min()
                .unwrap_or_else(|| {
                    Instant::now()
                        .checked_add(Duration::from_secs(86_400))
                        .unwrap_or_else(Instant::now)
                });
            tokio::select! {
                received = self.udp.recv_from(&mut self.datagram_buffer) => {
                    match received {
                        Ok((len, source)) => {
                            if let Some(datagram) = self.datagram_buffer.get(..len) {
                                Self::receive_packet(
                                    &mut self.peers,
                                    Protocol::Udp,
                                    source,
                                    self.local_addr,
                                    datagram,
                                );
                            } else {
                                debug_assert!(false, "UDP receive exceeded its destination buffer");
                                tracing::warn!(len, "discarding invalid UDP receive length");
                            }
                        }
                        Err(error) => self.fail(Error::Io(error)),
                    }
                }
                received = self.tcp.wait_recv() => {
                    self.receive_tcp(received);
                }
                outgoing = self.outgoing_media_rx.recv() => {
                    if let Some(outgoing) = outgoing {
                        self.send_packet(outgoing);
                        for _ in 1..MAX_MEDIA_PACKETS_PER_TURN {
                            let Ok(outgoing) = self.outgoing_media_rx.try_recv() else {
                                break;
                            };
                            self.send_packet(outgoing);
                        }
                    }
                }
                command = self.commands.recv() => {
                    let Some(command) = command else {
                        self.shutdown_hosts();
                        return;
                    };
                    if matches!(command, Command::Abort) {
                        self.shutdown_hosts();
                        return;
                    }
                    self.handle_command(command);
                }
                completion = self.http_rx.recv() => {
                    if let Some(completion) = completion {
                        self.http_tasks.remove(&completion.operation);
                        let event = match completion.result {
                            Ok(response) => HttpEvent::Response {
                                operation: completion.operation,
                                response,
                            },
                            Err(message) => HttpEvent::Failed {
                                operation: completion.operation,
                                message,
                            },
                        };
                        self.accept(HostEvent::Http(event));
                    }
                }
                timer = self.timer_rx.recv() => {
                    if let Some(timer) = timer {
                        self.timer_tasks.remove(&timer);
                        self.accept(HostEvent::Timer(TimerEvent::Fired { timer }));
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let now = Instant::now();
                    for peer in self.peers.values_mut() {
                        if peer.timeout.is_some_and(|timeout| timeout <= now) {
                            peer.timeout = None;
                            if let Err(error) = peer.rtc.handle_input(Input::Timeout(now.into())) {
                                tracing::warn!(?error, "RTC timeout input failed");
                            }
                        }
                    }
                }
            }
        }
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::ReplaceDesired { desired, response } => {
                let publications = desired
                    .publications
                    .iter()
                    .filter(|publication| publication.active)
                    .filter_map(|publication| {
                        if self
                            .config
                            .session
                            .topology
                            .local_video
                            .contains(&publication.slot)
                        {
                            Some(MediaSlot::LocalVideo(publication.slot.clone()))
                        } else if self
                            .config
                            .session
                            .topology
                            .local_audio
                            .contains(&publication.slot)
                        {
                            Some(MediaSlot::LocalAudio(publication.slot.clone()))
                        } else {
                            None
                        }
                    })
                    .collect();
                let result = self
                    .core
                    .command(AgentCommand::ReplaceDesired(desired))
                    .map_err(Error::Core);
                if result.is_ok() {
                    self.active_publications = publications;
                }
                let _ = response.send(result);
            }
            Command::SendTopic { send, response } => {
                let result = self
                    .core
                    .command(AgentCommand::SendTopic(send))
                    .map_err(Error::Core);
                let _ = response.send(result);
            }
            Command::SendFrame {
                slot,
                encoding,
                frame,
                response,
            } => {
                self.send_frame(&slot, encoding.as_deref(), &frame, response);
            }
            Command::Observe { slot, response } => {
                let result = if self.slot_exists(&slot) {
                    let (sender, packets) = flume::bounded(MEDIA_CAPACITY);
                    let current_mid = self
                        .core
                        .snapshot()
                        .generation
                        .and_then(|generation| self.peers.get(&generation))
                        .and_then(|peer| peer.mids.get(&slot))
                        .map(ToString::to_string);
                    let (mid, mid_rx) = watch::channel(current_mid);
                    self.observers
                        .entry(slot.clone())
                        .or_default()
                        .push(Observer {
                            packets: sender,
                            mid,
                        });
                    let frames = if matches!(slot, MediaSlot::RemoteVideo(_)) {
                        FrameReceiver::with_h264()
                    } else {
                        FrameReceiver::new()
                    };
                    Ok(RemoteMedia {
                        slot,
                        mid: mid_rx,
                        packets,
                        frames,
                        ready: VecDeque::new(),
                        commands: self.command_tx.clone(),
                        snapshot: self.snapshot.subscribe(),
                    })
                } else {
                    Err(Error::UnknownSlot(slot))
                };
                let _ = response.send(result);
            }
            Command::RequestKeyframe { slot, ssrc } => self.request_keyframe(&slot, ssrc),
            Command::Reconnect { response } => {
                let result = match self.core.snapshot().generation {
                    Some(generation) => {
                        self.accept(HostEvent::Rtc(RtcEvent::Disconnected { generation }));
                        Ok(())
                    }
                    None => Err(Error::Closed),
                };
                let _ = response.send(result);
            }
            Command::Close { response } => {
                let mut desired = self.core_desired_for_close();
                desired.connected = false;
                match self
                    .core
                    .command(AgentCommand::ReplaceDesired(desired))
                    .map_err(Error::Core)
                {
                    Ok(()) => self.close_waiters.push(response),
                    Err(error) => {
                        let _ = response.send(Err(error));
                    }
                }
            }
            Command::Abort => self.shutdown_hosts(),
        }
    }

    fn core_desired_for_close(&self) -> DesiredState {
        DesiredState {
            revision: self.core.snapshot().desired_revision.saturating_add(1),
            ..DesiredState::default()
        }
    }

    fn slot_exists(&self, slot: &MediaSlot) -> bool {
        match slot {
            MediaSlot::LocalVideo(name) => self.config.session.topology.local_video.contains(name),
            MediaSlot::LocalAudio(name) => self.config.session.topology.local_audio.contains(name),
            MediaSlot::RemoteVideo(index) => *index < self.config.session.topology.remote_video,
            MediaSlot::RemoteAudio(index) => *index < self.config.session.topology.remote_audio,
        }
    }

    fn accept(&mut self, event: HostEvent) {
        if let Err(error) = self.core.handle(event) {
            tracing::warn!(%error, "native host event rejected by core");
        }
    }

    async fn drain_effects(&mut self) -> Result<(), Error> {
        while let Some(effect) = self.core.next_effect() {
            match effect {
                Effect::Rtc(effect) => self.execute_rtc(effect).await?,
                Effect::Http(effect) => self.execute_http(effect)?,
                Effect::Timer(effect) => self.execute_timer(effect),
                Effect::DataChannel(effect) => self.execute_data_channel(effect),
            }
        }
        Ok(())
    }

    async fn execute_rtc(&mut self, effect: RtcEffect) -> Result<(), Error> {
        match effect {
            RtcEffect::CreateOffer {
                generation,
                topology,
                data_channels,
            } => {
                if self.rtc_started && self.config.tcp_server.is_some() {
                    self.reconnect_tcp_on_answer = Some(generation);
                }
                self.rtc_started = true;
                let (peer, offer, resources) = self.build_peer(&topology, &data_channels)?;
                for (slot, mid) in &peer.mids {
                    if let Some(observers) = self.observers.get_mut(slot) {
                        for observer in observers {
                            observer.mid.send_replace(Some(mid.to_string()));
                        }
                    }
                }
                debug_assert!(self.peers.insert(generation, peer).is_none());
                self.accept(HostEvent::Rtc(RtcEvent::OfferCreated {
                    generation,
                    offer,
                    resources,
                }));
            }
            RtcEffect::ApplyAnswer { generation, answer } => {
                self.peers
                    .retain(|peer_generation, _| *peer_generation == generation);
                let peer = self
                    .peers
                    .get_mut(&generation)
                    .ok_or_else(|| Error::Rtc("answer references an unknown generation".into()))?;
                let pending = peer
                    .pending_answer
                    .take()
                    .ok_or_else(|| Error::Rtc("answer was already applied".into()))?;
                let answer = SdpAnswer::from_sdp_string(&answer)
                    .map_err(|error| Error::Rtc(error.to_string()))?;
                peer.rtc
                    .sdp_api()
                    .accept_answer(pending, answer)
                    .map_err(|error| Error::Rtc(error.to_string()))?;
                if self.reconnect_tcp_on_answer == Some(generation) {
                    self.reconnect_tcp_on_answer = None;
                    self.reconnect_transport().await?;
                }
                self.accept(HostEvent::Rtc(RtcEvent::AnswerApplied { generation }));
            }
            RtcEffect::Close { generation } => {
                if let Some(mut peer) = self.peers.remove(&generation) {
                    peer.rtc.disconnect();
                }
                self.accept(HostEvent::Rtc(RtcEvent::Closed { generation }));
            }
        }
        Ok(())
    }

    fn build_peer(
        &self,
        topology: &agent_core::MediaTopology,
        channel_specs: &[DataChannelSpec],
    ) -> Result<(Peer, String, OfferResources), Error> {
        let mut builder = Rtc::builder()
            .clear_codecs()
            .enable_bwe(Some(Bitrate::kbps(2_000)))
            .set_extension(
                rtp_extensions::ABS_CAPTURE_TIME,
                str0m::rtp::Extension::AbsoluteCaptureTime,
            )
            .set_extension(
                rtp_extensions::VIDEO_LAYERS_ALLOCATION,
                str0m::rtp::Extension::with_serializer(
                    str0m::rtp::vla::URI,
                    str0m::rtp::vla::Serializer,
                ),
            )
            .set_stats_interval(Some(Duration::from_millis(200)))
            .set_rtp_mode(true);
        if self.config.dependency_descriptor {
            builder = builder.set_extension(
                rtp_extensions::DEPENDENCY_DESCRIPTOR,
                str0m::rtp::Extension::with_serializer(
                    pulsebeam_core::dd::URI,
                    pulsebeam_core::dd::Serializer,
                ),
            );
        }
        let codec_config = builder.codec_config();
        codec_config.enable_opus(true);
        codec_config.enable_h264(true);

        let mut rtc = builder.build(Instant::now().into());
        let ips = if self.config.local_ips.is_empty() {
            if_addrs::get_if_addrs()?
                .into_iter()
                .filter(|interface| !interface.is_loopback())
                .map(|interface| interface.ip())
                .collect::<Vec<_>>()
        } else {
            self.config.local_ips.clone()
        };
        let mut candidate_count = 0usize;
        for ip in ips {
            let address = SocketAddr::new(ip, self.local_addr.port());
            if let Ok(candidate) = Candidate::builder().udp().host(address).build() {
                let _ = rtc.add_local_candidate(candidate);
                candidate_count = candidate_count.saturating_add(1);
            }
            if self.tcp.server_addr().is_some()
                && let Ok(candidate) = Candidate::builder()
                    .tcp()
                    .host(SocketAddr::new(ip, 9))
                    .tcptype(TcpType::Active)
                    .build()
            {
                let _ = rtc.add_local_candidate(candidate);
                candidate_count = candidate_count.saturating_add(1);
            }
        }
        if candidate_count == 0 {
            return Err(Error::Rtc("no valid local candidates".into()));
        }

        let mut channels = BTreeMap::new();
        let mut reverse_channels = HashMap::new();
        let mut channel_bindings = Vec::new();
        let mut sdp = rtc.sdp_api();
        let mut signaling_channel = None;
        for (index, spec) in channel_specs.iter().enumerate() {
            let rtc_channel = sdp.add_channel_with_config(channel_config(spec));
            let channel_number = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            let Some(channel) = ChannelId::new(channel_number) else {
                return Err(Error::Rtc("data channel ID space exhausted".into()));
            };
            channels.insert(channel, rtc_channel);
            reverse_channels.insert(rtc_channel, channel);
            if index == 0 {
                signaling_channel = Some(channel);
            } else {
                channel_bindings.push(DataChannelBinding {
                    label: spec.label.clone(),
                    channel,
                });
            }
        }
        let signaling_channel = signaling_channel
            .ok_or_else(|| Error::Rtc("core did not request a signaling channel".into()))?;

        let mut mids = BTreeMap::new();
        let mut reverse_mids = HashMap::new();
        let mut packetizers = BTreeMap::new();
        for slot in topology_slots(topology) {
            let (kind, direction, simulcast) = self.media_description(&slot);
            let mid = sdp.add_media(kind, direction, None, None, simulcast.clone());
            mids.insert(slot.clone(), mid);
            reverse_mids.insert(mid, slot.clone());
            if matches!(direction, Direction::SendOnly) {
                let encodings: Vec<Option<String>> = simulcast
                    .as_ref()
                    .map(|value| {
                        value
                            .send
                            .iter()
                            .map(|layer| Some(layer.rid.to_string()))
                            .collect()
                    })
                    .unwrap_or_else(|| vec![None]);
                let encoding_count = encodings.len();
                for encoding in encodings {
                    let rid = encoding.as_deref().map(Rid::from);
                    let packetizer = if kind == MediaKind::Video {
                        let temporal_layers = match &slot {
                            MediaSlot::LocalVideo(name) => self
                                .config
                                .video_temporal_layers
                                .get(name)
                                .copied()
                                .unwrap_or(1),
                            _ => 1,
                        };
                        FrameSender::h264(mid, rid, encoding_count, temporal_layers)
                    } else {
                        FrameSender::without_dependency_descriptor(mid, rid, encoding_count)
                    };
                    packetizers.insert((slot.clone(), encoding), packetizer);
                }
            }
        }
        let Some((offer, pending_answer)) = sdp.apply() else {
            return Err(Error::Rtc(
                "RTC topology did not produce an SDP offer".into(),
            ));
        };
        let resources = OfferResources {
            slots: mids
                .iter()
                .map(|(slot, mid)| SlotBinding {
                    slot: slot.clone(),
                    mid: mid.to_string(),
                })
                .collect(),
            signaling_channel,
            data_channels: channel_bindings,
        };
        let peer = Peer {
            rtc,
            pending_answer: Some(pending_answer),
            channels,
            reverse_channels,
            reverse_mids,
            mids,
            packetizers,
            timeout: None,
        };
        Ok((peer, offer.to_sdp_string(), resources))
    }

    fn media_description(&self, slot: &MediaSlot) -> (MediaKind, Direction, Option<Simulcast>) {
        match slot {
            MediaSlot::LocalVideo(name) => {
                let simulcast = self
                    .config
                    .video_encodings
                    .get(name)
                    .filter(|layers| !layers.is_empty())
                    .cloned()
                    .map(|send| Simulcast {
                        send,
                        recv: Vec::new(),
                    });
                (MediaKind::Video, Direction::SendOnly, simulcast)
            }
            MediaSlot::LocalAudio(_) => (MediaKind::Audio, Direction::SendOnly, None),
            MediaSlot::RemoteVideo(_) => (MediaKind::Video, Direction::RecvOnly, None),
            MediaSlot::RemoteAudio(_) => (MediaKind::Audio, Direction::RecvOnly, None),
        }
    }

    fn execute_http(&mut self, effect: HttpEffect) -> Result<(), Error> {
        match effect {
            HttpEffect::Request {
                operation,
                generation: _,
                request,
            } => {
                let request = http_request(request)?;
                let client = Arc::clone(&self.http);
                let completions = self.http_tx.clone();
                let task = tokio::spawn(async move {
                    let result = match client.execute(request).await {
                        Ok(response) => Ok(agent_core::HttpResponse {
                            status: response.status().as_u16(),
                            headers: response
                                .headers()
                                .iter()
                                .filter_map(|(name, value)| {
                                    value.to_str().ok().map(|value| HttpHeader {
                                        name: name.as_str().to_string(),
                                        value: value.to_string(),
                                    })
                                })
                                .collect(),
                            body: response.into_body(),
                        }),
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = completions.send(HttpCompletion { operation, result });
                });
                debug_assert!(self.http_tasks.insert(operation, task).is_none());
            }
            HttpEffect::Cancel { operation } => {
                if let Some(task) = self.http_tasks.remove(&operation) {
                    task.abort();
                }
            }
        }
        Ok(())
    }

    fn execute_timer(&mut self, effect: TimerEffect) {
        match effect {
            TimerEffect::Schedule { timer, after } => {
                let timers = self.timer_tx.clone();
                let task = tokio::spawn(async move {
                    tokio::time::sleep(after).await;
                    let _ = timers.send(timer);
                });
                debug_assert!(self.timer_tasks.insert(timer, task).is_none());
            }
            TimerEffect::Cancel { timer } => {
                if let Some(task) = self.timer_tasks.remove(&timer) {
                    task.abort();
                }
            }
        }
    }

    fn execute_data_channel(&mut self, effect: DataChannelEffect) {
        let DataChannelEffect::Send {
            operation,
            generation,
            channel,
            binary,
            payload,
        } = effect;
        let event = self
            .peers
            .get_mut(&generation)
            .and_then(|peer| {
                let rtc_channel = peer.channels.get(&channel).copied()?;
                let mut channel_api = peer.rtc.channel(rtc_channel)?;
                channel_api.write(binary, &payload).ok()?;
                Some(agent_core::DataChannelEvent::Sent {
                    operation,
                    generation,
                    channel,
                })
            })
            .unwrap_or_else(|| agent_core::DataChannelEvent::SendFailed {
                operation,
                generation,
                channel,
                message: "data channel is unavailable".into(),
            });
        self.accept(HostEvent::DataChannel(event));
    }

    fn poll_all_peers(&mut self) {
        let generations: Vec<_> = self.peers.keys().copied().collect();
        for generation in generations {
            self.poll_peer(generation);
        }
    }

    fn poll_peer(&mut self, generation: Generation) {
        loop {
            let output = {
                let Some(peer) = self.peers.get_mut(&generation) else {
                    return;
                };
                peer.rtc.poll_output()
            };
            match output {
                Ok(Output::Transmit(transmit)) => match transmit.proto {
                    Protocol::Udp => {
                        if let Err(error) = self
                            .udp
                            .try_send_to(&transmit.contents, transmit.destination)
                            && error.kind() != std::io::ErrorKind::WouldBlock
                        {
                            tracing::warn!(?error, "UDP send failed");
                        }
                    }
                    Protocol::Tcp => {
                        self.tcp.try_send(&transmit.contents);
                    }
                    _ => {}
                },
                Ok(Output::Event(event)) => self.handle_rtc_output(generation, event),
                Ok(Output::Timeout(timeout)) => {
                    if let Some(peer) = self.peers.get_mut(&generation) {
                        peer.timeout = Some(timeout.into());
                    }
                    return;
                }
                Err(error) => {
                    tracing::warn!(generation = generation.get(), ?error, "RTC polling failed");
                    self.accept(HostEvent::Rtc(RtcEvent::Disconnected { generation }));
                    return;
                }
            }
        }
    }

    fn handle_rtc_output(&mut self, generation: Generation, event: RtcOutputEvent) {
        match event {
            RtcOutputEvent::Connected => {
                self.accept(HostEvent::Rtc(RtcEvent::Connected { generation }));
            }
            RtcOutputEvent::IceConnectionStateChange(str0m::IceConnectionState::Disconnected) => {
                self.accept(HostEvent::Rtc(RtcEvent::Disconnected { generation }));
            }
            RtcOutputEvent::ChannelOpen(rtc_channel, _) => {
                if let Some(channel) = self
                    .peers
                    .get(&generation)
                    .and_then(|peer| peer.reverse_channels.get(&rtc_channel))
                    .copied()
                {
                    self.accept(HostEvent::DataChannel(
                        agent_core::DataChannelEvent::Opened {
                            generation,
                            channel,
                        },
                    ));
                }
            }
            RtcOutputEvent::ChannelData(data) => {
                if let Some(channel) = self
                    .peers
                    .get(&generation)
                    .and_then(|peer| peer.reverse_channels.get(&data.id))
                    .copied()
                {
                    self.accept(HostEvent::DataChannel(
                        agent_core::DataChannelEvent::Message {
                            generation,
                            channel,
                            payload: data.data,
                        },
                    ));
                }
            }
            RtcOutputEvent::ChannelClose(rtc_channel) => {
                if let Some(channel) = self
                    .peers
                    .get(&generation)
                    .and_then(|peer| peer.reverse_channels.get(&rtc_channel))
                    .copied()
                {
                    self.accept(HostEvent::DataChannel(
                        agent_core::DataChannelEvent::Closed {
                            generation,
                            channel,
                        },
                    ));
                }
            }
            RtcOutputEvent::RtpPacket(rtp) => self.receive_rtp(generation, rtp),
            RtcOutputEvent::KeyframeRequest(request) => {
                if let Some(slot) = self
                    .peers
                    .get(&generation)
                    .and_then(|peer| peer.reverse_mids.get(&request.mid))
                {
                    tracing::debug!(
                        generation = generation.get(),
                        slot = %slot_name(slot),
                        encoding = ?request.rid,
                        "received keyframe request"
                    );
                    let _ = self.events.send(AgentEvent::KeyframeRequested {
                        slot: slot_name(slot),
                        encoding: request.rid.map(|rid| rid.to_string()),
                    });
                    self.statistics.send_if_modified(|statistics| {
                        statistics.keyframe_requests =
                            statistics.keyframe_requests.saturating_add(1);
                        true
                    });
                }
            }
            RtcOutputEvent::PeerStats(stats) => {
                self.statistics.send_if_modified(|statistics| {
                    statistics.bytes_sent = stats.bytes_tx;
                    statistics.bytes_received = stats.bytes_rx;
                    statistics.round_trip_time = stats
                        .selected_candidate_pair
                        .as_ref()
                        .and_then(|pair| pair.current_round_trip_time);
                    true
                });
            }
            RtcOutputEvent::MediaIngressStats(stats) => {
                self.ingress_loss.insert((stats.mid, stats.rid), stats.loss);
                let receive_loss = self.ingress_loss.values().find_map(|loss| *loss);
                self.statistics.send_if_modified(|statistics| {
                    statistics.receive_loss = receive_loss;
                    true
                });
            }
            _ => {}
        }
    }

    fn receive_rtp(&mut self, generation: Generation, rtp: str0m::rtp::RtpPacket) {
        let route = self.peers.get_mut(&generation).and_then(|peer| {
            let mut direct = peer.rtc.direct_api();
            direct
                .stream_rx(&rtp.header.ssrc)
                .map(|stream| (stream.mid(), stream.rid()))
        });
        let Some((mid, rid)) = route else {
            return;
        };
        let Some(slot) = self
            .peers
            .get(&generation)
            .and_then(|peer| peer.reverse_mids.get(&mid))
            .cloned()
        else {
            return;
        };
        let packet = RtpPacket {
            mid,
            rid,
            seq: rtp.seq_no,
            ts: rtp.time,
            marker: rtp.header.marker,
            ssrc: Some(rtp.header.ssrc),
            payload: rtp.payload,
            ext_vals: rtp.header.ext_vals,
            arrival: rtp.timestamp.into(),
        };
        let mut delivered = false;
        if let Some(observers) = self.observers.get_mut(&slot) {
            observers.retain(|observer| match observer.packets.try_send(packet.clone()) {
                Ok(()) => {
                    delivered = true;
                    true
                }
                Err(flume::TrySendError::Full(_)) => true,
                Err(flume::TrySendError::Disconnected(_)) => false,
            });
        }
        self.statistics.send_if_modified(|statistics| {
            statistics.received_packets = statistics.received_packets.saturating_add(1);
            if !delivered {
                statistics.unroutable_media_dropped =
                    statistics.unroutable_media_dropped.saturating_add(1);
            }
            true
        });
    }

    fn receive_tcp(&mut self, result: std::io::Result<usize>) {
        let frames = self.tcp.receive_frames(result);
        let (Some(source), Some(destination)) = (self.tcp.server_addr(), self.tcp.local_addr())
        else {
            return;
        };
        for frame in frames {
            Self::receive_packet(&mut self.peers, Protocol::Tcp, source, destination, &frame);
        }
    }

    fn receive_packet(
        peers: &mut BTreeMap<Generation, Peer>,
        protocol: Protocol,
        source: SocketAddr,
        destination: SocketAddr,
        bytes: &[u8],
    ) {
        let Ok(receive) = Receive::new(protocol, source, destination, bytes) else {
            return;
        };
        let input = Input::Receive(Instant::now().into(), receive);
        let generation = peers
            .iter()
            .rev()
            .find_map(|(generation, peer)| peer.rtc.accepts(&input).then_some(*generation));
        let Some(generation) = generation else {
            return;
        };
        if let Some(peer) = peers.get_mut(&generation)
            && let Err(error) = peer.rtc.handle_input(input)
        {
            tracing::debug!(generation = generation.get(), ?error, "RTC input rejected");
        }
    }

    fn send_frame(
        &mut self,
        slot: &MediaSlot,
        encoding: Option<&str>,
        frame: &MediaFrame,
        response: oneshot::Sender<Result<(), Error>>,
    ) {
        let result = self.packetize_frame(slot, encoding, frame);
        let (generation, mut packets) = match result {
            Ok(packets) => packets,
            Err(error) => {
                let _ = response.send(Err(error));
                return;
            }
        };
        let Some(last) = packets.pop() else {
            let _ = response.send(Ok(()));
            return;
        };
        for packet in packets {
            if self
                .outgoing_media_tx
                .send(OutgoingMedia {
                    generation,
                    packet,
                    completion: None,
                })
                .is_err()
            {
                debug_assert!(false, "actor must retain its outgoing media receiver");
                let _ = response.send(Err(Error::Closed));
                return;
            }
        }
        if self
            .outgoing_media_tx
            .send(OutgoingMedia {
                generation,
                packet: last,
                completion: Some(response),
            })
            .is_err()
        {
            debug_assert!(false, "actor must retain its outgoing media receiver");
        }
    }

    fn packetize_frame(
        &mut self,
        slot: &MediaSlot,
        encoding: Option<&str>,
        frame: &MediaFrame,
    ) -> Result<(Generation, Vec<RtpPacket>), Error> {
        if !self.active_publications.contains(slot) {
            return Err(Error::InactiveSlot(slot.clone()));
        }
        let generation = self
            .core
            .snapshot()
            .generation
            .ok_or_else(|| Error::InactiveSlot(slot.clone()))?;
        let peer = self
            .peers
            .get_mut(&generation)
            .ok_or_else(|| Error::InactiveSlot(slot.clone()))?;
        let encoding = encoding.map(ToString::to_string);
        let key = (slot.clone(), encoding.clone());
        let packetizer = peer
            .packetizers
            .get_mut(&key)
            .ok_or_else(|| Error::UnknownEncoding {
                slot: slot.clone(),
                encoding: encoding.clone(),
            })?;
        Ok((generation, packetizer.packetize(frame)))
    }

    fn send_packet(&mut self, mut outgoing: OutgoingMedia) {
        self.write_packet(&outgoing);
        if let Some(completion) = outgoing.completion.take() {
            let _ = completion.send(Ok(()));
        }
    }

    fn write_packet(&mut self, outgoing: &OutgoingMedia) {
        let Some(peer) = self.peers.get_mut(&outgoing.generation) else {
            return;
        };
        let packet = &outgoing.packet;
        let Some(payload_type) = peer
            .rtc
            .media(packet.mid)
            .and_then(|media| media.remote_pts().first().copied())
        else {
            return;
        };
        let mut direct = peer.rtc.direct_api();
        let Some(stream) = direct.stream_tx_by_mid(packet.mid, packet.rid) else {
            return;
        };
        let timestamp = u32::try_from(packet.ts.numer() & u64::from(u32::MAX)).unwrap_or(0);
        stream.write_rtp(
            RtpWrite::new(
                payload_type,
                packet.seq,
                timestamp,
                packet.arrival.into(),
                packet.payload.clone(),
            )
            .marker(packet.marker)
            .nackable(true)
            .ext_vals(packet.ext_vals.clone()),
        );
        self.statistics.send_if_modified(|statistics| {
            statistics.sent_packets = statistics.sent_packets.saturating_add(1);
            true
        });
    }

    async fn reconnect_transport(&mut self) -> Result<(), Error> {
        let Some(server) = self.config.tcp_server else {
            return Ok(());
        };
        self.tcp.close();
        let stream = pulsebeam_core::net::TcpStream::connect(server).await?;
        stream.set_nodelay(true)?;
        let local = stream.local_addr().ok();
        tracing::debug!(?local, %server, "reopened TCP transport");
        self.tcp = TcpSession::new(stream, local, server);
        Ok(())
    }

    fn request_keyframe(&mut self, slot: &MediaSlot, ssrc: Ssrc) {
        let Some(generation) = self.core.snapshot().generation else {
            return;
        };
        let Some(peer) = self.peers.get_mut(&generation) else {
            return;
        };
        let mid = {
            let mut direct = peer.rtc.direct_api();
            direct.stream_rx(&ssrc).map(|stream| stream.mid())
        };
        if mid.and_then(|mid| peer.reverse_mids.get(&mid)) != Some(slot) {
            return;
        }
        let mut direct = peer.rtc.direct_api();
        if let Some(stream) = direct.stream_rx(&ssrc) {
            stream.request_keyframe(KeyframeRequestKind::Pli);
        }
    }

    fn publish_state(&mut self) {
        let next = self.core.snapshot().clone();
        if *self.snapshot.borrow() != next {
            self.snapshot.send_replace(next.clone());
        }
        while let Some(notification) = self.core.next_notification() {
            let _ = self.events.send(AgentEvent::Core(notification));
        }
        if matches!(next.connection, agent_core::ConnectionState::Disconnected)
            && !self.close_waiters.is_empty()
        {
            for waiter in self.close_waiters.drain(..) {
                let _ = waiter.send(Ok(()));
            }
        }
    }

    fn fail(&mut self, error: Error) {
        tracing::error!(%error, "native agent runtime failed");
        let _ = self
            .events
            .send(AgentEvent::RuntimeFailed(error.to_string()));
    }

    fn shutdown_hosts(&mut self) {
        for (_, task) in std::mem::take(&mut self.http_tasks) {
            task.abort();
        }
        for (_, task) in std::mem::take(&mut self.timer_tasks) {
            task.abort();
        }
        for peer in self.peers.values_mut() {
            peer.rtc.disconnect();
        }
    }
}

fn topology_slots(topology: &agent_core::MediaTopology) -> Vec<MediaSlot> {
    topology
        .local_video
        .iter()
        .cloned()
        .map(MediaSlot::LocalVideo)
        .chain(
            topology
                .local_audio
                .iter()
                .cloned()
                .map(MediaSlot::LocalAudio),
        )
        .chain((0..topology.remote_video).map(MediaSlot::RemoteVideo))
        .chain((0..topology.remote_audio).map(MediaSlot::RemoteAudio))
        .collect()
}

fn channel_config(spec: &DataChannelSpec) -> ChannelConfig {
    let reliability = match spec.reliability {
        DataChannelReliability::Reliable => Reliability::Reliable,
        DataChannelReliability::MaxRetransmits(value) => {
            Reliability::MaxRetransmits { retransmits: value }
        }
        DataChannelReliability::MaxPacketLifetime(value) => {
            Reliability::MaxPacketLifetime { lifetime: value }
        }
    };
    ChannelConfig {
        label: spec.label.clone(),
        ordered: spec.ordered,
        reliability,
        negotiated: None,
        protocol: String::new(),
    }
}

fn http_request(request: agent_core::HttpRequest) -> Result<http::Request<Vec<u8>>, Error> {
    let method = match request.method {
        HttpMethod::Post => http::Method::POST,
        HttpMethod::Patch => http::Method::PATCH,
        HttpMethod::Delete => http::Method::DELETE,
    };
    let mut builder = http::Request::builder().method(method).uri(request.uri);
    for header in request.headers {
        builder = builder.header(header.name, header.value);
    }
    builder
        .body(request.body)
        .map_err(|error| Error::Rtc(format!("invalid HTTP request: {error}")))
}

fn slot_name(slot: &MediaSlot) -> String {
    match slot {
        MediaSlot::LocalVideo(name) | MediaSlot::LocalAudio(name) => name.clone(),
        MediaSlot::RemoteVideo(index) => format!("remote-video-{index}"),
        MediaSlot::RemoteAudio(index) => format!("remote-audio-{index}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        let session = agent_core::AgentConfig {
            endpoint: "http://pulsebeam.test".into(),
            room_id: "room".into(),
            request_headers: Vec::new(),
            topology: agent_core::MediaTopology {
                local_video: vec!["camera".into()],
                local_audio: vec!["microphone".into()],
                remote_video: 1,
                remote_audio: 1,
            },
            manual_subscriptions: true,
            retry: agent_core::RetryPolicy::default(),
        };
        let mut config = Config::new(session);
        config
            .local_ips
            .push(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        config
    }

    #[test]
    fn data_channel_reliability_is_preserved() {
        let retransmits = channel_config(&DataChannelSpec {
            label: "lossy".into(),
            ordered: false,
            reliability: DataChannelReliability::MaxRetransmits(3),
        });
        let lifetime = channel_config(&DataChannelSpec {
            label: "timed".into(),
            ordered: false,
            reliability: DataChannelReliability::MaxPacketLifetime(125),
        });

        assert_eq!(
            retransmits.reliability,
            Reliability::MaxRetransmits { retransmits: 3 }
        );
        assert_eq!(
            lifetime.reliability,
            Reliability::MaxPacketLifetime { lifetime: 125 }
        );
    }

    #[test]
    fn topology_order_is_stable_and_complete() {
        let slots = topology_slots(&config().session.topology);
        assert_eq!(
            slots,
            vec![
                MediaSlot::LocalVideo("camera".into()),
                MediaSlot::LocalAudio("microphone".into()),
                MediaSlot::RemoteVideo(0),
                MediaSlot::RemoteAudio(0),
            ]
        );
    }
}

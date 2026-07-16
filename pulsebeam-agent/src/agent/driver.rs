use crate::MediaFrame;
use crate::agent::controller::{BitrateController, BitrateControllerConfig, LayerController};
use crate::agent::handles::{
    DataPublisher, DataSubscriber, LocalTrack, OutgoingCommand, RemoteTrack,
};
use crate::agent::mailbox;
use crate::agent::slots::SlotManager;
use crate::api::{ApiError, HttpApiClient, UpdateParticipantRequest};
use crate::manager::{Subscription, SubscriptionManager};
use crate::media::KeyframeNotifier;
use crate::tcp::TcpSession;
use http::Uri;
use pulsebeam_core::net::UdpSocket;
use pulsebeam_proto::namespace;
use pulsebeam_proto::prelude::Message;
use pulsebeam_proto::signaling::Track;
use pulsebeam_proto::{signaling, signaling::ServerMessage};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;
use str0m::IceConnectionState;
use str0m::bwe::{Bitrate, BweKind};
use str0m::channel::{ChannelConfig, ChannelData, ChannelId, Reliability};
use str0m::media::{Direction, MediaAdded, MediaKind, Mid, Rid};
use str0m::rtp::AbsCaptureTime;
use str0m::{
    Event, Input, Output, Rtc,
    net::{Protocol, Receive},
};
use tokio::time::Instant;

const MIN_QUANTA: Duration = Duration::from_millis(1);
const STATE_DEBOUNCE: Duration = Duration::from_millis(300);
const BWE_SLOW_INTERVAL: Duration = Duration::from_millis(200);

pub type ParticipantId = String;

#[derive(Debug, Default, Clone)]
pub struct AgentStats {
    pub peer: Option<str0m::stats::PeerStats>,
    pub tracks: HashMap<Mid, TrackStats>,
}

impl AgentStats {
    pub fn total_rx_bytes(&self) -> u64 {
        self.tracks
            .values()
            .flat_map(|t| t.rx_layers.values())
            .map(|s| s.bytes)
            .sum()
    }

    pub fn total_tx_bytes(&self) -> u64 {
        self.tracks
            .values()
            .flat_map(|t| t.tx_layers.values())
            .map(|s| s.bytes)
            .sum()
    }
}

#[derive(Debug, Default, Clone)]
pub struct TrackStats {
    pub kind: Option<MediaKind>,
    pub rx_layers: HashMap<Option<Rid>, str0m::stats::MediaIngressStats>,
    pub tx_layers: HashMap<Option<Rid>, str0m::stats::MediaEgressStats>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoPreset {
    Camera,
    Screen,
}

impl VideoPreset {
    pub fn base_bitrate(&self) -> u64 {
        match self {
            Self::Camera => 1_250_000,
            Self::Screen => 2_500_000,
        }
    }

    pub fn content_hint(&self) -> &str {
        match self {
            Self::Camera => "motion",
            Self::Screen => "text",
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AgentError {
    #[error("API call failed: {0}")]
    Api(#[from] ApiError),
    #[error("RTC Error: {0}")]
    Rtc(#[from] str0m::RtcError),
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Protocol Error: {0}")]
    Protocol(String),
    #[error("No valid network candidates found")]
    NoCandidates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataTrackDirection {
    Publish,
    Subscribe,
}

#[derive(Debug, Clone)]
struct DataTrackBinding {
    direction: DataTrackDirection,
    topic: String,
}

fn data_track_label(direction: DataTrackDirection, topic: &str) -> String {
    let lane = match direction {
        DataTrackDirection::Publish => "pub",
        DataTrackDirection::Subscribe => "sub",
    };
    format!("v1/rt/{lane}/{topic}")
}

fn parse_data_track_label(label: &str) -> Option<(DataTrackDirection, String)> {
    let rest = label.strip_prefix("v1/rt/")?;
    let (lane, topic) = rest.split_once('/')?;
    let direction = match lane {
        "pub" => DataTrackDirection::Publish,
        "sub" => DataTrackDirection::Subscribe,
        _ => return None,
    };
    Some((direction, topic.to_string()))
}

pub enum AgentEvent {
    DataPublisherDeclared(DataPublisher),
    DataSubscriberDeclared(DataSubscriber),
    LocalTrackAdded(LocalTrack),
    RemoteTrackDiscovered(Track),
    RemoteTrackAdded(RemoteTrack),
    Connected,
    Disconnected(String),
}

pub(crate) struct DriverInit {
    pub addr: SocketAddr,
    pub rtc: Rtc,
    pub socket: UdpSocket,
    pub tcp: TcpSession,
    pub api: HttpApiClient,
    pub signaling_cid: ChannelId,
    pub resource_uri: Uri,
    pub participant_id: String,
    pub medias: Vec<MediaAdded>,
}

struct NetworkSubsystem {
    addr: SocketAddr,
    socket: UdpSocket,
    buf: Vec<u8>,
    tcp: TcpSession,
}

struct DataSubsystem {
    signaling_cid: ChannelId,
    data_channels: HashMap<ChannelId, DataTrackBinding>,
    data_pub_topics: HashMap<String, DataPublisher>,
    data_sub_topics: HashMap<String, DataSubscriber>,
    data_targets: HashMap<String, mailbox::Sender<Vec<u8>>>,
}

struct MediaSubsystem {
    media_targets: HashMap<Mid, mailbox::Sender<MediaFrame>>,
    layer_ctrl: LayerController,
    desired_ctrl: BitrateController,
    last_desired: Bitrate,
}

struct SubscriptionSubsystem {
    sub_manager: SubscriptionManager,
    desired_subscriptions: HashMap<String, Subscription>,
    pending_deadline: Option<Instant>,
}

struct SessionSubsystem {
    api: HttpApiClient,
    resource_uri: Uri,
    participant_id: String,
    disconnected_reason: Option<String>,
    retry_count: u32,
    is_reconnecting: bool,
    reconnect_deadline: Option<Instant>,
}

struct TimerSubsystem {
    notifier: tokio::sync::Notify,
    sleep: Pin<Box<tokio::time::Sleep>>,
    rtc_deadline: Option<Instant>,
    bwe_next_tick: Instant,
}

pub struct AgentDriver {
    rtc: Rtc,
    stats: AgentStats,
    pending_events: VecDeque<AgentEvent>,

    outgoing_tx: mailbox::Sender<OutgoingCommand>,
    outgoing_rx: mailbox::Receiver<OutgoingCommand>,

    slot_manager: SlotManager,
    now: Instant,

    network: NetworkSubsystem,
    data: DataSubsystem,
    media: MediaSubsystem,
    subscriptions: SubscriptionSubsystem,
    session: SessionSubsystem,
    timers: TimerSubsystem,
}

impl AgentDriver {
    pub(crate) fn new(init: DriverInit) -> Self {
        let (outgoing_tx, outgoing_rx) = mailbox::bounded(256);
        let now = Instant::now();

        let mut driver = Self {
            rtc: init.rtc,
            stats: AgentStats::default(),
            pending_events: VecDeque::new(),
            outgoing_tx,
            outgoing_rx,
            slot_manager: SlotManager::new(),
            now,
            network: NetworkSubsystem {
                addr: init.addr,
                socket: init.socket,
                buf: vec![0u8; 2048],
                tcp: init.tcp,
            },
            data: DataSubsystem {
                signaling_cid: init.signaling_cid,
                data_channels: HashMap::new(),
                data_pub_topics: HashMap::new(),
                data_sub_topics: HashMap::new(),
                data_targets: HashMap::new(),
            },
            media: MediaSubsystem {
                media_targets: HashMap::new(),
                layer_ctrl: LayerController::new(),
                desired_ctrl: BitrateControllerConfig::default().build(),
                last_desired: Bitrate::bps(0),
            },
            subscriptions: SubscriptionSubsystem {
                sub_manager: SubscriptionManager::new(
                    init.medias
                        .iter()
                        .filter(|m| m.direction == Direction::RecvOnly)
                        .map(|m| m.mid)
                        .collect(),
                ),
                desired_subscriptions: HashMap::new(),
                pending_deadline: None,
            },
            session: SessionSubsystem {
                api: init.api,
                resource_uri: init.resource_uri,
                participant_id: init.participant_id,
                disconnected_reason: None,
                retry_count: 0,
                is_reconnecting: false,
                reconnect_deadline: None,
            },
            timers: TimerSubsystem {
                notifier: tokio::sync::Notify::new(),
                sleep: Box::pin(tokio::time::sleep(MIN_QUANTA)),
                rtc_deadline: None,
                bwe_next_tick: now + BWE_SLOW_INTERVAL,
            },
        };

        for media in init.medias {
            driver.handle_media_added(media);
        }

        driver
    }

    pub fn stats(&self) -> &AgentStats {
        &self.stats
    }

    pub fn participant_id(&self) -> &ParticipantId {
        &self.session.participant_id
    }

    pub fn declare_publish_topic(&mut self, topic: &str) -> Result<ChannelId, AgentError> {
        let cid = self.ensure_data_topic(DataTrackDirection::Publish, topic)?;
        self.data.data_pub_topics.insert(
            topic.to_string(),
            DataPublisher::new(cid, topic.to_string(), self.outgoing_tx.clone()),
        );
        Ok(cid)
    }

    pub fn declare_subscribe_topic(&mut self, topic: &str) -> Result<ChannelId, AgentError> {
        let cid = self.ensure_data_topic(DataTrackDirection::Subscribe, topic)?;
        let (tx, rx) = mailbox::bounded(8);
        self.data.data_sub_topics.insert(
            topic.to_string(),
            DataSubscriber::new(cid, topic.to_string(), rx),
        );
        self.data.data_targets.insert(topic.to_string(), tx);
        Ok(cid)
    }

    pub async fn shutdown(&mut self) {
        if let Err(e) = self
            .session
            .api
            .delete_participant_by_uri(self.session.resource_uri.clone())
            .await
        {
            tracing::warn!(error = ?e, "failed to delete participant on shutdown");
        }
        self.rtc.disconnect();
        self.timers.notifier.notify_one();
    }

    pub fn set_subscriptions(&mut self, subs: Vec<Subscription>) {
        self.subscriptions.desired_subscriptions.clear();
        for sub in subs {
            self.handle_outgoing_command(OutgoingCommand::SetSubscription(sub));
        }
        self.subscriptions.pending_deadline = Some(self.now + STATE_DEBOUNCE);
        self.flush_pending_state();
        self.timers.notifier.notify_one();
    }

    pub async fn poll(&mut self) -> Option<AgentEvent> {
        if let Some(ev) = self.pending_events.pop_front() {
            return Some(ev);
        }

        loop {
            let Some(deadline) = self.poll_rtc() else {
                return self.pending_events.pop_front();
            };
            self.timers.rtc_deadline = Some(deadline);

            if let Some(ev) = self.pending_events.pop_front() {
                return Some(ev);
            }

            self.now = Instant::now();
            self.process_due_timers().await;

            if let Some(ev) = self.pending_events.pop_front() {
                return Some(ev);
            }

            self.reset_sleep_to_next_deadline();

            tokio::select! {
                biased;
                _ = self.timers.notifier.notified() => {}
                res = self.network.socket.recv_from(&mut self.network.buf) => {
                    if let Ok((n, source)) = res {
                        match self.network.buf[..n].try_into() {
                            Ok(contents) => {
                                let _ = self.rtc.handle_input(Input::Receive(
                                    Instant::now().into(),
                                    Receive {
                                        proto: Protocol::Udp,
                                        source,
                                        destination: self.network.addr,
                                        contents,
                                    }
                                ));
                            }
                            Err(_) => {
                                tracing::warn!(n, "UDP datagram too large for RTC buffer, discarding");
                            }
                        }
                    }
                }
                res = self.network.tcp.wait_recv() => {
                    self.network.tcp.on_recv(res, &mut self.rtc);
                }
                Ok(cmd) = self.outgoing_rx.recv() => {
                    self.handle_outgoing_command(cmd);
                }
                _ = self.timers.sleep.as_mut() => {
                    self.on_sleep_tick().await;
                }
            }
        }
    }

    pub async fn run(mut self) {
        while self.poll().await.is_some() {}
        self.shutdown().await;
    }

    fn reset_sleep_to_next_deadline(&mut self) {
        let next = self
            .next_deadline()
            .unwrap_or_else(|| Instant::now() + MIN_QUANTA);
        if self.timers.sleep.deadline() != next {
            self.timers.sleep.as_mut().reset(next);
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        min_deadline(
            self.timers.rtc_deadline,
            min_deadline(
                self.subscriptions.pending_deadline,
                min_deadline(
                    self.session.reconnect_deadline,
                    Some(self.timers.bwe_next_tick),
                ),
            ),
        )
    }

    async fn on_sleep_tick(&mut self) {
        self.now = Instant::now();

        if self
            .timers
            .rtc_deadline
            .is_some_and(|deadline| self.now >= deadline)
        {
            match self.rtc.handle_input(Input::Timeout(self.now.into())) {
                Ok(_) => {}
                Err(_) => self.emit(AgentEvent::Disconnected("RTC Timeout".into())),
            }
        }

        self.process_due_timers().await;
    }

    async fn process_due_timers(&mut self) {
        let now = self.now;

        if self
            .subscriptions
            .pending_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.flush_pending_state();
        }

        if self
            .session
            .reconnect_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.perform_reconnect().await;
        }

        while now >= self.timers.bwe_next_tick {
            let desired_bps = self.media.layer_ctrl.tick(now);
            let desired_bitrate = Bitrate::from(desired_bps.max(0.0) as u64);
            let filtered_bitrate = self.media.desired_ctrl.update(desired_bitrate);
            if filtered_bitrate != self.media.last_desired {
                self.media.last_desired = filtered_bitrate;
                self.rtc.bwe().set_desired_bitrate(filtered_bitrate);
            }
            self.timers.bwe_next_tick += BWE_SLOW_INTERVAL;
        }
    }

    fn handle_outgoing_command(&mut self, cmd: OutgoingCommand) {
        match cmd {
            OutgoingCommand::SendData(e) => {
                let Some(mut channel) = self.rtc.channel(e.channel_id) else {
                    return;
                };
                let _ = channel.write(true, &e.payload);
            }
            OutgoingCommand::SendMedia(e) => {
                let paused = self.media.layer_ctrl.is_paused(e.mid, e.rid);
                self.media.layer_ctrl.record_frame(
                    e.mid,
                    e.rid,
                    e.frame.data.len(),
                    Instant::now(),
                );

                if paused {
                    return;
                }

                if let Some(mut writer) = self.rtc.writer(e.mid) {
                    let Some(pt) = writer.payload_params().next().map(|p| p.pt()) else {
                        return;
                    };
                    if let Some(rid) = e.rid {
                        writer = writer.rid(rid);
                    }
                    if let Some(abs_capture_time) = e.frame.abs_capture_time {
                        writer = writer.abs_capture_time(AbsCaptureTime {
                            capture_time: abs_capture_time,
                            clock_offset: None,
                        });
                    }
                    let _ = writer.write(pt, e.frame.capture_time.into(), e.frame.ts, e.frame.data);
                }
            }
            OutgoingCommand::SetSubscription(sub) => {
                self.subscriptions
                    .desired_subscriptions
                    .insert(sub.track_id.clone(), sub);
                let desired = self
                    .subscriptions
                    .desired_subscriptions
                    .values()
                    .cloned()
                    .collect();
                self.subscriptions.sub_manager.set_desired(desired);
                self.subscriptions.pending_deadline = Some(self.now + STATE_DEBOUNCE);
            }
        }
    }

    fn poll_rtc(&mut self) -> Option<Instant> {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Transmit(tx)) => match tx.proto {
                    Protocol::Udp => {
                        let _ = self
                            .network
                            .socket
                            .try_send_to(&tx.contents, tx.destination);
                    }
                    Protocol::Tcp => {
                        self.network.tcp.try_send(&tx.contents);
                    }
                    _ => {}
                },
                Ok(Output::Event(e)) => match e {
                    Event::ChannelOpen(cid, label) => {
                        if label == namespace::Signaling::Reliable.as_str() {
                            self.data.signaling_cid = cid;
                            self.subscriptions.sub_manager.reset_active_assignments();
                            self.subscriptions.pending_deadline = Some(Instant::now());
                        } else if let Some((direction, topic)) = parse_data_track_label(&label) {
                            self.data
                                .data_channels
                                .entry(cid)
                                .or_insert(DataTrackBinding {
                                    direction,
                                    topic: topic.to_string(),
                                });
                            match direction {
                                DataTrackDirection::Publish => {
                                    if let Some(publisher) = self.data.data_pub_topics.get(&topic) {
                                        self.emit(AgentEvent::DataPublisherDeclared(
                                            publisher.clone(),
                                        ));
                                    } else {
                                        tracing::warn!("no pending pub topic for {}.", topic);
                                    }
                                }
                                DataTrackDirection::Subscribe => {
                                    if let Some(sub) = self.data.data_sub_topics.get(&topic) {
                                        self.emit(AgentEvent::DataSubscriberDeclared(sub.clone()));
                                    } else {
                                        tracing::warn!("no pending sub topic for {}.", topic);
                                    }
                                }
                            }
                        }
                    }
                    Event::ChannelData(data) => {
                        if data.id == self.data.signaling_cid {
                            self.handle_signaling_data(data);
                        } else {
                            self.dispatch_data_message(data);
                        }
                    }
                    Event::MediaAdded(media) => self.handle_media_added(media),
                    Event::MediaData(data) => {
                        if let Some(tx) = self.media.media_targets.get(&data.mid) {
                            let _ = tx.try_send(data.into());
                        }
                    }
                    Event::IceConnectionStateChange(state) => {
                        if state == IceConnectionState::Disconnected {
                            self.schedule_reconnect(Instant::now());
                        }
                    }
                    Event::Connected => {
                        self.emit(AgentEvent::Connected);
                    }
                    Event::PeerStats(stats) => {
                        self.stats.peer = Some(stats);
                    }
                    Event::MediaIngressStats(stats) => {
                        let track_stats = self.stats.tracks.entry(stats.mid).or_default();
                        track_stats.rx_layers.insert(stats.rid, stats);
                    }
                    Event::MediaEgressStats(stats) => {
                        let track_stats = self.stats.tracks.entry(stats.mid).or_default();
                        track_stats.tx_layers.insert(stats.rid, stats);
                    }
                    Event::KeyframeRequest(req) => {
                        self.media
                            .layer_ctrl
                            .request_keyframe(req.mid, req.rid, req.kind);
                    }
                    Event::EgressBitrateEstimate(BweKind::Twcc(available)) => {
                        self.media.layer_ctrl.update_available(available);
                    }
                    _ => {}
                },
                Ok(Output::Timeout(t)) => {
                    return Some(t.into());
                }
                Err(e) => {
                    self.session.disconnected_reason = Some(format!("RTC Error: {:?}", e));
                    self.rtc.disconnect();
                    return None;
                }
            }
        }
    }

    fn dispatch_data_message(&mut self, data: ChannelData) {
        let Some(binding) = self.data.data_channels.get(&data.id) else {
            return;
        };
        let Some(target) = self.data.data_targets.get(&binding.topic) else {
            return;
        };
        let _ = target.try_send(data.data);
    }

    fn handle_media_added(&mut self, media: MediaAdded) {
        let mid = media.mid;
        self.stats.tracks.entry(mid).or_default().kind = Some(media.kind);
        match media.direction {
            Direction::SendOnly => {
                let rids = if let Some(layers) = media.simulcast {
                    layers.send.iter().map(|s| Some(s.rid)).collect()
                } else {
                    vec![None]
                };

                for rid in rids {
                    let (kf_notifier, kf_rx) = KeyframeNotifier::pair();
                    if media.kind.is_video() {
                        self.media.layer_ctrl.register(mid, rid, kf_notifier);
                    }
                    self.emit(AgentEvent::LocalTrackAdded(LocalTrack {
                        kind: media.kind,
                        mid,
                        rid,
                        keyframe_rx: kf_rx,
                        tx: self.outgoing_tx.clone(),
                    }));
                }
            }
            Direction::RecvOnly => {
                self.slot_manager.register(mid);
            }
            _ => {}
        }
    }

    fn handle_signaling_data(&mut self, cd: ChannelData) {
        let Ok(msg) = ServerMessage::decode(cd.data.as_slice()) else {
            return;
        };

        let Some(payload) = msg.payload else {
            return;
        };

        match payload {
            signaling::server_message::Payload::Update(update) => {
                let (assignments, discovered) = self.slot_manager.sync(update);
                for track in discovered {
                    self.emit(AgentEvent::RemoteTrackDiscovered(track));
                }
                for (mid, track) in assignments {
                    let (tx, rx) = mailbox::bounded(256);
                    self.media.media_targets.insert(mid, tx);
                    self.emit(AgentEvent::RemoteTrackAdded(RemoteTrack { mid, track, rx }));
                }
            }
            signaling::server_message::Payload::Error(err) => {
                tracing::warn!("signaling error: {}", err);
            }
        }
    }

    fn emit(&mut self, event: AgentEvent) {
        self.pending_events.push_back(event);
    }

    fn flush_pending_state(&mut self) {
        let Some(mut ch) = self.rtc.channel(self.data.signaling_cid) else {
            self.subscriptions.pending_deadline = Some(self.now + STATE_DEBOUNCE);
            return;
        };

        let requests = self.subscriptions.sub_manager.reconcile();
        if requests.is_empty() {
            self.subscriptions.pending_deadline = None;
            return;
        }

        let msg = signaling::ClientMessage {
            payload: Some(signaling::client_message::Payload::Intent(
                signaling::ClientIntent {
                    upstream_intents: vec![],
                    downstream_requests: requests,
                },
            )),
        };
        let encoded = msg.encode_to_vec();
        if let Err(err) = ch.write(true, encoded.as_slice()) {
            tracing::warn!("failed to send signaling: {:?}", err);
            self.subscriptions.pending_deadline = Some(self.now + STATE_DEBOUNCE);
        } else {
            self.subscriptions.pending_deadline = None;
        }
    }

    fn schedule_reconnect(&mut self, now: Instant) {
        if self.session.is_reconnecting {
            return;
        }

        let delay = match self.session.retry_count {
            0 => Duration::ZERO,
            1 => Duration::from_millis(500),
            n => Duration::from_millis(500 * 2u64.pow(n.min(10) - 1)).min(Duration::from_secs(5)),
        };

        self.session.retry_count += 1;
        self.session.reconnect_deadline = Some(now + delay);
    }

    async fn perform_reconnect(&mut self) {
        self.session.is_reconnecting = true;
        self.session.reconnect_deadline = None;
        self.stats.peer = None;

        match self.try_reconnect().await {
            Ok(_) => {
                self.session.is_reconnecting = false;
                self.session.retry_count = 0;
                self.emit(AgentEvent::Connected);
            }
            Err(_) => {
                self.session.is_reconnecting = false;
                self.schedule_reconnect(Instant::now());
            }
        }
    }

    async fn try_reconnect(&mut self) -> Result<(), AgentError> {
        self.renegotiate().await
    }

    async fn renegotiate(&mut self) -> Result<(), AgentError> {
        let (offer, pending) = {
            let sdp_api = self.rtc.sdp_api();
            match sdp_api.apply() {
                Some(pair) => pair,
                None => {
                    return Ok(());
                }
            }
        };

        let resp = self
            .session
            .api
            .update_participant(
                self.session.resource_uri.clone(),
                UpdateParticipantRequest { offer },
            )
            .await?;

        self.rtc
            .sdp_api()
            .accept_answer(pending, resp.answer)
            .map_err(AgentError::Rtc)?;

        Ok(())
    }

    fn ensure_data_topic(
        &mut self,
        direction: DataTrackDirection,
        topic: &str,
    ) -> Result<ChannelId, AgentError> {
        let existing = match direction {
            DataTrackDirection::Publish => {
                self.data.data_pub_topics.get(topic).map(|p| p.channel_id)
            }
            DataTrackDirection::Subscribe => {
                self.data.data_sub_topics.get(topic).map(|s| s.channel_id)
            }
        };
        if let Some(cid) = existing {
            return Ok(cid);
        }

        let topic_owned = topic.to_string();
        let cfg = ChannelConfig {
            label: data_track_label(direction, &topic_owned),
            ordered: false,
            reliability: Reliability::MaxRetransmits { retransmits: 0 },
            negotiated: None,
            protocol: "".to_string(),
        };
        let mut sdp_api = self.rtc.sdp_api();
        let cid = sdp_api.add_channel_with_config(cfg);
        if let Some((_offer, _pending)) = sdp_api.apply() {
            return Err(AgentError::Protocol(
                "data channel declaration unexpectedly requires renegotiation".into(),
            ));
        }

        Ok(cid)
    }
}

fn min_deadline(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

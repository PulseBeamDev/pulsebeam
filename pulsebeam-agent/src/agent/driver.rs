use crate::MediaFrame;
use crate::agent::controller::{BitrateController, BitrateControllerConfig, LayerController};
use crate::agent::handles::{
    DataPublisher, DataSubscriber, LocalEncoding, OrderedTopicPublisher, OrderedTopicSubscriber,
    OutgoingCommand, PublicationLease, RemoteTrack,
};
use crate::agent::mailbox;
use crate::agent::ordered_topic::OrderedTopics;
use crate::agent::slots::SlotManager;
use crate::api::{ApiError, HttpApiClient, UpdateParticipantRequest};
use crate::manager::{SubscriptionManager, VideoSubscription};
use crate::media::{KeyframeNotifier, KeyframeReceiver};
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
use str0m::rtp::vla::{
    ResolutionAndFramerate, SimulcastStreamAllocation, SpatialLayerAllocation,
    TemporalLayerAllocation, VideoLayersAllocation,
};
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
pub struct StatisticsSnapshot {
    pub(crate) peer: Option<str0m::stats::PeerStats>,
    pub(crate) tracks: HashMap<Mid, TrackStats>,
}

impl StatisticsSnapshot {
    pub fn is_connected(&self) -> bool {
        self.peer.is_some()
    }

    pub fn bytes_sent(&self) -> u64 {
        self.peer.as_ref().map_or(0, |peer| peer.bytes_tx)
    }

    pub fn bytes_received(&self) -> u64 {
        self.peer.as_ref().map_or(0, |peer| peer.bytes_rx)
    }

    pub fn round_trip_time(&self) -> Option<Duration> {
        self.peer
            .as_ref()?
            .selected_candidate_pair
            .as_ref()?
            .current_round_trip_time
    }

    pub fn receive_loss(&self) -> Option<f32> {
        self.tracks
            .values()
            .flat_map(|track| track.rx_layers.values())
            .find_map(|layer| layer.loss)
    }

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
pub(crate) struct TrackStats {
    kind: Option<MediaKind>,
    rx_layers: HashMap<Option<Rid>, str0m::stats::MediaIngressStats>,
    tx_layers: HashMap<Option<Rid>, str0m::stats::MediaEgressStats>,
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
    #[error("Agent runner is no longer available")]
    Closed,
    #[error("No reserved {0:?} publication is available")]
    MediaCapacity(MediaKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataTrackDirection {
    Publish,
    Subscribe,
}

#[derive(Debug, Clone)]
struct DataTrackBinding {
    topic: String,
    scope: Option<String>,
}

fn data_track_label(direction: DataTrackDirection, topic: &str, scope: Option<&str>) -> String {
    debug_assert!(!topic.is_empty());
    debug_assert!(scope.is_none() || direction == DataTrackDirection::Subscribe);
    debug_assert!(scope.is_none_or(|scope| !scope.is_empty() && !scope.contains('/')));

    let lane = match direction {
        DataTrackDirection::Publish => "pub",
        DataTrackDirection::Subscribe => "sub",
    };
    match scope {
        Some(scope) => format!("v1/rt/{lane}/{topic}/{scope}"),
        None => format!("v1/rt/{lane}/{topic}"),
    }
}

fn parse_data_track_label(label: &str) -> Option<(DataTrackDirection, String, Option<String>)> {
    let rest = label.strip_prefix("v1/rt/")?;
    let (lane, rest) = rest.split_once('/')?;
    let direction = match lane {
        "pub" => DataTrackDirection::Publish,
        "sub" => DataTrackDirection::Subscribe,
        _ => return None,
    };
    let (topic, scope) = match rest.split_once('/') {
        Some((topic, scope)) => (topic.to_string(), Some(scope.to_string())),
        None => (rest.to_string(), None),
    };
    if direction == DataTrackDirection::Publish && scope.is_some() {
        return None;
    }
    debug_assert!(!topic.is_empty());
    debug_assert!(scope.as_ref().is_none_or(|scope| !scope.is_empty()));
    Some((direction, topic, scope))
}

pub(crate) enum AgentEvent {
    StatsUpdated,
    RemoteTrackDiscovered(Track),
    RemoteTrackRemoved(String),
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
    data_pub_topics: HashMap<String, ChannelId>,
    data_sub_topics: HashMap<(String, Option<String>), ChannelId>,
    data_targets: HashMap<(String, Option<String>), mailbox::Sender<Vec<u8>>>,
}

struct MediaSubsystem {
    media_targets: HashMap<Mid, mailbox::Sender<MediaFrame>>,
    upstream_slots: HashMap<Mid, UpstreamSlot>,
    pending_media_subscriptions:
        HashMap<String, tokio::sync::oneshot::Sender<Result<RemoteTrack, AgentError>>>,
    layer_ctrl: LayerController,
    desired_ctrl: BitrateController,
    last_desired: Bitrate,
}

struct UpstreamSlot {
    kind: MediaKind,
    generation: u64,
    active: bool,
    encodings: Vec<(Option<Rid>, KeyframeReceiver)>,
}

impl UpstreamSlot {
    fn activate(&mut self, mid: Mid) -> PublicationLease {
        debug_assert!(!self.active);
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.generation = 1;
        }
        self.active = true;
        PublicationLease {
            mid,
            generation: self.generation,
        }
    }

    fn accepts(&self, lease: PublicationLease) -> bool {
        self.active && self.generation == lease.generation
    }

    fn deactivate(&mut self, lease: PublicationLease) -> bool {
        if !self.accepts(lease) {
            return false;
        }
        self.active = false;
        true
    }
}

struct SubscriptionSubsystem {
    sub_manager: SubscriptionManager,
    desired_subscriptions: HashMap<String, VideoSubscription>,
    pending_deadline: Option<Instant>,
    /// (min, max) receiver playout delay in ms; `None` = adaptive default.
    playout_delay_ms: Option<(u32, u32)>,
    upstream_active: HashMap<Mid, bool>,
    upstream_dirty: bool,
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

pub(crate) struct AgentDriver {
    rtc: Rtc,
    stats: StatisticsSnapshot,
    pending_events: VecDeque<AgentEvent>,
    shutdown_responses: Vec<tokio::sync::oneshot::Sender<()>>,
    shutdown_requested: bool,

    outgoing_tx: mailbox::Sender<OutgoingCommand>,
    outgoing_rx: mailbox::Receiver<OutgoingCommand>,

    slot_manager: SlotManager,
    now: Instant,

    network: NetworkSubsystem,
    data: DataSubsystem,
    ordered_topics: OrderedTopics,
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
            stats: StatisticsSnapshot::default(),
            pending_events: VecDeque::new(),
            shutdown_responses: Vec::new(),
            shutdown_requested: false,
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
            ordered_topics: OrderedTopics::new(),
            media: MediaSubsystem {
                media_targets: HashMap::new(),
                upstream_slots: HashMap::new(),
                pending_media_subscriptions: HashMap::new(),
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
                playout_delay_ms: None,
                upstream_active: HashMap::new(),
                upstream_dirty: false,
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

    pub fn stats(&self) -> &StatisticsSnapshot {
        &self.stats
    }

    pub fn participant_id(&self) -> &ParticipantId {
        &self.session.participant_id
    }

    pub(crate) fn command_sender(&self) -> mailbox::Sender<OutgoingCommand> {
        self.outgoing_tx.clone()
    }

    pub(crate) fn take_shutdown_responses(&mut self) -> Vec<tokio::sync::oneshot::Sender<()>> {
        std::mem::take(&mut self.shutdown_responses)
    }

    fn declare_latest_publisher(&mut self, topic: &str) -> Result<DataPublisher, AgentError> {
        let cid = self.ensure_data_topic(DataTrackDirection::Publish, topic, None)?;
        self.data.data_pub_topics.insert(topic.to_string(), cid);
        Ok(DataPublisher::new(
            cid,
            topic.to_string(),
            self.outgoing_tx.clone(),
        ))
    }

    fn declare_latest_subscriber(
        &mut self,
        topic: &str,
        publisher_id: Option<&str>,
    ) -> Result<DataSubscriber, AgentError> {
        let cid = self.ensure_data_topic(DataTrackDirection::Subscribe, topic, publisher_id)?;
        let (tx, rx) = mailbox::bounded(8);
        let key = (topic.to_string(), publisher_id.map(str::to_string));
        self.data.data_sub_topics.insert(key.clone(), cid);
        self.data.data_targets.insert(key, tx);
        Ok(DataSubscriber::new(
            topic.to_string(),
            publisher_id.map(str::to_string),
            rx,
        ))
    }

    fn declare_ordered_publish_topic(
        &mut self,
        topic: &str,
    ) -> Result<OrderedTopicPublisher, AgentError> {
        self.ordered_topics
            .declare_publisher(&mut self.rtc, topic, self.outgoing_tx.clone())
            .map_err(|()| {
                AgentError::Protocol(
                    "reliable channel declaration unexpectedly requires renegotiation".into(),
                )
            })
    }

    fn declare_ordered_subscribe_topic(
        &mut self,
        topic: &str,
    ) -> Result<OrderedTopicSubscriber, AgentError> {
        self.ordered_topics
            .declare_subscriber(&mut self.rtc, topic)
            .map_err(|()| {
                AgentError::Protocol(
                    "reliable channel declaration unexpectedly requires renegotiation".into(),
                )
            })
    }

    fn publish_local_track(
        &mut self,
        kind: MediaKind,
    ) -> Result<super::session::LocalTrack, AgentError> {
        let Some((&mid, slot)) = self
            .media
            .upstream_slots
            .iter_mut()
            .find(|(_, slot)| slot.kind == kind && !slot.active)
        else {
            return Err(AgentError::MediaCapacity(kind));
        };
        let lease = slot.activate(mid);
        let encodings = slot
            .encodings
            .iter()
            .map(|(rid, keyframe_rx)| LocalEncoding {
                mid,
                rid: *rid,
                lease,
                keyframe_rx: keyframe_rx.clone(),
                tx: self.outgoing_tx.clone(),
            })
            .collect();
        self.set_upstream_active(mid, true);
        Ok(super::session::LocalTrack::new(
            kind,
            lease,
            encodings,
            self.outgoing_tx.clone(),
        ))
    }

    fn unpublish_local_track(&mut self, lease: PublicationLease) {
        let Some(slot) = self.media.upstream_slots.get_mut(&lease.mid) else {
            debug_assert!(
                false,
                "publication lease references an unknown upstream slot"
            );
            return;
        };
        if !slot.deactivate(lease) {
            return;
        }
        self.set_upstream_active(lease.mid, false);
    }

    fn set_upstream_active(&mut self, mid: Mid, active: bool) {
        self.subscriptions.upstream_active.insert(mid, active);
        self.subscriptions.upstream_dirty = true;
        self.subscriptions.pending_deadline = Some(self.now);
        self.timers.notifier.notify_one();
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

    /// Set the receiver playout-delay bounds (ms) signaled to the server. Forces
    /// a full intent resend so the change takes effect even without a
    /// subscription change. `None` restores the adaptive default; `Some((0, 0))`
    /// disables all receiver smoothing.
    fn set_playout_delay(&mut self, bounds: Option<(u32, u32)>) {
        self.subscriptions.playout_delay_ms = bounds;
        self.subscriptions.sub_manager.reset_active_assignments();
        self.subscriptions.pending_deadline = Some(self.now + STATE_DEBOUNCE);
        self.flush_pending_state();
        self.timers.notifier.notify_one();
    }

    pub(crate) async fn poll(&mut self) -> Option<AgentEvent> {
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
                    if self.shutdown_requested {
                        return None;
                    }
                }
                _ = self.timers.sleep.as_mut() => {
                    self.on_sleep_tick().await;
                }
            }
        }
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
                if let Err(payload) =
                    self.ordered_topics
                        .send(&mut self.rtc, e.channel_id, e.payload)
                {
                    let Some(mut channel) = self.rtc.channel(e.channel_id) else {
                        return;
                    };
                    let _ = channel.write(true, &payload);
                }
            }
            OutgoingCommand::SendMedia(e) => {
                let Some(slot) = self.media.upstream_slots.get(&e.lease.mid) else {
                    return;
                };
                if !slot.accepts(e.lease) {
                    return;
                }
                let encoding_exists = slot.encodings.iter().any(|(rid, _)| *rid == e.rid);
                debug_assert!(encoding_exists);
                if !encoding_exists {
                    return;
                }
                let mid = e.lease.mid;
                let paused = self.media.layer_ctrl.is_paused(mid, e.rid);
                self.media
                    .layer_ctrl
                    .record_frame(mid, e.rid, e.frame.data.len(), Instant::now());

                if paused {
                    return;
                }

                if let Some(mut writer) = self.rtc.writer(mid) {
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
                    // Declare the encoder's target so the SFU allocates against it rather than
                    // inferring cost from bytes on the wire, which for screen content is a far
                    // more variable signal (near zero while static, full rate on a scroll).
                    if let Some(target_bps) = e.frame.target_bitrate_bps {
                        writer = writer.user_extension_value(vla_for(target_bps, e.frame.resolution));
                    }
                    let _ = writer.write(pt, e.frame.capture_time.into(), e.frame.ts, e.frame.data);
                }
            }
            OutgoingCommand::SetPlayoutDelay(bounds) => {
                self.set_playout_delay(bounds);
            }
            OutgoingCommand::Publish { kind, response } => {
                let result = self.publish_local_track(kind);
                let _ = response.send(result);
            }
            OutgoingCommand::Unpublish { lease, response } => {
                self.unpublish_local_track(lease);
                if let Some(response) = response {
                    let _ = response.send(Ok(()));
                }
            }
            OutgoingCommand::SubscribeMedia {
                subscription,
                response,
            } => {
                let track_id = subscription.track_id.clone();
                if let Some((mid, track)) = self.slot_manager.assigned(&track_id) {
                    let (tx, rx) = mailbox::bounded(256);
                    self.media.media_targets.insert(mid, tx);
                    let _ = response.send(Ok(RemoteTrack::new(mid, track, rx)));
                    self.subscriptions
                        .desired_subscriptions
                        .insert(track_id, subscription);
                    let desired = self
                        .subscriptions
                        .desired_subscriptions
                        .values()
                        .cloned()
                        .collect();
                    self.subscriptions.sub_manager.set_desired(desired);
                    self.subscriptions.pending_deadline = Some(self.now);
                    self.flush_pending_state();
                    self.timers.notifier.notify_one();
                    return;
                }
                if self
                    .media
                    .pending_media_subscriptions
                    .contains_key(&track_id)
                {
                    let _ = response.send(Err(AgentError::Protocol(
                        "media publication is already being subscribed".into(),
                    )));
                    return;
                }
                self.media
                    .pending_media_subscriptions
                    .insert(track_id.clone(), response);
                self.subscriptions
                    .desired_subscriptions
                    .insert(track_id, subscription);
                let desired = self
                    .subscriptions
                    .desired_subscriptions
                    .values()
                    .cloned()
                    .collect();
                self.subscriptions.sub_manager.set_desired(desired);
                self.subscriptions.pending_deadline = Some(self.now);
                self.flush_pending_state();
                self.timers.notifier.notify_one();
            }
            OutgoingCommand::Shutdown(response) => {
                self.shutdown_responses.push(response);
                self.shutdown_requested = true;
                self.rtc.disconnect();
                self.timers.notifier.notify_one();
            }
            OutgoingCommand::DeclareOrderedPublisher { topic, response } => {
                let result = self.declare_ordered_publish_topic(&topic);
                let _ = response.send(result);
            }
            OutgoingCommand::DeclareOrderedSubscriber { topic, response } => {
                let result = self.declare_ordered_subscribe_topic(&topic);
                let _ = response.send(result);
            }
            OutgoingCommand::DeclareLatestPublisher { topic, response } => {
                let result = self.declare_latest_publisher(&topic);
                let _ = response.send(result);
            }
            OutgoingCommand::DeclareLatestSubscriber {
                topic,
                publisher_id,
                response,
            } => {
                let result = self.declare_latest_subscriber(&topic, publisher_id.as_deref());
                let _ = response.send(result);
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
                        } else if let Some((_direction, topic, scope)) =
                            parse_data_track_label(&label)
                        {
                            self.data
                                .data_channels
                                .entry(cid)
                                .or_insert(DataTrackBinding {
                                    topic: topic.to_string(),
                                    scope: scope.clone(),
                                });
                        } else {
                            self.ordered_topics.open_channel(cid, &label);
                        }
                    }
                    Event::ChannelData(data) => {
                        if data.id == self.data.signaling_cid {
                            self.handle_signaling_data(data);
                        } else if !self.ordered_topics.handle_data(&mut self.rtc, &data) {
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
                        self.emit(AgentEvent::StatsUpdated);
                    }
                    Event::MediaIngressStats(stats) => {
                        let track_stats = self.stats.tracks.entry(stats.mid).or_default();
                        track_stats.rx_layers.insert(stats.rid, stats);
                        self.emit(AgentEvent::StatsUpdated);
                    }
                    Event::MediaEgressStats(stats) => {
                        let track_stats = self.stats.tracks.entry(stats.mid).or_default();
                        track_stats.tx_layers.insert(stats.rid, stats);
                        self.emit(AgentEvent::StatsUpdated);
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
        let key = (binding.topic.clone(), binding.scope.clone());
        let Some(target) = self.data.data_targets.get(&key) else {
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

                let mut encodings = Vec::with_capacity(rids.len());
                for rid in rids {
                    let (kf_notifier, kf_rx) = KeyframeNotifier::pair();
                    if media.kind.is_video() {
                        self.media.layer_ctrl.register(mid, rid, kf_notifier);
                    }
                    encodings.push((rid, kf_rx));
                }
                let previous = self.media.upstream_slots.insert(
                    mid,
                    UpstreamSlot {
                        kind: media.kind,
                        generation: 0,
                        active: false,
                        encodings,
                    },
                );
                debug_assert!(previous.is_none());
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
                let (assignments, discovered, removed) = self.slot_manager.sync(update);
                for track in discovered {
                    self.emit(AgentEvent::RemoteTrackDiscovered(track));
                }
                for track_id in removed {
                    self.emit(AgentEvent::RemoteTrackRemoved(track_id));
                }
                for (mid, track) in assignments {
                    let (tx, rx) = mailbox::bounded(256);
                    self.media.media_targets.insert(mid, tx);
                    let track_id = track.id.clone();
                    let remote_track = RemoteTrack::new(mid, track, rx);
                    if let Some(response) = self.media.pending_media_subscriptions.remove(&track_id)
                    {
                        let _ = response.send(Ok(remote_track));
                    }
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

        let (downstream_dirty, requests) = self.subscriptions.sub_manager.reconcile();
        if !downstream_dirty && !self.subscriptions.upstream_dirty {
            self.subscriptions.pending_deadline = None;
            return;
        }

        let msg = signaling::ClientMessage {
            payload: Some(signaling::client_message::Payload::Intent(
                signaling::ClientIntent {
                    upstream_intents: self
                        .subscriptions
                        .upstream_active
                        .iter()
                        .map(|(mid, active)| signaling::UpstreamIntent {
                            mid: mid.to_string(),
                            active: *active,
                        })
                        .collect(),
                    downstream_requests: requests,
                    playout_delay: self
                        .subscriptions
                        .playout_delay_ms
                        .map(|(min_ms, max_ms)| signaling::PlayoutDelay { min_ms, max_ms }),
                },
            )),
        };
        let encoded = msg.encode_to_vec();
        if let Err(err) = ch.write(true, encoded.as_slice()) {
            tracing::warn!("failed to send signaling: {:?}", err);
            self.subscriptions.pending_deadline = Some(self.now + STATE_DEBOUNCE);
        } else {
            self.subscriptions.pending_deadline = None;
            self.subscriptions.upstream_dirty = false;
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
        scope: Option<&str>,
    ) -> Result<ChannelId, AgentError> {
        let existing = match direction {
            DataTrackDirection::Publish => self.data.data_pub_topics.get(topic).copied(),
            DataTrackDirection::Subscribe => {
                let key = (topic.to_string(), scope.map(str::to_string));
                self.data.data_sub_topics.get(&key).copied()
            }
        };
        if let Some(cid) = existing {
            return Ok(cid);
        }

        let cfg = ChannelConfig {
            label: data_track_label(direction, topic, scope),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reused_upstream_slot_rejects_previous_generation() {
        let mid = Mid::from("video");
        let mut slot = UpstreamSlot {
            kind: MediaKind::Video,
            generation: 0,
            active: false,
            encodings: Vec::new(),
        };

        let camera = slot.activate(mid);
        assert!(slot.accepts(camera));
        assert!(slot.deactivate(camera));
        assert!(!slot.accepts(camera));

        let screen = slot.activate(mid);
        assert_ne!(camera.generation, screen.generation);
        assert!(!slot.accepts(camera));
        assert!(slot.accepts(screen));
        assert!(!slot.deactivate(camera));
        assert!(slot.accepts(screen));
    }
}

/// A single-stream Video Layers Allocation declaring one layer's target bitrate.
fn vla_for(target_bps: u64, resolution: Option<(u16, u16, u8)>) -> VideoLayersAllocation {
    VideoLayersAllocation {
        current_simulcast_stream_index: 0,
        simulcast_streams: vec![SimulcastStreamAllocation {
            spatial_layers: vec![SpatialLayerAllocation {
                temporal_layers: vec![TemporalLayerAllocation {
                    cumulative_kbps: target_bps / 1000,
                }],
                resolution_and_framerate: resolution.map(|(width, height, framerate)| {
                    ResolutionAndFramerate {
                        width,
                        height,
                        framerate,
                    }
                }),
            }],
        }],
    }
}

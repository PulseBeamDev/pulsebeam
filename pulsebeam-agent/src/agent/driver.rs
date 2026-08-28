use crate::RtpPacket;
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
/// How many packets may wait for the assignment that says where they go.
///
/// Enough for the keyframe a stream opens with - a 720p one runs to a few dozen packets - and
/// small enough that a peer sending unroutable media cannot cost more than a few hundred kilobytes.
const UNROUTED_CAPACITY: usize = 128;

/// How long a packet may wait to be routed before it is stale.
///
/// Not a round trip. The assignment rides the data channel, whose lane is the last to come up —
/// SCTP on top of DTLS — and on a lossy link its first messages are retransmitted like anything
/// else; measured at 1.5-2s from first media to first assignment on a cellular profile. Sized for
/// a round trip, this dropped the opening of every stream on exactly the links least able to
/// spare it.
///
/// The objection to holding longer — that a late packet reads as a gap the receiver must wait out
/// — applies to a stream that has moved on. A slot with no target has delivered nothing, so there
/// is no frontier to arrive behind, and this only ever holds slots in that state. Memory is
/// bounded by [`UNROUTED_CAPACITY`], which is the real limit; this is the backstop for a slot that
/// never becomes routable at all.
const UNROUTED_MAX_WAIT: Duration = Duration::from_secs(3);

use pulsebeam_proto::signaling::Publication as Track;
use pulsebeam_proto::{signaling, signaling::ServerMessage};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;
use str0m::IceConnectionState;
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpOffer, SdpPendingOffer};
use str0m::channel::{ChannelConfig, ChannelData, ChannelId, Reliability};
use str0m::media::{Direction, KeyframeRequestKind, MediaAdded, MediaKind, Mid, Rid, Simulcast};
use str0m::rtp::{RtpWrite, Ssrc};
use str0m::{
    Candidate, Event, Input, Output, Rtc, RtcConfig,
    net::{Protocol, Receive},
};
use tokio::time::Instant;

const MIN_QUANTA: Duration = Duration::from_millis(1);
const STATE_DEBOUNCE: Duration = Duration::from_millis(300);
const BWE_SLOW_INTERVAL: Duration = Duration::from_millis(200);

pub type ParticipantId = String;

#[derive(Clone)]
pub(crate) struct MediaTemplate {
    pub kind: MediaKind,
    pub direction: Direction,
    pub simulcast: Option<Simulcast>,
}

#[derive(Clone)]
pub(crate) struct RtcTemplate {
    config: RtcConfig,
    candidates: Vec<Candidate>,
    medias: Vec<MediaTemplate>,
}

type BuiltRtc = (
    Rtc,
    ChannelId,
    Vec<MediaAdded>,
    Vec<ChannelId>,
    SdpOffer,
    SdpPendingOffer,
);

impl RtcTemplate {
    pub(crate) fn new(
        config: RtcConfig,
        candidates: Vec<Candidate>,
        medias: Vec<MediaTemplate>,
    ) -> Self {
        debug_assert!(!candidates.is_empty());
        Self {
            config,
            candidates,
            medias,
        }
    }

    pub(crate) fn build(
        &self,
    ) -> Result<(Rtc, ChannelId, Vec<MediaAdded>, SdpOffer, SdpPendingOffer), AgentError> {
        let (rtc, signaling_cid, medias, _, offer, pending) =
            self.build_with_channels(Vec::new())?;
        Ok((rtc, signaling_cid, medias, offer, pending))
    }

    pub(crate) fn build_with_channels(
        &self,
        channels: Vec<ChannelConfig>,
    ) -> Result<BuiltRtc, AgentError> {
        let mut rtc = self.config.clone().build(Instant::now().into());
        for candidate in &self.candidates {
            let _ = rtc.add_local_candidate(candidate.clone());
        }

        let mut sdp = rtc.sdp_api();
        let signaling_cid = sdp.add_channel_with_config(ChannelConfig {
            label: namespace::Signaling::Reliable.as_str().to_string(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: None,
            protocol: String::new(),
        });
        let channel_ids = channels
            .into_iter()
            .map(|config| sdp.add_channel_with_config(config))
            .collect();
        let medias = self
            .medias
            .iter()
            .map(|media| {
                let mid = sdp.add_media(
                    media.kind,
                    media.direction,
                    None,
                    None,
                    media.simulcast.clone(),
                );
                MediaAdded {
                    mid,
                    kind: media.kind,
                    direction: media.direction,
                    simulcast: media.simulcast.clone(),
                }
            })
            .collect();
        let Some((offer, pending)) = sdp.apply() else {
            return Err(AgentError::Protocol(
                "an RTC template must produce its initial SDP offer".into(),
            ));
        };
        Ok((rtc, signaling_cid, medias, channel_ids, offer, pending))
    }
}

#[derive(Debug, Default, Clone)]
pub struct StatisticsSnapshot {
    pub(crate) peer: Option<str0m::stats::PeerStats>,
    pub(crate) tracks: HashMap<Mid, TrackStats>,
    /// Cumulative keyframe (PLI/FIR) requests this publisher has received. A
    /// healthy stream needs only the occasional one (a new subscriber, a switch);
    /// a constantly climbing count means downstream cannot decode — the signature
    /// of a broken forwarding/reassembly path.
    pub(crate) keyframe_requests_received: u64,
    /// Media the agent received and could not hand to anyone.
    ///
    /// Should be zero. Anything else is silent loss inside the client: packets that arrived, were
    /// decrypted and demuxed, and then went nowhere because no slot claimed them before the hold
    /// window ran out. Its absence is why a 34-packet drop once needed probes at every hop to
    /// find - the frames were missing at the application and present at the wire, and nothing
    /// measured in between.
    pub(crate) unroutable_media_dropped: u64,
    /// Media held until the assignment naming its slot arrived, then delivered.
    ///
    /// Expected to be small and non-zero: the SFU forwards as soon as it has a slot, which can
    /// beat the assignment over the data channel. A large or growing figure means signalling is
    /// lagging media badly enough to be worth looking at, even though nothing was lost.
    pub(crate) media_held_for_routing: u64,
}

impl StatisticsSnapshot {
    pub fn is_connected(&self) -> bool {
        self.peer.is_some()
    }

    /// Cumulative keyframe requests received (see field docs).
    pub fn keyframe_requests_received(&self) -> u64 {
        self.keyframe_requests_received
    }

    /// Media that arrived and could not be delivered to anyone (see field docs). Should be zero.
    pub fn unroutable_media_dropped(&self) -> u64 {
        self.unroutable_media_dropped
    }

    /// Media delayed until its slot became routable, then delivered (see field docs).
    pub fn media_held_for_routing(&self) -> u64 {
        self.media_held_for_routing
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
    ParticipantsChanged {
        added: Vec<ParticipantId>,
        removed: Vec<ParticipantId>,
        snapshot: bool,
    },
    RemoteTrackDiscovered(Track),
    RemoteTrackRemoved(String),
    /// Who the SFU is forwarding audio for, loudest first, whenever that changes.
    SpeakersChanged(Vec<crate::agent::slots::Speaker>),
    /// The SFU stopped forwarding this track - it is out of bandwidth for it, not gone. A UI
    /// should show a placeholder rather than the blank it would otherwise render.
    RemoteTrackPaused(String),
    /// Forwarding resumed.
    RemoteTrackResumed(String),
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
    /// The connection generation, echoed as `If-Match` on the next reconnect and replaced by the
    /// one the server answers with. Identity is the participant id; this says which connection.
    pub etag: String,
    #[cfg(feature = "sim")]
    pub room_id: String,
    pub participant_id: String,
    pub medias: Vec<MediaAdded>,
    pub rtc_template: RtcTemplate,
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
    channel_remap: HashMap<ChannelId, ChannelId>,
}

impl DataSubsystem {
    fn channel_templates(&self) -> Vec<(ChannelId, ChannelConfig)> {
        self.data_pub_topics
            .iter()
            .map(|(topic, channel_id)| {
                (
                    *channel_id,
                    ChannelConfig {
                        label: data_track_label(DataTrackDirection::Publish, topic, None),
                        ordered: false,
                        reliability: Reliability::MaxRetransmits { retransmits: 0 },
                        negotiated: None,
                        protocol: String::new(),
                    },
                )
            })
            .chain(
                self.data_sub_topics
                    .iter()
                    .map(|((topic, scope), channel_id)| {
                        (
                            *channel_id,
                            ChannelConfig {
                                label: data_track_label(
                                    DataTrackDirection::Subscribe,
                                    topic,
                                    scope.as_deref(),
                                ),
                                ordered: false,
                                reliability: Reliability::MaxRetransmits { retransmits: 0 },
                                negotiated: None,
                                protocol: String::new(),
                            },
                        )
                    }),
            )
            .collect()
    }
}

struct MediaSubsystem {
    media_targets: HashMap<Mid, mailbox::Sender<RtpPacket>>,
    publication_sources: HashMap<String, Track>,
    /// Packets that arrived before the assignment saying which track their slot carries.
    ///
    /// Media and signalling travel separately, and the SFU starts forwarding as soon as it has a
    /// slot - which can be before the assignment naming it reaches the client. Those first packets
    /// are the keyframe the stream opens with, and dropping them costs the viewer the picture
    /// until the next one, which on a settled stream can be seconds away.
    unrouted: VecDeque<(Mid, Option<Instant>, RtpPacket)>,
    /// Slots that lost held media before their assignment arrived.
    ///
    /// A dropped packet is usually part of the keyframe the stream opens with,
    /// and losing any of it costs the whole frame. The SFU will not send
    /// another unprompted: it requested one when it switched the slot, saw it
    /// answered, and from its side the switch is complete — so waiting is
    /// waiting for the publisher's next periodic keyframe, which on a settled
    /// stream is seconds away or never.
    ///
    /// Asking once, when the assignment finally lands, is the recovery that
    /// does not depend on media and signalling beating each other across a
    /// boundary that does not order them.
    lost_before_routing: HashMap<Mid, Option<Rid>>,
    /// Cache of which (mid, rid) each incoming SSRC belongs to. `Event::RtpPacket`
    /// carries only the SSRC, so the mapping is resolved once via the DirectApi and
    /// reused. Mirrors the SFU's `incoming_rtp_routes`.
    incoming_rtp_routes: HashMap<Ssrc, (Mid, Option<Rid>)>,
    media_targets_by_track: HashMap<String, mailbox::Sender<RtpPacket>>,
    upstream_slots: HashMap<Mid, UpstreamSlot>,
    upstream_order: Vec<Mid>,
    pending_media_subscriptions:
        HashMap<String, tokio::sync::oneshot::Sender<Result<RemoteTrack, AgentError>>>,
    /// Mailboxes handed to a subscriber before the SFU assigned the track a slot.
    ///
    /// A hidden subscription (`target_height = 0`) is answered immediately with a `RemoteTrack`
    /// that carries no media yet; its sender waits here until an assignment arrives, so raising
    /// the height later starts feeding the handle the subscriber already holds.
    pending_media_targets: HashMap<String, mailbox::Sender<RtpPacket>>,
    /// Where to hand audio tracks the SFU decides to forward, once someone has asked for them.
    audio_sink: Option<mailbox::Sender<RemoteTrack>>,
    layer_ctrl: LayerController,
    desired_ctrl: BitrateController,
    last_desired: Bitrate,
}

struct UpstreamSlot {
    kind: MediaKind,
    generation: u64,
    active: bool,
    encodings: Vec<(Option<Rid>, KeyframeReceiver)>,
    keyframe_notifiers: Vec<KeyframeNotifier>,
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
    parked_subscriptions: HashMap<String, VideoSubscription>,
    parked_publications: HashMap<String, Track>,
    pending_deadline: Option<Instant>,
    /// (min, max) receiver playout delay in ms; `None` = adaptive default.
    playout_delay_ms: Option<(u32, u32)>,
    upstream_active: HashMap<Mid, bool>,
    /// How this client wants its audio slots filled. `None` until it says,
    /// which the server reads as auto with no pins.
    audio_intent: Option<pulsebeam_proto::signaling::AudioIntent>,
    upstream_dirty: bool,
}

struct SessionSubsystem {
    api: HttpApiClient,
    resource_uri: Uri,
    /// The connection generation, echoed as `If-Match` on the next reconnect and replaced by the
    /// one the server answers with. Identity is the participant id; this says which connection.
    etag: String,
    #[cfg(feature = "sim")]
    room_id: String,
    participant_id: String,
    disconnected_reason: Option<String>,
    retry_count: u32,
    is_reconnecting: bool,
    reconnect_deadline: Option<Instant>,
    rtc_failed: bool,
}

struct TimerSubsystem {
    notifier: tokio::sync::Notify,
    sleep: Pin<Box<tokio::time::Sleep>>,
    rtc_deadline: Option<Instant>,
    bwe_next_tick: Instant,
}

pub(crate) struct AgentDriver {
    rtc: Rtc,
    rtc_template: RtcTemplate,
    upstream_mid_remap: HashMap<Mid, Mid>,
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
        let mut rtc = init.rtc;
        rtc.bwe().set_current_bitrate(Bitrate::ZERO);

        let mut driver = Self {
            rtc,
            rtc_template: init.rtc_template,
            upstream_mid_remap: HashMap::new(),
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
                channel_remap: HashMap::new(),
            },
            ordered_topics: OrderedTopics::new(init.participant_id.clone()),
            media: MediaSubsystem {
                media_targets: HashMap::new(),
                publication_sources: HashMap::new(),
                incoming_rtp_routes: HashMap::new(),
                media_targets_by_track: HashMap::new(),
                upstream_slots: HashMap::new(),
                upstream_order: Vec::new(),
                pending_media_subscriptions: HashMap::new(),
                pending_media_targets: HashMap::new(),
                unrouted: VecDeque::new(),
                lost_before_routing: HashMap::new(),
                audio_sink: None,
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
                parked_subscriptions: HashMap::new(),
                parked_publications: HashMap::new(),
                pending_deadline: None,
                playout_delay_ms: None,
                upstream_active: HashMap::new(),
                audio_intent: None,
                upstream_dirty: false,
            },
            session: SessionSubsystem {
                api: init.api,
                resource_uri: init.resource_uri,
                etag: init.etag,
                #[cfg(feature = "sim")]
                room_id: init.room_id,
                participant_id: init.participant_id,
                disconnected_reason: None,
                retry_count: 0,
                is_reconnecting: false,
                reconnect_deadline: None,
                rtc_failed: false,
            },
            timers: TimerSubsystem {
                notifier: tokio::sync::Notify::new(),
                sleep: Box::pin(tokio::time::sleep(MIN_QUANTA)),
                rtc_deadline: None,
                bwe_next_tick: now.checked_add(BWE_SLOW_INTERVAL).unwrap_or(now),
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

    #[cfg(feature = "sim")]
    pub fn room_id(&self) -> &str {
        &self.session.room_id
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
        let Some(&physical_mid) = self
            .media
            .upstream_slots
            .iter()
            .find(|(_, slot)| slot.kind == kind && !slot.active)
            .map(|(mid, _)| mid)
        else {
            return Err(AgentError::MediaCapacity(kind));
        };
        let logical_mid = self.logical_upstream_mid(physical_mid);
        let Some(slot) = self.media.upstream_slots.get_mut(&physical_mid) else {
            return Err(AgentError::MediaCapacity(kind));
        };
        let physical_lease = slot.activate(physical_mid);
        let lease = PublicationLease {
            mid: logical_mid,
            generation: physical_lease.generation,
        };
        let encodings = slot
            .encodings
            .iter()
            .map(|(rid, keyframe_rx)| LocalEncoding {
                mid: logical_mid,
                rid: *rid,
                lease,
                keyframe_rx: keyframe_rx.clone(),
                tx: self.outgoing_tx.clone(),
            })
            .collect();
        self.set_upstream_active(logical_mid, true);
        Ok(super::session::LocalTrack::new(
            kind,
            lease,
            encodings,
            self.outgoing_tx.clone(),
        ))
    }

    fn unpublish_local_track(&mut self, lease: PublicationLease) {
        let physical_mid = self.physical_upstream_mid(lease.mid);
        let Some(slot) = self.media.upstream_slots.get_mut(&physical_mid) else {
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

    /// Replace the audio policy and schedule it.
    ///
    /// Declarative, so this overwrites rather than merges: the intent on the wire is the whole
    /// policy, and a caller drops a pin by sending an intent without it.
    fn set_audio_intent(&mut self, intent: crate::agent::AudioIntent) {
        let wire = pulsebeam_proto::signaling::AudioIntent {
            pinned: intent.pinned,
            auto: intent.auto,
        };
        if self.subscriptions.audio_intent.as_ref() == Some(&wire) {
            return;
        }
        self.subscriptions.audio_intent = Some(wire);
        self.subscriptions.upstream_dirty = true;
        self.subscriptions.pending_deadline = Some(self.now);
        self.timers.notifier.notify_one();
    }

    fn set_upstream_active(&mut self, mid: Mid, active: bool) {
        self.subscriptions.upstream_active.insert(mid, active);
        self.subscriptions.upstream_dirty = true;
        self.subscriptions.pending_deadline = Some(self.now);
        self.timers.notifier.notify_one();
    }

    fn physical_upstream_mid(&self, logical_mid: Mid) -> Mid {
        self.upstream_mid_remap
            .get(&logical_mid)
            .copied()
            .unwrap_or(logical_mid)
    }

    fn logical_upstream_mid(&self, physical_mid: Mid) -> Mid {
        self.upstream_mid_remap
            .iter()
            .find_map(|(logical, physical)| (*physical == physical_mid).then_some(*logical))
            .unwrap_or(physical_mid)
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
        self.subscriptions.pending_deadline =
            Some(self.now.checked_add(STATE_DEBOUNCE).unwrap_or(self.now));
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
                        match self.network.buf.get(..n).unwrap_or_default().try_into() {
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
        let next = self.next_deadline().unwrap_or_else(|| {
            let now = Instant::now();
            now.checked_add(MIN_QUANTA).unwrap_or(now)
        });
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
            let desired_bitrate =
                Bitrate::from(crate::media::saturating_u64_from_f64(desired_bps.max(0.0)));
            let filtered_bitrate = self.media.desired_ctrl.update(desired_bitrate);
            if filtered_bitrate != self.media.last_desired {
                self.media.last_desired = filtered_bitrate;
                self.rtc.bwe().set_desired_bitrate(filtered_bitrate);
            }
            self.timers.bwe_next_tick = self
                .timers
                .bwe_next_tick
                .checked_add(BWE_SLOW_INTERVAL)
                .unwrap_or(self.timers.bwe_next_tick);
        }
    }

    fn handle_outgoing_command(&mut self, cmd: OutgoingCommand) {
        match cmd {
            OutgoingCommand::SendData(e) => {
                let channel_id = self
                    .data
                    .channel_remap
                    .get(&e.channel_id)
                    .copied()
                    .unwrap_or(e.channel_id);
                if let Err(payload) = self
                    .ordered_topics
                    .send(&mut self.rtc, channel_id, e.payload)
                {
                    let Some(mut channel) = self.rtc.channel(channel_id) else {
                        return;
                    };
                    let _ = channel.write(true, &payload);
                }
            }
            OutgoingCommand::SendMedia(e) => {
                let mid = self
                    .upstream_mid_remap
                    .get(&e.lease.mid)
                    .copied()
                    .unwrap_or(e.lease.mid);
                let Some(slot) = self.media.upstream_slots.get(&mid) else {
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
                let paused = self.media.layer_ctrl.is_paused(mid, e.rid);
                self.media.layer_ctrl.record_frame(
                    mid,
                    e.rid,
                    e.packet.payload.len(),
                    Instant::now(),
                );

                if paused {
                    return;
                }

                // The pipeline already packetized the frame and set the DD / VLA /
                // abs-capture-time extensions on `ext_vals`; the agent just writes
                // the raw RTP. str0m still owns SRTP, RTX, sender reports, and BWE.
                let Some(pt) = self
                    .rtc
                    .media(mid)
                    .and_then(|m| m.remote_pts().first().copied())
                else {
                    return;
                };
                let packet = e.packet;
                let mut api = self.rtc.direct_api();
                let Some(stream) = api.stream_tx_by_mid(mid, e.rid) else {
                    return;
                };
                let rtp = RtpWrite::new(
                    pt,
                    packet.seq,
                    u32::try_from(packet.ts.numer() & u64::from(u32::MAX)).unwrap_or(0),
                    packet.arrival.into(),
                    packet.payload,
                )
                .marker(packet.marker)
                .nackable(true)
                .ext_vals(packet.ext_vals);
                stream.write_rtp(rtp);
            }
            OutgoingCommand::SetPlayoutDelay(bounds) => {
                self.set_playout_delay(bounds);
            }
            OutgoingCommand::SetAudioIntent(intent) => {
                self.set_audio_intent(intent);
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
            OutgoingCommand::ReceiveAudio { response } => {
                let (tx, rx) = mailbox::bounded(16);
                self.media.audio_sink = Some(tx);
                let _ = response.send(rx);
            }
            OutgoingCommand::SubscribeMedia {
                subscription,
                response,
            } => {
                let track_id = subscription.track_id.clone();
                if let Some((mid, track)) = self.slot_manager.assigned(&track_id) {
                    let (tx, rx) = mailbox::bounded(256);
                    self.media.media_targets.insert(mid, tx);
                    if let Some(tx) = self.media.media_targets.get(&mid).cloned() {
                        self.media
                            .media_targets_by_track
                            .insert(track_id.clone(), tx);
                    }
                    let _ = response.send(Ok(RemoteTrack::new(track, rx)));
                    self.deliver_unrouted();
                    self.subscriptions.parked_subscriptions.remove(&track_id);
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
                    || self.media.pending_media_targets.contains_key(&track_id)
                {
                    let _ = response.send(Err(AgentError::Protocol(
                        "media publication is already being subscribed".into(),
                    )));
                    return;
                }
                // The track is known but holds no slot - a hidden subscription, which the SFU is
                // never going to assign. Answer now with a handle whose mailbox is wired up if and
                // when the subscriber raises the height, rather than leaving the caller awaiting an
                // assignment that is not coming.
                if let Some(track) = self.slot_manager.known(&track_id) {
                    let (tx, rx) = mailbox::bounded(256);
                    self.media
                        .media_targets_by_track
                        .insert(track_id.clone(), tx.clone());
                    self.media
                        .pending_media_targets
                        .insert(track_id.clone(), tx);
                    let _ = response.send(Ok(RemoteTrack::new(track, rx)));
                } else {
                    self.media
                        .pending_media_subscriptions
                        .insert(track_id.clone(), response);
                }
                self.subscriptions.parked_subscriptions.remove(&track_id);
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
        if self.session.rtc_failed {
            let now = Instant::now();
            return Some(
                self.session
                    .reconnect_deadline
                    .unwrap_or_else(|| now.checked_add(MIN_QUANTA).unwrap_or(now)),
            );
        }

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
                            self.data.data_channels.entry(cid).or_insert_with(|| {
                                DataTrackBinding {
                                    topic: topic.to_string(),
                                    scope: scope.clone(),
                                }
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
                    Event::RtpPacket(rtp) => {
                        let ssrc = rtp.header.ssrc;
                        let route = match self.media.incoming_rtp_routes.get(&ssrc).copied() {
                            Some(route) => Some(route),
                            None => {
                                let mut api = self.rtc.direct_api();
                                api.stream_rx(&ssrc).map(|s| (s.mid(), s.rid()))
                            }
                        };
                        if let Some((mid, rid)) = route {
                            self.media.incoming_rtp_routes.insert(ssrc, (mid, rid));
                            let packet = RtpPacket {
                                mid,
                                rid,
                                seq: rtp.seq_no,
                                ts: rtp.time,
                                marker: rtp.header.marker,
                                payload_type: Some(*rtp.header.payload_type),
                                ssrc: Some(ssrc),
                                payload: rtp.payload,
                                ext_vals: rtp.header.ext_vals,
                                arrival: rtp.timestamp.into(),
                            };
                            match self.media.media_targets.get(&mid) {
                                Some(tx) => {
                                    let _ = tx.try_send(packet);
                                }
                                None => {
                                    let deadline = self.now.checked_add(UNROUTED_MAX_WAIT);
                                    self.media.unrouted.push_back((mid, deadline, packet));
                                    while self.media.unrouted.len() > UNROUTED_CAPACITY {
                                        if let Some((lost, _, packet)) =
                                            self.media.unrouted.pop_front()
                                        {
                                            self.media.lost_before_routing.insert(lost, packet.rid);
                                        }
                                        self.stats.unroutable_media_dropped =
                                            self.stats.unroutable_media_dropped.wrapping_add(1);
                                    }
                                }
                            }
                        }
                    }
                    Event::IceConnectionStateChange(state) => {
                        if state == IceConnectionState::Disconnected {
                            if self.session.disconnected_reason.is_none() {
                                self.session.disconnected_reason =
                                    Some("ICE connection disconnected".into());
                                self.emit(AgentEvent::Disconnected(
                                    "ICE connection disconnected".into(),
                                ));
                            }
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
                        self.stats.keyframe_requests_received =
                            self.stats.keyframe_requests_received.wrapping_add(1);
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
                    self.session.disconnected_reason = Some(format!("RTC Error: {e:?}"));
                    self.rtc.disconnect();
                    self.session.rtc_failed = true;
                    self.emit(AgentEvent::Disconnected(format!("RTC Error: {e:?}")));
                    self.schedule_reconnect(Instant::now());
                    let now = Instant::now();
                    return Some(
                        self.session
                            .reconnect_deadline
                            .unwrap_or_else(|| now.checked_add(MIN_QUANTA).unwrap_or(now)),
                    );
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
                if self.media.upstream_slots.contains_key(&mid) {
                    return;
                }
                let rids = if let Some(layers) = media.simulcast {
                    layers.send.iter().map(|s| Some(s.rid)).collect()
                } else {
                    vec![None]
                };

                let mut encodings = Vec::with_capacity(rids.len());
                let mut keyframe_notifiers = Vec::with_capacity(rids.len());
                for rid in rids {
                    let (kf_notifier, kf_rx) = KeyframeNotifier::pair();
                    if media.kind.is_video() {
                        self.media
                            .layer_ctrl
                            .register(mid, rid, kf_notifier.clone());
                    }
                    keyframe_notifiers.push(kf_notifier);
                    encodings.push((rid, kf_rx));
                }
                self.media.upstream_order.push(mid);
                let previous = self.media.upstream_slots.insert(
                    mid,
                    UpstreamSlot {
                        kind: media.kind,
                        generation: 0,
                        active: false,
                        encodings,
                        keyframe_notifiers,
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
            signaling::server_message::Payload::State(update) => {
                let added: Vec<ParticipantId> = update
                    .participants_added
                    .iter()
                    .map(|participant| participant.participant_id.clone())
                    .collect();
                let removed = update.participants_removed.clone();
                let snapshot = update.snapshot;
                let sync = self.slot_manager.sync(update);
                if snapshot || !added.is_empty() || !removed.is_empty() {
                    self.emit(AgentEvent::ParticipantsChanged {
                        added,
                        removed,
                        snapshot,
                    });
                }
                let (assignments, discovered, removed) = (
                    sync.new_assignments,
                    sync.newly_discovered_tracks,
                    sync.removed_tracks,
                );
                for (track_id, paused) in sync.pause_changes {
                    // The SFU told us it started or stopped forwarding. Without this an
                    // application cannot tell a paused stream from a dead network, so it shows a
                    // blank tile where a placeholder belongs.
                    self.emit(if paused {
                        AgentEvent::RemoteTrackPaused(track_id)
                    } else {
                        AgentEvent::RemoteTrackResumed(track_id)
                    });
                }
                for track in discovered {
                    self.restore_parked_subscription(&track);
                    self.media
                        .publication_sources
                        .insert(track.track_id.clone(), track.clone());
                    self.emit(AgentEvent::RemoteTrackDiscovered(track));
                }
                for track_id in &removed {
                    let source = self.media.publication_sources.remove(track_id);
                    self.forget_track(track_id, source);
                }
                if !removed.is_empty() {
                    self.subscriptions.pending_deadline = Some(self.now);
                    self.timers.notifier.notify_one();
                }
                for track_id in removed {
                    self.media.pending_media_targets.remove(&track_id);
                    self.emit(AgentEvent::RemoteTrackRemoved(track_id));
                }
                if sync.speakers_changed {
                    self.emit(AgentEvent::SpeakersChanged(self.slot_manager.speakers()));
                }
                for (mid, track) in sync.audio_arrivals {
                    // Nobody subscribed to this: the SFU chose to forward it, and it is described
                    // entirely by the assignment - a speaker never appears in `tracks_upsert`.
                    let (tx, rx) = mailbox::bounded(256);
                    self.media.media_targets.insert(mid, tx);
                    if let Some(sink) = &self.media.audio_sink {
                        let _ = sink.try_send(RemoteTrack::new(track, rx));
                    }
                }
                for (mid, track) in assignments {
                    let track_id = track.track_id.clone();
                    // A subscriber already holds the receiving half, from a subscription answered
                    // before this assignment existed. Wire its sender to the slot rather than
                    // replacing it, or the handle it is holding would never receive anything.
                    if let Some(tx) = self.media.pending_media_targets.remove(&track_id) {
                        self.media
                            .media_targets_by_track
                            .insert(track_id.clone(), tx.clone());
                        self.media.media_targets.insert(mid, tx);
                        self.deliver_unrouted();
                        continue;
                    }
                    if let Some(tx) = self.media.media_targets_by_track.get(&track_id).cloned() {
                        self.media.media_targets.insert(mid, tx);
                        self.deliver_unrouted();
                        continue;
                    }
                    let (tx, rx) = mailbox::bounded(256);
                    self.media
                        .media_targets_by_track
                        .insert(track_id.clone(), tx.clone());
                    self.media.media_targets.insert(mid, tx);
                    let remote_track = RemoteTrack::new(track, rx);
                    if let Some(response) = self.media.pending_media_subscriptions.remove(&track_id)
                    {
                        let _ = response.send(Ok(remote_track));
                    }
                }
                // Last, once every slot this update touched is routable.
                self.deliver_unrouted();
            }
            signaling::server_message::Payload::Error(err) => {
                tracing::warn!("signaling error: {}", err);
            }
        }
    }

    /// Drop every trace of a track the room says has gone.
    ///
    /// Both halves, and that is the point. `desired_subscriptions` is the source of truth - every
    /// `set_desired` is rebuilt from it - while `sub_manager` holds what was derived from it last.
    /// Clearing only the derived half looks like it works until the next subscription rebuilds
    /// `desired` from a map that still holds the departed track, puts it back, and has the client
    /// asking the SFU for a publisher who left.
    fn forget_track(&mut self, track_id: &str, source: Option<Track>) {
        if let Some(subscription) = self.subscriptions.desired_subscriptions.remove(track_id) {
            const MAX_PARKED_SUBSCRIPTIONS: usize = 64;
            if self.subscriptions.parked_subscriptions.len() >= MAX_PARKED_SUBSCRIPTIONS
                && !self
                    .subscriptions
                    .parked_subscriptions
                    .contains_key(track_id)
                && let Some(oldest) = self
                    .subscriptions
                    .parked_subscriptions
                    .keys()
                    .next()
                    .cloned()
            {
                self.subscriptions.parked_subscriptions.remove(&oldest);
            }
            self.subscriptions
                .parked_subscriptions
                .insert(track_id.to_owned(), subscription);
            if let Some(source) = source {
                self.subscriptions
                    .parked_publications
                    .insert(track_id.to_owned(), source);
            }
        }
        self.subscriptions.sub_manager.remove_track(track_id);
    }

    fn restore_parked_subscription(&mut self, track: &Track) {
        let parked_id = if self
            .subscriptions
            .parked_subscriptions
            .contains_key(&track.track_id)
        {
            Some(track.track_id.clone())
        } else {
            self.subscriptions
                .parked_publications
                .iter()
                .find(|(_, old)| {
                    old.participant_id == track.participant_id && old.kind == track.kind
                })
                .map(|(track_id, _)| track_id.clone())
        };
        let Some(parked_id) = parked_id else {
            return;
        };
        let Some(mut subscription) = self.subscriptions.parked_subscriptions.remove(&parked_id)
        else {
            return;
        };
        self.subscriptions.parked_publications.remove(&parked_id);
        subscription.track_id.clone_from(&track.track_id);
        self.subscriptions
            .desired_subscriptions
            .insert(track.track_id.clone(), subscription);
        if let Some(target) = self.media.media_targets_by_track.remove(&parked_id) {
            self.media
                .media_targets_by_track
                .insert(track.track_id.clone(), target);
        }
        let desired = self
            .subscriptions
            .desired_subscriptions
            .values()
            .cloned()
            .collect();
        self.subscriptions.sub_manager.set_desired(desired);
        self.subscriptions.pending_deadline = Some(self.now);
        self.timers.notifier.notify_one();
    }

    /// Hand over packets that were waiting for the assignment naming their slot.
    ///
    /// In arrival order, and only for slots now routable; anything still unrouted stays held until
    /// it is claimed or goes stale.
    fn deliver_unrouted(&mut self) {
        if self.media.unrouted.is_empty() {
            return;
        }
        let now = self.now;
        let mut still_waiting = VecDeque::with_capacity(self.media.unrouted.len());
        while let Some((mid, deadline, packet)) = self.media.unrouted.pop_front() {
            if deadline.is_none_or(|deadline| now > deadline) {
                self.media.lost_before_routing.insert(mid, packet.rid);
                self.stats.unroutable_media_dropped =
                    self.stats.unroutable_media_dropped.wrapping_add(1);
                continue;
            }
            match self.media.media_targets.get(&mid) {
                Some(tx) => {
                    let _ = tx.try_send(packet);
                    self.stats.media_held_for_routing =
                        self.stats.media_held_for_routing.wrapping_add(1);
                }
                None => still_waiting.push_back((mid, deadline, packet)),
            }
        }
        self.media.unrouted = still_waiting;
        self.recover_lost_keyframes();
    }

    /// Ask the SFU for a keyframe on every routable slot that lost held media.
    ///
    /// Once, per loss: the request travels to the publisher and comes back as a
    /// keyframe, and asking again before that round trip completes only costs
    /// bandwidth. If this one is lost too the slot goes on receiving
    /// undecodable frames, which is the same place it was — but it is no longer
    /// the only outcome.
    fn recover_lost_keyframes(&mut self) {
        if self.media.lost_before_routing.is_empty() {
            return;
        }
        let mut api = self.rtc.direct_api();
        self.media.lost_before_routing.retain(|mid, rid| {
            if !self.media.media_targets.contains_key(mid) {
                // Still unassigned. Hold the request until it can be answered
                // by a stream this client can actually route.
                return true;
            }
            let Some(stream) = api.stream_rx_by_mid(*mid, *rid) else {
                return false;
            };
            stream.request_keyframe(KeyframeRequestKind::Pli);
            tracing::debug!(?mid, ?rid, "requesting a keyframe after losing held media");
            false
        });
    }

    fn emit(&mut self, event: AgentEvent) {
        self.pending_events.push_back(event);
    }

    fn flush_pending_state(&mut self) {
        if self.rtc.channel(self.data.signaling_cid).is_none() {
            self.subscriptions.pending_deadline =
                Some(self.now.checked_add(STATE_DEBOUNCE).unwrap_or(self.now));
            return;
        }

        let (downstream_dirty, requests) = self.subscriptions.sub_manager.reconcile();
        if !downstream_dirty && !self.subscriptions.upstream_dirty {
            self.subscriptions.pending_deadline = None;
            return;
        }

        let msg = signaling::ClientMessage {
            payload: Some(signaling::client_message::Payload::Intent(
                signaling::ClientIntent {
                    publish: self
                        .subscriptions
                        .upstream_active
                        .iter()
                        .map(|(mid, active)| signaling::PublishIntent {
                            mid: self.physical_upstream_mid(*mid).to_string(),
                            active: *active,
                        })
                        .collect(),
                    video: requests,
                    audio: self.subscriptions.audio_intent.clone(),
                    ext: self.subscriptions.playout_delay_ms.map(|(min_ms, max_ms)| {
                        signaling::Extensions {
                            playout_delay: Some(signaling::PlayoutDelay { min_ms, max_ms }),
                        }
                    }),
                },
            )),
        };
        let Some(mut ch) = self.rtc.channel(self.data.signaling_cid) else {
            self.subscriptions.pending_deadline =
                Some(self.now.checked_add(STATE_DEBOUNCE).unwrap_or(self.now));
            return;
        };
        let encoded = msg.encode_to_vec();
        if let Err(err) = ch.write(true, encoded.as_slice()) {
            tracing::warn!("failed to send signaling: {:?}", err);
            self.subscriptions.pending_deadline =
                Some(self.now.checked_add(STATE_DEBOUNCE).unwrap_or(self.now));
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
            n => {
                Duration::from_millis(500u64.saturating_mul(2u64.pow(n.min(10).saturating_sub(1))))
                    .min(Duration::from_secs(5))
            }
        };

        self.session.retry_count = self.session.retry_count.saturating_add(1);
        self.session.reconnect_deadline = Some(now.checked_add(delay).unwrap_or(now));
    }

    async fn perform_reconnect(&mut self) {
        self.session.is_reconnecting = true;
        self.session.reconnect_deadline = None;
        self.stats.peer = None;

        match self.try_reconnect().await {
            Ok(_) => {
                self.session.rtc_failed = false;
                self.session.is_reconnecting = false;
                self.session.retry_count = 0;
                self.emit(AgentEvent::Connected);
            }
            Err(error) => {
                tracing::warn!(?error, "participant reconnect attempt failed");
                self.session.is_reconnecting = false;
                self.schedule_reconnect(Instant::now());
            }
        }
    }

    async fn try_reconnect(&mut self) -> Result<(), AgentError> {
        let channel_templates = self
            .data
            .channel_templates()
            .into_iter()
            .chain(self.ordered_topics.channel_templates())
            .collect::<Vec<_>>();
        let channel_configs = channel_templates
            .iter()
            .map(|(_, config)| config.clone())
            .collect();
        let (mut rtc, signaling_cid, medias, channel_ids, offer, pending) =
            self.rtc_template.build_with_channels(channel_configs)?;
        let resp = self
            .session
            .api
            .update_participant(
                self.session.resource_uri.clone(),
                UpdateParticipantRequest {
                    offer,
                    etag: self.session.etag.clone(),
                },
            )
            .await?;
        self.session.etag = resp.etag;
        rtc.sdp_api()
            .accept_answer(pending, resp.answer)
            .map_err(AgentError::Rtc)?;

        self.rebind_channels(signaling_cid, &channel_templates, &channel_ids);
        self.rebind_media(medias)?;
        self.rtc = rtc;
        self.rtc.bwe().set_current_bitrate(Bitrate::ZERO);
        self.network.tcp = self.reconnect_tcp().await?;
        self.media.incoming_rtp_routes.clear();
        self.subscriptions.sub_manager.reset_active_assignments();
        self.subscriptions.upstream_dirty = true;
        self.subscriptions.pending_deadline = Some(Instant::now());
        self.session.disconnected_reason = None;
        self.timers.notifier.notify_one();
        Ok(())
    }

    fn rebind_channels(
        &mut self,
        signaling_cid: ChannelId,
        templates: &[(ChannelId, ChannelConfig)],
        channel_ids: &[ChannelId],
    ) {
        debug_assert_eq!(templates.len(), channel_ids.len());
        let mut remap = HashMap::with_capacity(templates.len());
        for ((old_id, config), new_id) in templates.iter().zip(channel_ids.iter().copied()) {
            debug_assert_ne!(*old_id, new_id);
            remap.insert(*old_id, new_id);
            if let Some(binding) = self.data.data_channels.get(old_id).cloned() {
                self.data.data_channels.insert(new_id, binding);
            } else if let Some((_direction, topic, scope)) = parse_data_track_label(&config.label) {
                self.data
                    .data_channels
                    .insert(new_id, DataTrackBinding { topic, scope });
            }
        }
        self.data.signaling_cid = signaling_cid;
        self.data.channel_remap.clone_from(&remap);
        self.ordered_topics.rebind_channels(&remap);
    }

    fn rebind_media(&mut self, medias: Vec<MediaAdded>) -> Result<(), AgentError> {
        let new_upstream: Vec<_> = medias
            .iter()
            .filter(|media| media.direction == Direction::SendOnly)
            .collect();
        let old_order = std::mem::take(&mut self.media.upstream_order);
        if old_order.len() != new_upstream.len() {
            return Err(AgentError::Protocol(format!(
                "reconnect changed upstream media capacity from {} to {}",
                old_order.len(),
                new_upstream.len()
            )));
        }

        let mut old_slots = std::mem::take(&mut self.media.upstream_slots);
        let mut new_slots = HashMap::with_capacity(new_upstream.len());
        let mut remap = HashMap::with_capacity(new_upstream.len());
        let mut new_order = Vec::with_capacity(new_upstream.len());
        for (old_mid, new_media) in old_order.into_iter().zip(new_upstream) {
            let new_mid = new_media.mid;
            let Some(old_slot) = old_slots.remove(&old_mid) else {
                return Err(AgentError::Protocol(format!(
                    "reconnect lost upstream media slot {old_mid}"
                )));
            };
            if old_slot.kind != new_media.kind {
                return Err(AgentError::Protocol(
                    "reconnect changed upstream media kind".into(),
                ));
            }
            if old_mid != new_mid && old_slot.kind.is_video() {
                self.media.layer_ctrl.rebind(old_mid, new_mid);
            }
            let encodings = old_slot
                .encodings
                .iter()
                .zip(old_slot.keyframe_notifiers.iter())
                .map(|((rid, _), notifier)| (*rid, notifier.receiver()))
                .collect::<Vec<_>>();
            if encodings.len() != old_slot.encodings.len() {
                return Err(AgentError::Protocol(
                    "reconnect lost a keyframe notifier".into(),
                ));
            }
            let new_slot = UpstreamSlot {
                kind: old_slot.kind,
                generation: old_slot.generation,
                active: old_slot.active,
                encodings,
                keyframe_notifiers: old_slot.keyframe_notifiers,
            };
            remap.insert(old_mid, new_mid);
            new_order.push(new_mid);
            new_slots.insert(new_mid, new_slot);
        }
        if !old_slots.is_empty() {
            return Err(AgentError::Protocol(
                "reconnect left an upstream media slot unmapped".into(),
            ));
        }

        let recv_mids = medias
            .iter()
            .filter(|media| media.direction == Direction::RecvOnly)
            .map(|media| media.mid)
            .collect::<Vec<_>>();
        self.media.upstream_slots = new_slots;
        self.media.upstream_order = new_order;
        self.upstream_mid_remap = remap;
        self.media.media_targets.clear();
        self.media.incoming_rtp_routes.clear();
        self.media.unrouted.clear();
        self.media.lost_before_routing.clear();
        self.slot_manager.replace_slots(recv_mids.clone());
        self.subscriptions.sub_manager.replace_slots(recv_mids);
        Ok(())
    }

    async fn reconnect_tcp(&mut self) -> Result<TcpSession, AgentError> {
        let Some(server_addr) = self.network.tcp.server_addr() else {
            return Ok(TcpSession::inactive());
        };
        self.network.tcp.close();
        let stream = pulsebeam_core::net::TcpStream::connect(server_addr).await?;
        let _ = stream.set_nodelay(true);
        let local_addr = stream.local_addr().ok();
        Ok(TcpSession::new(stream, local_addr, server_addr))
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
            keyframe_notifiers: Vec::new(),
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

// VLA construction moved to `crate::pipeline` (the frame→RTP layer that now owns
// header-extension emission).

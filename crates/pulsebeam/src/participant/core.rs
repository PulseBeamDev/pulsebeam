use super::signaling::SignalingWrite;
use crate::control::native::OutputError;
#[cfg(test)]
use crate::control::native::failure;
use crate::control::native::handoff::{EndpointHandoff, Fact, Opened};
#[cfg(test)]
mod fabric_shadow_tests;
mod native_data;
mod native_receiver;
mod native_rtp;
#[cfg(test)]
mod topic_tests;
use crate::media::{Direction, KeyframeRequestKind, MediaKind, Mid, Pt, Ssrc};
use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use pulsebeam_rtc::{DataChannelEvent, DataMessage, Event};
use pulsebeam_runtime::net::{self};
use pulsebeam_runtime::time::Instant;
use std::time::Duration;

use crate::control::{NegotiatedResources, controller::SignalingProtocol};
use crate::entity::{self, PublicationKey, TrackKind};
use crate::id::ShardId;
use crate::keys::TrackHandle;
use crate::log::{LogCtx, plog_debug, plog_info, plog_warn};
use crate::participant::data_fabric::{self, Lease as FabricDataLease};
use crate::participant::downstream::SlotConfig;
use crate::participant::effect::ParticipantEffect;
use crate::participant::event::ParticipantSink;
use crate::participant::reverse::{ReverseInput, ReversePacket};
use crate::shard::router::{FabricLocalBranch, FabricSourceHandle};

use crate::participant::TrackPacketRef;
#[cfg(test)]
use crate::participant::upstream::{MAX_UPSTREAM_ENCODED_STREAMS, UpstreamRouteTable};
use crate::participant::{
    batcher::NetworkEgress,
    downstream::DownstreamAllocator,
    transport::{
        AppliedMutation, IngressResult, RtcDriver, RtpWriteCommand, TransportMutation,
        TransportPollOutput,
    },
    upstream::{IncomingRtpRoute, UpstreamAllocator, UpstreamSlotKey},
};
use crate::rtp::cache::TrackStreamCache;
use crate::rtp::fabric_egress::{AdmissionError, CandidatePacket, SenderLeg, SourceMetadata};
use crate::rtp::fabric_ingress::{DropReason, Observation, RtpIngressShadow};
use crate::track::{
    self, DataLane, KEYFRAME_DEBOUNCE, SourceEncoding, StreamId, StreamWrite, StreamWriter, Track,
};
use pulsebeam_core::fabric::{WireError, media_source::Rejected};

const SLOW_POLL_INTERVAL: Duration = Duration::from_millis(100);
const POLL_WORK_BUDGET: usize = 256;
const NATIVE_PREPARATION_BUDGET: usize = 32;

enum RtcDrainResult {
    Quiescent(Instant),
    Pending,
    Closed,
}

fn inline_rtc_timeout(deadline: Instant, wall_now: Instant) -> Option<Instant> {
    let latest = wall_now
        .checked_add(pulsebeam_runtime::SHARD_TIMER_QUANTUM)
        .unwrap_or(wall_now);
    (deadline <= latest).then_some(deadline.max(wall_now))
}

pub struct TrackMapping {
    pub mid: Mid,
    pub publication_id: PublicationKey,
    pub kind: MediaKind,
}

#[derive(thiserror::Error, Debug)]
pub enum DisconnectReason {
    #[error("RTC connection closed: {0:?}")]
    RtcClosed(pulsebeam_rtc::CloseReason),
    #[error("Native signaling input error")]
    SignalingInput,
    #[error("Native signaling output error")]
    SignalingOutput,
    #[error("Native signaling channel closed")]
    SignalingClosed,
    #[error("ICE connection disconnected")]
    IceDisconnected,
    #[error("Unsupported media direction (must be SendOnly or RecvOnly)")]
    InvalidMediaDirection,
    #[error("Exceeded maximum upstream tracks: only 2 video and 2 audio allowed")]
    TooManyUpstreamTracks,
    #[error("Room closed")]
    RoomClosed,
    #[error("System terminated")]
    SystemTerminated,
}

pub struct ConnectionConfig {
    pub scope: entity::ConnectionScope,
    pub participant_id: entity::ParticipantId,
    pub connection_id: entity::ConnectionId,
    pub profile: SignalingProtocol,
    pub authorization_expiry: Option<pulsebeam_core::auth::AuthorizationExpiry>,
    pub rtc: pulsebeam_rtc::AcceptedConnection,
    pub resources: NegotiatedResources,
}

impl std::fmt::Debug for ConnectionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionConfig")
            .field("connection_id", &self.connection_id)
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

pub(crate) enum ParticipantInput<'a> {
    Network {
        batch: net::RecvPacketBatch,
        source_shard: crate::id::ShardId,
    },
    Timeout(Instant),
    PublicationCutoff {
        reference: u64,
        success: bool,
    },
    Authorization {
        revision: u64,
        expiry: Option<i64>,
    },
    Track {
        key: TrackHandle,
        packet: TrackPacketRef<'a>,
        cache: Option<&'a TrackStreamCache>,
    },
    Reverse {
        stream: TrackHandle,
        packet: ReversePacket,
    },
}

impl ConnectionConfig {
    pub fn ufrag(&self) -> String {
        self.resources.local_ufrag.clone()
    }
}

struct PendingSdpSender {
    media_index: u32,
    kind: MediaKind,
    track_id: Option<entity::TrackId>,
    codec: Option<pulsebeam_rtc::CodecInfo>,
}

struct BoundFabricOutput {
    branch: FabricLocalBranch,
    leg: SenderLeg<FabricLocalBranch>,
    source: SourceMetadata,
}

impl BoundFabricOutput {
    fn contains_branch(&self, branch: FabricLocalBranch) -> bool {
        self.branch == branch || self.leg.committed_branch() == Some(branch)
    }
}

struct BoundFabricIngress {
    publication: PublicationKey,
    fanout: TrackHandle,
    ingress: RtpIngressShadow,
    publisher_source: Option<FabricSourceHandle>,
    geometry_refresh_at: Option<Instant>,
    loss_measurement: FabricLossMeasurement,
    #[cfg(test)]
    observation: Option<Observation>,
}

#[derive(Default)]
enum FabricLossMeasurement {
    #[default]
    Continuous,
    AwaitingPrimary,
    ResumedAt(u16),
}

impl FabricLossMeasurement {
    fn observe_primary(&mut self, sequence: u16) {
        if matches!(self, Self::AwaitingPrimary) {
            *self = Self::ResumedAt(sequence);
        }
    }

    fn admits(&mut self, sequence: u16) -> bool {
        match *self {
            Self::Continuous => true,
            Self::ResumedAt(first) if sequence.wrapping_sub(first) < (1 << 15) => {
                *self = Self::Continuous;
                true
            }
            Self::AwaitingPrimary | Self::ResumedAt(_) => false,
        }
    }
}

impl BoundFabricIngress {
    fn geometry_refresh_due(&mut self, arrival: Instant) -> bool {
        let Some(due) = self.geometry_refresh_at else {
            self.geometry_refresh_at = arrival.checked_add(Duration::from_millis(500));
            return false;
        };
        if arrival < due {
            return false;
        }
        self.geometry_refresh_at = arrival.checked_add(Duration::from_secs(5));
        true
    }

    fn input(
        &mut self,
        now: Instant,
        rtp: pulsebeam_rtc::AuthenticatedRtp<'_>,
        events: &mut impl ParticipantSink,
    ) {
        let Some(source) = self.publisher_source else {
            self.observe(now, rtp);
            return;
        };
        if !rtp.is_repair && !rtp.wire_payload().is_empty() {
            self.loss_measurement
                .observe_primary(rtp.wire_header().sequence);
        }
        if let Ok(Some(input)) = self.ingress.prepare(rtp) {
            events.publish_fabric_media(source, input);
        }
    }

    fn observe(&mut self, now: Instant, rtp: pulsebeam_rtc::AuthenticatedRtp<'_>) {
        let outcome = self.ingress.observe(now.into(), rtp);
        #[cfg(test)]
        if let Ok(observation) = outcome {
            self.observation = Some(observation);
        }
        match outcome {
            Ok(Observation::Media {
                first: true,
                wan_plaintext_bytes,
                ..
            }) => {
                metrics::histogram!("participant_fabric_shadow_wan_plaintext_bytes")
                    .record(wan_plaintext_bytes as f64);
            }
            Err(DropReason::Admission(Rejected::Wire(WireError::Oversize))) => {
                metrics::counter!("participant_fabric_shadow_reject", "reason" => "oversize")
                    .increment(1);
            }
            _ => {}
        }
    }
}

/// Shard-owned media execution for one participant connection incarnation.
pub struct Connection {
    // Hot: touched on every packet
    transport: RtcDriver,
    downstream: DownstreamAllocator,
    stream_writer: StreamWriter,
    // Warm: touched per poll cycle
    upstream: UpstreamAllocator,
    sdp_senders: HashMap<Mid, PendingSdpSender>,
    negotiated: NegotiatedResources,
    signaling_profile: SignalingProtocol,
    media_ingress: HashMap<u32, Box<BoundFabricIngress>>,
    media_egress: std::collections::BTreeMap<Mid, BoundFabricOutput>,
    fenced_fabric_sources: HashSet<FabricSourceHandle>,
    pub(crate) participant_id: entity::ParticipantId,
    pub(crate) connection_id: entity::ConnectionId,
    last_keyframe_request: HashMap<(Mid, SourceEncoding), Instant>,
    pending_keyframe_requests: HashSet<(Mid, SourceEncoding)>,

    /// The compiled stream a published channel forwards into, recorded by the
    /// shard once it has minted one. Keyed by channel so an arriving SCTP
    /// frame reaches its fanout without hashing a room, a publisher or a
    /// topic — the identity it would otherwise have to reassemble on every
    /// packet.
    fabric_data: HashMap<crate::media::ChannelId, FabricDataLease>,
    native_data: native_data::NativeData,
    native_execution: native_data::NativeExecution,
    native_preparation_budget: usize,
    native_receivers: native_receiver::NativeReceivers,
    native_rtp: native_rtp::NativeRtp,

    /// Attributes str0m's own logs to this participant, for the simulator only.
    ///
    /// str0m is a library and logs without any notion of which peer it is serving, so a trace
    /// contains interleaved lines from every connection with nothing to tell them apart. That is
    /// not a cosmetic problem: probe results were read off such a trace and attributed to the
    /// wrong link, producing a confident and wrong diagnosis. A span makes the attribution part
    /// of the record instead of an inference.
    ///
    /// Built once here rather than per call. Constructing a span allocates and records fields,
    /// so doing it inside the poll loop is what makes this expensive; entering an existing one is
    /// comparatively cheap. It is still `sim`-only, because on the packet path even that cost is
    /// unwarranted for something only a human reading a trace benefits from.
    // Cold: touched rarely
    disconnect_reason: Option<DisconnectReason>,
    native_handoff: Option<EndpointHandoff>,
    native_terminal: bool,
    native_channel: Option<crate::media::ChannelId>,
    renewal_pending: bool,
    authorization_expiry: Option<pulsebeam_core::auth::AuthorizationExpiry>,
    authorization_revision: u64,
    last_slow_poll: Instant,
    pub(crate) scope: entity::ConnectionScope,
    pub(crate) shard_id: ShardId,
}

impl Connection {
    pub fn new(mut cfg: ConnectionConfig, shard_id: ShardId, udp_gso_size: usize) -> Self {
        let rtc = cfg.rtc;
        let ctx = LogCtx {
            connection_id: cfg.connection_id,
        };
        let native_handoff = cfg.resources.native_handoff.take();
        let now = Instant::now();
        #[cfg(feature = "sim")]
        let sim_span = tracing::info_span!("peer", connection_id = %cfg.connection_id);
        let mut participant = Self {
            transport: RtcDriver::new(
                rtc,
                udp_gso_size,
                now,
                #[cfg(feature = "sim")]
                sim_span,
            ),
            stream_writer: StreamWriter::new(),
            participant_id: cfg.participant_id,
            connection_id: cfg.connection_id,
            upstream: UpstreamAllocator::new(ctx),
            sdp_senders: HashMap::new(),
            negotiated: cfg.resources,
            signaling_profile: cfg.profile,
            media_ingress: HashMap::new(),
            media_egress: Default::default(),
            fenced_fabric_sources: HashSet::new(),
            downstream: DownstreamAllocator::new(ctx),
            disconnect_reason: None,
            native_handoff,
            native_terminal: false,
            native_channel: None,
            renewal_pending: false,
            authorization_expiry: cfg.authorization_expiry,
            authorization_revision: u64::from(cfg.authorization_expiry.is_some()),
            last_slow_poll: now,
            last_keyframe_request: HashMap::new(),
            pending_keyframe_requests: HashSet::new(),
            fabric_data: HashMap::new(),
            native_data: native_data::NativeData::default(),
            native_execution: native_data::NativeExecution::default(),
            native_preparation_budget: NATIVE_PREPARATION_BUDGET,
            native_receivers: native_receiver::NativeReceivers::default(),
            native_rtp: native_rtp::NativeRtp::default(),
            scope: cfg.scope,
            shard_id,
        };
        #[cfg(feature = "sim")]
        participant
            .downstream
            .video
            .set_simulator_subscriber(cfg.participant_id);
        if participant.negotiated.signaling_channel.is_some()
            && participant.native_handoff.is_none()
        {
            participant.disconnect(DisconnectReason::SignalingOutput);
        }
        for media in participant.negotiated.media_sections().to_vec() {
            participant.install_sdp_media(media);
        }
        participant
    }

    fn log_ctx(&self) -> LogCtx {
        LogCtx {
            connection_id: self.connection_id,
        }
    }

    #[allow(
        dead_code,
        reason = "the replacement signaling path consumes negotiated receiver coordinates in Plan 07"
    )]
    pub(crate) fn receiver_index(&self, kind: MediaKind, mid: Mid) -> Option<u32> {
        self.downstream.receiver_index(kind, mid)
    }

    pub fn apply(&mut self, effect: ParticipantEffect, track_handle: Option<TrackHandle>) {
        match effect {
            ParticipantEffect::TrackCandidateAdded { track } => {
                let Some(key) = track_handle else {
                    debug_assert!(false, "a track effect must resolve to a local handle");
                    return;
                };
                self.add_track_candidate(key, *track);
            }
            ParticipantEffect::TrackCandidateRemoved {
                publication_id,
                reason,
            } => {
                self.native_source_removed(publication_id, reason);
                let Some(key) = track_handle else {
                    debug_assert!(false, "a track effect must resolve to a local handle");
                    return;
                };
                self.remove_track_candidate(key, publication_id);
            }
            ParticipantEffect::TrackSubscribed { publication_id } => {
                let Some(key) = track_handle else {
                    debug_assert!(false, "a track effect must resolve to a local handle");
                    return;
                };
                if self.native_receivers.contains_source(publication_id) {
                    self.native_source_binding(publication_id, key, true);
                } else {
                    self.activate_track_binding(key, publication_id);
                }
            }
            ParticipantEffect::TrackUnsubscribed { publication_id } => {
                let Some(key) = track_handle else {
                    debug_assert!(false, "a track effect must resolve to a local handle");
                    return;
                };
                if self.native_receivers.contains_source(publication_id) {
                    self.native_source_binding(publication_id, key, false);
                } else {
                    self.deactivate_track_binding(key, publication_id);
                }
            }
            ParticipantEffect::TrackPublished { publication_id } => {
                let Some(key) = track_handle else {
                    debug_assert!(false, "a track effect must resolve to a local handle");
                    return;
                };
                self.retain_media_ingress(|binding| {
                    binding.publication != publication_id || binding.fanout == key
                });
                if publication_id.kind() != TrackKind::Data {
                    if self.native_rtp.bind_source(publication_id, key)
                        || (!self.upstream.owns_managed_track(publication_id)
                            && self.upstream.mid_for_track_id(publication_id).is_some())
                    {
                        self.upstream.bind_track_handle(publication_id, key);
                    }
                    return;
                }
            }
            ParticipantEffect::TrackUnpublished { publication_id } => {
                let Some(key) = track_handle else {
                    debug_assert!(false, "a track effect must resolve to a local handle");
                    return;
                };
                self.retain_media_ingress(|binding| {
                    binding.publication != publication_id || binding.fanout != key
                });
                if publication_id.kind() != TrackKind::Data {
                    if !self.upstream.owns_managed_track(publication_id) {
                        self.upstream.unbind_track_handle(publication_id, key);
                    }
                    return;
                }
            }
        }
    }

    pub(crate) fn input<'a>(&mut self, input: ParticipantInput<'a>) {
        match input {
            ParticipantInput::Network {
                batch,
                source_shard,
            } => self.on_ingress(batch, source_shard),
            ParticipantInput::Timeout(now) => self.on_timeout(now),
            ParticipantInput::Authorization { revision, expiry } => {
                if self.native_terminal {
                    return;
                }
                if revision > self.authorization_revision
                    && let Some(expiry) = expiry
                {
                    let Ok(expiry_seconds) = u64::try_from(expiry) else {
                        return;
                    };
                    self.authorization_revision = revision;
                    self.authorization_expiry = Some(
                        pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds(
                            expiry_seconds,
                        ),
                    );
                    self.renewal_pending = false;
                } else if revision == self.authorization_revision
                    && expiry.is_none()
                    && self.renewal_pending
                {
                    self.renewal_pending = false;
                }
            }
            ParticipantInput::PublicationCutoff { reference, success } => {
                self.complete_native_resource_cutoff(reference, success);
            }
            ParticipantInput::Track { key, packet, cache } => {
                self.on_track_packet(key, packet, cache);
            }
            ParticipantInput::Reverse { stream, packet } => self.on_reverse(stream, packet),
        }
    }

    fn add_track_candidate(&mut self, key: TrackHandle, track: Track) {
        let publication_id = track.id();
        debug_assert_ne!(track.meta().connection_id, self.connection_id);
        debug_assert_eq!(publication_id.kind(), track.kind());
        if !self.downstream.register_candidate(key, &track) {
            return;
        }
        if self.native_receivers.contains_source(publication_id) {
            self.native_source_added(key, &track);
        }
        if track.kind() != TrackKind::Data {
            self.on_track_published(key, track);
        }
    }

    fn activate_track_binding(&mut self, key: TrackHandle, publication_id: PublicationKey) {
        let Some(candidate) = self.downstream.track_candidate(key) else {
            debug_assert!(false, "binding activation requires a candidate");
            return;
        };
        debug_assert_eq!(candidate.publication_id, publication_id);
        self.downstream.activate_track_binding(key, publication_id);
    }

    fn deactivate_track_binding(&mut self, key: TrackHandle, publication_id: PublicationKey) {
        let Some(candidate) = self.downstream.track_candidate(key) else {
            debug_assert!(false, "binding deactivation requires a candidate");
            return;
        };
        debug_assert_eq!(candidate.publication_id, publication_id);
        self.downstream
            .deactivate_track_binding(key, publication_id);
    }

    fn remove_track_candidate(&mut self, key: TrackHandle, publication_id: PublicationKey) {
        let Some(candidate) = self.downstream.remove_track_candidate(key) else {
            return;
        };
        debug_assert_eq!(candidate.publication_id, publication_id);
        if self.native_receivers.contains_source(publication_id)
            && publication_id.kind() == TrackKind::Video
        {
            self.downstream
                .video
                .forget_owned_track(key, publication_id);
            self.downstream.dirty_allocation = true;
        } else if publication_id.kind() != TrackKind::Data {
            let _ = self.on_tracks_unpublished(std::slice::from_ref(&publication_id));
        }
    }

    fn on_track_packet(
        &mut self,
        key: TrackHandle,
        packet: TrackPacketRef<'_>,
        cache: Option<&TrackStreamCache>,
    ) {
        let Some(entry) = self.downstream.track_candidate(key) else {
            debug_assert!(false, "a TrackPacket must target an installed track");
            return;
        };
        if entry.participant_id == self.participant_id {
            debug_assert!(false, "a forwarding plan must target a remote track");
            return;
        }
        if entry.participant_id == self.participant_id {
            debug_assert!(false, "a participant must never receive its own track");
            return;
        }
        let kind = entry.publication_id.kind();
        let participant_id = entry.participant_id;
        let publication_id = entry.publication_id;
        self.native_rtp_delivery(key, packet, cache);
        match kind {
            TrackKind::Video if self.downstream.video.has_negotiated_slots() => {
                #[cfg(feature = "sim")]
                crate::sim_metrics::record_forwarded_media_for(
                    self.participant_id,
                    packet.payload.len() as u64,
                );
                let promoted = self.downstream.on_forward_rtp(
                    key,
                    packet.arrival_ts,
                    cache,
                    &mut self.stream_writer,
                );
                let _ = promoted;
            }
            TrackKind::Audio if self.downstream.audio.has_negotiated_slots() => {
                debug_assert!(cache.is_none(), "audio forwarding has no video cache");
                let origin = crate::entity::AudioOrigin {
                    participant: participant_id,
                    track: publication_id,
                };
                self.downstream
                    .on_forward_audio_rtp(origin, packet, &mut self.stream_writer);
                self.downstream.take_audio_speakers_changed();
            }
            TrackKind::Data => debug_assert!(false, "data tracks carry bytes, not RTP"),
            TrackKind::Audio | TrackKind::Video => {}
        }
    }

    fn on_ingress(&mut self, batch: net::RecvPacketBatch, source_shard: crate::id::ShardId) {
        self.transport.enqueue_ingress(batch, source_shard);
    }

    pub(crate) fn bind_tcp_flow(&mut self, flow: crate::ice_tcp::TcpFlow) -> bool {
        self.transport.bind_tcp_flow(flow)
    }

    pub(crate) fn retire_tcp_flow(&mut self, route: crate::ice_tcp::FlowRoute) {
        self.transport.retire_tcp_flow(route);
    }

    pub(crate) fn enqueue_tcp(&mut self, packet: crate::ice_tcp::TcpPacket) -> bool {
        self.transport.enqueue_tcp(packet)
    }

    pub(crate) fn staged_handoff(
        &self,
        participant: crate::keys::ParticipantHandle,
    ) -> Option<super::batcher::Handoff> {
        self.transport.staged_handoff(participant)
    }

    pub(crate) fn dispatch_handoff(&mut self, handoff: super::batcher::Handoff, authorized: bool) {
        self.transport.dispatch_handoff(handoff, authorized);
    }

    pub(crate) fn network_pending(&self) -> bool {
        self.transport.network_pending()
    }

    pub(crate) fn handoff_pending(&self, id: u64) -> bool {
        self.transport.handoff_pending(id)
    }

    pub(crate) fn resolve_handoff(&mut self, id: u64, accepted: bool) -> bool {
        self.transport.resolve_handoff(id, accepted)
    }

    pub fn drain_network(&mut self, egress: &mut impl NetworkEgress) {
        self.transport.drain_network(egress);
    }

    fn on_timeout(&mut self, now: Instant) {
        self.transport.enqueue_timeout(now);
    }

    fn on_track_published(&mut self, key: TrackHandle, track: Track) {
        if track.meta().connection_id == self.connection_id {
            debug_assert!(false, "the controller must not install a loopback track");
            return;
        }
        plog_info!(
            self.log_ctx(),
            track = %track.id(),
            origin = %track.meta().origin,
            "participant received published track"
        );
        let publication = track.id();
        self.downstream.install_track(key, track);
        if let Some(media) = self
            .negotiated
            .media_sections()
            .iter()
            .find(|media| media.receive_publication == Some(publication))
        {
            match media.kind {
                MediaKind::Audio => {
                    self.downstream
                        .set_audio_intent(crate::participant::intent::AudioIntent {
                            pinned: vec![publication],
                            auto: false,
                        });
                }
                MediaKind::Video => self.downstream.video.bind_sdp_slot(media.mid, publication),
            }
        }
    }

    fn on_tracks_unpublished(&mut self, tracks: &[PublicationKey]) -> bool {
        let mut removed = false;
        for publication_id in tracks {
            removed |= self.downstream.remove_track(publication_id);
        }
        removed
    }

    fn on_reverse(&mut self, stream: TrackHandle, packet: ReversePacket) {
        match packet.decode() {
            Some(ReverseInput::Keyframe { encoding, kind }) => {
                let Some(publication_id) = self.upstream.track_for_fanout(stream) else {
                    debug_assert!(
                        false,
                        "a reverse keyframe route must name a published track"
                    );
                    return;
                };
                self.enqueue_remote_keyframe((publication_id, encoding), kind);
            }
            None => {
                debug_assert!(false, "reverse route carried an invalid endpoint envelope");
            }
        }
    }

    fn enqueue_remote_keyframe(&mut self, stream_id: StreamId, kind: KeyframeRequestKind) {
        let Some(mid) = (if self.upstream.owns_managed_track(stream_id.0) {
            self.native_rtp.keyframe_mid(stream_id.0, stream_id.1)
        } else {
            self.upstream.mid_for_track_id(stream_id.0)
        }) else {
            plog_warn!(self.log_ctx(), stream_id = ?stream_id.0, "unknown upstream track for keyframe request");
            return;
        };
        let key = (mid, stream_id.1);
        let now = Instant::now();
        if self.pending_keyframe_requests.contains(&key) {
            plog_debug!(
                self.log_ctx(),
                ?stream_id,
                "deduplicated pending keyframe request"
            );
            return;
        }
        if let Some(last) = self.last_keyframe_request.get(&key)
            && now.duration_since(*last) < KEYFRAME_DEBOUNCE
        {
            plog_debug!(
                self.log_ctx(),
                ?stream_id,
                "debounced duplicate keyframe request"
            );
            return;
        }
        if self
            .transport
            .enqueue_mutation(TransportMutation::Keyframe {
                mid,
                encoding: stream_id.1,
                kind,
            })
        {
            debug_assert!(self.pending_keyframe_requests.insert(key));
        }
    }

    fn poll_slow(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        // Measure before allocating: the monitors produce this tick's numbers,
        // and running the allocator first would decide against last tick's.
        self.upstream.poll_slow(now);
        self.native_rtp.poll_stats(&mut self.upstream, now);
        let assignments_changed = self.downstream.poll_slow(now, events);
        self.transport
            .set_desired_bitrate(self.downstream.desired_bitrate());
        if assignments_changed {
            self.stage_allocated_rtp_receivers(events);
        }
    }

    fn apply_one_rtc_mutation(&mut self, now: Instant) -> bool {
        if self
            .authorization_expiry
            .is_some_and(|expiry| expiry.is_expired_at(native_data::unix_now()))
        {
            // A controller renewal may be in flight; fence old work without declaring expiry terminal.
            self.stream_writer.discard();
            self.pending_keyframe_requests.clear();
            self.transport.discard_mutations();
            return false;
        }
        if let Some(write) = self.stream_writer.pop() {
            let (pkt, publication_id, mid, rid, ssrc, pt, kind) = match write {
                StreamWrite::Video {
                    pkt,
                    publication_id,
                    mid,
                    rid,
                    ssrc,
                    pt,
                } => (pkt, publication_id, mid, rid, ssrc, pt, MediaKind::Video),
                StreamWrite::Audio {
                    pkt,
                    publication_id,
                    mid,
                    ssrc,
                    pt,
                } => (pkt, publication_id, mid, None, ssrc, pt, MediaKind::Audio),
            };
            if self.native_receivers.owns_mid(mid)
                && !self
                    .native_receivers
                    .can_write_rtp(mid, publication_id, ssrc, pt)
            {
                return true;
            }
            let result = self.transport.apply_rtp_command(RtpWriteCommand {
                pkt,
                publication_id,
                mid,
                rid,
                ssrc,
                pt,
                kind,
                now,
            });
            match result {
                AppliedMutation::KeyframeUnavailable { mid, encoding } => {
                    debug_assert!(self.pending_keyframe_requests.remove(&(mid, encoding)));
                }
                AppliedMutation::KeyframeRequested { .. }
                | AppliedMutation::RtpWritten
                | AppliedMutation::RtpNotWritten => {}
            }
            return true;
        }
        let applied = self.transport.apply_next_mutation();
        match applied {
            Some(AppliedMutation::KeyframeRequested { mid, encoding }) => {
                debug_assert!(self.pending_keyframe_requests.remove(&(mid, encoding)));
                self.last_keyframe_request.insert((mid, encoding), now);
            }
            Some(AppliedMutation::KeyframeUnavailable { mid, encoding }) => {
                debug_assert!(self.pending_keyframe_requests.remove(&(mid, encoding)));
            }
            _ => {}
        }
        applied.is_some()
    }

    pub(crate) fn poll(
        &mut self,
        now: Instant,
        events: &mut impl ParticipantSink,
    ) -> Option<Instant> {
        // A disconnect can be observed while work for this participant is still
        // queued, so poll can be re-entered after it has already exited.
        if self.transport.is_exited() {
            return None;
        }

        // Entered once per poll cycle rather than per packet, so every str0m line produced by
        // this participant's work carries its identity for the cost of one guard.
        //
        // Cloned first because the guard would otherwise hold a borrow of `self` for the whole
        // function. `Span` is a handle, so this is a refcount bump rather than a rebuild.
        #[cfg(feature = "sim")]
        let sim_span = self.transport.sim_span.clone();
        #[cfg(feature = "sim")]
        let _sim_guard = sim_span.enter();

        self.advance_rtc_clock(now, now);
        self.begin_native_control_turn(now, events);
        let mut work_budget = POLL_WORK_BUDGET;
        let mut control_blocked = false;
        'drain: loop {
            if work_budget == 0 {
                metrics::counter!("participant_poll_budget_hit").increment(1);
                let next = now.checked_add(Duration::from_micros(1)).unwrap_or(now);
                return Some(next);
            }
            work_budget = work_budget.saturating_sub(1);
            // Offer control before media draining can exhaust this turn. Drain RTC after
            // each batch so continuous control churn cannot starve packet service either.
            let mut control_progress = false;
            if !control_blocked && self.native_handoff.is_some() {
                match self.poll_native_signaling() {
                    Ok(result) => {
                        control_blocked = matches!(result, SignalingWrite::Blocked);
                        control_progress = !matches!(result, SignalingWrite::Idle);
                    }
                    Err(err) => {
                        plog_warn!(self.log_ctx(), ?err, "native signaling output failed");
                        self.disconnect(DisconnectReason::SignalingOutput);
                        // Drain the aborted RTC below to emit its terminal lifecycle event.
                        control_blocked = true;
                    }
                }
            }
            if self.transport.needs_drain() {
                match self.poll_rtc(now, events, &mut work_budget) {
                    RtcDrainResult::Quiescent(deadline) => {
                        self.transport.set_drain_result(Some(deadline));
                    }
                    RtcDrainResult::Pending => {
                        return Some(now.checked_add(Duration::from_micros(1)).unwrap_or(now));
                    }
                    RtcDrainResult::Closed => {
                        self.transport.set_drain_result(None);
                        // Controller is responsible to clean up tracks.
                        events.exit();
                        return None;
                    }
                }
            }
            debug_assert!(self.transport.deadline().is_some());

            if let Some(deadline) = self.transport.take_timeout() {
                self.advance_rtc_clock(deadline.max(now), now);
                self.transport.timeout();
                continue;
            }

            if self.advance_native_source(events) {
                continue;
            }

            if self.apply_one_rtc_mutation(now) {
                self.transport.mark_needs_drain();
                continue;
            }

            if now.saturating_duration_since(self.last_slow_poll) >= SLOW_POLL_INTERVAL {
                self.poll_slow(now, events);
                self.last_slow_poll = now;
                self.transport.mark_needs_drain();
                continue;
            }

            self.advance_rtc_clock(now, now);
            let receive_at = self.transport.clock();
            while self.transport.has_pending_ingress() {
                match self.transport.receive_pending(receive_at) {
                    IngressResult::Empty => continue,
                    IngressResult::Received => continue 'drain,
                }
            }

            if control_progress {
                continue;
            }

            if self.downstream.dirty_allocation {
                let assignments_changed = self.downstream.update_allocations(now);
                self.transport
                    .set_desired_bitrate(self.downstream.desired_bitrate());
                self.downstream.reconcile_routes(now, events);
                if assignments_changed {
                    self.stage_allocated_rtp_receivers(events);
                }
                self.transport.mark_needs_drain();
                continue;
            }

            let next_slow_poll = self
                .last_slow_poll
                .checked_add(SLOW_POLL_INTERVAL)
                .unwrap_or(self.last_slow_poll);
            // A drained Rtc always reports one; falling back to the slow poll
            // costs a tick of latency rather than the process.
            let deadline = self
                .transport
                .deadline()
                .unwrap_or(next_slow_poll)
                .min(next_slow_poll);
            let deadline = self
                .native_data
                .deadline()
                .into_iter()
                .chain(self.native_receivers.deadline())
                .fold(deadline, Instant::min);

            if let Some(rtc_now) = inline_rtc_timeout(deadline, now) {
                self.advance_rtc_clock(rtc_now, now);
                self.transport.timeout();
                continue;
            }

            return Some(
                self.native_preparation_deadline(now)
                    .map_or(deadline, |next| next.min(deadline)),
            );
        }
    }

    fn begin_native_control_turn(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        self.native_preparation_budget = NATIVE_PREPARATION_BUDGET;
        if self
            .native_handoff
            .as_ref()
            .is_some_and(EndpointHandoff::failed)
        {
            self.disconnect(DisconnectReason::SignalingOutput);
            return;
        }
        self.evaluate_native_preparations(events);
        self.expire_native_data(now, events);
    }

    fn native_preparation_deadline(&mut self, now: Instant) -> Option<Instant> {
        if self
            .native_handoff
            .as_mut()
            .is_some_and(EndpointHandoff::commands_pending)
        {
            return Some(now.checked_add(Duration::from_micros(1)).unwrap_or(now));
        }
        None
    }

    fn advance_rtc_clock(&mut self, candidate: Instant, wall_now: Instant) {
        self.transport.advance_clock(candidate, wall_now);
    }

    pub(crate) fn retire_fabric_rtp_scope(&mut self, branch: FabricLocalBranch) {
        self.downstream.video.retire_fabric_selection(branch);
        if let Some(scope) = branch.scope() {
            self.transport.retire_fabric_rtp_scope(scope);
        }
    }

    pub(crate) fn discard_pending_fabric_rtp(&mut self, scope: pulsebeam_rtc::MediaScope) -> bool {
        self.transport.discard_pending_fabric_rtp(scope)
    }

    pub(crate) fn attach_fabric_rtp_output(
        &mut self,
        branch: FabricLocalBranch,
        leg: SenderLeg<FabricLocalBranch>,
        source: SourceMetadata,
    ) -> bool {
        let output = leg.output();
        if !self.fabric_io_authorized()
            || self.media_egress.len() >= crate::route::fabric::local::MAX_BRANCHES
            || self.media_egress.contains_key(&output.mid)
            || self.has_fabric_rtp_output(branch)
            || !self.transport.fabric_rtp_sender_is_fresh(output)
        {
            return false;
        }
        self.media_egress.insert(
            output.mid,
            BoundFabricOutput {
                branch,
                leg,
                source,
            },
        );
        true
    }

    pub(crate) fn selected_fabric_video_encoding(
        &self,
        mid: Mid,
        track: TrackHandle,
    ) -> Option<(crate::track::SourceEncoding, u8)> {
        self.downstream.video.selected_encoding(mid, track)
    }

    pub(crate) fn fabric_video_output_selects(
        &self,
        branch: FabricLocalBranch,
        track: TrackHandle,
        encoding: crate::route::fabric::RtpSourceEncoding,
        target: u8,
    ) -> bool {
        self.media_egress.iter().any(|(&mid, output)| {
            output.contains_branch(branch)
                && self.downstream.video.allows_fabric_encoding(
                    mid,
                    branch,
                    (track, encoding, target),
                    output.branch == branch,
                )
        })
    }

    pub(crate) fn fabric_rtp_output_branches(
        &self,
        mid: Mid,
    ) -> Option<(FabricLocalBranch, Option<FabricLocalBranch>)> {
        let output = self.media_egress.get(&mid)?;
        Some((output.branch, output.leg.committed_branch()))
    }

    pub(crate) fn whep_rtp_output(
        &self,
        publication: PublicationKey,
        profile: pulsebeam_core::fabric::Profile,
    ) -> Option<(Mid, u32)> {
        if self.signaling_profile != SignalingProtocol::Whep {
            return None;
        }
        let kind = match publication.kind() {
            TrackKind::Audio => MediaKind::Audio,
            TrackKind::Video => MediaKind::Video,
            TrackKind::Data => return None,
        };
        let media = self.negotiated.media_sections().iter().find(|media| {
            media.receive_publication == Some(publication)
                && media.kind == kind
                && media.direction == Direction::SendOnly
        })?;
        if self.transport.sdp_fabric_send_profile(media.mid) != Some(profile) {
            return None;
        }
        Some((media.mid, self.transport.stream_tx_ssrc(media.mid)?.into()))
    }

    #[cfg(feature = "sim")]
    pub(crate) fn has_whep_output(&self) -> bool {
        self.negotiated
            .media_sections()
            .iter()
            .any(|media| media.receive_publication.is_some())
    }

    pub(crate) fn bind_local_rtp_output(
        &mut self,
        branch: FabricLocalBranch,
        mid: Mid,
        primary: u32,
        emission: crate::route::fabric::local::Emission,
    ) -> bool {
        use crate::rtp::fabric_egress::{
            CaptureClock, EndpointHints, Mapping, OutputLeg, Vad, timeline,
        };
        use crate::rtp::fabric_ingress::AUDIO_LEVEL_URI;
        use pulsebeam_core::fabric::ExtensionKind;
        let profile = emission.profile();
        let requires_dd = emission.selects_dd_encoding();
        if let Some(output) = self.media_egress.get_mut(&mid) {
            if output.leg.output().primary != Some(primary) {
                return false;
            }
            output.branch = branch;
            return true;
        }
        let Some(form) = self.transport.local_rtp_output_form(mid, primary) else {
            return false;
        };
        let mut mappings = arrayvec::ArrayVec::<_, 1>::new();
        if profile == pulsebeam_core::fabric::Profile::Opus
            && let Some(id) = self
                .transport
                .fabric_rtp_extension_id(mid, primary, AUDIO_LEVEL_URI)
        {
            mappings.push(Mapping {
                id,
                uri: AUDIO_LEVEL_URI,
                attributes: None,
            });
        }
        if emission.includes(ExtensionKind::DependencyDescriptor)
            && let Some(id) =
                self.transport
                    .fabric_rtp_extension_id(mid, primary, pulsebeam_core::dd::URI)
        {
            mappings.push(Mapping {
                id,
                uri: pulsebeam_core::dd::URI,
                attributes: None,
            });
        }
        let Ok(hints) = EndpointHints::new(profile, form, &mappings) else {
            return false;
        };
        let [.., a, b, c, d, e, f] = *self.connection_id.as_bytes();
        let Some(timeline) = timeline::Timeline::new(
            u16::from_be_bytes([a, b]),
            u32::from_be_bytes([c, d, e, f]),
            profile.clock_rate(),
            64,
        ) else {
            return false;
        };
        let mut leg = SenderLeg::new(
            OutputLeg {
                mid,
                primary: Some(primary),
                max_wire_bytes: usize::MAX,
                replace_queued: false,
            },
            hints,
            timeline,
        );
        if requires_dd {
            leg = leg.with_required_dd_evidence(u16::from_be_bytes([a, b]));
        }
        if requires_dd {
            let Some(pair) = self.ready_fabric_receiver_fid(mid, primary) else {
                return false;
            };
            leg = leg.with_answer_fid(pair);
        } else if profile == pulsebeam_core::fabric::Profile::Av1 {
            // Native joins this branch only after the receiver reports its applied binding.
            let Some(pair) = self.transport.fabric_rtp_answer_fid(mid, primary) else {
                return false;
            };
            leg = leg
                .with_answer_fid(pair)
                .with_dependency_descriptors(u16::from_be_bytes([a, b]));
        }
        self.attach_fabric_rtp_output(
            branch,
            leg,
            SourceMetadata {
                vad: Vad::On,
                capture: CaptureClock::Unmapped,
                same_topology: false,
            },
        )
    }

    pub(crate) fn has_fabric_rtp_output(&self, branch: FabricLocalBranch) -> bool {
        self.media_egress
            .values()
            .any(|output| output.contains_branch(branch))
    }

    pub(crate) fn queue_bound_fabric_rtp(
        &mut self,
        branch: FabricLocalBranch,
        packet: CandidatePacket<'_, '_>,
        video_source: Option<(TrackHandle, crate::route::fabric::RtpSourceEncoding)>,
        now: Instant,
    ) -> Result<Option<FabricLocalBranch>, AdmissionError> {
        use pulsebeam_core::dd::output::Mode;

        self.advance_rtc_clock(now, now);
        let (&mid, output) = self
            .media_egress
            .iter_mut()
            .find(|(_, output)| output.contains_branch(branch))
            .ok_or(AdmissionError::Rtc(
                pulsebeam_rtc::WriteError::UnknownStream,
            ))?;
        let target = match packet.2.mode {
            Mode::Filtered(index) => Some(index),
            Mode::Whole => None,
        };
        let previous = output.leg.committed_branch();
        let profile = packet.1.header().profile;
        output.leg.stage_branch(branch);
        self.transport
            .queue_fabric_rtp(&mut output.leg, branch, output.source, packet)?;
        if previous != Some(branch) {
            use pulsebeam_core::fabric::Profile;
            let profile = match profile {
                Profile::Opus => Some("opus"),
                Profile::H264 => Some("h264"),
                Profile::Av1 => Some("av1"),
                Profile::Data => None,
            };
            if let Some(profile) = profile {
                metrics::counter!("fabric_rtp_branch_admissions", "profile" => profile)
                    .increment(1);
            }
        }
        if let Some((source, target)) = video_source.zip(target) {
            self.downstream
                .video
                .note_fabric_admission(mid, branch, source, target);
        }
        Ok(previous.filter(|active| *active != branch))
    }

    pub(crate) fn queue_fabric_rtp(
        &mut self,
        leg: &mut crate::rtp::fabric_egress::SenderLeg<crate::shard::router::FabricLocalBranch>,
        branch: crate::shard::router::FabricLocalBranch,
        source: crate::rtp::fabric_egress::SourceMetadata,
        packet: crate::rtp::fabric_egress::CandidatePacket<'_, '_>,
        now: Instant,
    ) -> Result<(), crate::rtp::fabric_egress::AdmissionError> {
        self.advance_rtc_clock(now, now);
        self.transport.queue_fabric_rtp(leg, branch, source, packet)
    }

    pub(crate) fn matches_network_repair(
        &self,
        branch: crate::shard::router::FabricLocalBranch,
        child: crate::route::fabric::network_repair::RepairChild,
        packet: &crate::route::fabric::local::OwnedPacket,
    ) -> bool {
        self.connection_id == child.receiver
            && self.reliable_subscriber_branch(branch).is_some_and(|lease| {
                lease.endpoint.source == child.route.source && lease.endpoint.branch == child.route.branch
            })
            && self.fabric_data.values().any(|lease| {
                matches!(lease, FabricDataLease::Output(output) if output.branch == branch && output.matches_repair(packet, child.demand.request.fragment))
            })
    }

    pub(crate) fn write_fabric_data(
        &mut self,
        branch: crate::shard::router::FabricLocalBranch,
        packet: crate::route::fabric::local::PacketView<'_>,
        repair: Option<crate::route::fabric::local::RepairFragment>,
    ) -> Result<(), data_fabric::WriteError> {
        let output = self
            .fabric_data
            .values()
            .find_map(|lease| match lease {
                FabricDataLease::Output(output) if output.branch == branch => Some(output),
                _ => None,
            })
            .ok_or(data_fabric::WriteError::Binding)?;
        let installation = output.installation;
        if !self.fabric_io_authorized()
            || (self
                .native_receivers
                .channels
                .contains_key(&installation.channel())
                && !self.native_receivers.fabric_can_write(installation))
        {
            return Err(data_fabric::WriteError::Binding);
        }
        if repair.is_some() && self.reliable_subscriber_branch(branch).is_none() {
            return Err(data_fabric::WriteError::Binding);
        }
        output.write(&mut self.transport, packet, repair)
    }

    pub(crate) fn attach_fabric_data_output(&mut self, output: data_fabric::Output) -> bool {
        if !self.can_stage_fabric_data(output.installation, false)
            || self.fabric_data.values().any(|entry| matches!(entry, FabricDataLease::Output(other) if other.branch == output.branch))
        {
            return false;
        }
        self.fabric_data.insert(
            output.installation.channel(),
            FabricDataLease::Output(output),
        );
        true
    }

    fn can_stage_fabric_data(
        &self,
        installation: pulsebeam_rtc::DataChannelInstallation,
        publisher: bool,
    ) -> bool {
        let cid = installation.channel();
        self.fabric_io_authorized()
            && !self
                .negotiated
                .signaling_channel
                .as_ref()
                .is_some_and(|channel| channel.designates(cid))
            && (!self.owns_data_channel(cid)
                || if publisher {
                    self.native_data.staging_fabric_installation(installation)
                } else {
                    self.native_receivers
                        .staging_fabric_installation(installation)
                })
            && !self.fabric_data.contains_key(&cid)
            && (matches!(
                self.transport.data_channel_installation(installation),
                Some(pulsebeam_rtc::DataChannelInstallationState::Pending)
            ) || self.transport.supports_fabric_data(installation))
    }

    pub(crate) fn attach_fabric_data_source(
        &mut self,
        source: FabricSourceHandle,
        binding: data_fabric::InstalledBinding,
    ) -> bool {
        if !self.can_stage_fabric_data(binding.channel.association, true)
            || self.fabric_data.values().any(
                |entry| matches!(entry, FabricDataLease::Input(input) if input.source == source),
            )
        {
            return false;
        }
        let Some(input) = data_fabric::Input::from_publisher(source, binding) else {
            return false;
        };
        self.fabric_data
            .insert(input.installation.channel(), FabricDataLease::Input(input));
        true
    }

    pub(crate) fn fabric_data_source_authorized(&self, source: FabricSourceHandle) -> bool {
        self.fabric_data_input_installation(source).is_some()
    }

    pub(crate) fn fabric_data_input_installation(
        &self,
        source: FabricSourceHandle,
    ) -> Option<pulsebeam_rtc::DataChannelInstallation> {
        if !self.fabric_io_authorized() {
            return None;
        }
        self.fabric_data.values().find_map(|entry| match entry {
            FabricDataLease::Input(input)
                if input.source == source
                    && self.transport.supports_fabric_data(input.installation) =>
            {
                Some(input.installation)
            }
            _ => None,
        })
    }

    fn retire_fabric_data(
        &mut self,
        cid: crate::media::ChannelId,
        events: &mut impl ParticipantSink,
    ) {
        if let Some(entry) = self.fabric_data.remove(&cid) {
            self.transport
                .cancel_data_channel_installation(entry.installation());
            if let FabricDataLease::Input(input) = entry {
                events.fence_fabric_source(input.source);
            }
        }
    }

    fn receive_fabric_data(
        &mut self,
        cid: crate::media::ChannelId,
        message: &DataMessage,
        now: Instant,
        events: &mut impl ParticipantSink,
    ) -> bool {
        let Some(entry) = self.fabric_data.get(&cid) else {
            return false;
        };
        if !self.fabric_io_authorized()
            || !self.transport.supports_fabric_data(entry.installation())
        {
            self.retire_fabric_data(cid, events);
            return true;
        }
        if self.native_data.channels.contains_key(&cid) && !self.native_data.can_write(cid) {
            return true;
        }
        if let DataMessage::Binary(bytes) = message
            && let Some(FabricDataLease::Input(input)) = self.fabric_data.get(&cid)
        {
            if input.reliable_endpoint.is_some() && !self.native_data.can_write(cid) {
                return true;
            }
            match events.publish_fabric_data(input.source, input.installation, bytes, now) {
                Ok(()) => (),
                Err(data_fabric::Error::SourceConflict) => {
                    self.retire_fabric_data(cid, events);
                }
                Err(_) => {}
            }
        }
        true
    }

    pub(crate) fn reliable_publisher_lease(
        &self,
        source: FabricSourceHandle,
        installation: pulsebeam_rtc::DataChannelInstallation,
    ) -> Option<(u64, pulsebeam_proto::data_control::PublisherBinding)> {
        let cid = installation.channel();
        let &owned = self.native_data.channels.get(&cid)?;
        let FabricDataLease::Input(input) = self.fabric_data.get(&cid)? else {
            return None;
        };
        if input.source != source
            || input.installation != installation
            || !self.fabric_source_authorized(source)
            || !self.native_data.can_write(cid)
            || !self.transport.supports_fabric_data(installation)
        {
            return None;
        }
        Some((owned, input.reliable_endpoint?))
    }

    pub(crate) fn authorize_fabric_original_request(
        &mut self,
        source: FabricSourceHandle,
        installation: pulsebeam_rtc::DataChannelInstallation,
    ) -> Option<(u64, pulsebeam_proto::data_control::PublisherBinding)> {
        self.observe_native_terminal();
        self.reliable_publisher_lease(source, installation)
    }

    pub(crate) fn send_fabric_original_request(
        &mut self,
        owned: u64,
        endpoint: pulsebeam_proto::data_control::PublisherBinding,
        request: pulsebeam_proto::data_control::OriginalDemand,
    ) {
        use pulsebeam_proto::{
            data_control::{self, PublisherObservation, publisher_observation},
            state::{NativeDataControl, OwnedDataControl, native_data_control::Endpoint},
        };
        let observation = PublisherObservation {
            record: Some(publisher_observation::Record::RequestOriginal(request)),
        };
        if let Ok(record) = data_control::encode(endpoint, &observation) {
            self.write_fabric_control(NativeDataControl {
                endpoint: Some(Endpoint::Publisher(OwnedDataControl { owned, record })),
            });
        }
    }

    fn receive_original_unavailable(
        &mut self,
        frame: pulsebeam_proto::state::OwnedDataControl,
        now: Instant,
        events: &mut impl ParticipantSink,
    ) {
        use pulsebeam_proto::data_control::{
            self, OriginalUnavailable, PublisherControl, publisher_control::Record,
        };
        let Some(cid) = self.native_data.publication_channel(frame.owned) else {
            return;
        };
        let Some(FabricDataLease::Input(input)) = self.fabric_data.get(&cid) else {
            return;
        };
        let Some((owned, endpoint)) =
            self.reliable_publisher_lease(input.source, input.installation)
        else {
            return;
        };
        if owned != frame.owned {
            return;
        }
        let Ok(PublisherControl {
            record:
                Some(Record::Unavailable(OriginalUnavailable {
                    demand: Some(demand),
                })),
        }) = data_control::decode(endpoint, &frame.record)
        else {
            return;
        };
        events.settle_fabric_original_unavailable(input.source, input.installation, demand, now);
    }

    pub(crate) fn acknowledge_fabric_admission(
        &mut self,
        source: FabricSourceHandle,
        admission: data_fabric::ReliableAdmission,
    ) -> bool {
        use pulsebeam_proto::{
            data_control::{self, PublisherObservation, SourceUnit, publisher_observation},
            state::{NativeDataControl, OwnedDataControl, native_data_control::Endpoint},
        };

        self.observe_native_terminal();
        let Some((owned, endpoint)) = self.reliable_publisher_lease(source, admission.installation)
        else {
            return false;
        };
        let observation = PublisherObservation {
            record: Some(publisher_observation::Record::Admission(SourceUnit {
                source_incarnation: endpoint.source.get(),
                unit_seq: Some(admission.unit.unit_seq),
            })),
        };
        let Ok(record) = data_control::encode(endpoint, &observation) else {
            return false;
        };
        self.write_fabric_control(NativeDataControl {
            endpoint: Some(Endpoint::Publisher(OwnedDataControl { owned, record })),
        })
    }

    pub(crate) fn write_fabric_delivery_report(
        &mut self,
        source: FabricSourceHandle,
        installation: pulsebeam_rtc::DataChannelInstallation,
        report: crate::route::fabric::local::DeliveryReport,
    ) -> crate::route::fabric::local::ExpectationWrite {
        use crate::route::fabric::local::{DeliveryOutcome, ExpectationWrite};
        use pulsebeam_proto::{
            data_control::{
                self, BranchUnit, ControlOutcome, Expectation, PublisherObservation, SourceUnit,
                publisher_observation,
            },
            state::{NativeDataControl, OwnedDataControl, native_data_control::Endpoint},
        };
        self.observe_native_terminal();
        let Some((owned, endpoint)) = self.reliable_publisher_lease(source, installation) else {
            return ExpectationWrite::Revoked;
        };
        let outcome = match report.outcome {
            DeliveryOutcome::Confirmed => ControlOutcome::Confirmed,
            DeliveryOutcome::Failed => ControlOutcome::Failed,
            DeliveryOutcome::Unknown => ControlOutcome::Unknown,
        };
        let observation = PublisherObservation {
            record: Some(publisher_observation::Record::Delivery(
                data_control::DeliveryOutcome {
                    expectation: Some(Expectation {
                        unit: Some(BranchUnit {
                            source: Some(SourceUnit {
                                source_incarnation: endpoint.source.get(),
                                unit_seq: Some(report.unit.unit_seq),
                            }),
                            branch_incarnation: report.branch_incarnation.get(),
                        }),
                        fragment_count: u32::from(report.unit.fragment_count),
                    }),
                    outcome: outcome as i32,
                },
            )),
        };
        let Ok(record) = data_control::encode(endpoint, &observation) else {
            return ExpectationWrite::Revoked;
        };
        if self.write_fabric_control(NativeDataControl {
            endpoint: Some(Endpoint::Publisher(OwnedDataControl { owned, record })),
        }) {
            ExpectationWrite::Queued
        } else {
            ExpectationWrite::Backpressured
        }
    }

    pub(crate) fn write_fabric_expectation(
        &mut self,
        branch: crate::shard::router::FabricLocalBranch,
        unit: crate::route::fabric::local::DataExpectation,
        incarnation: std::num::NonZeroU64,
    ) -> crate::route::fabric::local::ExpectationWrite {
        use crate::route::fabric::local::ExpectationWrite;
        use pulsebeam_proto::data_control::subscriber_observation::Record;

        self.observe_native_terminal();
        let Some(lease) = self
            .reliable_subscriber_branch(branch)
            .filter(|lease| lease.endpoint.branch == incarnation)
        else {
            return ExpectationWrite::Revoked;
        };
        if self.write_fabric_subscriber_control(lease, unit, Record::Expectation) {
            ExpectationWrite::Queued
        } else {
            ExpectationWrite::Backpressured
        }
    }

    pub(crate) fn write_fabric_receipt_ack(
        &mut self,
        lease: data_fabric::SubscriberLease,
        unit: crate::route::fabric::local::DataExpectation,
    ) -> bool {
        self.observe_native_terminal();
        self.reliable_subscriber_lease(lease.owned) == Some(lease)
            && self.write_fabric_subscriber_control(
                lease,
                unit,
                pulsebeam_proto::data_control::subscriber_observation::Record::ReceiptAck,
            )
    }

    fn write_fabric_subscriber_control(
        &mut self,
        lease: data_fabric::SubscriberLease,
        unit: crate::route::fabric::local::DataExpectation,
        observation: impl FnOnce(
            pulsebeam_proto::data_control::Expectation,
        )
            -> pulsebeam_proto::data_control::subscriber_observation::Record,
    ) -> bool {
        use pulsebeam_proto::data_control::{BranchUnit, Expectation, SourceUnit};
        self.write_fabric_subscriber_observation(
            lease,
            observation(Expectation {
                unit: Some(BranchUnit {
                    source: Some(SourceUnit {
                        source_incarnation: lease.endpoint.source.get(),
                        unit_seq: Some(unit.unit_seq),
                    }),
                    branch_incarnation: lease.endpoint.branch.get(),
                }),
                fragment_count: u32::from(unit.fragment_count),
            }),
        )
    }

    pub(crate) fn write_fabric_repair_outcome(
        &mut self,
        lease: data_fabric::SubscriberLease,
        request: crate::route::fabric::local::RepairRequest,
        status: pulsebeam_proto::data_control::RepairStatus,
    ) -> bool {
        use pulsebeam_proto::data_control::{
            BranchUnit, RepairDemand, RepairOutcome, SourceUnit, subscriber_observation,
        };
        self.observe_native_terminal();
        if self.reliable_subscriber_lease(lease.owned) != Some(lease) {
            return false;
        }
        self.write_fabric_subscriber_observation(
            lease,
            subscriber_observation::Record::Repair(RepairOutcome {
                demand: Some(RepairDemand {
                    unit: Some(BranchUnit {
                        source: Some(SourceUnit {
                            source_incarnation: lease.endpoint.source.get(),
                            unit_seq: Some(request.fragment.unit_seq),
                        }),
                        branch_incarnation: lease.endpoint.branch.get(),
                    }),
                    fragment_index: Some(u32::from(request.fragment.index)),
                    request_id: request.id.get(),
                }),
                status: status as i32,
            }),
        )
    }

    fn write_fabric_subscriber_observation(
        &mut self,
        lease: data_fabric::SubscriberLease,
        record: pulsebeam_proto::data_control::subscriber_observation::Record,
    ) -> bool {
        use pulsebeam_proto::{
            data_control::{self, SubscriberObservation},
            state::{NativeDataControl, OwnedDataControl, native_data_control::Endpoint},
        };
        let Ok(record) = data_control::encode(
            lease.endpoint,
            &SubscriberObservation {
                record: Some(record),
            },
        ) else {
            return false;
        };
        self.write_fabric_control(NativeDataControl {
            endpoint: Some(Endpoint::Subscriber(OwnedDataControl {
                owned: lease.owned,
                record,
            })),
        })
    }

    fn write_fabric_control(&mut self, control: pulsebeam_proto::state::NativeDataControl) -> bool {
        let Some(cid) = self
            .native_channel_id()
            .filter(|_| self.native_handoff.is_some())
        else {
            return false;
        };
        if !self.fabric_io_authorized() || self.transport.signaling_pending() {
            return false;
        }
        let Ok(bytes) = pulsebeam_proto::state::codec::encode_data_control(&control) else {
            return false;
        };
        self.transport.write_channel(cid, true, &bytes)
    }

    fn receive_native_clock_control(
        &mut self,
        frame: pulsebeam_proto::state::NativeClockControl,
        now: Instant,
        events: &mut impl ParticipantSink,
    ) {
        use pulsebeam_proto::state::{
            NativeClockControl,
            codec::{ClockDirection, encode_clock_control},
        };
        if !self.fabric_io_authorized() || self.native_handoff.is_none() {
            return;
        }
        let Some(control) = frame.exchange else {
            return;
        };
        let Some(FabricDataLease::Input(input)) = self
            .native_data
            .publication_channel(control.owned)
            .and_then(|cid| self.fabric_data.get(&cid))
        else {
            return;
        };
        if let Some(response) =
            events.native_clock_control(input.source, input.installation, now, control)
            && let Some(channel) = self.native_channel_id()
            && !self.transport.signaling_pending()
            && let Ok(bytes) = encode_clock_control(
                &NativeClockControl {
                    exchange: Some(response),
                },
                ClockDirection::ServerToClient,
            )
        {
            let _ = self.transport.write_channel(channel, true, &bytes);
        }
    }

    fn receive_fabric_control(
        &mut self,
        frame: pulsebeam_proto::state::NativeDataControl,
        now: Instant,
        events: &mut impl ParticipantSink,
    ) {
        use super::event::{FabricFeedbackKind, FabricSubscriberFeedback};
        use crate::route::fabric::local::{
            DataExpectation, PropagationOutcome, RepairFragment, RepairRequest,
        };
        use pulsebeam_proto::{
            data_control::{self, ControlOutcome, SubscriberControl, subscriber_control::Record},
            state::native_data_control::Endpoint,
        };
        use std::num::NonZeroU64;
        let frame = match frame.endpoint {
            Some(Endpoint::Subscriber(frame)) => frame,
            Some(Endpoint::Publisher(frame)) => {
                self.receive_original_unavailable(frame, now, events);
                return;
            }
            None => return,
        };
        let Some(lease) = self.reliable_subscriber_lease(frame.owned) else {
            return;
        };
        let Ok(SubscriberControl {
            record: Some(record),
        }) = data_control::decode(lease.endpoint, &frame.record)
        else {
            return;
        };
        let expectation = |expected: data_control::Expectation| -> Option<DataExpectation> {
            Some(DataExpectation {
                unit_seq: expected.unit?.source?.unit_seq?,
                fragment_count: u16::try_from(expected.fragment_count).ok()?,
            })
        };
        let repair = |demand: data_control::RepairDemand| -> Option<RepairRequest> {
            Some(RepairRequest {
                fragment: RepairFragment {
                    unit_seq: demand.unit?.source?.unit_seq?,
                    index: u16::try_from(demand.fragment_index?).ok()?,
                },
                id: NonZeroU64::new(demand.request_id)?,
            })
        };
        let kind = match record {
            Record::ExpectationOutcome(ack) => {
                let Some(unit) = ack.expectation.and_then(expectation) else {
                    return;
                };
                let outcome = match ControlOutcome::try_from(ack.outcome) {
                    Ok(ControlOutcome::Confirmed) => PropagationOutcome::Confirmed,
                    Ok(ControlOutcome::Failed) => PropagationOutcome::Failed,
                    Ok(ControlOutcome::Unknown) => PropagationOutcome::Unknown,
                    _ => return,
                };
                Some(FabricFeedbackKind::Propagation(unit, outcome))
            }
            Record::AssemblyReceipt(expected) => {
                expectation(expected).map(FabricFeedbackKind::Assembled)
            }
            Record::RequestRepair(demand) => repair(demand).map(FabricFeedbackKind::Repair),
            Record::CancelRepair(demand) => repair(demand).map(FabricFeedbackKind::CancelRepair),
        };
        if let Some(kind) = kind {
            events.observe_fabric_subscriber(FabricSubscriberFeedback { lease, kind });
        }
    }

    pub(crate) fn prepare_whip_rtp_ingress(
        &mut self,
        publication: PublicationKey,
        fanout: TrackHandle,
    ) -> Option<(crate::track::TrackMeta, RtpIngressShadow)> {
        if self.signaling_profile != SignalingProtocol::Whip
            || self.authorization_expiry.is_none()
            || !self.fabric_io_authorized()
            || self.upstream.track_fanout(publication) != Some(fanout)
        {
            return None;
        }
        let mid = self.upstream.mid_for_track_id(publication)?;
        let (profile, binding) = self.transport.sdp_fabric_receive_binding(mid)?;
        if self.media_ingress.contains_key(&binding.primary_ssrc) {
            return None;
        }
        let ingress = RtpIngressShadow::bind_local_endpoint(profile, binding)?;
        let (track, announced) = self.upstream.announce_state_mut(mid)?;
        (*announced && track.id() == publication).then(|| (track.meta().clone(), ingress))
    }

    /// Attach one explicitly qualified diagnostic binding to an installed local
    /// media publication. It emits measurements only and is not enabled by
    /// negotiation or publication creation.
    pub fn attach_fabric_media_shadow(
        &mut self,
        publication: PublicationKey,
        ingress: RtpIngressShadow,
    ) -> bool {
        self.bind_fabric_media(publication, ingress, None)
    }

    pub(crate) fn attach_fabric_media_source(
        &mut self,
        publication: PublicationKey,
        ingress: RtpIngressShadow,
        source: FabricSourceHandle,
    ) -> bool {
        self.fabric_source_authorized(source)
            && self.bind_fabric_media(publication, ingress, Some(source))
    }

    fn bind_fabric_media(
        &mut self,
        publication: PublicationKey,
        ingress: RtpIngressShadow,
        publisher_source: Option<FabricSourceHandle>,
    ) -> bool {
        if publication.kind() == TrackKind::Data || !self.media_ingress_authorized() {
            return false;
        }
        let Some(fanout) = self.upstream.track_fanout(publication) else {
            return false;
        };
        let std::collections::hash_map::Entry::Vacant(entry) =
            self.media_ingress.entry(ingress.primary_ssrc())
        else {
            return false;
        };
        entry.insert(Box::new(BoundFabricIngress {
            publication,
            fanout,
            ingress,
            publisher_source,
            geometry_refresh_at: None,
            loss_measurement: FabricLossMeasurement::default(),
            #[cfg(test)]
            observation: None,
        }));
        true
    }

    pub(crate) fn claim_fabric_sender_report(
        &mut self,
        ssrc: u32,
        mid: Mid,
    ) -> Option<pulsebeam_rtc::SenderReport> {
        self.transport.claim_fabric_sender_report(ssrc, mid)
    }

    pub(crate) fn request_fabric_refresh(&mut self, ssrc: u32) -> bool {
        self.fabric_io_authorized() && self.transport.request_fabric_refresh(ssrc)
    }

    pub(crate) fn fabric_io_authorized(&self) -> bool {
        !self.native_terminal && self.media_ingress_authorized()
    }

    pub(crate) fn fence_fabric_media(&mut self, source: FabricSourceHandle) {
        self.fenced_fabric_sources.insert(source);
    }

    pub(crate) fn fabric_source_authorized(&self, source: FabricSourceHandle) -> bool {
        !self.fenced_fabric_sources.contains(&source) && self.fabric_io_authorized()
    }

    pub(crate) fn measure_fabric_dd(
        &mut self,
        source: FabricSourceHandle,
        encoding: crate::route::fabric::RtpSourceEncoding,
        now: Instant,
        facts: Option<crate::rtp::normalize::StreamFacts>,
        sample: crate::route::fabric::local::DecodeTargetSample,
    ) {
        let primary = *encoding.primary;
        if self
            .media_ingress
            .get(&primary)
            .is_some_and(|binding| binding.publisher_source == Some(source))
            && let Some(slot) = self.native_rtp.fabric_measurement_slot(primary)
            && let Some(track) = self.upstream.track_mut(slot)
        {
            track
                .monitor
                .measure_fabric_dd(encoding.primary, encoding.rid, now, facts, sample);
        }
    }

    pub(crate) fn detach_fabric_media_source(&mut self, source: FabricSourceHandle) {
        self.retain_media_ingress(|binding| binding.publisher_source != Some(source));
    }

    fn retain_media_ingress(&mut self, retain: impl Fn(&BoundFabricIngress) -> bool) {
        self.media_ingress.retain(|_, binding| {
            if retain(binding) {
                return true;
            }
            if let Some(source) = binding.publisher_source {
                self.fenced_fabric_sources.remove(&source);
            }
            false
        });
    }

    fn media_ingress_authorized(&self) -> bool {
        self.disconnect_reason.is_none()
            && !self
                .authorization_expiry
                .is_some_and(|expiry| expiry.is_expired_at(native_data::unix_now()))
    }

    fn poll_transport(
        &mut self,
        now: Instant,
        events: &mut impl ParticipantSink,
    ) -> TransportPollOutput {
        if let Some(clock) = events.media_clock() {
            self.transport.set_sender_report_time(clock);
        }
        if self.media_ingress.is_empty() || !self.media_ingress_authorized() {
            return self.transport.poll_output(None);
        }
        let upstream = &self.upstream;
        let native_rtp = &self.native_rtp;
        let native_authorized = self.authorization_expiry.is_some();
        let ingress = &mut self.media_ingress;
        let fenced = &self.fenced_fabric_sources;
        let mut observer = |rtp: pulsebeam_rtc::AuthenticatedRtp<'_>| {
            let Some(shadow) = ingress.get_mut(&rtp.primary_ssrc) else {
                return;
            };
            if shadow
                .publisher_source
                .is_some_and(|source| fenced.contains(&source))
                || upstream.track_fanout(shadow.publication) != Some(shadow.fanout)
            {
                return;
            }
            let Ok(mid) = Mid::new(rtp.mid) else {
                return;
            };
            let Ok(rid) = rtp.rid.map(crate::media::Rid::new).transpose() else {
                return;
            };
            let authorized = if native_rtp.owns_encoding(rtp.primary_ssrc) {
                native_authorized
                    && native_rtp
                        .ingress_route(rtp.primary_ssrc, mid, rid)
                        .is_some_and(|route| {
                            route.publication_id == shadow.publication
                                && route.fanout == Some(shadow.fanout)
                        })
            } else {
                !upstream
                    .route_for_ssrc(rtp.primary_ssrc.into())
                    .is_some_and(|route| {
                        route.mid != mid || route.rid.is_some() && route.rid != rid
                    })
                    && upstream
                        .slot_for_mid(mid)
                        .is_some_and(|(_, publication)| publication == shadow.publication)
            };
            if authorized {
                shadow.input(now, rtp, events);
            }
        };
        self.transport.poll_output(Some(&mut observer))
    }

    fn poll_rtc(
        &mut self,
        now: Instant,
        events: &mut impl ParticipantSink,
        work_budget: &mut usize,
    ) -> RtcDrainResult {
        loop {
            if *work_budget == 0 {
                return RtcDrainResult::Pending;
            }
            *work_budget = work_budget.saturating_sub(1);
            match self.poll_transport(now, events) {
                TransportPollOutput::Pending => return RtcDrainResult::Pending,
                TransportPollOutput::Timeout(deadline) => {
                    return RtcDrainResult::Quiescent(deadline);
                }
                TransportPollOutput::Transmit => {}
                TransportPollOutput::Event(event) => self.handle_event(now, *event, events),
                TransportPollOutput::Closed(reason) => {
                    self.disconnect(DisconnectReason::RtcClosed(reason));
                    return RtcDrainResult::Closed;
                }
            }
        }
    }

    fn handle_event(&mut self, now: Instant, e: Event, events: &mut impl ParticipantSink) {
        match e {
            Event::AuthenticatedPath(path) => {
                if let Some(source_shard) = self.transport.authenticated_source(path)
                    && path.protocol == pulsebeam_rtc::NetworkProtocol::Udp
                {
                    // TCP is already bound to its owner at stream adoption, not UDP steering.
                    events.connected(path.remote, path.local, source_shard);
                }
            }
            Event::Rtp { mid, rid, packet } => {
                let Ok(mid) = Mid::new(&mid) else {
                    return;
                };
                let Ok(rid) = rid.as_deref().map(crate::media::Rid::new).transpose() else {
                    return;
                };
                self.measure_incoming_dd_rtp(packet, mid, rid);
            }
            Event::SenderReport(report) => self.handle_sender_report(now, report, events),
            Event::EncodingDiscovered(info) => {
                if self.native_terminal
                    || self
                        .native_handoff
                        .as_ref()
                        .is_some_and(EndpointHandoff::execution_stopped)
                {
                    return;
                }
                let Ok(mid) = Mid::new(&info.mid) else {
                    return;
                };
                if self.native_rtp.owns_encoding(info.ssrc) {
                    self.transport.register_owned_encoding(info);
                } else if self.activate_sdp_sender(mid) {
                    self.transport.register_encoding(info);
                    self.handle_stream_paused(mid, false, events);
                }
            }
            Event::EncodingRetired { ssrc, .. } => {
                if let Some(binding) = self.media_ingress.get(&ssrc) {
                    if let Some(source) = binding.publisher_source {
                        if self.fenced_fabric_sources.insert(source) {
                            events.fence_fabric_source(source);
                        }
                    } else {
                        self.media_ingress.remove(&ssrc);
                    }
                }
                self.native_rtp_ended(ssrc);
                self.transport.retire_encoding(ssrc);
                self.upstream.clear_routes();
            }
            Event::ScopedKeyframeRequested { scope } => {
                if self.fabric_io_authorized()
                    && let Some(branch) = crate::shard::router::FabricLocalBranch::from_scope(scope)
                {
                    events.request_fabric_refresh(branch);
                }
            }
            Event::KeyframeRequested { ssrc } => {
                if let Some(req) = self.transport.keyframe_request(ssrc)
                    && let Some((fanout, layer)) = self.downstream.handle_keyframe_request(now, req)
                {
                    events.request_reverse(
                        fanout,
                        ReversePacket::keyframe(layer.encoding, KeyframeRequestKind::Pli),
                    );
                }
            }
            Event::BandwidthEstimate(available) => self
                .downstream
                .update_bitrate(now, crate::bitrate::Bitrate::from(available.as_bps())),
            Event::DataChannel(DataChannelEvent::Opened {
                channel: cid,
                config: cfg,
            }) => {
                if let Some(channel) = self
                    .negotiated
                    .signaling_channel
                    .as_ref()
                    .filter(|channel| channel.designates(cid))
                {
                    if !channel.matches(cid, &cfg) {
                        self.disconnect(DisconnectReason::SignalingInput);
                        return;
                    }
                    plog_info!(self.log_ctx(), "internal media signaling is opened");
                    self.native_channel = Some(cid);
                    if let Some(handoff) = self.native_handoff.as_mut() {
                        if !handoff.opened(Opened {
                            channel: cid,
                            at: self.transport.clock(),
                        }) {
                            self.disconnect(DisconnectReason::SignalingOutput);
                        }
                    }
                    return;
                }
                if let Some(entry) = self.fabric_data.get(&cid) {
                    if !self.transport.supports_fabric_data(entry.installation()) {
                        self.retire_fabric_data(cid, events);
                    }
                    return;
                }
                if self.owns_data_channel(cid) {
                    self.native_data_opened(cid, cfg, events);
                    return;
                }
                self.transport.close_native_data(cid);
            }
            Event::DataChannel(
                DataChannelEvent::ResetComplete { .. } | DataChannelEvent::ResetFailed { .. },
            ) => {
                self.evaluate_native_preparations(events);
            }
            Event::DataChannel(DataChannelEvent::InstallationFailed { installation }) => {
                let cid = installation.channel();
                if self
                    .fabric_data
                    .get(&cid)
                    .is_some_and(|entry| entry.installation() == installation)
                {
                    self.retire_fabric_data(cid, events);
                }
                self.native_data_installation_failed(installation);
                self.evaluate_native_preparations(events);
            }
            Event::DataChannel(DataChannelEvent::Closed { channel: cid }) => {
                if Some(cid) == self.native_channel_id() {
                    self.disconnect(DisconnectReason::SignalingClosed);
                    return;
                }
                if self.fabric_data.contains_key(&cid) {
                    self.retire_fabric_data(cid, events);
                }
                if self.owns_data_channel(cid) {
                    self.native_data_closed(cid, events);
                }
            }
            Event::DataChannel(DataChannelEvent::Message {
                channel: cid,
                message,
            }) => {
                if self.receive_fabric_data(cid, &message, now, events) {
                    return;
                }
                let DataMessage::Binary(bytes) = message else {
                    if Some(cid) == self.native_channel_id() {
                        self.disconnect(DisconnectReason::SignalingClosed);
                    }
                    return;
                };
                if Some(cid) == self.native_channel_id() {
                    use pulsebeam_proto::state::codec::{
                        ClockDirection, decode_clock_control_frame,
                    };
                    if let Some(control) =
                        decode_clock_control_frame(&bytes, ClockDirection::ClientToServer)
                    {
                        if let Ok(control) = control {
                            self.receive_native_clock_control(control, now, events);
                        }
                        return;
                    }
                    if let Some(control) =
                        pulsebeam_proto::state::codec::decode_data_control_frame(&bytes)
                    {
                        if let Ok(control) = control {
                            self.receive_fabric_control(control, now, events);
                        }
                        return;
                    }
                    if let Some(handoff) = &self.native_handoff {
                        if !handoff.input(bytes) {
                            self.disconnect(DisconnectReason::SignalingInput);
                        }
                        return;
                    }
                    return;
                }
            }
            _ => {
                // tracing::warn!("unhandled event: {e:?}");
            }
        }
    }

    fn evaluate_native_preparations(&mut self, events: &mut impl ParticipantSink) {
        self.observe_native_terminal();
        if self.native_terminal {
            return;
        }
        if self.native_handoff.is_some() {
            while self.native_preparation_budget > 0 {
                let Some(command) = self
                    .native_handoff
                    .as_mut()
                    .and_then(EndpointHandoff::command)
                else {
                    break;
                };
                self.native_preparation_budget = self.native_preparation_budget.saturating_sub(1);
                self.execute_native_command(command, events);
                if self.disconnect_reason.is_some() {
                    break;
                }
            }
            return;
        }
    }

    fn owns_data_channel(&self, channel: crate::media::ChannelId) -> bool {
        self.native_data.channels.contains_key(&channel)
            || self.native_receivers.channels.contains_key(&channel)
    }

    fn handle_upstream_track_state(
        &mut self,
        mid: Mid,
        active: bool,
        events: &mut impl ParticipantSink,
    ) {
        if active {
            let Some((descriptor, in_topology)) = self.upstream.announce_state_mut(mid) else {
                return;
            };
            if *in_topology {
                return;
            }
            let track = descriptor.clone();
            *in_topology = true;
            events.publish_track(track);
            return;
        }

        let Some((descriptor, in_topology)) = self.upstream.announce_state_mut(mid) else {
            return;
        };
        if !*in_topology {
            return;
        }
        let meta = descriptor.meta().clone();
        *in_topology = false;
        events.unpublish_track(meta);
    }

    fn handle_stream_paused(&mut self, mid: Mid, paused: bool, events: &mut impl ParticipantSink) {
        // SDP-configured upstream slots publish on activity; pausing does not unpublish.
        if !paused {
            self.handle_upstream_track_state(mid, true, events);
        }
    }

    fn install_sdp_media(&mut self, media: crate::control::NegotiatedMedia) {
        let Some(resource) = self.negotiated.media(media.mid) else {
            self.disconnect(DisconnectReason::InvalidMediaDirection);
            return;
        };
        if resource.kind != media.kind || resource.direction != media.direction {
            self.disconnect(DisconnectReason::InvalidMediaDirection);
            return;
        }
        match media.direction {
            Direction::RecvOnly => {
                self.sdp_senders.insert(
                    media.mid,
                    PendingSdpSender {
                        media_index: resource.media_index,
                        kind: media.kind,
                        track_id: media.publish_name,
                        codec: media.publish_codec,
                    },
                );
            }
            Direction::SendOnly => {
                self.try_add_downstream_slot(resource.media_index, media.mid, media.kind);
            }
            _ => self.disconnect(DisconnectReason::InvalidMediaDirection),
        }
    }

    fn activate_sdp_sender(&mut self, mid: Mid) -> bool {
        let Some(room) = self.scope.joined_room() else {
            return false;
        };
        let Some(sender) = self.sdp_senders.remove(&mid) else {
            return self.upstream.slot_for_mid(mid).is_some();
        };
        let kind = match sender.kind {
            MediaKind::Audio => TrackKind::Audio,
            MediaKind::Video => TrackKind::Video,
        };
        if !self
            .upstream
            .add_published_track(sender.media_index, mid, kind, || {
                let meta = track::TrackMeta {
                    room,
                    id: PublicationKey::new(kind, entity::PublicationId::mint()),
                    origin: self.participant_id,
                    connection_id: self.connection_id,
                    track_id: sender.track_id,
                };
                let format = sender.codec.and_then(crate::control::sdp::codec_format);
                match sender.kind {
                    MediaKind::Audio => {
                        let mut track = track::new_audio(mid, meta);
                        if let Track::Audio(audio) = &mut track.1 {
                            audio.format = format;
                        }
                        track
                    }
                    MediaKind::Video => {
                        let mut track =
                            track::new_video(mid, meta, self.transport.incoming_layers(mid));
                        if let Track::Video(video) = &mut track.1 {
                            video.format = format;
                        }
                        track
                    }
                }
            })
        {
            self.disconnect(DisconnectReason::TooManyUpstreamTracks);
            return false;
        }
        true
    }

    fn preferred_send_pt(&self, mid: Mid, kind: MediaKind) -> Option<Pt> {
        self.transport.preferred_send_pt(mid, kind)
    }

    fn try_add_downstream_slot(&mut self, media_index: u32, mid: Mid, kind: MediaKind) {
        if self.downstream.has_slot(kind, mid) {
            return;
        }

        let ctx = self.log_ctx();
        let Some(pt) = self.preferred_send_pt(mid, kind) else {
            plog_warn!(
                ctx,
                media_index,
                ?kind,
                "no negotiated PT available for downstream slot"
            );
            return;
        };

        let Some(ssrc) = self.transport.stream_tx_ssrc(mid) else {
            plog_warn!(
                ctx,
                media_index,
                ?kind,
                "missing transmit stream for downstream slot"
            );
            return;
        };

        self.downstream.add_slot(SlotConfig {
            media_index,
            mid,
            // TODO: don't ignore simulcast receivers
            rid: None,
            pt,
            ssrc,
            kind,
        });
    }

    fn measure_incoming_dd_rtp(
        &mut self,
        rtp: pulsebeam_rtc::RtpPacket,
        mid: Mid,
        rid: Option<crate::media::Rid>,
    ) {
        let ssrc = rtp.header.ssrc;
        let sequence = rtp.header.sequence;
        if !self.media_ingress_authorized()
            || self.authorization_expiry.is_none()
            || !self.native_rtp.requires_dd_fabric(ssrc)
        {
            return;
        }
        if self
            .media_ingress
            .get(&ssrc)
            .and_then(|binding| binding.publisher_source)
            .is_some_and(|source| self.fabric_source_authorized(source))
            && let Some(route) = self.native_rtp.ingress_route(ssrc, mid, rid)
            && let Some(incoming) = self.transport.convert_rtp(rtp, mid, rid)
            && let Some(binding) = self.media_ingress.get_mut(&ssrc)
            && binding.loss_measurement.admits(sequence)
            && let Some(track) = self.upstream.track_mut(route.upstream_slot)
            && track.monitor.measure_fabric(rid, &incoming.packet)
            && binding.geometry_refresh_due(incoming.packet.arrival_ts)
        {
            self.enqueue_remote_keyframe(
                (route.publication_id, route.encoding),
                KeyframeRequestKind::Pli,
            );
        }
    }

    fn handle_sender_report(
        &mut self,
        now: Instant,
        report: pulsebeam_rtc::SenderReport,
        events: &mut impl ParticipantSink,
    ) {
        if self.fabric_io_authorized() {
            match self
                .media_ingress
                .get(&report.ssrc)
                .and_then(|ingress| ingress.publisher_source)
            {
                Some(source) if !self.fenced_fabric_sources.contains(&source) => {
                    events.observe_fabric_sender_report(source, now, report);
                }
                None => self.transport.defer_fabric_sender_report(report),
                _ => (),
            }
        }
    }

    fn poll_native_signaling(&mut self) -> Result<SignalingWrite, OutputError> {
        self.observe_native_terminal();
        if let Some(handoff) = &mut self.native_handoff {
            if handoff.failed() {
                return Err(OutputError::InvalidObservation);
            }
            if !self.transport.signaling_pending()
                && let Some(output) = handoff.output()
                && self.transport.offer_signaling(output).is_err()
            {
                return Err(OutputError::InvalidObservation);
            }
            let result = self.transport.poll_signaling();
            match result {
                SignalingWrite::Admitted(progress) if !handoff.progress(progress) => {
                    return Err(OutputError::InvalidObservation);
                }
                SignalingWrite::Expired => return Err(OutputError::ProgressExpired),
                _ => {}
            }
            return Ok(result);
        }
        Ok(SignalingWrite::Idle)
    }

    pub(crate) fn native_ready(&mut self) {
        self.observe_native_terminal();
        if let Some(handoff) = &self.native_handoff {
            handoff.acknowledge_wakeup();
        }
    }

    fn native_channel_id(&self) -> Option<crate::media::ChannelId> {
        self.native_channel
    }

    fn disconnect(&mut self, reason: DisconnectReason) {
        if self.disconnect_reason.is_some() {
            return;
        }
        plog_info!(self.log_ctx(), %reason, "Connection disconnecting");
        self.disconnect_reason = Some(reason);
        self.media_ingress.clear();
        self.fenced_fabric_sources.clear();
        self.fabric_data.clear();
        self.stream_writer.discard();
        self.transport.disconnect();
    }
}

#[cfg(test)]
mod rtc_clock_tests {
    use super::*;

    fn execution_handoff(
        resources: &mut NegotiatedResources,
    ) -> (
        entity::ConnectionId,
        crate::control::native::handoff::ControllerHandoff,
        pulsebeam_queue::Receiver<entity::ConnectionId>,
    ) {
        let connection = entity::ConnectionId::new();
        let (controller, receiver) =
            crate::control::native::fixture::execution_handoff(resources, connection);
        (connection, controller, receiver)
    }

    #[test]
    fn sdp_resources_bootstrap_with_or_without_optional_control() {
        for profile in [SignalingProtocol::Whip, SignalingProtocol::State] {
            for control in [false, true] {
                let mut peer = str0m::RtcConfig::new().build(std::time::Instant::now());
                let mut changes = peer.sdp_api();
                changes.add_media(
                    str0m::media::MediaKind::Audio,
                    str0m::media::Direction::SendOnly,
                    None,
                    None,
                    None,
                );
                if profile == SignalingProtocol::State {
                    changes.add_media(
                        str0m::media::MediaKind::Audio,
                        str0m::media::Direction::RecvOnly,
                        None,
                        None,
                        None,
                    );
                }
                changes.add_channel("test".into());
                let offer = changes.apply().unwrap().0.to_sdp_string();
                let (mut rtc, _, mut resources) =
                    crate::control::Negotiator::new(vec![pulsebeam_rtc::LocalCandidate::Udp(
                        "127.0.0.1:6000".parse().unwrap(),
                    )])
                    .create_answer(
                        offer,
                        crate::control::ufrag::test_credentials(),
                        pulsebeam_runtime::net::ReceiveTime::now(),
                        control.then(|| {
                            crate::control::rtp::transport_capabilities(
                                &crate::control::rtp::capabilities(),
                            )
                            .unwrap()
                        }),
                    )
                    .unwrap();
                if profile == SignalingProtocol::Whip {
                    resources
                        .name_whip_tracks(&rtc.session, &Default::default())
                        .unwrap();
                }
                let media = resources.media_sections().to_vec();
                let (connection_id, _control) = if control {
                    rtc.connection
                        .update_rtp(pulsebeam_rtc::RtpConfigurationPatch::default())
                        .unwrap();
                    let (id, controller, ready) = execution_handoff(&mut resources);
                    (id, Some((controller, ready)))
                } else {
                    (entity::ConnectionId::new(), None)
                };
                let offered = rtc.session.offered_media.clone();
                let room_id = entity::RoomId::new("sdp-bootstrap").unwrap();
                let config = crate::control::core::ControllerCore::with_placement(
                    16,
                    crate::control::core::RoomPartitionPlacement::Balanced,
                )
                .prepare_participant(
                    rtc,
                    resources,
                    crate::control::controller::ParticipantState {
                        room_id: Some(room_id),
                        participant_id: crate::entity::fixture_participant_id(),
                        connection_id,
                        old_connection_id: None,
                        authorization: None,
                        selection: crate::control::sdp::Selection::Native,
                        profile,
                        rtp_capabilities: None,
                    },
                    entity::ProjectRoomKey::fixture(room_id).into(),
                );
                assert_eq!(config.rtc.session.offered_media, offered);
                assert_eq!(
                    config.resources.media_sections().len(),
                    if profile == SignalingProtocol::State {
                        0
                    } else {
                        media.len()
                    }
                );
                let mut connection = Connection::new(config, ShardId::new(0), 8);
                assert!(connection.disconnect_reason.is_none());
                for section in media {
                    if profile == SignalingProtocol::State {
                        assert!(connection.upstream.slot_for_mid(section.mid).is_none());
                        assert!(
                            connection
                                .receiver_index(section.kind, section.mid)
                                .is_none()
                        );
                        continue;
                    }
                    match section.direction {
                        Direction::RecvOnly => {
                            assert!(connection.sdp_senders.contains_key(&section.mid));
                            assert!(connection.upstream.slot_for_mid(section.mid).is_none());
                            assert!(connection.activate_sdp_sender(section.mid));
                            let admitted = connection.upstream.slot_for_mid(section.mid).unwrap();
                            assert_eq!(admitted.1.kind(), TrackKind::Audio);
                            let (publication, _) =
                                connection.upstream.announce_state_mut(section.mid).unwrap();
                            assert_eq!(
                                publication.meta().track_id.unwrap().as_str(),
                                section.mid.as_str()
                            );
                            assert!(connection.activate_sdp_sender(section.mid));
                            assert_eq!(
                                connection.upstream.slot_for_mid(section.mid),
                                Some(admitted)
                            );
                        }
                        Direction::SendOnly => assert_eq!(
                            connection.receiver_index(section.kind, section.mid),
                            Some(section.media_index)
                        ),
                        _ => panic!("unidirectional SDP resource"),
                    }
                }
            }
        }
    }

    #[test]
    fn authenticated_route_uses_processed_input_across_budget_continuations() {
        pulsebeam_runtime::testing::run_local(
            pulsebeam_runtime::testing::test_host_ip("192.168.251.10"),
            async {
                let now = Instant::now();
                let remote = "127.0.0.1:5000".parse().unwrap();
                let local = "127.0.0.1:6000".parse().unwrap();
                let (rtc, mut resources, valid) =
                    crate::control::application_only_test_connection_and_binding();
                let (connection_id, _controller, _ready) = execution_handoff(&mut resources);
                let received_at = net::ReceiveTime {
                    monotonic: now,
                    wall: std::time::SystemTime::UNIX_EPOCH,
                };
                assert_eq!(valid.get(4..8), Some([0x21, 0x12, 0xa4, 0x42].as_slice()));
                let mut invalid = valid.clone();
                invalid.get_mut(20..).unwrap().fill(0);
                let mut participant = Connection::new(
                    ConnectionConfig {
                        scope: crate::entity::ProjectRoomKey::fixture(
                            entity::RoomId::new("route-proof").unwrap(),
                        )
                        .into(),
                        participant_id: crate::entity::fixture_participant_id(),
                        connection_id,
                        profile: SignalingProtocol::State,
                        authorization_expiry: None,
                        rtc,
                        resources,
                    },
                    ShardId::new(0),
                    8,
                );
                let input = |payload: &[u8], source_shard| ParticipantInput::Network {
                    batch: net::RecvPacketBatch {
                        src: remote,
                        dst: local,
                        buf: payload.to_vec(),
                        stride: payload.len(),
                        len: payload.len(),
                        offset: 0,
                        transport: net::Transport::Udp(net::UdpMode::Scalar),
                        received_at,
                    },
                    source_shard,
                };
                for _ in 1..super::super::transport::MAX_PENDING_INGRESS {
                    participant.input(input(&invalid, ShardId::new(1)));
                }
                participant.input(input(&valid, ShardId::new(2)));
                let mut sink = crate::participant::event::test_utils::MockParticipantSink::new();
                let mut next = participant.poll(now, &mut sink).unwrap();
                assert!(sink.connected_calls.is_empty());
                assert!(next <= now + pulsebeam_runtime::SHARD_TIMER_QUANTUM);
                for _ in 0..4 {
                    let deadline = participant.poll(next, &mut sink).unwrap();
                    if !sink.connected_calls.is_empty() {
                        break;
                    }
                    assert!(deadline > next);
                    next = deadline;
                }
                assert_eq!(sink.connected_calls, vec![(remote, local, ShardId::new(2))]);
                assert_eq!(sink.exit_count, 0);
            },
        );
    }

    #[test]
    fn native_readiness_overflow_emits_one_terminal_exit() {
        let (rtc, mut resources, _) =
            crate::control::application_only_test_connection_and_binding();
        let connection_id = entity::ConnectionId::new();
        let (sender, mut ready) = pulsebeam_queue::bounded(std::num::NonZeroUsize::MIN);
        let unrelated = entity::ConnectionId::new();
        sender.try_send(unrelated).unwrap();
        let (controller, endpoint) = crate::control::native::handoff::pair(connection_id, sender);
        resources.native_handoff = Some(endpoint);
        let mut participant = Connection::new(
            ConnectionConfig {
                scope: crate::entity::ProjectRoomKey::fixture(
                    entity::RoomId::new("native-overflow").unwrap(),
                )
                .into(),
                participant_id: crate::entity::fixture_participant_id(),
                connection_id,
                profile: SignalingProtocol::State,
                authorization_expiry: None,
                rtc,
                resources,
            },
            ShardId::new(0),
            8,
        );
        let mut sink = crate::participant::event::test_utils::MockParticipantSink::new();

        participant.record_native_resource_observation(
            crate::participant::policy::ResourceObservation::Ended { binding: 1 },
        );
        assert!(controller.failed());
        assert_eq!(ready.try_recv().unwrap(), unrelated);
        assert!(ready.try_recv().is_err());
        let now = Instant::now();
        assert_eq!(participant.poll(now, &mut sink), None);
        assert_eq!(sink.exit_count, 1);
        assert!(participant.transport.is_exited());
        assert_eq!(participant.poll(now, &mut sink), None);
        assert_eq!(sink.exit_count, 1);
    }

    #[test]
    fn grant_replacement_updates_without_in_band_request_and_fences_stale_completions() {
        let (rtc, mut resources, _) =
            crate::control::application_only_test_connection_and_binding();
        let (connection_id, _controller, _ready) = execution_handoff(&mut resources);
        let mut participant = Connection::new(
            ConnectionConfig {
                scope: crate::entity::ProjectRoomKey::fixture(
                    entity::RoomId::new("grant-proof").unwrap(),
                )
                .into(),
                participant_id: crate::entity::fixture_participant_id(),
                connection_id,
                profile: SignalingProtocol::State,
                authorization_expiry: Some(
                    pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds(200),
                ),
                rtc,
                resources,
            },
            ShardId::new(0),
            8,
        );
        assert!(!participant.renewal_pending);
        let old_scope = crate::participant::policy::ExecutionScope {
            binding: 1,
            sequence: 1,
            authorization: 1,
        };
        assert!(participant.native_execution.begin(old_scope));
        assert!(participant.native_execution_current(old_scope, 100));
        participant.input(ParticipantInput::Authorization {
            revision: 2,
            expiry: Some(150),
        });
        assert_eq!(participant.authorization_revision, 2);
        assert_eq!(
            participant.authorization_expiry.unwrap().unix_seconds(),
            150
        );
        participant.renewal_pending = true;
        for (revision, expiry) in [(1, Some(300)), (1, None), (2, Some(400))] {
            participant.input(ParticipantInput::Authorization { revision, expiry });
            assert_eq!(participant.authorization_revision, 2);
            assert_eq!(
                participant.authorization_expiry.unwrap().unix_seconds(),
                150
            );
            assert!(participant.renewal_pending);
        }
        participant.input(ParticipantInput::Authorization {
            revision: 3,
            expiry: Some(160),
        });
        assert_eq!(participant.authorization_revision, 3);
        assert_eq!(
            participant.authorization_expiry.unwrap().unix_seconds(),
            160
        );
        assert!(!participant.renewal_pending);
        assert!(!participant.native_execution_current(old_scope, 100));
        let current = crate::participant::policy::ExecutionScope {
            sequence: 2,
            authorization: 3,
            ..old_scope
        };
        assert!(participant.native_execution.begin(current));
        participant.native_execution.finish(old_scope);
        assert!(participant.native_execution_current(current, 159));
        assert!(!participant.native_execution_current(current, 160));
        assert!(!participant.native_callback_fenced(current.binding));
        let other = crate::participant::policy::ExecutionScope {
            binding: 2,
            sequence: 3,
            ..current
        };
        assert!(participant.native_execution.begin(other));
        assert!(participant.native_execution_current(current, 100));
        assert!(participant.native_execution_current(other, 100));
        let retired = crate::participant::policy::ExecutionScope {
            sequence: 4,
            authorization: 1,
            ..other
        };
        assert!(participant.native_execution.retire(retired));
        assert!(!participant.native_execution.retire(retired));
        assert!(!participant.native_execution_current(other, 100));
        assert!(participant.native_execution_current(current, 100));
        participant.native_execution.finish(current);
        assert!(!participant.native_execution_current(current, 100));
        assert!(!participant.native_callback_fenced(current.binding));
        assert!(!participant.native_execution.begin(old_scope));
        assert!(!participant.native_execution.begin(other));
        let unadmitted = crate::participant::policy::ExecutionScope {
            binding: 3,
            sequence: 5,
            ..current
        };
        let retired = crate::participant::policy::ExecutionScope {
            sequence: 6,
            ..unadmitted
        };
        assert!(participant.native_execution.retire(retired));
        assert!(!participant.native_execution.begin(unadmitted));
        let replacement = crate::participant::policy::ExecutionScope {
            sequence: 7,
            ..unadmitted
        };
        assert!(participant.native_execution.begin(replacement));
        assert!(!participant.native_execution.retire(retired));
        assert!(participant.native_execution_current(replacement, 100));
        participant.native_execution.fence(replacement.binding);
        participant.native_execution.finish(replacement);
        assert!(!participant.native_execution_current(replacement, 100));
        assert!(participant.native_callback_fenced(replacement.binding));
        let late = crate::participant::policy::ExecutionScope {
            sequence: 8,
            ..replacement
        };
        assert!(!participant.native_execution.begin(late));
        let unrelated = crate::participant::policy::ExecutionScope {
            binding: 4,
            sequence: 9,
            ..replacement
        };
        assert!(participant.native_execution.begin(unrelated));
        assert!(participant.native_execution_current(unrelated, 100));
        assert!(!participant.native_execution.retire(retired));
        let retry = crate::participant::policy::ExecutionScope {
            sequence: 10,
            ..replacement
        };
        assert!(!participant.native_execution.begin(retry));
        let retirement = crate::participant::policy::ExecutionScope {
            sequence: 11,
            ..replacement
        };
        assert!(participant.native_execution.retire(retirement));
        assert!(participant.native_execution_current(unrelated, 100));
        let observation = crate::participant::policy::ExecutionScope {
            sequence: 12,
            ..unrelated
        };
        assert!(participant.native_execution.advance(observation));
        participant.native_execution.finish(observation);
        assert!(participant.native_execution_current(unrelated, 100));
        participant.native_execution.fence(replacement.binding);
        let observation = crate::participant::policy::ExecutionScope {
            sequence: 13,
            ..replacement
        };
        assert!(!participant.native_execution.advance(observation));
        assert!(participant.native_execution_current(unrelated, 100));
        participant.disconnect(DisconnectReason::SignalingOutput);
        assert!(participant.native_callback_fenced(unrelated.binding));
    }

    #[test]
    fn sub_quantum_deadlines_are_bounded_by_the_sampled_wall_time() {
        let wall = Instant::now();
        let due = wall + Duration::from_micros(30);
        assert_eq!(inline_rtc_timeout(due, wall), Some(due));
        let delayed_wake = due + Duration::from_millis(50);
        assert_eq!(inline_rtc_timeout(due, delayed_wake), Some(delayed_wake));
        let distant = wall + pulsebeam_runtime::SHARD_TIMER_QUANTUM * 2;
        assert_eq!(inline_rtc_timeout(distant, wall), None);
        for tick in 1..=1_000 {
            let now = wall + pulsebeam_runtime::SHARD_TIMER_QUANTUM * tick;
            let deadline = now + Duration::from_micros(30);
            let rtc = inline_rtc_timeout(deadline, now).unwrap();
            assert!(rtc >= now);
            assert!(rtc <= now + pulsebeam_runtime::SHARD_TIMER_QUANTUM);
        }
    }
}

#[cfg(test)]
mod upstream_route_table_tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;
    use pulsebeam_runtime::rand::{RngCore, seeded_rng};

    fn publication_id(label: &str) -> PublicationKey {
        entity::ConnectionId::new().fixture_publication(entity::TrackKind::Video, label)
    }

    fn route(ssrc: u32) -> IncomingRtpRoute {
        IncomingRtpRoute {
            ssrc: Ssrc::from(ssrc),
            mid: Mid::from("0"),
            rid: None,
            encoding: SourceEncoding::Rid(None),
            upstream_slot: UpstreamSlotKey::Audio(0),
            publication_id: publication_id("t"),
            fanout: None,
        }
    }

    #[test]
    fn authorization_and_late_removal_fence_only_the_old_fabric_binding() {
        let now = Instant::now();
        let participant_id = crate::entity::fixture_participant_id();
        let connection_id = entity::ConnectionId::new();
        let mut connection = Connection::new(
            ConnectionConfig {
                scope: crate::entity::ProjectRoomKey::fixture(
                    entity::RoomId::new("late-rid").unwrap(),
                )
                .into(),
                participant_id,
                connection_id,
                profile: SignalingProtocol::Whip,
                authorization_expiry: None,
                rtc: crate::control::application_only_test_connection(),
                resources: NegotiatedResources::empty_for_test(),
            },
            ShardId::new(0),
            8,
        );
        let mid = Mid::from("video");
        let rid = crate::media::Rid::from("q");
        let (track, descriptor) = track::test_utils::make_video_track(
            participant_id,
            connection_id,
            mid,
            vec![crate::media::SimulcastLayer { rid }],
        );
        let publication_id = descriptor.id();
        assert!(
            connection
                .upstream
                .add_published_track(0, mid, TrackKind::Video, || (track, descriptor))
        );
        let mut handles = slotmap::SlotMap::<TrackHandle, ()>::with_key();
        let old = handles.insert(());
        let current = handles.insert(());
        connection.apply(
            ParticipantEffect::TrackPublished { publication_id },
            Some(old),
        );

        let shadow = || {
            use crate::rtp::fabric_ingress::{EndpointBinding, ExtensionForm};
            use pulsebeam_core::fabric::media_source::{Anchors, Limits};
            RtpIngressShadow::new(
                EndpointBinding {
                    mid: mid.as_str().into(),
                    rid: Some(rid.as_str().into()),
                    primary_ssrc: 77,
                    codec: pulsebeam_rtc::RtpReceiveCodec {
                        payload_type: 96,
                        name: "h264".into(),
                        clock_rate: 90_000,
                        channels: None,
                        profile: Some(pulsebeam_rtc::CodecProfile::H264 {
                            profile_level_id: None,
                            packetization_mode: 1,
                            level_asymmetry_allowed: false,
                        }),
                    },
                    repair: None,
                    receive_extensions: Box::new([]),
                    extension_form: ExtensionForm::OneByte,
                    allow_mixed_extensions: false,
                    qualified_extensions: Default::default(),
                    capture_clock_ssrc: None,
                    allow_csrc_stripping: false,
                },
                Anchors::default(),
                Limits {
                    positions: std::num::NonZeroU16::new(4).unwrap(),
                    max_age: Duration::from_millis(120),
                    max_idle: Duration::from_secs(2),
                },
            )
            .unwrap()
        };
        assert!(connection.media_ingress.is_empty());
        connection.authorization_expiry =
            Some(pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds(0));
        assert!(!connection.attach_fabric_media_shadow(publication_id, shadow()));
        connection.authorization_expiry = None;
        assert!(connection.attach_fabric_media_shadow(publication_id, shadow()));
        assert!(!connection.attach_fabric_media_shadow(publication_id, shadow()));
        connection.apply(
            ParticipantEffect::TrackPublished { publication_id },
            Some(current),
        );
        let _ = connection.poll_transport(
            now,
            &mut crate::participant::event::test_utils::MockParticipantSink::default(),
        );
        assert!(
            connection.media_ingress.is_empty(),
            "replacement fences the old shadow"
        );
        assert!(connection.attach_fabric_media_shadow(publication_id, shadow()));
        connection.apply(
            ParticipantEffect::TrackUnpublished { publication_id },
            Some(old),
        );
        assert!(
            !connection.media_ingress.is_empty(),
            "late removal cannot clear the successor"
        );
        assert_eq!(
            connection.upstream.track_fanout(publication_id),
            Some(current)
        );
        assert_eq!(connection.upstream.track_for_fanout(old), None);
        assert_eq!(
            connection.upstream.track_for_fanout(current),
            Some(publication_id)
        );
    }

    /// The table used to be direct-mapped on `ssrc % MAX_UPSTREAM_ENCODED_STREAMS`,
    /// so any two SSRCs congruent modulo that constant evicted each other and
    /// every packet from both took the expensive `direct_api()` miss path. A
    /// client picks its SSRCs at random, so this is the common case, not a
    /// corner: among six streams the collision probability is over 90%.
    #[test]
    fn ssrcs_congruent_modulo_the_capacity_do_not_evict_each_other() {
        let mut table = UpstreamRouteTable::default();
        let ssrcs: Vec<u32> = (0..MAX_UPSTREAM_ENCODED_STREAMS)
            .map(|k| {
                0x1000_0000
                    + u32::try_from(k).unwrap()
                        * u32::try_from(MAX_UPSTREAM_ENCODED_STREAMS).unwrap()
            })
            .collect();

        for ssrc in &ssrcs {
            table.insert(route(*ssrc));
        }

        for ssrc in &ssrcs {
            assert_eq!(
                table.get(Ssrc::from(*ssrc)).map(|route| route.ssrc),
                Some(Ssrc::from(*ssrc)),
                "every inserted route must remain retrievable"
            );
        }
    }

    /// The key array and the payload array must describe the same streams.
    ///
    /// They are separate allocations kept aligned by index, which is what makes
    /// the search read four bytes per stream instead of the whole route. A
    /// removal that reorders one and not the other silently starts routing a
    /// stream's packets to another stream's track — no panic, no metric, just
    /// media arriving on the wrong slot. `swap_remove` on both is what keeps
    /// them aligned, and reordering is exactly what it does.
    #[test]
    fn removal_keeps_the_key_and_payload_arrays_describing_the_same_streams() {
        let mut table = UpstreamRouteTable::default();
        let ssrcs: Vec<u32> = (0..MAX_UPSTREAM_ENCODED_STREAMS)
            .map(|k| 0x2000_0000 + u32::try_from(k).unwrap())
            .collect();
        for ssrc in &ssrcs {
            table.insert(route(*ssrc));
        }

        // Remove from the front, which is where swap_remove reorders most.
        for removed in 0..ssrcs.len() {
            table.remove(Ssrc::from(ssrcs[removed]));
            assert_eq!(
                table.ssrcs.len(),
                table.routes.len(),
                "the arrays must stay the same length"
            );
            for (index, key) in table.ssrcs.iter().enumerate() {
                assert_eq!(
                    table.routes[index].ssrc, *key,
                    "routes[{index}] must be the route for ssrcs[{index}]"
                );
            }
            for surviving in ssrcs.iter().skip(removed.saturating_add(1)) {
                assert_eq!(
                    table.get(Ssrc::from(*surviving)).map(|r| r.ssrc),
                    Some(Ssrc::from(*surviving)),
                    "removing one stream must not lose another"
                );
            }
        }
        assert!(table.ssrcs.is_empty() && table.routes.is_empty());
    }

    /// The property that outlives any particular table implementation: a route
    /// that fits is a route that resolves, whatever the SSRCs happen to be.
    #[test]
    fn every_route_that_fits_is_retrievable() {
        let mut rng = seeded_rng(0xB0A7);
        for _ in 0..256 {
            let mut table = UpstreamRouteTable::default();
            let mut ssrcs = Vec::with_capacity(MAX_UPSTREAM_ENCODED_STREAMS);
            while ssrcs.len() < MAX_UPSTREAM_ENCODED_STREAMS {
                let candidate = rng.next_u32();
                if !ssrcs.contains(&candidate) {
                    ssrcs.push(candidate);
                }
            }

            for ssrc in &ssrcs {
                table.insert(route(*ssrc));
            }
            for ssrc in &ssrcs {
                assert!(
                    table.get(Ssrc::from(*ssrc)).is_some(),
                    "ssrc {ssrc:#x} was evicted by a route that should have had its own slot"
                );
            }
        }
    }

    #[test]
    fn reinserting_an_ssrc_updates_in_place_rather_than_consuming_a_slot() {
        let mut table = UpstreamRouteTable::default();
        for k in 0..MAX_UPSTREAM_ENCODED_STREAMS {
            table.insert(route(1000 + u32::try_from(k).unwrap()));
        }
        let mut updated = route(1000);
        updated.upstream_slot = UpstreamSlotKey::Video(5);
        table.insert(updated);

        assert_eq!(
            table.get(Ssrc::from(1000u32)).map(|r| r.upstream_slot),
            Some(UpstreamSlotKey::Video(5))
        );
        for k in 1..MAX_UPSTREAM_ENCODED_STREAMS {
            assert!(
                table
                    .get(Ssrc::from(1000 + u32::try_from(k).unwrap()))
                    .is_some()
            );
        }
    }

    #[test]
    fn removing_one_route_leaves_the_others() {
        let mut table = UpstreamRouteTable::default();
        for k in 0..MAX_UPSTREAM_ENCODED_STREAMS {
            table.insert(route(2000 + u32::try_from(k).unwrap()));
        }
        table.remove(Ssrc::from(2003u32));

        assert!(table.get(Ssrc::from(2003u32)).is_none());
        for k in 0..MAX_UPSTREAM_ENCODED_STREAMS {
            if k != 3 {
                assert!(
                    table
                        .get(Ssrc::from(2000 + u32::try_from(k).unwrap()))
                        .is_some()
                );
            }
        }
    }
}

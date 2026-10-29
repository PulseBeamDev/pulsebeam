use super::signaling_v1::{AppliedInput, NativeSession};
use crate::media::{Direction, KeyframeRequestKind, MediaKind, Mid, Pt, Ssrc};
use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use pulsebeam_proto::topic::TopicFrame;
use pulsebeam_rtc::{DataChannelEvent, DataMessage, Event};
use pulsebeam_runtime::net::{self};
use std::time::Duration;
use tokio::time::Instant;

use crate::control::{NegotiatedResources, controller::ConnectionProfile};
use crate::entity::{self, TrackId, TrackKind};
use crate::id::ShardId;
use crate::keys::TrackHandle;
use crate::log::{LogCtx, plog_debug, plog_info, plog_trace, plog_warn};
use crate::participant::data::{DataChannelState, DataOpenError, DataState};
use crate::participant::downstream::SlotConfig;
use crate::participant::effect::ParticipantEffect;
use crate::participant::event::ParticipantSink;
use crate::participant::reverse::{ReverseInput, ReversePacket};

#[cfg(test)]
use crate::participant::upstream::{
    MAX_UPSTREAM_ENCODED_STREAMS, UpstreamRouteTable, UpstreamSlotKey,
};
use crate::participant::{TrackPacket, TrackPacketRef};
use crate::participant::{
    batcher::NetworkEgress,
    downstream::DownstreamAllocator,
    transport::{
        AppliedMutation, IngressResult, RtpWriteCommand, Transport, TransportMutation,
        TransportPollOutput,
    },
    upstream::{IncomingRtpRoute, UpstreamAllocator},
};
use crate::rtp::cache::TrackStreamCache;
use crate::track::{
    self, DataLane, DataTopicChannel, DataTrackDirection, DataTrackIntent, DataTrackIntentError,
    KEYFRAME_DEBOUNCE, StreamId, StreamWrite, StreamWriter, Track,
};

const SLOW_POLL_INTERVAL: Duration = Duration::from_millis(100);
const POLL_WORK_BUDGET: usize = 256;

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
    pub track_id: TrackId,
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
    #[error("Invalid data channel protocol: {0}")]
    InvalidDataTrackIntent(#[from] DataTrackIntentError),
    #[error("Duplicate data channel label for same direction: {0}")]
    DuplicateDataChannelLabel(DataTopicChannel),
    #[error("Exceeded maximum upstream tracks: only 2 video and 2 audio allowed")]
    TooManyUpstreamTracks,
    #[error(
        "Exceeded maximum data topic channels: only 64 channels (across all topics/scopes) allowed"
    )]
    TooManyDataTopicChannels,
    #[error("Room closed")]
    RoomClosed,
    #[error("System terminated")]
    SystemTerminated,
}

pub struct ParticipantConfig {
    pub manual_sub: bool,
    pub room_id: entity::RoomId,
    pub participant_id: entity::ParticipantId,
    pub participant_external_id: entity::ParticipantExternalId,
    pub connection_id: entity::ConnectionId,
    pub profile: ConnectionProfile,
    pub authorization_expiry: Option<pulsebeam_core::auth::AuthorizationExpiry>,
    pub rtc: pulsebeam_rtc::AcceptedConnection,
    pub resources: NegotiatedResources,
}

impl std::fmt::Debug for ParticipantConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParticipantConfig")
            .field("participant_id", &self.participant_id)
            .field("connection_id", &self.connection_id)
            .field("profile", &self.profile)
            .field("resources", &self.resources)
            .finish_non_exhaustive()
    }
}

pub(crate) enum ParticipantInput<'a> {
    Network {
        batch: net::RecvPacketBatch,
        source_shard: crate::id::ShardId,
    },
    Timeout(Instant),
    Authorization(Option<i64>),
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

impl ParticipantConfig {
    pub fn ufrag(&self) -> String {
        self.resources.local_ufrag.clone()
    }
}

pub struct Participant {
    // Hot: touched on every packet
    transport: Transport,
    downstream: DownstreamAllocator,
    stream_writer: StreamWriter,
    // Warm: touched per poll cycle
    upstream: UpstreamAllocator,
    negotiated: NegotiatedResources,
    profile: ConnectionProfile,
    pub(crate) participant_id: entity::ParticipantId,
    participant_external_id: entity::ParticipantExternalId,
    pub(crate) connection_id: entity::ConnectionId,
    last_keyframe_request: HashMap<(Mid, Option<crate::media::Rid>), Instant>,
    pending_keyframe_requests: HashSet<(Mid, Option<crate::media::Rid>)>,

    /// The compiled stream a published channel forwards into, recorded by the
    /// shard once it has minted one. Keyed by channel so an arriving SCTP
    /// frame reaches its fanout without hashing a room, a publisher or a
    /// topic — the identity it would otherwise have to reassemble on every
    /// packet.
    data: DataState,

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
    signaling: NativeSession,
    renewal_pending: bool,
    playout_reconnect_sent: bool,
    authorization_expiry: Option<pulsebeam_core::auth::AuthorizationExpiry>,
    active_sender_indices: HashSet<u32>,
    last_slow_poll: Instant,
    pub(crate) room_id: entity::RoomId,
    pub(crate) shard_id: ShardId,
}

pub type ParticipantCore = Participant;

impl Participant {
    #[cfg(feature = "sim")]
    pub(crate) fn video_observation(&self) -> crate::sim_observation::VideoAllocationObservation {
        self.downstream.video.observation()
    }

    #[cfg(feature = "sim")]
    pub(crate) fn audio_observation(&self) -> Vec<crate::sim_observation::AudioSlotObservation> {
        self.downstream
            .audio_assignments()
            .into_iter()
            .map(|heard| crate::sim_observation::AudioSlotObservation {
                track: heard.origin.track,
                origin: heard.origin.participant,
                mid: heard.mid.to_string(),
                level_dbov: heard.level_dbov,
            })
            .collect()
    }

    pub fn new(cfg: ParticipantConfig, shard_id: ShardId, udp_gso_size: usize) -> Self {
        let rtc = cfg.rtc;
        let ctx = LogCtx {
            room_id: cfg.room_id,
            participant_id: cfg.participant_id,
        };
        let signaling = NativeSession::default();
        let now = Instant::now();
        #[cfg(feature = "sim")]
        let sim_span = tracing::info_span!(
            "peer",
            participant_id = %cfg.participant_id,
            room_id = %cfg.room_id
        );
        let mut participant = Self {
            transport: Transport::new(
                rtc,
                udp_gso_size,
                now,
                #[cfg(feature = "sim")]
                sim_span,
            ),
            stream_writer: StreamWriter::new(),
            participant_id: cfg.participant_id,
            participant_external_id: cfg.participant_external_id,
            connection_id: cfg.connection_id,
            profile: cfg.profile,
            upstream: UpstreamAllocator::new(ctx, cfg.connection_id),
            negotiated: cfg.resources,
            downstream: DownstreamAllocator::new(ctx, cfg.manual_sub),
            disconnect_reason: None,
            signaling,
            renewal_pending: false,
            playout_reconnect_sent: false,
            authorization_expiry: cfg.authorization_expiry,
            active_sender_indices: HashSet::new(),
            last_slow_poll: now,
            last_keyframe_request: HashMap::new(),
            pending_keyframe_requests: HashSet::new(),
            data: DataState::new(),
            room_id: cfg.room_id,
            shard_id,
        };
        for media in participant.negotiated.media_sections().to_vec() {
            participant.handle_media_added(media);
        }
        participant
    }

    fn log_ctx(&self) -> LogCtx {
        LogCtx {
            room_id: self.room_id,
            participant_id: self.participant_id,
        }
    }

    #[allow(
        dead_code,
        reason = "the replacement signaling path consumes negotiated sender coordinates in Plan 07"
    )]
    pub(crate) fn track_for_sender_index(&self, sender_index: u32) -> Option<TrackId> {
        self.upstream.track_for_sender_index(sender_index)
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
            ParticipantEffect::ParticipantsChanged { added, removed } => {
                self.signaling.apply_participants(added, removed);
            }
            ParticipantEffect::TrackCandidateAdded { track } => {
                let Some(key) = track_handle else {
                    debug_assert!(false, "a track effect must resolve to a local handle");
                    return;
                };
                self.add_track_candidate(key, track);
            }
            ParticipantEffect::TrackCandidateRemoved { track_id } => {
                let Some(key) = track_handle else {
                    debug_assert!(false, "a track effect must resolve to a local handle");
                    return;
                };
                self.remove_track_candidate(key, track_id);
            }
            ParticipantEffect::TrackSubscribed { track_id } => {
                let Some(key) = track_handle else {
                    debug_assert!(false, "a track effect must resolve to a local handle");
                    return;
                };
                self.activate_track_binding(key, track_id);
            }
            ParticipantEffect::TrackUnsubscribed { track_id } => {
                let Some(key) = track_handle else {
                    debug_assert!(false, "a track effect must resolve to a local handle");
                    return;
                };
                self.deactivate_track_binding(key, track_id);
            }
            ParticipantEffect::TrackPublished { track_id } => {
                let Some(key) = track_handle else {
                    debug_assert!(false, "a track effect must resolve to a local handle");
                    return;
                };
                self.upstream.bind_track_handle(track_id, key);
                if track_id.kind() == TrackKind::Data {
                    self.upstream.data.bind_source(track_id, key);
                }
            }
            ParticipantEffect::TrackUnpublished { track_id } => {
                let Some(key) = track_handle else {
                    debug_assert!(false, "a track effect must resolve to a local handle");
                    return;
                };
                self.upstream.unbind_track_handle(track_id, key);
                if track_id.kind() == TrackKind::Data {
                    self.upstream.data.unpublish(track_id);
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
            ParticipantInput::Authorization(expiry) => {
                if self.renewal_pending {
                    self.renewal_pending = false;
                    if let Some(expiry) = expiry {
                        self.authorization_expiry = u64::try_from(expiry)
                            .ok()
                            .map(pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds);
                    }
                    self.send_authorization_result(expiry);
                }
            }
            ParticipantInput::Track { key, packet, cache } => {
                self.on_track_packet(key, packet, cache);
            }
            ParticipantInput::Reverse { stream, packet } => self.on_reverse(stream, packet),
        }
    }

    fn add_track_candidate(&mut self, key: TrackHandle, track: Track) {
        let track_id = track.id();
        let participant_id = track.meta().origin;
        debug_assert_eq!(track_id.kind(), track.kind());
        if !self
            .downstream
            .add_track_candidate(key, &track, &self.data.channels_snapshot())
        {
            return;
        }
        debug_assert_ne!(participant_id, self.participant_id);
        if track.kind() != TrackKind::Data {
            self.on_track_published(key, track);
        } else {
            self.signaling.mark_dirty();
        }
    }

    fn activate_track_binding(&mut self, key: TrackHandle, track_id: TrackId) {
        let Some(candidate) = self.downstream.track_candidate(key) else {
            debug_assert!(false, "binding activation requires a candidate");
            return;
        };
        debug_assert_eq!(candidate.track_id, track_id);
        if track_id.kind() != TrackKind::Data {
            self.downstream.activate_track_binding(key, track_id);
        }
    }

    fn deactivate_track_binding(&mut self, key: TrackHandle, track_id: TrackId) {
        let Some(candidate) = self.downstream.track_candidate(key) else {
            debug_assert!(false, "binding deactivation requires a candidate");
            return;
        };
        debug_assert_eq!(candidate.track_id, track_id);
        if track_id.kind() != TrackKind::Data {
            self.downstream.deactivate_track_binding(key, track_id);
        }
    }

    fn remove_track_candidate(&mut self, key: TrackHandle, track_id: TrackId) {
        let Some(candidate) = self.downstream.remove_track_candidate(key) else {
            return;
        };
        debug_assert_eq!(candidate.track_id, track_id);
        debug_assert_ne!(candidate.participant_id, self.participant_id);
        if track_id.kind() != TrackKind::Data {
            let _ = self.on_tracks_unpublished(std::slice::from_ref(&track_id));
        } else {
            self.signaling.mark_dirty();
        }
    }

    fn on_track_packet(
        &mut self,
        key: TrackHandle,
        packet: TrackPacketRef<'_>,
        cache: Option<&TrackStreamCache>,
    ) {
        let TrackPacketRef::Rtp(packet) = packet else {
            let (stream, lane, bytes) = match packet {
                TrackPacketRef::Data { lane, bytes } => (key, lane, bytes),
                TrackPacketRef::Rtp(_) => {
                    debug_assert!(false, "the RTP path must be handled before data dispatch");
                    return;
                }
            };
            let Some(channel) = self.downstream.data.forwarding(stream) else {
                debug_assert!(false, "a data forwarding plan must have a receiver binding");
                return;
            };
            let required = if matches!(lane, DataLane::Ordered) {
                let Ok(frame) = pulsebeam_proto::topic::TopicFrame::decode_bounded(bytes) else {
                    return;
                };
                !matches!(
                    frame.body,
                    Some(pulsebeam_proto::topic::topic_frame::Body::Message(_))
                )
            } else {
                false
            };
            self.downstream.data.record_delivery(stream, bytes.len());
            self.enqueue_fanout(TransportMutation::Data {
                channel,
                bytes: bytes.to_vec(),
                required,
            });
            return;
        };
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
        let kind = entry.track_id.kind();
        let participant_id = entry.participant_id;
        let track_id = entry.track_id;
        match kind {
            TrackKind::Video => {
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
                if promoted {
                    self.signaling.mark_mapping_dirty();
                }
            }
            TrackKind::Audio => {
                debug_assert!(cache.is_none(), "audio forwarding has no video cache");
                let origin = crate::entity::AudioOrigin {
                    participant: participant_id,
                    track: track_id,
                };
                self.downstream
                    .on_forward_audio_rtp(origin, packet, &mut self.stream_writer);
                if self.downstream.take_audio_speakers_changed() {
                    self.signaling.mark_mapping_dirty();
                }
            }
            TrackKind::Data => debug_assert!(false, "data tracks carry bytes, not RTP"),
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

    pub fn drain_network(&mut self, egress: &mut impl NetworkEgress) {
        self.transport.drain_network(egress);
    }

    fn on_timeout(&mut self, now: Instant) {
        self.transport.enqueue_timeout(now);
    }

    fn on_track_published(&mut self, key: TrackHandle, track: Track) {
        if track.meta().origin == self.participant_id {
            debug_assert!(false, "the controller must not install a loopback track");
            return;
        }
        plog_info!(
            self.log_ctx(),
            track = %track.id(),
            origin = %track.meta().origin,
            "participant received published track"
        );
        self.downstream.install_track(key, track);
        self.signaling.mark_dirty();
    }

    fn on_tracks_unpublished(&mut self, tracks: &[TrackId]) -> bool {
        let mut removed = false;
        for track_id in tracks {
            removed |= self.downstream.remove_track(track_id);
        }
        if removed {
            self.signaling.mark_dirty();
        }
        removed
    }

    fn enqueue_fanout(&mut self, work: TransportMutation) -> bool {
        let required = matches!(&work, TransportMutation::Data { required: true, .. });
        let accepted = self.transport.enqueue_mutation(work);
        if required && !accepted {
            self.disconnect(DisconnectReason::SignalingOutput);
        }
        accepted
    }

    fn on_reverse(&mut self, stream: TrackHandle, packet: ReversePacket) {
        match packet.decode() {
            Some(ReverseInput::Keyframe { rid, kind }) => {
                let Some(track_id) = self.upstream.track_for_fanout(stream) else {
                    debug_assert!(
                        false,
                        "a reverse keyframe route must name a published track"
                    );
                    return;
                };
                self.enqueue_remote_keyframe((track_id, rid), kind);
            }
            Some(ReverseInput::ReliableControl(bytes)) => {
                let Some(channel) = self.upstream.data.source(stream) else {
                    debug_assert!(
                        false,
                        "a reliable control stream must have a publisher channel"
                    );
                    return;
                };
                let Ok(frame) = TopicFrame::decode_bounded(&bytes) else {
                    return;
                };
                let Some(publication) = self.data.publication(channel) else {
                    return;
                };
                if !frame.is_subscriber_control()
                    || frame.data_track_id() != Some(publication.id.as_str().as_str())
                {
                    return;
                }
                self.enqueue_fanout(TransportMutation::Data {
                    channel,
                    bytes,
                    required: true,
                });
            }
            None => {
                debug_assert!(false, "reverse route carried an invalid endpoint envelope");
            }
        }
    }

    fn enqueue_remote_keyframe(&mut self, stream_id: StreamId, kind: KeyframeRequestKind) {
        let Some(mid) = self.upstream.mid_for_track_id(stream_id.0) else {
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
        if self.enqueue_fanout(TransportMutation::Keyframe {
            mid,
            rid: stream_id.1,
            kind,
        }) {
            debug_assert!(self.pending_keyframe_requests.insert(key));
        }
    }

    fn poll_slow(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        // Measure before allocating: the monitors produce this tick's numbers,
        // and running the allocator first would decide against last tick's.
        self.upstream.poll_slow(now);
        let assignments_changed = self.downstream.poll_slow(now, events);
        self.transport
            .set_desired_bitrate(self.downstream.desired_bitrate());
        if assignments_changed {
            self.signaling.mark_mapping_dirty();
        }
    }

    fn apply_one_rtc_mutation(&mut self, now: Instant) -> bool {
        if let Some(write) = self.stream_writer.pop() {
            let (pkt, track_id, mid, rid, ssrc, pt, kind) = match write {
                StreamWrite::Video {
                    pkt,
                    track_id,
                    mid,
                    rid,
                    ssrc,
                    pt,
                } => (pkt, track_id, mid, rid, ssrc, pt, MediaKind::Video),
                StreamWrite::Audio {
                    pkt,
                    track_id,
                    mid,
                    ssrc,
                    pt,
                } => (pkt, track_id, mid, None, ssrc, pt, MediaKind::Audio),
            };
            let seq_no = pkt.seq_no;
            let Ok(playout_delay) = self.downstream.playout_delay_to_stamp(track_id, mid) else {
                self.send_playout_reconnect();
                return true;
            };
            let result = self.transport.apply_rtp_command(RtpWriteCommand {
                pkt,
                track_id,
                mid,
                rid,
                ssrc,
                pt,
                kind,
                now,
                playout_delay,
            });
            match result {
                AppliedMutation::ControlNotWritten => {
                    self.disconnect(DisconnectReason::SignalingOutput);
                }
                AppliedMutation::RtpWritten if playout_delay.is_some() => {
                    self.downstream.record_playout_delay_stamp(mid, rid, seq_no);
                }
                AppliedMutation::KeyframeUnavailable { mid, rid } => {
                    debug_assert!(self.pending_keyframe_requests.remove(&(mid, rid)));
                }
                AppliedMutation::KeyframeRequested { .. }
                | AppliedMutation::Applied
                | AppliedMutation::DataNotWritten
                | AppliedMutation::RtpWritten
                | AppliedMutation::RtpNotWritten => {}
            }
            return true;
        }
        let applied = self.transport.apply_next_mutation(now);
        match applied {
            Some(AppliedMutation::ControlNotWritten) => {
                self.disconnect(DisconnectReason::SignalingOutput);
            }
            Some(AppliedMutation::KeyframeRequested { mid, rid }) => {
                debug_assert!(self.pending_keyframe_requests.remove(&(mid, rid)));
                self.last_keyframe_request.insert((mid, rid), now);
            }
            Some(AppliedMutation::KeyframeUnavailable { mid, rid }) => {
                debug_assert!(self.pending_keyframe_requests.remove(&(mid, rid)));
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
        let mut work_budget = POLL_WORK_BUDGET;
        'drain: loop {
            if work_budget == 0 {
                metrics::counter!("participant_poll_budget_hit").increment(1);
                let next = now.checked_add(Duration::from_micros(1)).unwrap_or(now);
                return Some(next);
            }
            work_budget = work_budget.saturating_sub(1);
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

            if self.signaling.needs_poll() {
                let mut publications = self.downstream.native_publications();
                publications.extend(self.data.publications().cloned());
                let assignments = self.downstream.native_assignments();
                match self.signaling.poll(
                    &publications,
                    assignments,
                    self.participant_id,
                    self.participant_external_id.as_str(),
                ) {
                    Ok(Some(output)) => {
                        if self
                            .transport
                            .write_channel(output.cid, true, &output.bytes)
                        {
                            self.signaling.commit_sent();
                        } else {
                            self.signaling.retry_pending();
                        }
                        self.transport.mark_needs_drain();
                        continue;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        plog_warn!(self.log_ctx(), ?err, "native signaling output failed");
                        self.disconnect(DisconnectReason::SignalingOutput);
                        return None;
                    }
                }
            }

            if self.downstream.dirty_allocation {
                let assignments_changed = self.downstream.update_allocations(now);
                self.transport
                    .set_desired_bitrate(self.downstream.desired_bitrate());
                if assignments_changed {
                    self.signaling.mark_mapping_dirty();
                }
                self.downstream.reconcile_routes(now, events);
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

            if let Some(rtc_now) = inline_rtc_timeout(deadline, now) {
                self.advance_rtc_clock(rtc_now, now);
                self.transport.timeout();
                continue;
            }

            return Some(deadline);
        }
    }

    fn advance_rtc_clock(&mut self, candidate: Instant, wall_now: Instant) {
        self.transport.advance_clock(candidate, wall_now);
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
            match self.transport.poll_output() {
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
                self.handle_incoming_rtp(packet, mid, rid, events);
            }
            Event::SenderReport(report) => self.handle_sender_report(report),
            Event::EncodingDiscovered(info) => {
                let Ok(mid) = Mid::new(&info.mid) else {
                    return;
                };
                if let Some(report) = self.transport.register_encoding(info) {
                    self.handle_sender_report(report);
                }
                self.handle_stream_paused(mid, false, events);
            }
            Event::EncodingRetired { ssrc, .. } => {
                self.transport.retire_encoding(ssrc);
                self.upstream.clear_routes();
            }
            Event::KeyframeRequested { ssrc } => {
                if let Some(req) = self.transport.keyframe_request(ssrc)
                    && let Some((fanout, layer)) = self.downstream.handle_keyframe_request(now, req)
                {
                    events.request_reverse(
                        fanout,
                        ReversePacket::keyframe(layer.rid, KeyframeRequestKind::Pli),
                    );
                }
            }
            Event::BandwidthEstimate(available) => self
                .downstream
                .update_bitrate(now, crate::bitrate::Bitrate::from(available.as_bps())),
            Event::ReceiverReport(report) => {
                if let Some((mid, sequence)) = self.transport.receiver_report(report) {
                    self.downstream.handle_egress_stats(mid, None, sequence);
                }
            }
            Event::DataChannel(DataChannelEvent::Opened {
                channel: cid,
                config: cfg,
            }) => {
                let intent = match DataTrackIntent::try_from(&cfg) {
                    Ok(intent) => intent,
                    Err(err) => {
                        self.disconnect(err.into());
                        return;
                    }
                };

                match intent {
                    DataTrackIntent::InternalSignaling => {
                        if self.profile == ConnectionProfile::Native {
                            plog_info!(self.log_ctx(), "internal media signaling is opened");
                            self.signaling.set_cid(cid);
                            if let Some(expiry) = self.authorization_expiry
                                && let Ok(expiry) = i64::try_from(expiry.unix_seconds())
                            {
                                self.send_authorization_result(Some(expiry));
                            }
                        }
                    }

                    DataTrackIntent::UserTopic(channel) => {
                        if let Some(previous) = self.data.close(cid) {
                            self.downstream.data.close(cid);
                            self.upstream.data.close(cid);
                            self.release_data_channel(previous, events);
                        }
                        let publication = if channel.direction == DataTrackDirection::Publish {
                            let label =
                                crate::track::publication_label(channel.lane, &channel.topic);
                            let Some(id) =
                                self.data.allocate_publication(self.connection_id, &label)
                            else {
                                self.disconnect(DisconnectReason::SignalingOutput);
                                return;
                            };
                            Some(Track::data(
                                crate::track::TrackMeta {
                                    room_id: self.room_id,
                                    shard_id: self.shard_id,
                                    id,
                                    origin: self.participant_id,
                                    connection_id: self.connection_id,
                                    label: Some(label),
                                },
                                channel.topic.clone(),
                                channel.lane,
                                None,
                            ))
                        } else {
                            None
                        };
                        let meta = publication.as_ref().map(|track| track.meta().clone());
                        if let Err(error) = self.data.open(cid, channel.clone(), meta) {
                            self.disconnect(match error {
                                DataOpenError::DuplicateDataChannelLabel(channel) => {
                                    DisconnectReason::DuplicateDataChannelLabel(channel)
                                }
                                DataOpenError::TooManyDataTopicChannels => {
                                    DisconnectReason::TooManyDataTopicChannels
                                }
                            });
                            return;
                        }
                        if let Some(track) = publication {
                            self.upstream.data.publish(cid, &track);
                            events.publish_track(track);
                            self.signaling.mark_dirty();
                        } else {
                            events.subscribe_tracks(
                                DataState::selector(&channel),
                                crate::track::SelectionPolicy::All,
                            );
                        }
                    }
                }
            }
            Event::DataChannel(DataChannelEvent::Closed { channel: cid }) => {
                if Some(cid) == self.signaling.cid {
                    self.disconnect(DisconnectReason::SignalingClosed);
                    return;
                }
                let Some(channel) = self.data.close(cid) else {
                    return;
                };
                self.downstream.data.close(cid);
                self.upstream.data.close(cid);
                self.release_data_channel(channel, events);
                self.signaling.mark_dirty();
            }
            Event::DataChannel(DataChannelEvent::Message {
                channel: cid,
                message,
            }) => {
                let DataMessage::Binary(bytes) = message else {
                    if Some(cid) == self.signaling.cid {
                        self.disconnect(DisconnectReason::SignalingClosed);
                    }
                    return;
                };
                if Some(cid) == self.signaling.cid {
                    self.handle_native_signaling(&bytes, events);
                    return;
                }
                let Some(channel) = self.data.channel(cid).cloned() else {
                    return;
                };
                match (channel.lane, channel.direction) {
                    (lane, DataTrackDirection::Publish) => {
                        let Ok(frame) = TopicFrame::decode_bounded(bytes.as_ref()) else {
                            return;
                        };
                        if !self.data.accepts_publisher_frame(cid, &frame) {
                            return;
                        }
                        events.publish_track_packet(
                            self.upstream.data.published_stream(cid),
                            TrackPacket::Data {
                                lane,
                                bytes: bytes.to_vec(),
                            },
                        );
                    }
                    (DataLane::Ordered, DataTrackDirection::Subscribe) => {
                        let Ok(control) = TopicFrame::decode_bounded(bytes.as_ref()) else {
                            return;
                        };
                        if !control.is_subscriber_control() {
                            return;
                        }
                        let Some(track_id) = control
                            .data_track_id()
                            .and_then(|id| id.parse::<TrackId>().ok())
                        else {
                            return;
                        };
                        if let Some(stream) =
                            self.downstream
                                .data
                                .subscribed_stream(cid, &track_id, &|key| {
                                    self.downstream
                                        .track_candidate(key)
                                        .map(|entry| entry.track_id)
                                })
                        {
                            events.request_reverse(
                                stream,
                                ReversePacket::reliable_control(bytes.to_vec()),
                            );
                        }
                    }
                    (DataLane::Latest | DataLane::Lossy, DataTrackDirection::Subscribe) => {}
                }
            }
            _ => {
                // tracing::warn!("unhandled event: {e:?}");
            }
        }
    }

    fn handle_native_signaling(&mut self, bytes: &[u8], events: &mut impl ParticipantSink) {
        let senders = self
            .negotiated
            .media_sections()
            .iter()
            .filter_map(|media| {
                if media.direction != Direction::RecvOnly {
                    return None;
                }
                let kind = match media.kind {
                    MediaKind::Audio => TrackKind::Audio,
                    MediaKind::Video => TrackKind::Video,
                };
                Some((media.media_index, kind))
            })
            .collect();
        let result = self.signaling.apply_input(
            bytes,
            &senders,
            self.downstream.video.slot_count(),
            &mut self.upstream,
        );
        match result {
            Ok(AppliedInput::Intent { published, receive }) => {
                let next: HashSet<u32> = published.iter().map(|track| track.sender_index).collect();
                let removed: Vec<u32> = self
                    .active_sender_indices
                    .difference(&next)
                    .copied()
                    .collect();
                let added: Vec<u32> = next
                    .difference(&self.active_sender_indices)
                    .copied()
                    .collect();
                for (indices, active) in [(removed, false), (added, true)] {
                    for index in indices {
                        if let Some(mid) = self
                            .negotiated
                            .media_sections()
                            .iter()
                            .find(|media| media.media_index == index)
                            .map(|media| media.mid)
                        {
                            self.handle_upstream_track_state(mid, active, events);
                        }
                    }
                }
                self.active_sender_indices = next;
                if self.downstream.apply_native_receive(receive.as_ref()) {
                    self.send_playout_reconnect();
                }
            }
            Ok(AppliedInput::Replay) => {}
            Ok(AppliedInput::RenewAuthorization(token)) => {
                if self.renewal_pending {
                    self.send_authorization_result(None);
                } else {
                    self.renewal_pending = true;
                    events.renew_authorization(token);
                }
            }
            Err(error) => {
                if let Ok(bytes) = pulsebeam_proto::codec::encode_server(&error.response())
                    && let Some(cid) = self.signaling.cid
                {
                    self.transport.write_channel(cid, true, &bytes);
                    self.transport.mark_needs_drain();
                }
                plog_warn!(self.log_ctx(), ?error, "native signaling input rejected");
                self.disconnect(DisconnectReason::SignalingInput);
            }
        }
    }

    fn send_playout_reconnect(&mut self) {
        if self.playout_reconnect_sent {
            return;
        }
        self.playout_reconnect_sent = true;
        use pulsebeam_proto::signaling_v1::{Reconnect, ServerMessage, server_message::Payload};
        if let Some(cid) = self.signaling.cid
            && let Ok(bytes) = pulsebeam_proto::codec::encode_server(&ServerMessage {
                payload: Some(Payload::Reconnect(Reconnect {})),
            })
        {
            self.transport.write_channel(cid, true, &bytes);
            self.transport.mark_needs_drain();
        }
    }

    fn send_authorization_result(&mut self, expiry: Option<i64>) {
        use pulsebeam_proto::signaling_v1::{
            Authorization, Error, ErrorCode, ServerMessage, server_message::Payload,
        };
        let payload = match expiry {
            Some(expires_at_unix_seconds) => Payload::Authorization(Authorization {
                expires_at_unix_seconds,
            }),
            None => Payload::Error(Error {
                code: ErrorCode::AuthorizationRejected as i32,
                message: "authorization renewal rejected or busy".to_string(),
                fatal: false,
                intent_revision: None,
            }),
        };
        if let Some(cid) = self.signaling.cid
            && let Ok(bytes) = pulsebeam_proto::codec::encode_server(&ServerMessage {
                payload: Some(payload),
            })
        {
            self.transport.write_channel(cid, true, &bytes);
            self.transport.mark_needs_drain();
        }
    }

    fn release_data_channel(&mut self, state: DataChannelState, events: &mut impl ParticipantSink) {
        if let Some(meta) = state.publication {
            events.unpublish_track(meta);
        } else {
            events.unsubscribe_tracks(DataState::selector(&state.channel));
        }
    }

    fn handle_upstream_track_state(
        &mut self,
        mid: Mid,
        active: bool,
        events: &mut impl ParticipantSink,
    ) {
        if active {
            let Some((descriptor, in_topology)) = self.upstream.announce_state_mut(mid, true)
            else {
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

        let Some((descriptor, in_topology)) = self.upstream.announce_state_mut(mid, false) else {
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
        // WHIP has no native Intent: unpaused is its implicit publish signal.
        // Native tracks wait for a labeled Intent before becoming visible.
        // Paused never unpublishes; explicit state owns stop transitions.
        if !paused && self.profile == ConnectionProfile::Whip {
            self.handle_upstream_track_state(mid, true, events);
        }
    }

    fn handle_media_added(&mut self, media: crate::control::NegotiatedMedia) {
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
                let kind = match media.kind {
                    MediaKind::Audio => TrackKind::Audio,
                    MediaKind::Video => TrackKind::Video,
                };
                let label = whip_media_label(self.profile, media.kind);
                let track_id = self
                    .connection_id
                    .derive_track_id(kind, label.unwrap_or(&media.mid));
                let track_meta = track::TrackMeta {
                    room_id: self.room_id,
                    shard_id: self.shard_id,
                    id: track_id,
                    origin: self.participant_id,
                    connection_id: self.connection_id,
                    label: label.map(str::to_owned),
                };
                match media.kind {
                    MediaKind::Audio => {
                        let (tx, track) = track::new_audio(media.mid, track_meta);
                        if !self.upstream.add_published_track(
                            resource.media_index,
                            media.mid,
                            tx,
                            track,
                        ) {
                            self.disconnect(DisconnectReason::TooManyUpstreamTracks);
                        }
                    }
                    MediaKind::Video => {
                        let (tx, track) = track::new_video(
                            media.mid,
                            track_meta,
                            self.transport.incoming_layers(media.mid),
                        );
                        if !self.upstream.add_published_track(
                            resource.media_index,
                            media.mid,
                            tx,
                            track,
                        ) {
                            self.disconnect(DisconnectReason::TooManyUpstreamTracks);
                        }
                    }
                }
            }
            Direction::SendOnly => {
                self.try_add_downstream_slot(resource.media_index, media.mid, media.kind);
                self.signaling.mark_mapping_dirty();
            }
            _ => self.disconnect(DisconnectReason::InvalidMediaDirection),
        }
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
            plog_warn!(ctx, %mid, ?kind, "no negotiated PT available for downstream slot");
            return;
        };

        let Some(ssrc) = self.transport.stream_tx_ssrc(mid) else {
            plog_warn!(ctx, %mid, ?kind, "missing stream_tx_by_mid while adding downstream slot");
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

    fn handle_incoming_rtp(
        &mut self,
        rtp: pulsebeam_rtc::RtpPacket,
        mid: Mid,
        rid: Option<crate::media::Rid>,
        events: &mut impl ParticipantSink,
    ) {
        plog_trace!(self.log_ctx(), "tracing:rtp_event={}", rtp.header.sequence);
        let ssrc: Ssrc = rtp.header.ssrc.into();
        let incoming = if let Some(route) = self.upstream.route_for_ssrc(ssrc) {
            if route.mid != mid || route.rid != rid {
                return;
            }
            self.transport
                .convert_rtp(rtp, route.mid, route.rid)
                .map(|incoming| (route, incoming))
        } else {
            // Once per stream in a healthy participant. A rate proportional to
            // the packet rate means the table is evicting live routes.
            metrics::counter!("upstream_route_miss").increment(1);
            #[cfg(feature = "sim")]
            crate::sim_metrics::record_routing_counter("upstream_route_miss");
            self.transport
                .convert_rtp(rtp, mid, rid)
                .and_then(|incoming| {
                    let (upstream_slot, track_id) = self.upstream.slot_for_mid(incoming.mid)?;
                    let route = IncomingRtpRoute {
                        ssrc,
                        mid: incoming.mid,
                        rid: incoming.rid,
                        upstream_slot,
                        track_id,
                        fanout: self.upstream.track_fanout(track_id),
                    };
                    self.upstream.cache_route(route);
                    Some((route, incoming))
                })
        };
        let Some((route, incoming)) = incoming else {
            return;
        };
        self.handle_incoming_rtp_after_lookup(route, incoming, events);
    }

    fn handle_incoming_rtp_after_lookup(
        &mut self,
        route: IncomingRtpRoute,
        incoming: crate::participant::transport::RtpIngress,
        events: &mut impl ParticipantSink,
    ) {
        let processed = self.upstream.handle_incoming_rtp(
            route.upstream_slot,
            route.mid,
            route.rid.as_ref(),
            incoming.packet,
            None,
        );
        if !processed.valid_route {
            self.upstream.remove_route(route.ssrc);
            return;
        }
        if processed.request_keyframe {
            self.enqueue_remote_keyframe((route.track_id, route.rid), KeyframeRequestKind::Pli);
        }
        if let Some(packet) = processed.first {
            events.publish_track_packet(route.fanout, TrackPacket::Rtp(packet));
        }
        for packet in processed.remaining {
            events.publish_track_packet(route.fanout, TrackPacket::Rtp(packet));
        }
    }

    fn handle_sender_report(&mut self, report: pulsebeam_rtc::SenderReport) {
        let arrival = report.arrival.into();
        if let Some((mid, report)) = self.transport.sender_report(report) {
            self.upstream.sender_report(mid, report, arrival);
        }
    }

    fn disconnect(&mut self, reason: DisconnectReason) {
        if self.disconnect_reason.is_some() {
            return;
        }
        plog_info!(self.log_ctx(), %reason, "Participant core disconnecting");
        self.disconnect_reason = Some(reason);
        self.stream_writer.discard();
        self.transport.disconnect();
    }
}

fn whip_media_label(profile: ConnectionProfile, kind: MediaKind) -> Option<&'static str> {
    match (profile, kind) {
        (ConnectionProfile::Whip, MediaKind::Audio) => Some("audio"),
        (ConnectionProfile::Whip, MediaKind::Video) => Some("video"),
        _ => None,
    }
}

#[cfg(test)]
mod rtc_clock_tests {
    use super::*;

    #[test]
    fn whip_uses_fixed_track_labels_independent_of_mid() {
        let connection = entity::ConnectionId::new();
        for (kind, track_kind, expected) in [
            (MediaKind::Audio, TrackKind::Audio, "audio"),
            (MediaKind::Video, TrackKind::Video, "video"),
        ] {
            let label = whip_media_label(ConnectionProfile::Whip, kind).unwrap();
            assert_eq!(label, expected);
            assert_eq!(
                connection.derive_track_id(track_kind, label),
                connection.derive_track_id(track_kind, expected)
            );
            assert!(whip_media_label(ConnectionProfile::Native, kind).is_none());
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
                let (rtc, resources, valid) =
                    crate::control::application_only_test_connection_and_binding();
                let received_at = net::ReceiveTime {
                    monotonic: now,
                    wall: std::time::SystemTime::UNIX_EPOCH,
                };
                assert_eq!(valid.get(4..8), Some([0x21, 0x12, 0xa4, 0x42].as_slice()));
                let mut invalid = valid.clone();
                invalid.get_mut(20..).unwrap().fill(0);
                let mut participant = Participant::new(
                    ParticipantConfig {
                        manual_sub: false,
                        room_id: entity::RoomId::from_external(
                            &entity::RoomExternalId::new("route-proof").unwrap(),
                        ),
                        participant_id: entity::ParticipantId::new(),
                        participant_external_id: entity::ParticipantExternalId::new("peer")
                            .unwrap(),
                        connection_id: entity::ConnectionId::new(),
                        profile: ConnectionProfile::Native,
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

    fn track_id(label: &str) -> TrackId {
        entity::ConnectionId::new().derive_track_id(entity::TrackKind::Video, label)
    }

    fn route(ssrc: u32) -> IncomingRtpRoute {
        IncomingRtpRoute {
            ssrc: Ssrc::from(ssrc),
            mid: Mid::from("0"),
            rid: None,
            upstream_slot: UpstreamSlotKey::Audio(0),
            track_id: track_id("t"),
            fanout: None,
        }
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

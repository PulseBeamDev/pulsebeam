use super::signaling::Signaling;
use ahash::{HashMap, HashMapExt};
use pulsebeam_proto::prelude::Message;
use pulsebeam_proto::reliable::{RelControl, rel_control};
use pulsebeam_runtime::net::{self};
use std::time::Duration;
use str0m::bwe::BweKind;
use str0m::media::{KeyframeRequestKind, MediaKind, Mid};
use str0m::{
    Event, Rtc, RtcError,
    media::{Direction, MediaAdded, Pt},
};
use tokio::time::Instant;

use crate::entity::{self, TrackId, TrackKind};
use crate::id::ShardId;
use crate::keys::TrackKey;
use crate::log::{LogCtx, plog_debug, plog_info, plog_trace, plog_warn};
use crate::participant::data::{DataOpenError, DataState};
use crate::participant::downstream::SlotConfig;
use crate::participant::effect::ParticipantEffect;
use crate::participant::event::ParticipantSink;
use crate::participant::reverse::{ReverseInput, ReversePacket};
use crate::participant::signaling;
#[cfg(test)]
use crate::participant::upstream::{
    MAX_UPSTREAM_ENCODED_STREAMS, UpstreamRouteTable, UpstreamSlotKey,
};
use crate::participant::{TrackPacket, TrackPacketRef};
use crate::participant::{
    batcher::NetworkEgress,
    downstream::DownstreamAllocator,
    transport::{
        AppliedMutation, IngressResult, RTC_OUTPUT_BUDGET, RtpWriteCommand, Transport,
        TransportMutation, TransportPollOutput,
    },
    upstream::{IncomingRtpRoute, UpstreamAllocator},
};
use crate::rtp::cache::TrackStreamCache;
use crate::track::{
    self, DataLane, DataTopicChannel, DataTrackDirection, DataTrackIntent, DataTrackIntentError,
    KEYFRAME_DEBOUNCE, StreamId, StreamWrite, StreamWriter, Track,
};
#[cfg(test)]
use str0m::rtp::Ssrc;

const SLOW_POLL_INTERVAL: Duration = Duration::from_millis(100);
const POLL_WORK_BUDGET: usize = 256;

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

/// Routing is not allowed to mutate an `Rtc`; it only queues work for the
/// participant's mutate-then-drain loop.
/// One deferred str0m mutation. The poll loop applies one item and immediately
/// returns to `poll_rtc()` before applying another mutation.
#[derive(thiserror::Error, Debug)]
pub enum DisconnectReason {
    #[error("RTC engine error")]
    RtcError(#[from] RtcError),
    #[error("Signaling error")]
    SignalingError(#[from] signaling::SignalingError),
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

#[derive(Debug)]
pub struct ParticipantConfig {
    pub manual_sub: bool,
    pub room_id: entity::RoomId,
    pub participant_id: entity::ParticipantId,
    pub rtc: Rtc,
}

pub(crate) enum ParticipantInput<'a> {
    Network {
        batch: net::RecvPacketBatch,
        source_shard: crate::id::ShardId,
    },
    Timeout(Instant),
    Track {
        key: TrackKey,
        packet: TrackPacketRef<'a>,
        cache: Option<&'a TrackStreamCache>,
    },
    Reverse {
        stream: TrackKey,
        packet: ReversePacket,
    },
}

impl ParticipantConfig {
    // TODO: wrap rtc instead
    pub fn ufrag(&mut self) -> String {
        self.rtc.direct_api().local_ice_credentials().ufrag
    }
}

pub struct Participant {
    // Hot: touched on every packet
    transport: Transport,
    downstream: DownstreamAllocator,
    stream_writer: StreamWriter,
    // Warm: touched per poll cycle
    upstream: UpstreamAllocator,
    pub(crate) participant_id: entity::ParticipantId,
    last_keyframe_request: HashMap<(Mid, Option<str0m::media::Rid>), Instant>,

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
    signaling: Signaling,
    last_slow_poll: Instant,
    pub(crate) room_id: entity::RoomId,
    pub(crate) shard_id: ShardId,
}

pub type ParticipantCore = Participant;

impl Participant {
    pub fn new(
        cfg: ParticipantConfig,
        shard_id: ShardId,
        udp_gso_size: usize,
        tcp_gso_size: usize,
    ) -> Self {
        let rtc = cfg.rtc;
        let ctx = LogCtx {
            room_id: cfg.room_id,
            participant_id: cfg.participant_id,
        };
        let signaling = Signaling::new(ctx);
        let now = Instant::now();
        #[cfg(feature = "sim")]
        let sim_span = tracing::info_span!(
            "peer",
            participant_id = %cfg.participant_id,
            room_id = %cfg.room_id
        );
        Self {
            transport: Transport::new(
                rtc,
                udp_gso_size,
                tcp_gso_size,
                now,
                #[cfg(feature = "sim")]
                sim_span,
            ),
            stream_writer: StreamWriter::new(),
            participant_id: cfg.participant_id,
            upstream: UpstreamAllocator::new(ctx),
            downstream: DownstreamAllocator::new(ctx, cfg.manual_sub),
            disconnect_reason: None,
            signaling,
            last_slow_poll: now,
            last_keyframe_request: HashMap::new(),
            data: DataState::new(),
            room_id: cfg.room_id,
            shard_id,
        }
    }

    fn log_ctx(&self) -> LogCtx {
        LogCtx {
            room_id: self.room_id,
            participant_id: self.participant_id,
        }
    }

    pub fn apply(&mut self, effect: ParticipantEffect) {
        match effect {
            ParticipantEffect::ParticipantsChanged { added, removed } => {
                self.signaling.apply_participants(added, removed);
            }
            ParticipantEffect::TrackCandidateAdded { key, track } => {
                self.add_track_candidate(key, track);
            }
            ParticipantEffect::TrackCandidateRemoved { key, track_id } => {
                self.remove_track_candidate(key, track_id);
            }
            ParticipantEffect::TrackSubscribed { key, track_id } => {
                self.activate_track_binding(key, track_id);
            }
            ParticipantEffect::TrackUnsubscribed { key, track_id } => {
                self.deactivate_track_binding(key, track_id);
            }
            ParticipantEffect::TrackPublished { key, track_id } => {
                self.upstream.bind_track_key(track_id, key);
                if track_id.kind() == TrackKind::Data {
                    self.upstream.data.bind_source(track_id, key);
                }
            }
            ParticipantEffect::TrackUnpublished { key, track_id } => {
                self.upstream.unbind_track_key(track_id, key);
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
            ParticipantInput::Track { key, packet, cache } => {
                self.on_track_packet(key, packet, cache);
            }
            ParticipantInput::Reverse { stream, packet } => self.on_reverse(stream, packet),
        }
    }

    fn add_track_candidate(&mut self, key: TrackKey, track: Track) {
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
        }
    }

    fn activate_track_binding(&mut self, key: TrackKey, track_id: TrackId) {
        let Some(candidate) = self.downstream.track_candidate(key) else {
            debug_assert!(false, "binding activation requires a candidate");
            return;
        };
        debug_assert_eq!(candidate.track_id, track_id);
        if track_id.kind() != TrackKind::Data {
            self.downstream.activate_track_binding(key, track_id);
        }
    }

    fn deactivate_track_binding(&mut self, key: TrackKey, track_id: TrackId) {
        let Some(candidate) = self.downstream.track_candidate(key) else {
            debug_assert!(false, "binding deactivation requires a candidate");
            return;
        };
        debug_assert_eq!(candidate.track_id, track_id);
        if track_id.kind() != TrackKind::Data {
            self.downstream.deactivate_track_binding(key, track_id);
        }
    }

    fn remove_track_candidate(&mut self, key: TrackKey, track_id: TrackId) {
        let Some(candidate) = self.downstream.remove_track_candidate(key) else {
            return;
        };
        debug_assert_eq!(candidate.track_id, track_id);
        debug_assert_ne!(candidate.participant_id, self.participant_id);
        if track_id.kind() != TrackKind::Data {
            let _ = self.on_tracks_unpublished(std::slice::from_ref(&track_id));
        }
    }

    fn on_track_packet(
        &mut self,
        key: TrackKey,
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
            self.downstream.data.record_delivery(stream, bytes.len());
            let _ = lane;
            self.enqueue_fanout(TransportMutation::Data {
                channel,
                bytes: bytes.to_vec(),
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
                    self.signaling.mark_assignments_dirty();
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
                    self.signaling.mark_assignments_dirty();
                }
            }
            TrackKind::Data => debug_assert!(false, "data tracks carry bytes, not RTP"),
        }
    }

    fn on_ingress(&mut self, batch: net::RecvPacketBatch, source_shard: crate::id::ShardId) {
        self.transport.enqueue_ingress(batch, source_shard);
    }

    pub fn drain_network(&mut self, egress: &mut impl NetworkEgress) {
        self.transport.drain_network(egress);
    }

    fn on_timeout(&mut self, now: Instant) {
        self.transport.enqueue_timeout(now);
    }

    fn on_track_published(&mut self, key: TrackKey, track: Track) {
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
        self.signaling.mark_tracks_dirty();
        self.signaling.mark_assignments_dirty();
        let intents = self.signaling.reconcile();
        self.downstream.apply_signaling_intents(intents);
    }

    fn on_tracks_unpublished(&mut self, tracks: &[TrackId]) -> bool {
        let mut removed = false;
        for track_id in tracks {
            removed |= self.downstream.remove_track(track_id);
        }
        if removed {
            self.signaling.mark_tracks_dirty();
            self.signaling.mark_assignments_dirty();
            let intents = self.signaling.reconcile();
            self.downstream.apply_signaling_intents(intents);
        }
        removed
    }

    fn enqueue_fanout(&mut self, work: TransportMutation) {
        self.transport.enqueue_mutation(work);
    }

    fn on_reverse(&mut self, stream: TrackKey, packet: ReversePacket) {
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
                self.enqueue_fanout(TransportMutation::Data { channel, bytes });
            }
            None => {
                debug_assert!(false, "reverse route carried an invalid endpoint envelope");
            }
        }
    }

    fn enqueue_remote_keyframe(&mut self, stream_id: StreamId, kind: KeyframeRequestKind) {
        let Some(mid) = self.upstream.mid_for_track_id(stream_id.0) else {
            plog_warn!(self.log_ctx(), track = ?stream_id.0, "unknown upstream track for keyframe request");
            return;
        };
        let key = (mid, stream_id.1);
        let now = Instant::now();
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
        self.last_keyframe_request.insert(key, now);
        self.enqueue_fanout(TransportMutation::Keyframe {
            mid,
            rid: stream_id.1,
            kind,
        });
    }

    fn poll_slow(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        // Measure before allocating: the monitors produce this tick's numbers,
        // and running the allocator first would decide against last tick's.
        self.upstream.poll_slow(now);
        let assignments_changed = self
            .transport
            .with_bwe(|bwe| self.downstream.poll_slow(now, bwe, events));
        if assignments_changed {
            self.signaling.mark_assignments_dirty();
        }
    }

    fn apply_one_rtc_mutation(&mut self, now: Instant) -> bool {
        if let Some(write) = self.stream_writer.pop() {
            let (pkt, mid, rid, ssrc, pt, kind) = match write {
                StreamWrite::Video {
                    pkt,
                    mid,
                    rid,
                    ssrc,
                    pt,
                } => (pkt, mid, rid, ssrc, pt, MediaKind::Video),
                StreamWrite::Audio { pkt, mid, ssrc, pt } => {
                    (pkt, mid, None, ssrc, pt, MediaKind::Audio)
                }
            };
            let seq_no = pkt.seq_no;
            let playout_delay = self.downstream.playout_delay_to_stamp();
            let result = self.transport.apply_rtp_command(RtpWriteCommand {
                pkt,
                mid,
                rid,
                ssrc,
                pt,
                kind,
                now,
                playout_delay,
            });
            match result {
                AppliedMutation::RecoveredStream {
                    kind,
                    mid,
                    rid,
                    ssrc,
                } => {
                    let refreshed = self.downstream.refresh_ssrc(kind, mid, rid, ssrc);
                    debug_assert!(refreshed, "recovered stream has no downstream slot");
                    if playout_delay.is_some() {
                        self.downstream.record_playout_delay_stamp(mid, rid, seq_no);
                    }
                }
                AppliedMutation::RtpWritten if playout_delay.is_some() => {
                    self.downstream.record_playout_delay_stamp(mid, rid, seq_no);
                }
                AppliedMutation::Applied
                | AppliedMutation::RtpNotWritten
                | AppliedMutation::RtpWritten => {}
            }
            return true;
        }
        self.transport.apply_next_mutation(now).is_some()
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
                let Some(rtc_deadline) = self.poll_rtc(now, events) else {
                    self.transport.set_drain_result(None);
                    // Controller is responsible to clean up tracks
                    events.exit();
                    return None;
                };
                self.transport.set_drain_result(Some(rtc_deadline));
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

            let ctx = self.log_ctx();
            self.advance_rtc_clock(now, now);
            let receive_at = self.transport.clock();
            while self.transport.has_pending_ingress() {
                match self.transport.receive_pending(receive_at) {
                    IngressResult::Empty => continue,
                    IngressResult::Malformed(src) => {
                        plog_warn!(ctx, %src, "Dropping malformed UDP packet");
                        continue;
                    }
                    IngressResult::Received => continue 'drain,
                }
            }

            let mut snapshot = self.downstream.signaling_snapshot();
            snapshot.participants = self.signaling.participants_snapshot();
            if let Some(output) = self.signaling.poll(&snapshot) {
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

            if self.downstream.dirty_allocation {
                let assignments_changed = self
                    .transport
                    .with_bwe(|bwe| self.downstream.update_allocations(now, bwe));
                if assignments_changed {
                    self.signaling.mark_assignments_dirty();
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

    /// Internal helper: Drains the RTC engine until it yields a Timeout.
    /// Handles Transmits (UDP/TCP) and Events (Logic).
    fn poll_rtc(&mut self, now: Instant, events: &mut impl ParticipantSink) -> Option<Instant> {
        let mut outputs = 0usize;

        loop {
            if outputs >= RTC_OUTPUT_BUDGET {
                metrics::counter!("participant_rtc_output_budget_hit").increment(1);
                break Some(now);
            }
            match self.transport.poll_output() {
                Ok(Some(TransportPollOutput::Timeout(deadline))) => {
                    break Some(deadline);
                }
                Ok(Some(TransportPollOutput::Transmit)) => {
                    outputs = outputs.saturating_add(1);
                }
                Ok(Some(TransportPollOutput::Event(event))) => {
                    outputs = outputs.saturating_add(1);
                    self.handle_event(now, *event, events);
                }
                Ok(None) => break None,
                Err(error) => {
                    self.disconnect(error.into());
                    break None;
                }
            }
        }
    }

    fn handle_event(&mut self, now: Instant, e: Event, events: &mut impl ParticipantSink) {
        match e {
            // `Connected` is DTLS; ICE reaching connected is what tells the
            // shard the peer address is authenticated, and that is handled
            // below. Falls through to the catch-all with every other event this
            // participant does not act on.
            Event::IceConnectionStateChange(state) if state.is_connected() => {
                let (ingress, source_shard) = self.transport.connection_context();
                if let (Some((source, destination)), Some(source_shard)) = (ingress, source_shard) {
                    events.connected(source, destination, source_shard);
                }
            }
            Event::IceConnectionStateChange(state) if state.is_disconnected() => {
                self.disconnect(DisconnectReason::IceDisconnected);
            }
            Event::MediaAdded(media) => {
                self.upstream.clear_routes();
                self.handle_media_added(media, events);
            }
            Event::MediaChanged(_) => self.upstream.clear_routes(),
            Event::RtpPacket(rtp) => self.handle_incoming_rtp(rtp, events),
            Event::KeyframeRequest(req) => {
                if let Some(layer) = self.downstream.handle_keyframe_request(req) {
                    let stream_id = layer.stream_id();
                    if let Some(fanout) = self.upstream.track_fanout(stream_id.0) {
                        events.request_reverse(
                            fanout,
                            ReversePacket::keyframe(stream_id.1, KeyframeRequestKind::Pli),
                        );
                    }
                }
            }
            Event::EgressBitrateEstimate(BweKind::Twcc(available)) => {
                self.downstream.update_bitrate(now, available);
            }
            Event::MediaEgressStats(stats) => {
                if let Some(remote) = stats.remote {
                    self.downstream.handle_egress_stats(
                        stats.mid,
                        stats.rid,
                        remote.maximum_sequence_number,
                    );
                }
            }
            Event::ChannelOpen(cid, _label) => {
                let Some(cfg) = self.transport.channel_config(cid) else {
                    return;
                };

                let intent = match DataTrackIntent::try_from(&cfg) {
                    Ok(intent) => intent,
                    Err(err) => {
                        self.disconnect(err.into());
                        return;
                    }
                };

                match intent {
                    DataTrackIntent::InternalSignaling => {
                        plog_info!(self.log_ctx(), "internal media signaling is opened");
                        self.signaling.set_cid(cid);
                    }

                    DataTrackIntent::UserTopic(channel) => {
                        if let Some(previous) = self.data.close(cid) {
                            self.release_data_channel(previous, events);
                        }
                        if let Err(error) = self.data.open(cid, channel.clone()) {
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
                        match channel.direction {
                            DataTrackDirection::Publish => {
                                let label =
                                    crate::track::publication_label(channel.lane, &channel.topic);
                                let track = Track::data(
                                    crate::track::TrackMeta {
                                        room_id: self.room_id,
                                        shard_id: self.shard_id,
                                        id: self
                                            .participant_id
                                            .derive_track_id(TrackKind::Data, &label),
                                        origin: self.participant_id,
                                    },
                                    channel.topic,
                                    channel.lane,
                                    None,
                                );
                                self.upstream.data.publish(cid, &track);
                                events.publish_track(track);
                            }
                            DataTrackDirection::Subscribe => {
                                events.subscribe_tracks(
                                    DataState::selector(&channel),
                                    crate::track::SelectionPolicy::All,
                                );
                            }
                        }
                    }
                }
            }
            Event::ChannelClose(cid) => {
                let Some(channel) = self.data.close(cid) else {
                    return;
                };
                self.downstream.data.close(cid);
                self.upstream.data.close(cid);
                self.release_data_channel(channel, events);
            }
            Event::ChannelData(data) => {
                if Some(data.id) == self.signaling.cid
                    && let Err(err) = self.signaling.handle_input(&data.data).map(|input_events| {
                        for input_event in input_events {
                            self.handle_signaling_input(input_event, events);
                        }
                        let intents = self.signaling.reconcile();
                        self.downstream.apply_signaling_intents(intents);
                    })
                {
                    self.disconnect(err.into());
                    return;
                }

                let Some(channel) = self.data.channel(data.id).cloned() else {
                    return;
                };
                if !data.binary {
                    return;
                }
                match (channel.lane, channel.direction) {
                    (lane, DataTrackDirection::Publish) => {
                        events.publish_track_packet(
                            self.upstream.data.published_stream(data.id),
                            TrackPacket::Data {
                                lane,
                                bytes: data.data.to_vec(),
                            },
                        );
                    }
                    (DataLane::Reliable, DataTrackDirection::Subscribe) => {
                        let Ok(control) = RelControl::decode(data.data.as_ref()) else {
                            return;
                        };
                        if !matches!(control.msg, Some(rel_control::Msg::Nack(_))) {
                            return;
                        }
                        if let Some(stream) = self.downstream.data.subscribed_stream(data.id) {
                            events.request_reverse(
                                stream,
                                ReversePacket::reliable_control(data.data.to_vec()),
                            );
                        }
                    }
                    (DataLane::Realtime, DataTrackDirection::Subscribe) => {}
                }
            }
            Event::StreamPaused(stream) => {
                self.handle_stream_paused(stream.mid, stream.paused, events);
            }
            _ => {
                // tracing::warn!("unhandled event: {e:?}");
            }
        }
    }

    fn handle_signaling_input(
        &mut self,
        event: signaling::SignalingInputEvent,
        events: &mut impl ParticipantSink,
    ) {
        match event {
            signaling::SignalingInputEvent::UpstreamTrackState { mid, active } => {
                self.handle_upstream_track_state(mid, active, events);
            }
        }
    }

    fn release_data_channel(
        &mut self,
        channel: DataTopicChannel,
        events: &mut impl ParticipantSink,
    ) {
        match channel.direction {
            DataTrackDirection::Publish => {
                let label = crate::track::publication_label(channel.lane, &channel.topic);
                events
                    .unpublish_track(self.participant_id.derive_track_id(TrackKind::Data, &label));
            }
            DataTrackDirection::Subscribe => {
                events.unsubscribe_tracks(DataState::selector(&channel));
            }
        }
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
        let track_id = descriptor.id();
        *in_topology = false;
        events.unpublish_track(track_id);
    }

    fn handle_stream_paused(&mut self, mid: Mid, paused: bool, events: &mut impl ParticipantSink) {
        // Treat unpaused as an implicit publish signal from str0m.
        // We intentionally do not unpublish on paused=true here; explicit
        // client intent is authoritative for stop/unpublish transitions.
        if !paused {
            self.handle_upstream_track_state(mid, true, events);
        }
    }

    fn handle_media_added(&mut self, media: MediaAdded, _events: &mut impl ParticipantSink) {
        match media.direction {
            Direction::RecvOnly => {
                let track_id = self
                    .participant_id
                    .derive_track_id(media.kind.into(), &media.mid);
                let track_meta = track::TrackMeta {
                    room_id: self.room_id,
                    shard_id: self.shard_id,
                    id: track_id,
                    origin: self.participant_id,
                };
                match media.kind {
                    MediaKind::Audio => {
                        let (tx, track) = track::new_audio(media.mid, track_meta);
                        if !self.upstream.add_published_track(media.mid, tx, track) {
                            self.disconnect(DisconnectReason::TooManyUpstreamTracks);
                        }
                    }
                    MediaKind::Video => {
                        let (tx, track) = track::new_video(
                            media.mid,
                            track_meta,
                            media.simulcast.map(|s| s.recv).unwrap_or_default(),
                        );
                        if !self.upstream.add_published_track(media.mid, tx, track) {
                            self.disconnect(DisconnectReason::TooManyUpstreamTracks);
                        }
                    }
                }
            }
            Direction::SendOnly => {
                self.try_add_downstream_slot(media.mid, media.kind);
                // Update signaling slot count AFTER adding the slot so the
                // server accepts ClientIntent requests up to the actual slot
                // count (previously this was called before add_slot, so the
                // count was always one behind and every intent was rejected).
                self.signaling
                    .set_slot_count(self.downstream.video.slot_count());
                self.signaling
                    .set_audio_slot_count(self.downstream.audio_slot_count());
            }
            _ => self.disconnect(DisconnectReason::InvalidMediaDirection),
        }
    }

    fn preferred_send_pt(&self, mid: Mid, kind: MediaKind) -> Option<Pt> {
        self.transport.preferred_send_pt(mid, kind)
    }

    fn try_add_downstream_slot(&mut self, mid: Mid, kind: MediaKind) {
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
        rtp: str0m::rtp::RtpPacket,
        events: &mut impl ParticipantSink,
    ) {
        plog_trace!(self.log_ctx(), "tracing:rtp_event={}", rtp.seq_no);
        let ssrc = rtp.header.ssrc;
        let incoming = if let Some(route) = self.upstream.route_for_ssrc(ssrc) {
            self.transport
                .convert_rtp(rtp, route.mid, route.rid)
                .map(|incoming| (route, incoming))
        } else {
            // Once per stream in a healthy participant. A rate proportional to
            // the packet rate means the table is evicting live routes.
            metrics::counter!("upstream_route_miss").increment(1);
            #[cfg(feature = "sim")]
            crate::sim_metrics::record_routing_counter("upstream_route_miss");
            self.transport.lookup_rtp(rtp).and_then(|incoming| {
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
        let mut rtp = incoming.packet;
        if self.upstream.handle_incoming_rtp(
            route.upstream_slot,
            route.mid,
            route.rid.as_ref(),
            &mut rtp,
            incoming.sender_info,
        ) {
            events.publish_track_packet(route.fanout, TrackPacket::Rtp(rtp));
        } else {
            self.upstream.remove_route(route.ssrc);
        }
    }

    fn disconnect(&mut self, reason: DisconnectReason) {
        if self.disconnect_reason.is_some() {
            return;
        }
        plog_info!(self.log_ctx(), %reason, "Participant core disconnecting");
        self.disconnect_reason = Some(reason);
        self.transport.disconnect();
    }
}

#[cfg(test)]
mod rtc_clock_tests {
    use super::*;

    #[test]
    fn sub_quantum_deadlines_do_not_accumulate_clock_lag() {
        let start = Instant::now();
        let mut wall = start;
        let mut rtc = start;

        for _ in 0..1_000 {
            wall += pulsebeam_runtime::SHARD_TIMER_QUANTUM;
            let deadline = rtc + Duration::from_micros(30);
            let candidate = inline_rtc_timeout(deadline, wall).expect("deadline is inline");
            rtc = rtc.max(candidate).max(wall);
            assert!(rtc >= wall);
            assert!(rtc <= wall + pulsebeam_runtime::SHARD_TIMER_QUANTUM);
        }

        assert_eq!(rtc, wall);
    }
}

#[cfg(test)]
mod upstream_route_table_tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use pulsebeam_runtime::rand::{RngCore, seeded_rng};

    fn track_id(label: &str) -> TrackId {
        entity::ParticipantId::from_bytes([7u8; 16])
            .derive_track_id(entity::TrackKind::Video, label)
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

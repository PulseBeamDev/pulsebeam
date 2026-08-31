use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use pulsebeam_rtc::{
    DataChannel, DataPayload, DependencyRewrite, EgressSlot, ExtendedMediaSequence,
    ExtendedRtpTimestamp, IngressStream, MediaKind as RtcMediaKind, MediaRewrite, NegotiatedMedia,
    RtcConnectionState, RtcEvent, RtcPeer,
};
use pulsebeam_runtime::net::RecvPacketBatch;
use tokio::time::Instant;

use crate::{
    entity::{ParticipantId, RoomId, TrackKind},
    id::ShardId,
    keys::TrackKey,
    participant::{
        ForwardPacket, TrackPacket,
        batcher::{Batcher, NetworkEgress, OwnedPacketQueue},
        data::{ChannelId, DataOpenError, DataState},
        derive_packet,
        direct_transport::DirectTransport,
        downstream::{DownstreamAllocator, SlotConfig},
        event::ParticipantSink,
        reverse::ReversePacket,
        signaling::Signaling,
        upstream::{IncomingRtpRoute, UpstreamAllocator},
    },
    rtp::cache::TrackStreamCache,
    rtp::{
        Codec, CodecPayloadTypes, KeyframeRequest, KeyframeRequestKind, MediaKind, MediaSectionId,
        PayloadType, Ssrc,
    },
    track::{
        self, DataLane, DataTopicChannel, DataTrackDirection, DataTrackIntent,
        DataTrackIntentError, StreamWrite, StreamWriter, Track, TrackMeta,
    },
};

use super::{ParticipantEffect, ParticipantInput, TrackPacketRef};
use crate::clock::WallAnchor;

const SLOW_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(thiserror::Error, Debug)]
pub enum DisconnectReason {
    #[error("RTC engine error: {0}")]
    Rtc(#[from] pulsebeam_rtc::RtcPeerError),
    #[error("Signaling error: {0}")]
    Signaling(#[from] crate::participant::signaling::SignalingError),
    #[error("ICE connection disconnected")]
    IceDisconnected,
    #[error("Invalid data channel intent: {0}")]
    InvalidDataTrackIntent(#[from] DataTrackIntentError),
    #[error("Duplicate data channel label for same direction: {0}")]
    DuplicateDataChannelLabel(DataTopicChannel),
    #[error("Exceeded maximum data topic channels")]
    TooManyDataTopicChannels,
}

#[derive(Clone)]
struct IngressRouteFacts {
    ingress: IngressStream,
    mid: MediaSectionId,
    rid: Option<crate::rtp::EncodingId>,
    ssrc: Ssrc,
    descriptor: pulsebeam_rtc::EncodedStreamDescriptor,
}

pub struct DirectParticipantCore {
    transport: Box<DirectTransport>,
    ingress: HashMap<IngressStream, IngressRouteFacts>,
    egress: HashMap<MediaSectionId, EgressSlot>,
    egress_mids: HashMap<EgressSlot, MediaSectionId>,
    pending_media: BTreeMap<u64, ForwardPacket>,
    initial_keyframes_requested: HashSet<IngressStream>,
    pending_keyframe_requests: HashSet<IngressStream>,
    upstream: UpstreamAllocator,
    downstream: DownstreamAllocator,
    stream_writer: StreamWriter,
    pub(crate) participant_id: ParticipantId,
    pub(crate) room_id: RoomId,
    pub(crate) shard_id: ShardId,
    participant_key: crate::keys::ParticipantKey,
    activation_pending: bool,
    ingress_shard: Option<ShardId>,
    udp_packets: OwnedPacketQueue,
    tcp_batcher: Batcher,
    pending_data: VecDeque<(ChannelId, Vec<u8>)>,
    data_channels: HashMap<DataChannel, ChannelId>,
    rtc_channels: HashMap<ChannelId, DataChannel>,
    next_data_channel: u16,
    data: DataState,
    signaling: Signaling,
    last_slow_poll: Instant,
    disconnect_reason: Option<DisconnectReason>,
}

impl DirectParticipantCore {
    #[allow(
        clippy::too_many_arguments,
        reason = "participant materialization takes its complete owned shard configuration"
    )]
    pub fn new(
        peer: RtcPeer,
        media: Box<[NegotiatedMedia]>,
        participant_id: ParticipantId,
        room_id: RoomId,
        shard_id: ShardId,
        participant_key: crate::keys::ParticipantKey,
        manual_sub: bool,
        udp_gso_size: usize,
        tcp_gso_size: usize,
        now: Instant,
    ) -> Result<Self, pulsebeam_rtc::RtcPeerError> {
        debug_assert!(
            std::mem::size_of::<Self>() < 4096,
            "participant core must keep live protocol components on the heap"
        );
        let mut ingress = HashMap::with_capacity(media.len());
        let mut egress = HashMap::with_capacity(media.len());
        let mut egress_mids = HashMap::with_capacity(media.len());
        let mut upstream = UpstreamAllocator::new(crate::log::LogCtx {
            room_id,
            participant_id,
        });
        let mut downstream = DownstreamAllocator::new(
            crate::log::LogCtx {
                room_id,
                participant_id,
            },
            manual_sub,
        );
        downstream.update_allocation_input(
            now,
            crate::participant::allocation::AllocationInput {
                estimate: crate::participant::downstream::INITIAL_BANDWIDTH,
            },
        );
        let mut published = HashSet::with_capacity(media.len());
        let mut next_ssrc = 1u32;
        for section in &media {
            let mid = MediaSectionId::from(section.mid());
            if let Some(stream) = section.ingress() {
                let rid = section.rid().map(crate::rtp::EncodingId::from);
                let ssrc = Ssrc::from(next_ssrc);
                next_ssrc = next_ssrc.wrapping_add(1).max(1);
                let previous = ingress.insert(
                    stream,
                    IngressRouteFacts {
                        ingress: stream,
                        mid,
                        rid,
                        ssrc,
                        descriptor: section
                            .packet_descriptor()
                            .cloned()
                            .ok_or(pulsebeam_rtc::RtcPeerError::UnknownIngress)?,
                    },
                );
                debug_assert!(previous.is_none(), "negotiated ingress handles are unique");
                if !published.insert(mid) {
                    continue;
                }
                let kind = match section.kind() {
                    RtcMediaKind::Audio => TrackKind::Audio,
                    RtcMediaKind::Video => TrackKind::Video,
                    RtcMediaKind::Application => continue,
                };
                let meta = TrackMeta {
                    room_id,
                    shard_id,
                    id: participant_id.derive_track_id(kind, &mid),
                    origin: participant_id,
                };
                let (sender, track) = match kind {
                    TrackKind::Audio => track::new_audio(mid, meta),
                    TrackKind::Video => track::new_video(
                        mid,
                        meta,
                        media
                            .iter()
                            .filter(|candidate| candidate.mid() == section.mid())
                            .filter_map(NegotiatedMedia::rid)
                            .map(crate::rtp::SimulcastEncoding::new)
                            .collect(),
                    ),
                    TrackKind::Data => continue,
                };
                let _ = upstream.add_published_track(mid, sender, track);
            }
            if let Some(slot) = section.egress() {
                let mut payload_types = CodecPayloadTypes::default();
                for codec in section.codecs() {
                    let Some(codec_kind) = Codec::from_name(codec.name()) else {
                        continue;
                    };
                    let payload_type = PayloadType::DEFAULT;
                    payload_types.insert(codec_kind, payload_type);
                }
                if payload_types.is_empty() {
                    continue;
                }
                let kind = match section.kind() {
                    RtcMediaKind::Audio => MediaKind::Audio,
                    RtcMediaKind::Video => MediaKind::Video,
                    RtcMediaKind::Application => continue,
                };
                let ssrc = Ssrc::from(next_ssrc);
                next_ssrc = next_ssrc.wrapping_add(1).max(1);
                downstream.add_slot(SlotConfig {
                    mid,
                    rid: None,
                    ssrc,
                    payload_types,
                    kind,
                });
                let previous = egress.insert(mid, slot);
                debug_assert!(previous.is_none());
                let previous = egress_mids.insert(slot, mid);
                debug_assert!(previous.is_none());
            }
        }
        let mut core = Self {
            transport: Box::new(DirectTransport::new(peer)),
            ingress,
            egress,
            egress_mids,
            pending_media: BTreeMap::new(),
            initial_keyframes_requested: HashSet::with_capacity(8),
            pending_keyframe_requests: HashSet::with_capacity(8),
            upstream,
            downstream,
            stream_writer: StreamWriter::new(),
            participant_id,
            room_id,
            shard_id,
            participant_key,
            activation_pending: false,
            ingress_shard: None,
            udp_packets: OwnedPacketQueue::with_capacity(udp_gso_size),
            tcp_batcher: Batcher::with_capacity(tcp_gso_size),
            pending_data: VecDeque::with_capacity(64),
            data_channels: HashMap::with_capacity(16),
            rtc_channels: HashMap::with_capacity(16),
            next_data_channel: 0,
            data: DataState::new(),
            signaling: Signaling::new(crate::log::LogCtx {
                room_id,
                participant_id,
            }),
            last_slow_poll: now,
            disconnect_reason: None,
        };
        core.signaling
            .set_slot_count(core.downstream.video.slot_count());
        core.signaling
            .set_audio_slot_count(core.downstream.audio_slot_count());
        Ok(core)
    }

    pub fn transport_mut(&mut self) -> &mut DirectTransport {
        &mut self.transport
    }

    pub fn upstream_mut(&mut self) -> &mut UpstreamAllocator {
        &mut self.upstream
    }

    pub fn downstream_mut(&mut self) -> &mut DownstreamAllocator {
        &mut self.downstream
    }

    pub fn stream_writer_mut(&mut self) -> &mut StreamWriter {
        &mut self.stream_writer
    }

    pub const fn participant_id(&self) -> ParticipantId {
        self.participant_id
    }

    pub const fn room_id(&self) -> RoomId {
        self.room_id
    }

    pub const fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    pub(crate) fn report_departure(
        &mut self,
        receipt: pulsebeam_rtc::DepartureReceipt,
        sent: bool,
        now: Instant,
    ) {
        let result = if sent {
            let result = self.transport.confirm_departure(receipt, now);
            #[cfg(feature = "sim")]
            if let Ok(Some(latency)) = result {
                crate::sim_metrics::record_forwarding_latency(
                    latency.service(),
                    latency.pacing(),
                    latency.egress(),
                    latency.total(),
                );
            }
            result.map(|_| ())
        } else {
            self.transport.abandon_departure(receipt)
        };
        debug_assert!(result.is_ok(), "every RTC departure receipt completes once");
    }

    pub(crate) fn apply(&mut self, effect: ParticipantEffect) {
        match effect {
            ParticipantEffect::ParticipantsChanged { added, removed } => {
                self.signaling.apply_participants(added, removed);
            }
            ParticipantEffect::TrackCandidateAdded { key, track } => {
                if self
                    .downstream
                    .add_track_candidate(key, &track, &self.data.channels_snapshot())
                    && track.kind() != TrackKind::Data
                {
                    self.downstream.install_track(key, track);
                    self.signaling.mark_tracks_dirty();
                    self.signaling.mark_assignments_dirty();
                }
            }
            ParticipantEffect::TrackCandidateRemoved { key, track_id } => {
                if self.downstream.remove_track_candidate(key).is_some()
                    && track_id.kind() != TrackKind::Data
                {
                    let _ = self.downstream.remove_track(&track_id);
                    self.signaling.mark_tracks_dirty();
                    self.signaling.mark_assignments_dirty();
                }
            }
            ParticipantEffect::TrackSubscribed { key, track_id } => {
                self.downstream.activate_track_binding(key, track_id);
                self.activation_pending = true;
            }
            ParticipantEffect::TrackUnsubscribed { key, track_id } => {
                self.downstream.deactivate_track_binding(key, track_id);
                self.activation_pending = true;
            }
            ParticipantEffect::TrackPublished { key, track_id } => {
                self.upstream.bind_track_key(track_id, key);
                if track_id.kind() == TrackKind::Data {
                    self.upstream.data.bind_source(track_id, key);
                } else if track_id.kind() == TrackKind::Video {
                    let routes = self.upstream.routes_for_track(track_id);
                    for route in routes {
                        self.request_initial_keyframe(key, route);
                    }
                }
            }
            ParticipantEffect::TrackUnpublished { key, track_id } => {
                for route in self.upstream.routes_for_track(track_id) {
                    self.initial_keyframes_requested.remove(&route.ingress);
                    self.pending_keyframe_requests.remove(&route.ingress);
                }
                self.upstream.unbind_track_key(track_id, key);
                if track_id.kind() == TrackKind::Data {
                    self.upstream.data.unpublish(track_id);
                }
            }
        }
    }

    pub(crate) fn input(&mut self, input: ParticipantInput<'_>) {
        match input {
            ParticipantInput::Network {
                batch,
                source_shard,
            } => self.enqueue_ingress(batch, source_shard),
            ParticipantInput::Timeout(now) => {
                if let Err(error) = self.transport.handle_timeout(now) {
                    self.disconnect_reason = Some(error.into());
                }
            }
            ParticipantInput::Track {
                now,
                key,
                packet,
                cache,
            } => self.handle_track(now, key, packet, cache),
            ParticipantInput::Reverse { stream, packet } => self.handle_reverse(stream, packet),
        }
    }

    pub(crate) fn replay_cached_track(
        &mut self,
        now: Instant,
        key: TrackKey,
        cache: &TrackStreamCache,
    ) -> bool {
        let Some(entry) = self.downstream.track_candidate(key) else {
            return false;
        };
        if entry.participant_id == self.participant_id {
            debug_assert!(
                false,
                "a participant must never receive its own cached track"
            );
            return false;
        }
        if entry.track_id.kind() != TrackKind::Video {
            debug_assert!(false, "only video tracks replay cached access units");
            return false;
        }
        let origin = entry.participant_id;
        debug_assert!(
            self.stream_writer.is_empty(),
            "cached replay starts with an empty media writer"
        );
        let changed =
            self.downstream
                .on_forward_rtp(key, now, Some(cache), &mut self.stream_writer);
        let rendered = !self.stream_writer.is_empty();
        self.drain_media_writer(now, origin, None, Some(cache));
        if changed {
            self.signaling.mark_assignments_dirty();
        }
        rendered
    }

    fn handle_track(
        &mut self,
        now: Instant,
        key: TrackKey,
        packet: TrackPacketRef<'_>,
        cache: Option<&TrackStreamCache>,
    ) {
        match packet {
            TrackPacketRef::Rtp {
                packet,
                audio_level_extension,
                media,
                ..
            } => {
                let Some(entry) = self.downstream.track_candidate(key) else {
                    debug_assert!(false, "a TrackPacket must target an installed track");
                    return;
                };
                if entry.participant_id == self.participant_id {
                    debug_assert!(false, "a participant must never receive its own track");
                    return;
                }
                let origin = entry.participant_id;
                match entry.track_id.kind() {
                    TrackKind::Video => {
                        if self
                            .downstream
                            .on_forward_rtp(key, now, cache, &mut self.stream_writer)
                        {
                            self.signaling.mark_assignments_dirty();
                        }
                    }
                    TrackKind::Audio => {
                        let origin = crate::entity::AudioOrigin {
                            participant: entry.participant_id,
                            track: entry.track_id,
                        };
                        self.downstream.on_forward_audio_rtp(
                            origin,
                            packet,
                            media,
                            audio_level_extension,
                            &mut self.stream_writer,
                        );
                        if self.downstream.take_audio_speakers_changed() {
                            self.signaling.mark_assignments_dirty();
                        }
                    }
                    TrackKind::Data => debug_assert!(false, "data tracks do not carry RTP"),
                }
                self.drain_media_writer(now, origin, Some(media), cache);
            }
            TrackPacketRef::Data { lane: _, bytes } => {
                let Some(channel) = self.downstream.data.forwarding(key) else {
                    return;
                };
                self.downstream.data.record_delivery(key, bytes.len());
                if self.pending_data.len() < self.pending_data.capacity() {
                    self.pending_data.push_back((channel, bytes.to_vec()));
                }
            }
        }
    }

    fn handle_reverse(&mut self, stream: TrackKey, packet: ReversePacket) {
        match packet.decode() {
            Some(crate::participant::reverse::ReverseInput::Keyframe { rid, kind }) => {
                self.request_remote_keyframe(stream, rid.as_ref(), kind);
            }
            Some(crate::participant::reverse::ReverseInput::ReliableControl(bytes)) => {
                let Some(channel) = self.upstream.data.source(stream) else {
                    return;
                };
                if self.pending_data.len() < self.pending_data.capacity() {
                    self.pending_data.push_back((channel, bytes));
                }
            }
            None => debug_assert!(false, "reverse route carried an invalid endpoint envelope"),
        }
    }

    fn request_remote_keyframe(
        &mut self,
        stream: TrackKey,
        rid: Option<&crate::rtp::EncodingId>,
        _kind: KeyframeRequestKind,
    ) {
        let Some(track_id) = self.upstream.track_for_fanout(stream) else {
            return;
        };
        let Some(route) = self.upstream.route_for_track(track_id, rid) else {
            return;
        };
        self.pending_keyframe_requests.insert(route.ingress);
    }

    fn request_initial_keyframe(&mut self, stream: TrackKey, route: IncomingRtpRoute) {
        if !self.initial_keyframes_requested.insert(route.ingress) {
            return;
        }
        self.request_remote_keyframe(stream, route.rid.as_ref(), KeyframeRequestKind::Pli);
    }

    pub fn enqueue_ingress(&mut self, batch: RecvPacketBatch, source_shard: ShardId) {
        self.ingress_shard = Some(source_shard);
        self.transport.enqueue(batch);
    }

    fn drain_media_writer(
        &mut self,
        now: Instant,
        origin: ParticipantId,
        current: Option<&ForwardPacket>,
        cache: Option<&TrackStreamCache>,
    ) {
        #[cfg(not(feature = "sim"))]
        let _ = origin;
        while let Some(write) = self.stream_writer.pop() {
            let (packet, mid, ssrc) = match write {
                StreamWrite::Video { pkt, mid, ssrc, .. }
                | StreamWrite::Audio { pkt, mid, ssrc, .. } => (pkt, mid, ssrc),
            };
            #[cfg(not(feature = "sim"))]
            let _ = ssrc;
            let Some(slot) = self.egress.get(&mid).copied() else {
                debug_assert!(false, "a downstream write has a negotiated RTC slot");
                continue;
            };
            let source = current
                .filter(|media| media.packet().packet_id() == packet.packet_id)
                .or_else(|| cache.and_then(|cache| cache.forward_for(&packet)));
            let Some(source) = source else {
                debug_assert!(
                    false,
                    "every routed packet retains authenticated media storage: packet={:?} current={:?} cache={}",
                    packet.packet_id,
                    current.map(|media| media.packet().packet_id()),
                    cache.is_some()
                );
                continue;
            };
            let dependency = packet
                .derived
                .raw_dependency_descriptor
                .as_ref()
                .filter(|descriptor| !descriptor.0.is_empty())
                .map(|descriptor| DependencyRewrite::new(descriptor.0.as_slice().into()));
            let rewrite = MediaRewrite {
                sequence: ExtendedMediaSequence::new(u64::from(packet.seq_no)),
                timestamp: ExtendedRtpTimestamp::new(packet.rtp_ts.numer()),
                marker: packet.marker,
                dependency,
            };
            #[cfg(feature = "sim")]
            let expected = (
                rewrite.timestamp.get(),
                source.packet().timestamp().get(),
                rewrite.marker,
            );
            let result = match source {
                ForwardPacket::Local(media) => self.transport.forward(now, slot, media, rewrite),
                ForwardPacket::Transit(media) => {
                    self.transport.forward_transit(now, slot, media, rewrite)
                }
            };
            match result {
                Ok(()) => {
                    #[cfg(feature = "sim")]
                    {
                        crate::sim_metrics::record_forwarded_media_for(
                            self.participant_id,
                            u64::try_from(source.packet().payload().len()).unwrap_or(u64::MAX),
                        );
                        match source.packet().kind() {
                            RtcMediaKind::Video => {
                                let height = match source.packet().rid() {
                                    Some("h") => 360,
                                    Some("f") => 720,
                                    _ => 180,
                                };
                                crate::sim_metrics::record_expected_video(
                                    self.participant_id,
                                    origin,
                                    expected.0,
                                    expected.1,
                                    height,
                                    expected.2,
                                );
                            }
                            RtcMediaKind::Audio => crate::sim_metrics::record_expected_audio(
                                self.participant_id,
                                *ssrc,
                                origin,
                                expected.0,
                                expected.1,
                            ),
                            RtcMediaKind::Application => {}
                        }
                    }
                }
                Err(error) => {
                    tracing::debug!(?error, %mid, "RTC rejected a forwarding decision");
                }
            }
        }
    }

    pub(crate) fn process(
        &mut self,
        now: Instant,
        wall: &WallAnchor,
        events: &mut impl ParticipantSink,
    ) -> Result<usize, pulsebeam_rtc::RtcPeerError> {
        let processed = self.transport.process_ingress(now, wall)?;
        if self
            .transport
            .next_deadline()
            .is_some_and(|deadline| deadline <= now)
        {
            self.transport.handle_timeout(now)?;
        }
        while let Some(event) = self.transport.poll_event() {
            self.handle_rtc_event(now, wall, event, events);
        }
        Ok(processed)
    }

    pub(crate) fn poll(
        &mut self,
        now: Instant,
        wall: &WallAnchor,
        events: &mut impl ParticipantSink,
    ) -> Option<Instant> {
        if self.disconnect_reason.is_some() {
            return None;
        }
        if self.process(now, wall, events).is_err() {
            self.disconnect_reason = Some(DisconnectReason::IceDisconnected);
            events.exit();
            return None;
        }
        for stream in self.pending_keyframe_requests.drain() {
            let result = self.transport.request_keyframe(now, stream);
            debug_assert!(result.is_ok(), "cached ingress handles remain valid");
        }
        if self.activation_pending {
            let (assignments_changed, allocation) = self.downstream.poll_slow(now, events);
            self.apply_allocation(now, allocation);
            if assignments_changed {
                self.signaling.mark_assignments_dirty();
            }
            self.activation_pending = false;
        }
        if now.saturating_duration_since(self.last_slow_poll) >= SLOW_POLL_INTERVAL {
            self.upstream.poll_slow(now);
            let (assignments_changed, allocation) = self.downstream.poll_slow(now, events);
            self.apply_allocation(now, allocation);
            if assignments_changed {
                self.signaling.mark_assignments_dirty();
            }
            self.last_slow_poll = now;
        }
        if self.signaling.needs_poll() {
            let mut snapshot = self.downstream.signaling_snapshot();
            snapshot.participants = self.signaling.participants_snapshot();
            if let Some(output) = self.signaling.poll(&snapshot) {
                if self.send_data(now, output.cid, output.bytes).is_ok() {
                    self.signaling.commit_sent();
                } else {
                    self.signaling.retry_pending();
                }
            }
        }
        self.write_pending(now);
        if self.settle_transport(now, wall, events).is_err() {
            self.disconnect_reason = Some(DisconnectReason::IceDisconnected);
            events.exit();
            return None;
        }
        let slow = self
            .last_slow_poll
            .checked_add(SLOW_POLL_INTERVAL)
            .unwrap_or(self.last_slow_poll);
        let deadline = self
            .transport
            .next_deadline()
            .map_or(slow, |deadline| deadline.min(slow));
        debug_assert!(
            deadline > now,
            "participant deadline must advance: deadline={deadline:?}, now={now:?}"
        );
        Some(deadline)
    }

    fn settle_transport(
        &mut self,
        now: Instant,
        wall: &WallAnchor,
        events: &mut impl ParticipantSink,
    ) -> Result<(), pulsebeam_rtc::RtcPeerError> {
        const MAX_IMMEDIATE_PASSES: usize = 64;
        for _ in 0..MAX_IMMEDIATE_PASSES {
            self.drain_transport_egress(now);
            if self
                .transport
                .next_deadline()
                .is_none_or(|deadline| deadline > now)
            {
                return Ok(());
            }
            self.transport.handle_timeout(now)?;
            while let Some(event) = self.transport.poll_event() {
                self.handle_rtc_event(now, wall, event, events);
            }
        }
        debug_assert!(false, "RTC work must quiesce within one participant poll");
        Ok(())
    }

    fn handle_rtc_event(
        &mut self,
        now: Instant,
        wall: &WallAnchor,
        event: RtcEvent,
        events: &mut impl ParticipantSink,
    ) {
        match event {
            RtcEvent::Media(media) => self.handle_media(media, wall, events),
            RtcEvent::ConnectionStateChanged(state) => {
                self.handle_connection_state(state, events);
            }
            RtcEvent::KeyframeRequested(slot) => {
                self.handle_downstream_keyframe_request(slot, events);
            }
            RtcEvent::BweCapacity(capacity) => self.downstream.update_allocation_input(
                now,
                crate::participant::allocation::AllocationInput {
                    estimate: crate::participant::allocation::Bitrate::bps(capacity.bitrate_bps()),
                },
            ),
            RtcEvent::DataChannelOpened {
                channel,
                label,
                protocol,
                mode,
            } => self.open_data_channel(channel, label, protocol, mode, events),
            RtcEvent::DataMessage { channel, payload } => {
                let Some(id) = self.data_channels.get(&channel).copied() else {
                    return;
                };
                match payload {
                    DataPayload::Text(text) => {
                        self.handle_data_message(id, false, text.into_bytes(), events);
                    }
                    DataPayload::Binary(bytes) => {
                        self.handle_data_message(id, true, bytes, events);
                    }
                }
            }
            RtcEvent::DataChannelClosed(channel) => self.close_data_channel(channel, events),
            RtcEvent::DataChannelUnavailable => {
                self.disconnect_reason = Some(DisconnectReason::IceDisconnected);
                events.exit();
            }
            RtcEvent::DataChannelReady | RtcEvent::DataBackpressure(_) => {}
        }
    }

    fn handle_media(
        &mut self,
        media: pulsebeam_rtc::MediaPacket,
        wall: &WallAnchor,
        events: &mut impl ParticipantSink,
    ) {
        let Some(facts) = self.ingress.get(&media.stream()) else {
            debug_assert!(false, "RTC media carries a negotiated ingress handle");
            return;
        };
        let ingress = facts.ingress;
        let mid = facts.mid;
        let rid = facts.rid;
        let ssrc = facts.ssrc;
        let descriptor = &facts.descriptor;
        let audio_level_extension = descriptor.extension_ids().audio_level();
        let Ok(packet) = derive_packet(&media, descriptor, wall) else {
            return;
        };
        let packet_id = media.packet_id();
        let previous = self
            .pending_media
            .insert(packet_id, ForwardPacket::Local(media));
        debug_assert!(
            previous.is_none(),
            "an authenticated packet enters normalization once"
        );
        while self.pending_media.len() > crate::rtp::cache::PACKET_WINDOW_CAPACITY.saturating_mul(4)
        {
            let Some((&oldest, _)) = self.pending_media.first_key_value() else {
                break;
            };
            let _ = self.pending_media.remove(&oldest);
        }
        let Some((slot, track_id)) = self.upstream.slot_for_mid(mid) else {
            return;
        };
        let route = IncomingRtpRoute {
            ingress,
            ssrc,
            mid,
            rid,
            upstream_slot: slot,
            track_id,
            fanout: self.upstream.track_fanout(track_id),
        };
        self.upstream.cache_route(route);
        let processed = self.upstream.handle_incoming_rtp(
            route.upstream_slot,
            route.mid,
            route.rid.as_ref(),
            route.ssrc,
            packet,
            None,
        );
        if !processed.valid_route {
            self.upstream.remove_route(route.ssrc);
            return;
        }
        if let Some(fanout) = route.fanout {
            self.request_initial_keyframe(fanout, route);
        }
        if route.fanout.is_none()
            && let Some((track, in_topology)) = self.upstream.announce_state_mut(route.mid)
            && !*in_topology
        {
            *in_topology = true;
            events.publish_track(track.clone());
        }
        if processed.request_keyframe {
            self.request_upstream_keyframe(route, events);
        }
        for packet in processed.first.into_iter().chain(processed.remaining) {
            let id = packet.packet_id;
            let Some(media) = self.pending_media.remove(&id) else {
                debug_assert!(
                    false,
                    "normalized packets retain their authenticated storage"
                );
                continue;
            };
            events.publish_track_packet(
                route.fanout,
                TrackPacket::Rtp {
                    packet,
                    encoding: route.rid,
                    audio_level_extension,
                    media,
                },
            );
        }
    }

    fn handle_connection_state(
        &mut self,
        state: RtcConnectionState,
        events: &mut impl ParticipantSink,
    ) {
        match state {
            RtcConnectionState::Connected => {
                let Some(source_shard) = self.ingress_shard else {
                    return;
                };
                let Some((source, destination)) = self.transport.ingress_context() else {
                    return;
                };
                events.connected(source, destination, source_shard);
            }
            RtcConnectionState::Closed | RtcConnectionState::Failed => events.exit(),
            RtcConnectionState::Negotiated
            | RtcConnectionState::Connecting
            | RtcConnectionState::Draining => {}
        }
    }

    fn apply_allocation(
        &mut self,
        now: Instant,
        allocation: crate::participant::allocation::AllocationOutput,
    ) {
        let desired = self
            .transport
            .set_desired_bitrate(now, allocation.desired.get());
        let current = self
            .transport
            .set_current_bitrate(now, allocation.allocated.get());
        debug_assert!(
            matches!(desired, Ok(()) | Err(pulsebeam_rtc::RtcPeerError::Closed))
                && matches!(current, Ok(()) | Err(pulsebeam_rtc::RtcPeerError::Closed)),
            "allocation demand only fails after the peer closes"
        );
    }

    fn write_pending(&mut self, now: Instant) {
        while let Some((channel, bytes)) = self.pending_data.pop_front() {
            match self.send_data(now, channel, bytes) {
                Ok(()) => {}
                Err((channel, bytes)) => {
                    self.pending_data.push_front((channel, bytes));
                    break;
                }
            }
        }
    }

    fn send_data(
        &mut self,
        now: Instant,
        channel: ChannelId,
        bytes: Vec<u8>,
    ) -> Result<(), (ChannelId, Vec<u8>)> {
        let Some(rtc) = self.rtc_channels.get(&channel).copied() else {
            return Err((channel, bytes));
        };
        self.transport
            .send_data(rtc, DataPayload::Binary(bytes.clone()), now)
            .map_err(|_| (channel, bytes))
    }

    fn drain_transport_egress(&mut self, now: Instant) {
        const MAX_TRANSMITS_PER_POLL: usize = 4_096;
        for _ in 0..MAX_TRANSMITS_PER_POLL {
            let Some(transmit) = self.transport.poll_transmit(now) else {
                return;
            };
            let (protocol, _source, destination, bytes, rtc) = transmit.into_parts();
            let receipt = Some(crate::participant::batcher::DepartureReceipt {
                participant: self.participant_key,
                rtc,
                sent: false,
            });
            match protocol {
                pulsebeam_rtc::DatagramProtocol::Udp => {
                    self.udp_packets
                        .push_back_with_receipt(destination, bytes, receipt);
                }
                pulsebeam_rtc::DatagramProtocol::Tcp => {
                    self.tcp_batcher
                        .push_back_with_receipt(destination, &bytes, receipt);
                }
            }
        }
        debug_assert!(
            false,
            "one participant poll exceeded its transmit work budget"
        );
    }

    pub(crate) fn drain_network(&mut self, egress: &mut impl NetworkEgress) {
        loop {
            match egress.append_udp(&mut self.udp_packets) {
                crate::participant::batcher::AppendStatus::Drained => break,
                crate::participant::batcher::AppendStatus::Full if !egress.flush() => break,
                crate::participant::batcher::AppendStatus::Full => {}
            }
        }
        loop {
            match egress.append_tcp(&mut self.tcp_batcher) {
                crate::participant::batcher::AppendStatus::Drained => break,
                crate::participant::batcher::AppendStatus::Full if !egress.flush() => break,
                crate::participant::batcher::AppendStatus::Full => {}
            }
        }
    }

    fn open_data_channel(
        &mut self,
        rtc: DataChannel,
        label: String,
        _protocol: String,
        mode: pulsebeam_rtc::DataChannelMode,
        events: &mut impl ParticipantSink,
    ) {
        let channel = ChannelId::new(self.next_data_channel);
        self.next_data_channel = self.next_data_channel.wrapping_add(1);
        if let Some(previous) = self.data_channels.insert(rtc, channel) {
            let _ = self.rtc_channels.remove(&previous);
            debug_assert!(false, "an open RTC channel handle is unique");
        }
        let previous = self.rtc_channels.insert(channel, rtc);
        debug_assert!(previous.is_none());
        let intent = match DataTrackIntent::from_channel(&label, mode) {
            Ok(intent) => intent,
            Err(error) => {
                self.disconnect_reason = Some(error.into());
                events.exit();
                return;
            }
        };
        match intent {
            DataTrackIntent::InternalSignaling => {
                self.signaling.set_cid(channel);
            }
            DataTrackIntent::UserTopic(topic) => {
                if let Some(previous) = self.data.close(channel) {
                    self.release_data_channel(previous, events);
                }
                if let Err(error) = self.data.open(channel, topic.clone()) {
                    self.disconnect_reason = Some(match error {
                        DataOpenError::DuplicateDataChannelLabel(value) => {
                            DisconnectReason::DuplicateDataChannelLabel(value)
                        }
                        DataOpenError::TooManyDataTopicChannels => {
                            DisconnectReason::TooManyDataTopicChannels
                        }
                    });
                    events.exit();
                    return;
                }
                match topic.direction {
                    DataTrackDirection::Publish => {
                        let label = crate::track::publication_label(topic.lane, &topic.topic);
                        let track = Track::data(
                            TrackMeta {
                                room_id: self.room_id,
                                shard_id: self.shard_id,
                                id: self.participant_id.derive_track_id(TrackKind::Data, &label),
                                origin: self.participant_id,
                            },
                            topic.topic,
                            topic.lane,
                            None,
                        );
                        self.upstream.data.publish(channel, &track);
                        events.publish_track(track);
                    }
                    DataTrackDirection::Subscribe => events.subscribe_tracks(
                        DataState::selector(&topic),
                        crate::track::SelectionPolicy::All,
                    ),
                }
            }
        }
    }

    fn close_data_channel(&mut self, rtc: DataChannel, events: &mut impl ParticipantSink) {
        let Some(channel) = self.data_channels.remove(&rtc) else {
            return;
        };
        let _ = self.rtc_channels.remove(&channel);
        let Some(topic) = self.data.close(channel) else {
            return;
        };
        self.downstream.data.close(channel);
        self.upstream.data.close(channel);
        self.release_data_channel(topic, events);
    }

    fn handle_data_message(
        &mut self,
        channel: ChannelId,
        binary: bool,
        payload: Vec<u8>,
        events: &mut impl ParticipantSink,
    ) {
        if Some(channel) == self.signaling.cid {
            match self.signaling.handle_input(&payload) {
                Ok(input_events) => {
                    for event in input_events {
                        let crate::participant::signaling::SignalingInputEvent::UpstreamTrackState {
                            mid,
                            active,
                        } = event;
                        if active
                            && let Some((track, in_topology)) =
                                self.upstream.announce_state_mut(mid)
                            && !*in_topology
                        {
                            *in_topology = true;
                            events.publish_track(track.clone());
                        }
                    }
                    self.downstream
                        .apply_signaling_intents(self.signaling.reconcile());
                    self.activation_pending = true;
                }
                Err(error) => {
                    self.disconnect_reason = Some(error.into());
                    events.exit();
                }
            }
            return;
        }
        let Some(topic) = self.data.channel(channel).cloned() else {
            return;
        };
        if !binary {
            return;
        }
        match (topic.lane, topic.direction) {
            (lane, DataTrackDirection::Publish) => events.publish_track_packet(
                self.upstream.data.published_stream(channel),
                TrackPacket::Data {
                    lane,
                    bytes: payload,
                },
            ),
            (DataLane::Reliable, DataTrackDirection::Subscribe) => {
                if let Some(stream) = self.downstream.data.subscribed_stream(channel) {
                    events.request_reverse(stream, ReversePacket::reliable_control(payload));
                }
            }
            (DataLane::Realtime, DataTrackDirection::Subscribe) => {}
        }
    }

    fn release_data_channel(&mut self, topic: DataTopicChannel, events: &mut impl ParticipantSink) {
        match topic.direction {
            DataTrackDirection::Publish => {
                let label = crate::track::publication_label(topic.lane, &topic.topic);
                events
                    .unpublish_track(self.participant_id.derive_track_id(TrackKind::Data, &label));
            }
            DataTrackDirection::Subscribe => events.unsubscribe_tracks(DataState::selector(&topic)),
        }
    }

    fn handle_downstream_keyframe_request(
        &mut self,
        slot: EgressSlot,
        events: &mut impl ParticipantSink,
    ) {
        let Some(mid) = self.egress_mids.get(&slot).copied() else {
            return;
        };
        let request = KeyframeRequest {
            mid,
            rid: None,
            kind: KeyframeRequestKind::Pli,
        };
        let Some((fanout, rid)) = self.downstream.handle_keyframe_request(request) else {
            return;
        };
        events.request_reverse(
            fanout,
            ReversePacket::keyframe(rid, KeyframeRequestKind::Pli),
        );
    }

    fn request_upstream_keyframe(
        &mut self,
        route: IncomingRtpRoute,
        events: &mut impl ParticipantSink,
    ) {
        if let Some(fanout) = route.fanout {
            events.request_reverse(
                fanout,
                ReversePacket::keyframe(route.rid, KeyframeRequestKind::Pli),
            );
        }
    }
}

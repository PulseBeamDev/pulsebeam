use std::net::SocketAddr;
use std::ops::DerefMut;
use std::{collections::VecDeque, ops::Deref};

use ahash::HashMap;
use indexmap::IndexSet;
use pulsebeam_runtime::net::{self, UnifiedSocket};
use pulsebeam_runtime::rand::{Rng, RngCore, SeedableRng};
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::{
    entity::{ParticipantId, RoomId},
    id::ShardId,
    participant::{
        ParticipantConfig, ParticipantCore,
        event::{
            EventQueue, ParticipantControlEvent, ParticipantEvent, ParticipantLifecycleEvent,
            ParticipantTimerEvent, ParticipantTopologyEvent, RtpEvent,
        },
    },
    rtp::RtpPacket,
    shard::{demux::Demuxer, timer::TimerWheel},
    track::StreamId,
};
use str0m::media::MediaKind;

use super::worker::{ClusterCommand, CrossShardEvent, ShardCommand, ShardEvent};

const MAX_PARTICIPANTS_PER_SHARD: usize = 2048;

pub(super) struct Routing {
    pub(super) kind: MediaKind,
    pub(super) subscribers: IndexSet<ParticipantId>,
    pub(super) remote_shards: IndexSet<ShardId>,
}

pub(super) struct RoomState {
    pub(super) members: IndexSet<ParticipantId>,
    pub(super) remote_shards: IndexSet<ShardId>,
    pub(super) audio_selector: crate::audio_selector::TopNAudioSelector,
}

impl RoomState {
    fn new(rng: &mut impl RngCore) -> Self {
        Self {
            members: IndexSet::new(),
            remote_shards: IndexSet::new(),
            audio_selector: crate::audio_selector::TopNAudioSelector::new(rng),
        }
    }
}

pub struct ParticipantMeta {
    span: tracing::Span,
    core: ParticipantCore,
}

impl Deref for ParticipantMeta {
    type Target = ParticipantCore;

    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl DerefMut for ParticipantMeta {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.core
    }
}

impl ParticipantMeta {
    fn with_span<R>(&mut self, f: impl FnOnce(&mut ParticipantCore) -> R) -> R {
        let _guard = self.span.enter();
        f(&mut self.core)
    }
}

/// Abstraction over the cross-shard message bus.
pub(crate) trait CrossShardSend {
    fn send(&self, shard_id: ShardId, ev: CrossShardEvent);
    fn broadcast<F: Fn() -> CrossShardEvent>(&self, make_ev: F);
    fn shard_id(&self) -> ShardId;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ParticipantShardMeta {
    shard_id: ShardId,
    room_id: RoomId,
}

/// Pure shard state — no I/O handles.
/// Methods that emit cross-shard messages accept `&impl CrossShardSend`.
pub(crate) struct ShardCore {
    pub(crate) shard_id: ShardId,
    max_gso_segments: usize,
    pub(super) demuxer: Demuxer,
    pub(super) participants: HashMap<ParticipantId, ParticipantMeta>,
    pub(super) rooms: HashMap<RoomId, RoomState>,
    pub(super) routing: HashMap<TrackId, Routing>,
    participant_shards: HashMap<ParticipantId, ParticipantShardMeta>,
    remote_participant_counts: HashMap<(RoomId, ShardId), usize>,
    timers: TimerWheel,
    pub(super) input_dirty: IndexSet<ParticipantId, ahash::RandomState>,
    pub(super) fanout_dirty: IndexSet<ParticipantId, ahash::RandomState>,
    events: VecDeque<ParticipantEvent>,
    rtp_events: VecDeque<RtpEvent>,
    pub(crate) shard_events: VecDeque<ShardEvent>,
    pub(crate) pending_close: VecDeque<SocketAddr>,
    rng: Rng,
}

impl ShardCore {
    pub(crate) fn new(shard_id: impl Into<ShardId>, max_gso_segments: usize, mut rng: Rng) -> Self {
        let shard_id = shard_id.into();
        let timers = TimerWheel::new(MAX_PARTICIPANTS_PER_SHARD);
        let input_dirty = IndexSet::with_capacity_and_hasher(
            MAX_PARTICIPANTS_PER_SHARD,
            ahash::RandomState::with_seeds(
                rng.next_u64(),
                rng.next_u64(),
                rng.next_u64(),
                rng.next_u64(),
            ),
        );
        let fanout_dirty = IndexSet::with_capacity_and_hasher(
            MAX_PARTICIPANTS_PER_SHARD,
            ahash::RandomState::with_seeds(
                rng.next_u64(),
                rng.next_u64(),
                rng.next_u64(),
                rng.next_u64(),
            ),
        );
        Self {
            shard_id,
            max_gso_segments,
            demuxer: Demuxer::new(shard_id.into()),
            participants: HashMap::default(),
            rooms: HashMap::default(),
            routing: HashMap::default(),
            participant_shards: HashMap::default(),
            remote_participant_counts: HashMap::default(),
            timers,
            input_dirty,
            fanout_dirty,
            events: VecDeque::with_capacity(MAX_PARTICIPANTS_PER_SHARD),
            rtp_events: VecDeque::with_capacity(MAX_PARTICIPANTS_PER_SHARD),
            shard_events: VecDeque::with_capacity(MAX_PARTICIPANTS_PER_SHARD),
            pending_close: VecDeque::new(),
            rng,
        }
    }

    pub(crate) fn next_timer_deadline(&mut self) -> Option<Instant> {
        self.timers.next_deadline()
    }

    pub(crate) fn fire_timers(&mut self, now: Instant) {
        self.timers.drain_expired(now, |participant_id| {
            if let Some(participant) = self.participants.get_mut(&participant_id) {
                participant.with_span(|core| core.on_timeout(now));
                self.input_dirty.insert(participant_id);
            }
        });
    }

    pub(crate) fn on_udp_batch(
        &mut self,
        batch: pulsebeam_runtime::net::RecvPacketBatch,
        router: &impl CrossShardSend,
    ) {
        let Some(participant_id) = self.demuxer.demux(&batch) else {
            return;
        };
        if let Some(participant) = self.participants.get_mut(&participant_id) {
            participant.with_span(|core| core.on_ingress(batch));
            self.input_dirty.insert(participant_id);
        } else if let Some(meta) = self.participant_shards.get(&participant_id) {
            router.send(
                meta.shard_id,
                CrossShardEvent::UdpPacket {
                    participant_id,
                    batch,
                },
            );
        }
    }

    pub(crate) fn poll_input(&mut self, now: Instant) {
        poll_participants(
            now,
            &self.input_dirty,
            &mut self.participants,
            &mut self.events,
            &mut self.rtp_events,
        );
    }

    pub(crate) fn flush_rtp_events(&mut self, router: &impl CrossShardSend) {
        while let Some(ev) = self.rtp_events.pop_front() {
            if ev.stream_id.0.kind().is_audio() {
                handle_audio_rtp(
                    ev,
                    &mut self.rooms,
                    &mut self.participants,
                    &mut self.fanout_dirty,
                    router,
                );
            } else {
                handle_rtp(
                    ev.stream_id,
                    &ev.pkt,
                    &self.routing,
                    &mut self.participants,
                    &mut self.fanout_dirty,
                    router,
                );
            }
        }
    }

    pub(crate) fn poll_fanout(&mut self, now: Instant) {
        poll_participants(
            now,
            &self.fanout_dirty,
            &mut self.participants,
            &mut self.events,
            &mut self.rtp_events,
        );
    }

    pub(crate) fn flush_participant_events(&mut self, router: &impl CrossShardSend) {
        while let Some(event) = self.events.pop_front() {
            match event {
                ParticipantEvent::Topology(ev) => {
                    handle_participant_topology(ev, &mut self.routing, &mut self.shard_events);
                }
                ParticipantEvent::Timer(ParticipantTimerEvent::DeadlineUpdated {
                    at,
                    participant_id,
                }) => {
                    self.timers.schedule(participant_id, at);
                }
                ParticipantEvent::Lifecycle(ParticipantLifecycleEvent::Exited {
                    participant_id,
                }) => {
                    self.remove_participant(&participant_id, router);
                    self.timers.cancel(&participant_id);
                    self.input_dirty.swap_remove(&participant_id);
                    self.shard_events
                        .push_back(ShardEvent::ParticipantExited(participant_id));
                }
                ParticipantEvent::Control(ev) => {
                    handle_participant_control(ev, &mut self.shard_events, router);
                }
            }
        }
    }

    pub(crate) fn flush_egress(
        &mut self,
        udp_socket: &UnifiedSocket,
        tcp_socket: &mut net::tcp::TcpTransport,
    ) {
        for participant_id in self
            .input_dirty
            .drain(..)
            .chain(self.fanout_dirty.drain(..))
        {
            let Some(participant) = self.participants.get_mut(&participant_id) else {
                continue;
            };
            participant.udp_batcher.flush(udp_socket);
            participant.tcp_batcher.flush_tcp(tcp_socket);
        }
    }

    pub(crate) fn flush_close_peers(
        &mut self,
        udp_socket: &mut UnifiedSocket,
        tcp_socket: &mut net::tcp::TcpTransport,
    ) {
        while let Some(addr) = self.pending_close.pop_front() {
            udp_socket.close_peer(&addr);
            tcp_socket.close_peer(&addr);
        }
    }

    pub(crate) fn on_command(
        &mut self,
        cmd: ShardCommand,
        router: &impl CrossShardSend,
    ) -> Option<()> {
        match cmd {
            ShardCommand::AddParticipant(cfg) => {
                self.add_participant(cfg, router);
            }
            ShardCommand::RemoveParticipant(participant_id) => {
                self.remove_participant(&participant_id, router);
            }
            ShardCommand::AddTcpConnection { .. } => {
                // TCP connection routing is handled by the shard worker; no shard core action is required.
            }
            ShardCommand::Cluster(cmd) => self.on_cluster_command(cmd, router)?,
        }
        Some(())
    }

    fn on_cluster_command(
        &mut self,
        cmd: ClusterCommand,
        _router: &impl CrossShardSend,
    ) -> Option<()> {
        match cmd {
            ClusterCommand::RequestKeyframe(req) => {
                let p = self.participants.get_mut(&req.origin)?;
                p.with_span(|core| {
                    core.handle_remote_keyframe_request(req.stream_id, req.kind);
                });
                self.input_dirty.insert(req.origin);
            }

            ClusterCommand::RegisterParticipant {
                shard_id,
                room_id,
                participant_id,
            } => {
                if shard_id != self.shard_id {
                    self.register_remote_participant(participant_id, room_id, shard_id);
                }
            }
            ClusterCommand::UnregisterParticipant {
                shard_id,
                room_id,
                participant_id,
            } => {
                self.unregister_remote_participant(
                    participant_id,
                    ParticipantShardMeta { shard_id, room_id },
                );
                let addrs = self.demuxer.unregister(participant_id);
                self.pending_close.extend(addrs);
            }

            ClusterCommand::PublishTrack(track, room_id) => {
                let publisher = track.meta.origin;
                let tracks = &[track];
                let room = self.rooms.get(&room_id)?;
                for &participant_id in &room.members {
                    if participant_id == publisher {
                        continue;
                    }
                    let Some(p) = self.participants.get_mut(&participant_id) else {
                        continue;
                    };
                    p.on_tracks_published(tracks);
                    self.input_dirty.insert(participant_id);
                }
            }
            ClusterCommand::UnpublishTracks {
                origin: _,
                room_id,
                track_ids,
            } => {
                if let Some(room) = self.rooms.get_mut(&room_id) {
                    for &track_id in &track_ids {
                        room.audio_selector.remove_track((track_id, None));
                    }
                }
                let room = self.rooms.get(&room_id)?;
                for participant_id in &room.members {
                    let Some(p) = self.participants.get_mut(participant_id) else {
                        continue;
                    };

                    if p.on_tracks_unpublished(track_ids.as_slice()) {
                        self.input_dirty.insert(*participant_id);
                    }
                }
            }

            ClusterCommand::SubscribeTrack {
                from_shard_id,
                track,
            } => {
                let routing = self.routing.entry(track.id).or_insert_with(|| Routing {
                    kind: track.kind,
                    subscribers: IndexSet::new(),
                    remote_shards: IndexSet::new(),
                });
                routing.remote_shards.insert(from_shard_id);
            }
            ClusterCommand::UnsubscribeTrack {
                from_shard_id,
                track,
            } => {
                if let Some(routing) = self.routing.get_mut(&track.id) {
                    routing.remote_shards.swap_remove(&from_shard_id);
                }
            }
        }
        Some(())
    }

    pub fn on_cross_shard_event(
        &mut self,
        ev: CrossShardEvent,
        _now: Instant,
        router: &impl CrossShardSend,
    ) {
        match ev {
            CrossShardEvent::RtpPublished { stream_id, pkt } => {
                handle_rtp(
                    stream_id,
                    &pkt,
                    &self.routing,
                    &mut self.participants,
                    &mut self.fanout_dirty,
                    router,
                );
            }
            CrossShardEvent::AudioRtpPublished {
                room_id,
                origin,
                stream_id,
                pkt,
            } => {
                let ev = RtpEvent {
                    stream_id,
                    pkt,
                    room_id,
                    origin,
                };
                handle_audio_rtp(
                    ev,
                    &mut self.rooms,
                    &mut self.participants,
                    &mut self.fanout_dirty,
                    router,
                );
            }
            CrossShardEvent::UdpPacket {
                participant_id,
                batch,
            } => {
                if let Some(participant) = self.participants.get_mut(&participant_id) {
                    participant.with_span(|core| core.on_ingress(batch));
                    self.input_dirty.insert(participant_id);
                }
            }
            CrossShardEvent::KeyframeRequested(req) => {
                if let Some(p) = self.participants.get_mut(&req.origin) {
                    p.with_span(|core| {
                        core.handle_remote_keyframe_request(req.stream_id, req.kind);
                    });
                    self.input_dirty.insert(req.origin);
                }
            }
        }
    }

    fn add_participant(&mut self, cfg: ParticipantConfig, router: &impl CrossShardSend) {
        let room_id = cfg.room_id;
        let participant_id = cfg.participant_id;
        self.remove_participant(&participant_id, router);
        let span = tracing::info_span!(
            "participant",
            %room_id,
            %participant_id
        );
        let mut participant_rng = Rng::seed_from_u64(self.rng.next_u64());
        let participant = ParticipantCore::new(
            cfg,
            self.shard_id,
            self.max_gso_segments,
            1,
            &mut participant_rng,
        );
        let meta = ParticipantMeta {
            core: participant,
            span,
        };
        self.participants.insert(participant_id, meta);
        tracing::info!(%participant_id, "participant added to shard");

        let room = self
            .rooms
            .entry(room_id)
            .or_insert_with(|| RoomState::new(&mut self.rng));
        room.members.insert(participant_id);
        self.input_dirty.insert(participant_id);
    }

    fn register_remote_participant(
        &mut self,
        participant_id: ParticipantId,
        room_id: RoomId,
        shard_id: ShardId,
    ) {
        let next = ParticipantShardMeta { room_id, shard_id };
        if self.participant_shards.get(&participant_id).copied() == Some(next) {
            self.rooms
                .entry(room_id)
                .or_insert_with(|| RoomState::new(&mut self.rng))
                .remote_shards
                .insert(shard_id);
            *self
                .remote_participant_counts
                .entry((room_id, shard_id))
                .or_insert(0) += 1;
            return;
        }
        if let Some(previous) = self.participant_shards.remove(&participant_id) {
            self.cleanup_remote_participant_membership(previous);
        }
        self.participant_shards.insert(participant_id, next);
        self.rooms
            .entry(room_id)
            .or_insert_with(|| RoomState::new(&mut self.rng))
            .remote_shards
            .insert(shard_id);
        *self
            .remote_participant_counts
            .entry((room_id, shard_id))
            .or_insert(0) += 1;
    }

    fn unregister_remote_participant(
        &mut self,
        participant_id: ParticipantId,
        expected: ParticipantShardMeta,
    ) {
        let Some(current) = self.participant_shards.get(&participant_id).copied() else {
            return;
        };
        if current != expected {
            tracing::warn!(
                %participant_id,
                current_shard = %current.shard_id,
                current_room = %current.room_id,
                expected_shard = %expected.shard_id,
                expected_room = %expected.room_id,
                "ignoring stale remote participant unregister"
            );
            return;
        }
        self.participant_shards.remove(&participant_id);
        self.cleanup_remote_participant_membership(current);
    }

    fn cleanup_remote_participant_membership(&mut self, meta: ParticipantShardMeta) {
        let key = (meta.room_id, meta.shard_id);
        let should_remove_shard = match self.remote_participant_counts.get_mut(&key) {
            Some(count) => {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.remote_participant_counts.remove(&key);
                    true
                } else {
                    false
                }
            }
            None => true,
        };

        if should_remove_shard && let Some(room) = self.rooms.get_mut(&meta.room_id) {
            room.remote_shards.swap_remove(&meta.shard_id);
            if room.members.is_empty() && room.remote_shards.is_empty() {
                self.rooms.remove(&meta.room_id);
            }
        }
    }

    fn remove_participant(
        &mut self,
        participant_id: &ParticipantId,
        _router: &impl CrossShardSend,
    ) -> Option<ParticipantMeta> {
        let participant = self.participants.remove(participant_id)?;
        if let Some(room) = self.rooms.get_mut(&participant.room_id) {
            room.members.swap_remove(participant_id);
            if room.members.is_empty() && room.remote_shards.is_empty() {
                self.rooms.remove(&participant.room_id);
            }
        }
        // Evict audio tracks from room selector.
        if let Some(room) = self.rooms.get_mut(&participant.room_id) {
            let audio_ids: Vec<_> = participant.upstream.audio_track_ids().collect();
            for id in audio_ids {
                room.audio_selector.remove_track((id, None));
            }
        }
        let addrs = self.demuxer.unregister(*participant_id);
        self.pending_close.extend(addrs);
        Some(participant)
    }
}

pub(super) fn poll_participants(
    now: Instant,
    dirty: &IndexSet<ParticipantId, ahash::RandomState>,
    participants: &mut HashMap<ParticipantId, ParticipantMeta>,
    events: &mut VecDeque<ParticipantEvent>,
    rtp_events: &mut VecDeque<RtpEvent>,
) {
    for participant_id in dirty {
        let Some(participant) = participants.get_mut(participant_id) else {
            continue;
        };
        let room_id = participant.room_id;
        let mut queue = EventQueue::new(participant_id, room_id, events, rtp_events);
        participant.with_span(|core| core.poll(now, &mut queue));
    }
}

pub(super) fn handle_rtp(
    stream_id: StreamId,
    pkt: &RtpPacket,
    routing: &HashMap<TrackId, Routing>,
    participants: &mut HashMap<ParticipantId, ParticipantMeta>,
    dirty: &mut IndexSet<ParticipantId, ahash::RandomState>,
    router: &impl CrossShardSend,
) -> Option<()> {
    let route = routing.get(&stream_id.0)?;
    match route.kind {
        MediaKind::Video => {
            for participant_id in &route.subscribers {
                let Some(sub) = participants.get_mut(participant_id) else {
                    continue;
                };
                sub.with_span(|core| core.on_forward_rtp(&stream_id, pkt));
                dirty.insert(*participant_id);
            }
            for &shard_id in &route.remote_shards {
                router.send(
                    shard_id,
                    CrossShardEvent::RtpPublished {
                        stream_id,
                        pkt: pkt.clone(),
                    },
                );
            }
        }
        MediaKind::Audio => {}
    }
    Some(())
}

pub(super) fn handle_audio_rtp(
    mut ev: RtpEvent,
    rooms: &mut HashMap<RoomId, RoomState>,
    participants: &mut HashMap<ParticipantId, ParticipantMeta>,
    dirty: &mut IndexSet<ParticipantId, ahash::RandomState>,
    router: &impl CrossShardSend,
) {
    tracing::trace!(
        target: crate::log::TARGET_AUDIO,
        room_id = %ev.room_id,
        origin = %ev.origin,
        stream_id = %ev.stream_id.0,
        seq_no = %ev.pkt.seq_no,
        "audio packet entered shard audio fanout"
    );

    let Some(room) = rooms.get_mut(&ev.room_id) else {
        tracing::warn!(
            target: crate::log::TARGET_AUDIO,
            room_id = %ev.room_id,
            "audio packet dropped: room missing"
        );
        return;
    };

    if participants.contains_key(&ev.origin) {
        for &shard_id in &room.remote_shards {
            router.send(
                shard_id,
                CrossShardEvent::AudioRtpPublished {
                    room_id: ev.room_id,
                    origin: ev.origin,
                    stream_id: ev.stream_id,
                    pkt: ev.pkt.clone(),
                },
            );
        }
    }

    let selection = room.audio_selector.filter(ev.stream_id, &mut ev.pkt);
    let Some(slot_idx) = selection else {
        return;
    };
    tracing::trace!(
        target: crate::log::TARGET_AUDIO,
        room_id = %ev.room_id,
        stream_id = %ev.stream_id.0,
        slot_idx = %slot_idx,
        members = room.members.len(),
        "audio selector produced slot"
    );
    for &participant_id in &room.members {
        if participant_id == ev.origin {
            continue;
        }
        let Some(sub) = participants.get_mut(&participant_id) else {
            continue;
        };
        tracing::trace!(
            target: crate::log::TARGET_AUDIO,
            %participant_id,
            slot_idx = %slot_idx,
            stream_id = %ev.stream_id.0,
            "forwarding audio packet to participant"
        );
        sub.with_span(|core| core.on_forward_audio_rtp(slot_idx, &ev.pkt));
        dirty.insert(participant_id);
    }
}

pub(super) fn handle_participant_topology(
    ev: ParticipantTopologyEvent,
    routing: &mut HashMap<TrackId, Routing>,
    shard_events: &mut VecDeque<ShardEvent>,
) -> Option<()> {
    match ev {
        ParticipantTopologyEvent::TrackSubscribed { track, subscriber } => {
            let entry = routing.entry(track.id).or_insert_with(|| Routing {
                kind: track.kind,
                subscribers: IndexSet::with_capacity(256),
                remote_shards: IndexSet::new(),
            });
            let was_empty = entry.subscribers.is_empty();
            entry.subscribers.insert(subscriber);
            if was_empty {
                shard_events.push_back(ShardEvent::TrackSubscribed(track));
            }
        }
        ParticipantTopologyEvent::TrackUnsubscribed { track, subscriber } => {
            if let Some(entry) = routing.get_mut(&track.id) {
                entry.subscribers.swap_remove(&subscriber);
                if entry.subscribers.is_empty() {
                    shard_events.push_back(ShardEvent::TrackUnsubscribed(track));
                }
            }
        }
    }

    Some(())
}

pub(super) fn handle_participant_control(
    ev: ParticipantControlEvent,
    shard_events: &mut VecDeque<ShardEvent>,
    router: &impl CrossShardSend,
) {
    match ev {
        ParticipantControlEvent::TrackPublished(track) => {
            shard_events.push_back(ShardEvent::TrackPublished(track));
        }
        ParticipantControlEvent::TrackUnpublished { origin, track_id } => {
            shard_events.push_back(ShardEvent::TrackUnpublished { origin, track_id });
        }
        ParticipantControlEvent::KeyframeRequested(req) => {
            if req.shard_id == router.shard_id() {
                shard_events.push_back(ShardEvent::KeyframeRequest(req));
            } else {
                router.send(req.shard_id, CrossShardEvent::KeyframeRequested(req));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use crate::{
        entity::ExternalRoomId,
        participant::event::ParticipantTopologyEvent,
        track::{GlobalKeyframeRequest, TrackMeta},
    };
    use str0m::media::{KeyframeRequestKind, MediaKind};

    use super::*;

    struct TestRouter {
        shard_id: ShardId,
        shard_count: usize,
        sent: RefCell<Vec<(ShardId, CrossShardEvent)>>,
    }

    impl TestRouter {
        fn new(shard_id: usize, shard_count: usize) -> Self {
            Self {
                shard_id: ShardId::new(shard_id),
                shard_count,
                sent: RefCell::new(Vec::new()),
            }
        }

        fn take_sent(&self) -> Vec<(ShardId, CrossShardEvent)> {
            std::mem::take(&mut *self.sent.borrow_mut())
        }
    }

    impl CrossShardSend for TestRouter {
        fn send(&self, shard_id: ShardId, ev: CrossShardEvent) {
            self.sent.borrow_mut().push((shard_id, ev));
        }

        fn broadcast<F: Fn() -> CrossShardEvent>(&self, make_ev: F) {
            let mut sent = self.sent.borrow_mut();
            for i in 0..self.shard_count {
                if ShardId::new(i) != self.shard_id {
                    sent.push((ShardId::new(i), make_ev()));
                }
            }
        }

        fn shard_id(&self) -> ShardId {
            self.shard_id
        }
    }

    fn room_id(s: &str) -> RoomId {
        RoomId::from_external(&ExternalRoomId::new(s).unwrap())
    }

    fn pid() -> ParticipantId {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        ParticipantId::new(&mut pulsebeam_runtime::rand::seeded_rng(
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ))
    }

    fn video_stream(p: ParticipantId) -> StreamId {
        (p.derive_track_id(MediaKind::Video, "v"), None)
    }

    fn audio_stream(p: ParticipantId) -> StreamId {
        (p.derive_track_id(MediaKind::Audio, "a"), None)
    }

    fn video_track(origin: ParticipantId, shard_id: usize) -> TrackMeta {
        TrackMeta {
            shard_id: ShardId::new(shard_id),
            id: origin.derive_track_id(MediaKind::Video, "v"),
            origin,
            kind: MediaKind::Video,
        }
    }

    fn empty_routing(kind: MediaKind) -> Routing {
        Routing {
            kind,
            subscribers: IndexSet::new(),
            remote_shards: IndexSet::new(),
        }
    }

    #[test]
    fn topology_subscribe_creates_routing_entry() {
        let mut routing: HashMap<TrackId, Routing> = HashMap::default();
        let track = video_track(pid(), 1);
        let subscriber = pid();

        handle_participant_topology(
            ParticipantTopologyEvent::TrackSubscribed {
                subscriber,
                track: track.clone(),
            },
            &mut routing,
            &mut VecDeque::new(),
        );

        assert!(routing.contains_key(&track.id));
        assert!(routing[&track.id].subscribers.contains(&subscriber));
    }

    #[test]
    fn topology_first_subscriber_notifies_publisher_shard() {
        let mut routing: HashMap<TrackId, Routing> = HashMap::default();
        let track = video_track(pid(), 1);
        let mut events = VecDeque::new();
        handle_participant_topology(
            ParticipantTopologyEvent::TrackSubscribed {
                subscriber: pid(),
                track: track.clone(),
            },
            &mut routing,
            &mut events,
        );

        assert_eq!(events.len(), 1);
        let ev = events.get(0).unwrap();
        assert!(
            matches!(ev, ShardEvent::TrackSubscribed(meta) if *meta == track),
            "first subscriber must emit TrackSubscribed for the publisher track"
        );
    }

    #[test]
    fn topology_second_subscriber_does_not_notify_publisher() {
        let mut routing: HashMap<TrackId, Routing> = HashMap::default();
        let track = video_track(pid(), 1);
        let mut events = VecDeque::new();

        handle_participant_topology(
            ParticipantTopologyEvent::TrackSubscribed {
                subscriber: pid(),
                track: track.clone(),
            },
            &mut routing,
            &mut events,
        );
        events.clear();

        handle_participant_topology(
            ParticipantTopologyEvent::TrackSubscribed {
                subscriber: pid(),
                track: track.clone(),
            },
            &mut routing,
            &mut events,
        );

        assert!(
            events.is_empty(),
            "second subscriber must not re-notify the publisher"
        );
        assert_eq!(routing[&track.id].subscribers.len(), 2);
    }

    #[test]
    fn topology_last_unsubscribe_emits_shard_event_and_clears_subscribers() {
        let mut routing: HashMap<TrackId, Routing> = HashMap::default();
        let track = video_track(pid(), 1);
        let subscriber = pid();
        let mut events = VecDeque::new();

        handle_participant_topology(
            ParticipantTopologyEvent::TrackSubscribed {
                subscriber,
                track: track.clone(),
            },
            &mut routing,
            &mut events,
        );
        events.clear();

        handle_participant_topology(
            ParticipantTopologyEvent::TrackUnsubscribed {
                subscriber,
                track: track.clone(),
            },
            &mut routing,
            &mut events,
        );

        assert_eq!(events.len(), 1, "must emit exactly one unsubscribe event");
        assert!(
            matches!(&events[0], ShardEvent::TrackUnsubscribed(meta) if *meta == track),
            "last subscriber must emit TrackUnsubscribed for the publisher track"
        );
        assert!(routing.contains_key(&track.id));
        assert!(routing[&track.id].subscribers.is_empty());
    }

    #[test]
    fn topology_unsubscribe_with_remaining_subscriber_does_not_notify() {
        let mut routing: HashMap<TrackId, Routing> = HashMap::default();
        let track = video_track(pid(), 1);
        let s1 = pid();
        let s2 = pid();
        let mut events = VecDeque::new();

        for s in [s1, s2] {
            handle_participant_topology(
                ParticipantTopologyEvent::TrackSubscribed {
                    subscriber: s,
                    track: track.clone(),
                },
                &mut routing,
                &mut events,
            );
        }
        events.clear();

        handle_participant_topology(
            ParticipantTopologyEvent::TrackUnsubscribed {
                subscriber: s1,
                track: track.clone(),
            },
            &mut routing,
            &mut events,
        );

        assert!(
            events.is_empty(),
            "must not notify while a local subscriber remains"
        );
        assert_eq!(routing[&track.id].subscribers.len(), 1);
    }

    #[test]
    fn topology_unsubscribe_keeps_entry_while_remote_shards_remain() {
        let mut routing: HashMap<TrackId, Routing> = HashMap::default();
        let track = video_track(pid(), 1);
        let subscriber = pid();
        let mut events = VecDeque::new();

        handle_participant_topology(
            ParticipantTopologyEvent::TrackSubscribed {
                subscriber,
                track: track.clone(),
            },
            &mut routing,
            &mut events,
        );
        routing
            .get_mut(&track.id)
            .unwrap()
            .remote_shards
            .insert(ShardId::new(2));
        events.clear();

        handle_participant_topology(
            ParticipantTopologyEvent::TrackUnsubscribed {
                subscriber,
                track: track.clone(),
            },
            &mut routing,
            &mut events,
        );

        assert!(
            routing.contains_key(&track.id),
            "entry must remain while remote shards are subscribed"
        );
    }

    #[test]
    fn handle_rtp_sends_to_all_remote_shards() {
        let router = TestRouter::new(0, 3);
        let stream = video_stream(pid());
        let mut routing: HashMap<TrackId, Routing> = HashMap::default();
        let mut route = empty_routing(MediaKind::Video);
        route.remote_shards.insert(ShardId::new(1));
        route.remote_shards.insert(ShardId::new(2));
        routing.insert(stream.0, route);

        let pkt = RtpPacket::default();
        let mut participants: HashMap<ParticipantId, ParticipantMeta> = HashMap::default();
        let mut dirty: IndexSet<ParticipantId, ahash::RandomState> =
            IndexSet::with_hasher(ahash::RandomState::default());

        handle_rtp(
            stream,
            &pkt,
            &routing,
            &mut participants,
            &mut dirty,
            &router,
        );

        let sent = router.take_sent();
        assert_eq!(sent.len(), 2, "must forward to both remote shards");
        let targets: Vec<ShardId> = sent.iter().map(|(id, _)| *id).collect();
        assert!(targets.contains(&ShardId::new(1)));
        assert!(targets.contains(&ShardId::new(2)));
        for (_, ev) in &sent {
            assert!(
                matches!(ev, CrossShardEvent::RtpPublished { .. }),
                "expected RtpPublished event"
            );
        }
    }

    #[test]
    fn handle_rtp_no_send_for_empty_remote_shards() {
        let router = TestRouter::new(0, 3);
        let stream = video_stream(pid());
        let mut routing: HashMap<TrackId, Routing> = HashMap::default();
        routing.insert(stream.0, empty_routing(MediaKind::Video)); // no remote_shards

        handle_rtp(
            stream,
            &RtpPacket::default(),
            &routing,
            &mut HashMap::default(),
            &mut IndexSet::with_hasher(ahash::RandomState::default()),
            &router,
        );

        assert!(router.take_sent().is_empty());
    }

    #[test]
    fn handle_rtp_ignores_unknown_stream() {
        let router = TestRouter::new(0, 3);
        let stream = video_stream(pid());
        let routing: HashMap<TrackId, Routing> = HashMap::default();

        handle_rtp(
            stream,
            &RtpPacket::default(),
            &routing,
            &mut HashMap::default(),
            &mut IndexSet::with_hasher(ahash::RandomState::default()),
            &router,
        );

        assert!(router.take_sent().is_empty());
    }

    #[test]
    fn audio_rtp_not_forwarded_when_origin_is_remote() {
        // The loop-prevention guard skips cross-shard fanout when the origin
        // participant is not in the local participants map.
        let router = TestRouter::new(0, 3);
        let origin = pid();
        let rid = room_id("test");

        let mut rooms: HashMap<RoomId, RoomState> = HashMap::default();
        let mut room = RoomState::new(&mut pulsebeam_runtime::rand::seeded_rng(42));
        room.remote_shards.insert(ShardId::new(1));
        room.remote_shards.insert(ShardId::new(2));
        rooms.insert(rid, room);

        let ev = RtpEvent {
            stream_id: audio_stream(origin),
            pkt: RtpPacket::default(),
            room_id: rid,
            origin,
        };

        handle_audio_rtp(
            ev,
            &mut rooms,
            &mut HashMap::default(),
            &mut IndexSet::with_hasher(ahash::RandomState::default()),
            &router,
        );

        assert!(
            router.take_sent().is_empty(),
            "must not forward when origin is not local"
        );
    }

    #[test]
    fn cluster_subscribe_track_adds_remote_shard_to_routing() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let track = video_track(pid(), 2);

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::SubscribeTrack {
                from_shard_id: ShardId::new(2),
                track: track.clone(),
            }),
            &router,
        );

        assert!(core.routing.contains_key(&track.id));
        assert!(
            core.routing[&track.id]
                .remote_shards
                .contains(&ShardId::new(2))
        );
    }

    #[test]
    fn cluster_unsubscribe_track_clears_remote_shard() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let track = video_track(pid(), 2);

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::SubscribeTrack {
                from_shard_id: ShardId::new(2),
                track: track.clone(),
            }),
            &router,
        );
        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnsubscribeTrack {
                from_shard_id: ShardId::new(2),
                track: track.clone(),
            }),
            &router,
        );

        assert!(core.routing.contains_key(&track.id));
        assert!(core.routing[&track.id].remote_shards.is_empty());
    }

    #[test]
    fn cluster_unsubscribe_track_keeps_local_subscribers() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let track = video_track(pid(), 2);
        let local_sub = pid();

        core.routing.insert(
            track.id,
            Routing {
                kind: MediaKind::Video,
                subscribers: {
                    let mut s = IndexSet::new();
                    s.insert(local_sub);
                    s
                },
                remote_shards: {
                    let mut s = IndexSet::new();
                    s.insert(ShardId::new(2));
                    s
                },
            },
        );

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnsubscribeTrack {
                from_shard_id: ShardId::new(2),
                track: track.clone(),
            }),
            &router,
        );

        assert!(
            core.routing.contains_key(&track.id),
            "entry must remain while a local subscriber exists"
        );
        assert!(core.routing[&track.id].remote_shards.is_empty());
    }

    #[test]
    fn register_remote_participant_populates_shard_map() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let participant = pid();
        let rid = room_id("r1");

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: ShardId::new(2),
                room_id: rid,
                participant_id: participant,
            }),
            &router,
        );

        assert!(core.rooms.contains_key(&rid));
        assert!(core.rooms[&rid].remote_shards.contains(&ShardId::new(2)));
        assert!(matches!(
            core.participant_shards.get(&participant),
            Some(meta) if meta.shard_id == ShardId::new(2) && meta.room_id == rid
        ));
    }

    #[test]
    fn register_local_shard_participant_is_no_op() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let participant = pid();
        let rid = room_id("local-noop");

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: ShardId::new(0),
                room_id: rid,
                participant_id: participant,
            }),
            &router,
        );

        assert!(
            !core.participant_shards.contains_key(&participant),
            "local participants must not be registered via RegisterParticipant"
        );
    }

    #[test]
    fn unregister_participant_removes_maps_and_queues_no_close_addrs_without_session() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let participant = pid();
        let rid = room_id("unregister");

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: ShardId::new(1),
                room_id: rid,
                participant_id: participant,
            }),
            &router,
        );

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnregisterParticipant {
                shard_id: ShardId::new(1),
                room_id: rid,
                participant_id: participant,
            }),
            &router,
        );

        assert!(!core.participant_shards.contains_key(&participant));
        assert!(!core.rooms.contains_key(&rid));
        assert!(core.pending_close.is_empty());
    }

    #[test]
    fn unregister_participant_keeps_remote_shard_until_last_peer_leaves() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let participant_a = pid();
        let participant_b = pid();
        let rid = room_id("unregister-keep-shard");

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: ShardId::new(1),
                room_id: rid,
                participant_id: participant_a,
            }),
            &router,
        );
        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: ShardId::new(1),
                room_id: rid,
                participant_id: participant_b,
            }),
            &router,
        );

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnregisterParticipant {
                shard_id: ShardId::new(1),
                room_id: rid,
                participant_id: participant_a,
            }),
            &router,
        );

        assert!(
            core.rooms[&rid].remote_shards.contains(&ShardId::new(1)),
            "room must keep shard 1 while another remote participant remains there"
        );
        assert!(
            core.participant_shards.contains_key(&participant_b),
            "remaining remote participant must still be tracked"
        );

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnregisterParticipant {
                shard_id: ShardId::new(1),
                room_id: rid,
                participant_id: participant_b,
            }),
            &router,
        );

        assert!(
            !core.rooms.contains_key(&rid),
            "room must be removed once the final remote participant leaves"
        );
    }

    #[test]
    fn register_remote_participant_replaces_previous_shard_membership() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let participant = pid();
        let rid = room_id("remote-move");

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: ShardId::new(1),
                room_id: rid,
                participant_id: participant,
            }),
            &router,
        );
        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: ShardId::new(2),
                room_id: rid,
                participant_id: participant,
            }),
            &router,
        );

        assert!(!core.rooms[&rid].remote_shards.contains(&ShardId::new(1)));
        assert!(core.rooms[&rid].remote_shards.contains(&ShardId::new(2)));
        assert!(matches!(
            core.participant_shards.get(&participant),
            Some(meta) if meta.shard_id == ShardId::new(2) && meta.room_id == rid
        ));
    }

    #[test]
    fn register_remote_participant_duplicate_is_idempotent() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let participant = pid();
        let rid = room_id("remote-dup");

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: ShardId::new(1),
                room_id: rid,
                participant_id: participant,
            }),
            &router,
        );
        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: ShardId::new(1),
                room_id: rid,
                participant_id: participant,
            }),
            &router,
        );

        assert_eq!(core.participant_shards.len(), 1);
        assert!(core.rooms.contains_key(&rid));
        assert_eq!(core.rooms[&rid].remote_shards.len(), 1);
        assert!(core.rooms[&rid].remote_shards.contains(&ShardId::new(1)));
    }

    #[test]
    fn audio_rtp_from_remote_origin_fans_out_to_local_subscriber_first() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let subscriber = pid();
        let origin_remote = pid();
        let rid = room_id("audio-subscriber-first");

        add_participant(&mut core, &router, subscriber, rid);
        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: ShardId::new(1),
                room_id: rid,
                participant_id: origin_remote,
            }),
            &router,
        );

        let mut pkt = RtpPacket::default();
        pkt.ext_vals.audio_level = Some(-30);
        pkt.arrival_ts = tokio::time::Instant::now();
        pkt.playout_time = pkt.arrival_ts;

        core.on_cross_shard_event(
            CrossShardEvent::AudioRtpPublished {
                room_id: rid,
                origin: origin_remote,
                stream_id: audio_stream(origin_remote),
                pkt,
            },
            tokio::time::Instant::now(),
            &router,
        );

        assert!(
            core.fanout_dirty.contains(&subscriber),
            "remote-origin audio must still be fanned out to local subscribers"
        );
        assert!(
            router.take_sent().is_empty(),
            "remote-origin audio on subscriber shard must not be re-broadcast"
        );
    }

    #[test]
    fn unregister_participant_ignores_stale_remote_registration() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let participant = pid();
        let old_room = room_id("remote-old");
        let new_room = room_id("remote-new");

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: ShardId::new(1),
                room_id: old_room,
                participant_id: participant,
            }),
            &router,
        );
        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: ShardId::new(2),
                room_id: new_room,
                participant_id: participant,
            }),
            &router,
        );

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnregisterParticipant {
                shard_id: ShardId::new(1),
                room_id: old_room,
                participant_id: participant,
            }),
            &router,
        );

        assert!(matches!(
            core.participant_shards.get(&participant),
            Some(meta) if meta.shard_id == ShardId::new(2) && meta.room_id == new_room
        ));
        assert!(!core.rooms.contains_key(&old_room));
        assert!(
            core.rooms[&new_room]
                .remote_shards
                .contains(&ShardId::new(2))
        );

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnregisterParticipant {
                shard_id: ShardId::new(2),
                room_id: new_room,
                participant_id: participant,
            }),
            &router,
        );

        assert!(!core.participant_shards.contains_key(&participant));
        assert!(!core.rooms.contains_key(&new_room));
    }

    #[test]
    fn control_local_keyframe_request_pushes_shard_event() {
        let router = TestRouter::new(1, 3);
        let mut shard_events: VecDeque<ShardEvent> = VecDeque::new();

        handle_participant_control(
            ParticipantControlEvent::KeyframeRequested(GlobalKeyframeRequest {
                shard_id: ShardId::new(1), // same shard → local
                origin: pid(),
                stream_id: video_stream(pid()),
                kind: KeyframeRequestKind::Pli,
            }),
            &mut shard_events,
            &router,
        );

        assert_eq!(shard_events.len(), 1);
        assert!(matches!(shard_events[0], ShardEvent::KeyframeRequest(_)));
        assert!(
            router.take_sent().is_empty(),
            "local keyframe must not cross shard boundaries"
        );
    }

    #[test]
    fn control_remote_keyframe_request_is_forwarded_cross_shard() {
        let router = TestRouter::new(1, 3);
        let mut shard_events: VecDeque<ShardEvent> = VecDeque::new();

        handle_participant_control(
            ParticipantControlEvent::KeyframeRequested(GlobalKeyframeRequest {
                shard_id: ShardId::new(0), // different shard → remote
                origin: pid(),
                stream_id: video_stream(pid()),
                kind: KeyframeRequestKind::Pli,
            }),
            &mut shard_events,
            &router,
        );

        assert!(
            shard_events.is_empty(),
            "remote keyframe must not create a local ShardEvent"
        );
        let sent = router.take_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].0,
            ShardId::new(0),
            "must be directed to the publisher's shard"
        );
        assert!(matches!(sent[0].1, CrossShardEvent::KeyframeRequested(_)));
    }

    // -----------------------------------------------------------------------
    // Helpers for participant lifecycle tests
    // -----------------------------------------------------------------------

    fn make_participant_cfg(participant_id: ParticipantId, room_id: RoomId) -> ParticipantConfig {
        ParticipantConfig {
            manual_sub: false,
            room_id,
            participant_id,
            rtc: str0m::RtcConfig::new().build(std::time::Instant::now()),
            available_tracks: vec![],
        }
    }

    fn make_video_track(origin: ParticipantId, shard_id: ShardId) -> crate::track::Track {
        use crate::rtp::monitor::StreamState;
        use crate::track::{LayerQuality, Track, TrackLayer, TrackMeta};
        let meta = TrackMeta {
            shard_id,
            id: origin.derive_track_id(MediaKind::Video, "v"),
            origin,
            kind: MediaKind::Video,
        };
        let layer = TrackLayer {
            meta: meta.clone(),
            rid: None,
            quality: LayerQuality::Low,
            state: StreamState::new(false, 100_000),
        };
        Track {
            meta,
            layers: vec![layer],
        }
    }

    fn add_participant(
        core: &mut ShardCore,
        router: &TestRouter,
        participant_id: ParticipantId,
        room_id: RoomId,
    ) {
        core.on_command(
            ShardCommand::AddParticipant(make_participant_cfg(participant_id, room_id)),
            router,
        );
        router.take_sent();
    }

    // -----------------------------------------------------------------------
    // Participant leave — cleanup invariants
    // -----------------------------------------------------------------------

    #[test]
    fn participant_leave_removed_from_participants() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let p = pid();
        let r = room_id("leave1");

        add_participant(&mut core, &router, p, r);
        assert!(core.participants.contains_key(&p));

        core.remove_participant(&p, &router);

        assert!(
            !core.participants.contains_key(&p),
            "participant must be gone from participants map"
        );
    }

    #[test]
    fn participant_leave_last_in_room_removes_room() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let p = pid();
        let r = room_id("leave2");

        add_participant(&mut core, &router, p, r);
        assert!(core.rooms.contains_key(&r));

        core.remove_participant(&p, &router);

        assert!(!core.rooms.contains_key(&r), "empty room must be removed");
    }

    #[test]
    fn participant_leave_last_in_room_broadcasts_room_left() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let p = pid();
        let r = room_id("leave3");

        add_participant(&mut core, &router, p, r);

        core.remove_participant(&p, &router);

        assert!(
            router.take_sent().is_empty(),
            "participant removal no longer emits cross-shard room membership events"
        );
    }

    #[test]
    fn participant_leave_not_last_member_room_persists() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let p1 = pid();
        let p2 = pid();
        let r = room_id("leave4");

        add_participant(&mut core, &router, p1, r);
        add_participant(&mut core, &router, p2, r);

        core.remove_participant(&p1, &router);

        assert!(
            core.rooms.contains_key(&r),
            "room must persist with remaining member"
        );
        assert!(
            core.rooms[&r].members.contains(&p2),
            "remaining member must still be in room"
        );
        assert!(
            !core.rooms[&r].members.contains(&p1),
            "removed participant must not be in room members"
        );
        assert!(
            !core.participants.contains_key(&p1),
            "removed participant must not be in participants map"
        );
    }

    #[test]
    fn participant_leave_not_last_member_no_broadcast() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let p1 = pid();
        let p2 = pid();
        let r = room_id("leave5");

        add_participant(&mut core, &router, p1, r);
        add_participant(&mut core, &router, p2, r);

        core.remove_participant(&p1, &router);

        assert!(
            router.take_sent().is_empty(),
            "must not broadcast RoomMemberLeft while other members remain"
        );
    }

    #[test]
    fn participant_leave_room_kept_when_remote_shards_present() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let p = pid();
        let r = room_id("leave6");

        add_participant(&mut core, &router, p, r);
        core.rooms
            .get_mut(&r)
            .unwrap()
            .remote_shards
            .insert(ShardId::new(2));

        core.remove_participant(&p, &router);

        assert!(
            core.rooms.contains_key(&r),
            "room must remain while remote shards are present"
        );
        assert!(
            core.rooms[&r].members.is_empty(),
            "local members list must be empty"
        );
        assert!(
            core.rooms[&r].remote_shards.contains(&ShardId::new(2)),
            "remote shard must remain registered"
        );
    }

    #[test]
    fn participant_rejoin_removes_previous_instance() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let p = pid();
        let r = room_id("leave7");

        add_participant(&mut core, &router, p, r);
        assert_eq!(core.participants.len(), 1);
        assert_eq!(core.rooms[&r].members.len(), 1);

        add_participant(&mut core, &router, p, r);

        assert_eq!(
            core.participants.len(),
            1,
            "must not have duplicate participant entries"
        );
        assert_eq!(
            core.rooms[&r].members.len(),
            1,
            "must not have duplicate room member entries"
        );
        assert!(core.participants.contains_key(&p));
    }

    #[test]
    fn lifecycle_exited_cleans_up_participant_and_emits_shard_event() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let p = pid();
        let r = room_id("leave8");

        add_participant(&mut core, &router, p, r);

        core.events.push_back(ParticipantEvent::Lifecycle(
            ParticipantLifecycleEvent::Exited { participant_id: p },
        ));
        core.flush_participant_events(&router);

        assert!(
            !core.participants.contains_key(&p),
            "participant must be removed on Exited lifecycle event"
        );
        assert!(
            !core.rooms.contains_key(&r),
            "empty room must be removed on participant exit"
        );
        assert!(
            core.shard_events
                .iter()
                .any(|e| matches!(e, ShardEvent::ParticipantExited(id) if *id == p)),
            "must emit ShardEvent::ParticipantExited"
        );
    }

    #[test]
    fn participant_leave_clears_routing_subscriber_entry() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let publisher = pid();
        let subscriber = pid();
        let r = room_id("leave9");
        let stream = video_stream(publisher);

        add_participant(&mut core, &router, subscriber, r);

        // Simulate the subscriber having a routing entry (as produced by reconcile_routes
        // → TopologyEvent::StreamSubscribed → flush_participant_events in the normal flow).
        handle_participant_topology(
            ParticipantTopologyEvent::TrackSubscribed {
                subscriber,
                track: video_track(publisher, 1),
            },
            &mut core.routing,
            &mut core.shard_events,
        );
        core.shard_events.clear();

        assert!(core.routing[&stream.0].subscribers.contains(&subscriber));

        // Simulate the downstream emitting StreamUnsubscribed on cleanup, as it does
        // when participant.downstream.unsubscribe_all() returns routes and
        // flush_participant_events processes them.
        core.events.push_back(ParticipantEvent::Topology(
            ParticipantTopologyEvent::TrackUnsubscribed {
                subscriber,
                track: video_track(publisher, 1),
            },
        ));
        core.flush_participant_events(&router);

        assert!(
            core.routing[&stream.0].subscribers.is_empty(),
            "routing subscribers must be cleaned up when subscriber unsubscribes"
        );
    }

    // -----------------------------------------------------------------------
    // UnpublishTracks — cleanup of departed participant's tracks from subscribers
    // -----------------------------------------------------------------------

    #[test]
    fn unpublish_tracks_removes_track_from_subscriber_downstream() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let a = pid(); // publisher (remote)
        let b = pid(); // local subscriber
        let r = room_id("unp1");

        add_participant(&mut core, &router, b, r);

        // Simulate B having A's video track (delivered earlier via PublishTrack).
        let a_track = make_video_track(a, ShardId::new(1));
        let track_id = a_track.meta.id;
        core.participants
            .get_mut(&b)
            .unwrap()
            .downstream
            .add_track(a_track);

        assert!(
            core.participants[&b]
                .downstream
                .video
                .tracks()
                .any(|t| t.id == track_id),
            "B must have A's track before UnpublishTracks"
        );

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnpublishTracks {
                room_id: r,
                origin: a,
                track_ids: vec![track_id],
            }),
            &router,
        );

        assert!(
            !core.participants[&b]
                .downstream
                .video
                .tracks()
                .any(|t| t.id == track_id),
            "B must NOT have A's track after UnpublishTracks"
        );
        assert!(
            core.input_dirty.contains(&b),
            "B must be dirty so signaling removes A's track from the client view"
        );
    }

    #[test]
    fn unpublish_tracks_marks_all_local_subscribers_dirty() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let a = pid(); // publisher
        let b = pid();
        let c = pid();
        let r = room_id("unp2");

        add_participant(&mut core, &router, b, r);
        add_participant(&mut core, &router, c, r);

        let a_track = make_video_track(a, ShardId::new(1));
        let track_id = a_track.meta.id;
        // Both B and C are subscribed to A's track.
        core.participants
            .get_mut(&b)
            .unwrap()
            .downstream
            .add_track(a_track.clone());
        core.participants
            .get_mut(&c)
            .unwrap()
            .downstream
            .add_track(a_track);
        core.input_dirty.clear();

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnpublishTracks {
                room_id: r,
                origin: a,
                track_ids: vec![track_id],
            }),
            &router,
        );

        assert!(
            core.input_dirty.contains(&b),
            "B must be dirty after UnpublishTracks"
        );
        assert!(
            core.input_dirty.contains(&c),
            "C must be dirty after UnpublishTracks"
        );
        assert!(
            !core.participants[&b]
                .downstream
                .video
                .tracks()
                .any(|t| t.id == track_id),
            "B must not retain A's track"
        );
        assert!(
            !core.participants[&c]
                .downstream
                .video
                .tracks()
                .any(|t| t.id == track_id),
            "C must not retain A's track"
        );
    }

    #[test]
    fn unpublish_tracks_no_op_when_no_subscriber_holds_track() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let a = pid();
        let b = pid();
        let r = room_id("unp3");

        add_participant(&mut core, &router, b, r);
        core.input_dirty.clear();

        // B never received A's track — command should be a no-op.
        let unknown_track_id = a.derive_track_id(MediaKind::Video, "v");
        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnpublishTracks {
                room_id: r,
                origin: a,
                track_ids: vec![unknown_track_id],
            }),
            &router,
        );

        assert!(
            core.input_dirty.is_empty(),
            "must not dirty participants when no subscriber held the track"
        );
    }

    #[test]
    fn unpublish_tracks_removes_all_tracks_in_batch_not_just_first() {
        // Regression test: when a publisher with multiple tracks (e.g. camera
        // + screenshare) exits, ALL tracks must be removed from the
        // subscriber's downstream — not just the first one in the batch.
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let a = pid(); // publisher (remote)
        let b = pid(); // local subscriber
        let r = room_id("unp-multi");

        add_participant(&mut core, &router, b, r);

        let camera_track = make_video_track(a, ShardId::new(1));
        let screenshare_track = make_video_track(a, ShardId::new(2));
        let camera_id = camera_track.meta.id;
        let screenshare_id = screenshare_track.meta.id;

        let p = core.participants.get_mut(&b).unwrap();
        p.downstream.add_track(camera_track);
        p.downstream.add_track(screenshare_track);

        assert!(
            core.participants[&b]
                .downstream
                .video
                .tracks()
                .any(|t| t.id == camera_id),
            "B must have camera before UnpublishTracks"
        );
        assert!(
            core.participants[&b]
                .downstream
                .video
                .tracks()
                .any(|t| t.id == screenshare_id),
            "B must have screenshare before UnpublishTracks"
        );

        // Both tracks are unpublished in a single batch (as happens when a
        // participant exits and delete_participant broadcasts UnpublishTracks).
        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnpublishTracks {
                room_id: r,
                origin: a,
                track_ids: vec![camera_id, screenshare_id],
            }),
            &router,
        );

        assert!(
            !core.participants[&b]
                .downstream
                .video
                .tracks()
                .any(|t| t.id == camera_id),
            "camera must be removed after UnpublishTracks"
        );
        assert!(
            !core.participants[&b]
                .downstream
                .video
                .tracks()
                .any(|t| t.id == screenshare_id),
            "screenshare must be removed after UnpublishTracks - ghost participant bug"
        );
    }
}

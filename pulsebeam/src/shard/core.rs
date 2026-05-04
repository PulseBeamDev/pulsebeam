use std::net::SocketAddr;
use std::ops::DerefMut;
use std::{collections::VecDeque, ops::Deref};

use ahash::HashMap;
use indexmap::IndexSet;
use pulsebeam_runtime::net::{self, UnifiedSocket};
use pulsebeam_runtime::rand::{Rng, RngCore, SeedableRng};
use tokio::time::Instant;

use crate::{
    entity::{ParticipantId, RoomId},
    participant::{
        ParticipantConfig, ParticipantCore,
        event::{
            ControlEvent, EventQueue, LifecycleEvent, ParticipantEvent, RtpEvent, TimerEvent,
            TopologyEvent,
        },
    },
    rtp::RtpPacket,
    shard::{demux::Demuxer, timer::TimerWheel},
    track::{StreamId, StreamWriter},
};
use str0m::media::MediaKind;

use super::worker::{ClusterCommand, CrossShardEvent, ShardCommand, ShardEvent};

const MAX_PARTICIPANTS_PER_SHARD: usize = 2048;

pub(super) struct Routing {
    pub(super) kind: MediaKind,
    pub(super) subscribers: IndexSet<ParticipantId>,
    pub(super) remote_shards: IndexSet<usize>,
}

pub(super) struct RoomState {
    pub(super) members: IndexSet<ParticipantId>,
    pub(super) remote_shards: IndexSet<usize>,
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

/// Abstraction over the cross-shard message bus.
pub(crate) trait CrossShardSend {
    fn send(&self, shard_id: usize, ev: CrossShardEvent);
    fn broadcast<F: Fn() -> CrossShardEvent>(&self, make_ev: F);
    fn shard_id(&self) -> usize;
}

/// Pure shard state — no I/O handles.
/// Methods that emit cross-shard messages accept `&impl CrossShardSend`.
pub(crate) struct ShardCore {
    pub(crate) shard_id: usize,
    max_gso_segments: usize,
    pub(super) demuxer: Demuxer,
    pub(super) participants: HashMap<ParticipantId, ParticipantMeta>,
    pub(super) rooms: HashMap<RoomId, RoomState>,
    pub(super) routing: HashMap<StreamId, Routing>,
    pub(super) participant_shards: HashMap<ParticipantId, usize>,
    remote_participant_ufrags: HashMap<ParticipantId, String>,
    timers: TimerWheel,
    pub(super) input_dirty: IndexSet<ParticipantId, ahash::RandomState>,
    pub(super) fanout_dirty: IndexSet<ParticipantId, ahash::RandomState>,
    events: VecDeque<ParticipantEvent>,
    rtp_events: VecDeque<RtpEvent>,
    pub(crate) shard_events: VecDeque<ShardEvent>,
    pub(crate) pending_cross_shard: VecDeque<CrossShardEvent>,
    pub(crate) pending_close: VecDeque<SocketAddr>,
    rng: Rng,
}

impl ShardCore {
    pub(crate) fn new(shard_id: usize, max_gso_segments: usize, mut rng: Rng) -> Self {
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
            demuxer: Demuxer::default(),
            participants: HashMap::default(),
            rooms: HashMap::default(),
            routing: HashMap::default(),
            participant_shards: HashMap::default(),
            remote_participant_ufrags: HashMap::default(),
            timers,
            input_dirty,
            fanout_dirty,
            events: VecDeque::with_capacity(MAX_PARTICIPANTS_PER_SHARD),
            rtp_events: VecDeque::with_capacity(MAX_PARTICIPANTS_PER_SHARD),
            shard_events: VecDeque::with_capacity(MAX_PARTICIPANTS_PER_SHARD),
            pending_cross_shard: VecDeque::with_capacity(64),
            pending_close: VecDeque::new(),
            rng,
        }
    }

    pub(crate) fn next_timer_deadline(&mut self) -> Option<Instant> {
        self.timers.next_deadline()
    }

    pub(crate) fn flush_cross_shard(&mut self, now: Instant, router: &impl CrossShardSend) {
        while let Some(ev) = self.pending_cross_shard.pop_front() {
            self.on_cross_shard_event(ev, now, router);
        }
    }

    pub(crate) fn fire_timers(&mut self, now: Instant) {
        self.timers.drain_expired(now, |participant_id| {
            if let Some(participant) = self.participants.get_mut(&participant_id) {
                participant.on_timeout(now);
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
            participant.on_ingress(batch);
            self.input_dirty.insert(participant_id);
        } else if let Some(&shard_id) = self.participant_shards.get(&participant_id) {
            router.send(
                shard_id,
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
                    handle_participant_topology(ev, &mut self.routing, router);
                }
                ParticipantEvent::Timer(TimerEvent::DeadlineUpdated { at, participant_id }) => {
                    self.timers.schedule(participant_id, at);
                }
                ParticipantEvent::Lifecycle(LifecycleEvent::Exited { participant_id }) => {
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
            ClusterCommand::RequestKeyframe(req) => {
                let p = self.participants.get_mut(&req.origin)?;
                let _guard = p.span.enter();
                p.core
                    .handle_remote_keyframe_request(req.stream_id, req.kind);
                self.input_dirty.insert(req.origin);
            }
            ClusterCommand::RegisterParticipant {
                shard_id,
                participant_id,
                ufrag,
            } => {
                if shard_id != self.shard_id {
                    self.demuxer
                        .register_ice_ufrag(ufrag.as_bytes(), participant_id);
                    self.remote_participant_ufrags.insert(participant_id, ufrag);
                    self.participant_shards.insert(participant_id, shard_id);
                }
            }
            ClusterCommand::UnregisterParticipant { participant_id } => {
                self.participant_shards.remove(&participant_id);
                if let Some(ufrag) = self.remote_participant_ufrags.remove(&participant_id) {
                    let addrs = self.demuxer.unregister(ufrag.as_bytes());
                    self.pending_close.extend(addrs);
                }
            }
            ClusterCommand::UnpublishTracks {
                origin: _,
                track_ids,
            } => {
                for (participant_id, participant) in self.participants.iter_mut() {
                    if participant.on_tracks_unpublished(track_ids.as_slice()) {
                        self.input_dirty.insert(*participant_id);
                    }
                }
            }
        }
        Some(())
    }

    fn on_cross_shard_event(
        &mut self,
        ev: CrossShardEvent,
        _now: Instant,
        router: &impl CrossShardSend,
    ) {
        match ev {
            CrossShardEvent::StreamSubscribed {
                stream_id,
                kind,
                from_shard_id,
            } => {
                let routing = self.routing.entry(stream_id).or_insert_with(|| Routing {
                    kind,
                    subscribers: IndexSet::new(),
                    remote_shards: IndexSet::new(),
                });
                routing.remote_shards.insert(from_shard_id);
            }
            CrossShardEvent::StreamUnsubscribed {
                stream_id,
                from_shard_id,
            } => {
                if let Some(routing) = self.routing.get_mut(&stream_id) {
                    routing.remote_shards.swap_remove(&from_shard_id);
                    if routing.subscribers.is_empty() && routing.remote_shards.is_empty() {
                        self.routing.remove(&stream_id);
                    }
                }
            }
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
            CrossShardEvent::RoomMemberJoined {
                room_id,
                from_shard_id,
            } => {
                self.rooms
                    .entry(room_id)
                    .or_insert_with(|| RoomState::new(&mut self.rng))
                    .remote_shards
                    .insert(from_shard_id);
            }
            CrossShardEvent::RoomMemberLeft {
                room_id,
                from_shard_id,
            } => {
                if let Some(room) = self.rooms.get_mut(&room_id) {
                    room.remote_shards.swap_remove(&from_shard_id);
                    if room.members.is_empty() && room.remote_shards.is_empty() {
                        self.rooms.remove(&room_id);
                    }
                }
            }
            CrossShardEvent::UdpPacket {
                participant_id,
                batch,
            } => {
                if let Some(participant) = self.participants.get_mut(&participant_id) {
                    participant.on_ingress(batch);
                    self.input_dirty.insert(participant_id);
                }
            }
            CrossShardEvent::KeyframeRequested(req) => {
                if let Some(p) = self.participants.get_mut(&req.origin) {
                    let _guard = p.span.enter();
                    p.core
                        .handle_remote_keyframe_request(req.stream_id, req.kind);
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
        let mut participant = ParticipantCore::new(
            cfg,
            self.shard_id,
            self.max_gso_segments,
            1,
            &mut participant_rng,
        );
        let ufrag = participant.ufrag();
        let meta = ParticipantMeta {
            core: participant,
            span,
        };
        self.demuxer
            .register_ice_ufrag(ufrag.as_bytes(), participant_id);
        self.participants.insert(participant_id, meta);
        tracing::info!(%participant_id, "participant added to shard");

        let room = self
            .rooms
            .entry(room_id)
            .or_insert_with(|| RoomState::new(&mut self.rng));
        let was_empty = room.members.is_empty();
        room.members.insert(participant_id);
        if was_empty {
            let shard_id = self.shard_id;
            router.broadcast(|| CrossShardEvent::RoomMemberJoined {
                room_id,
                from_shard_id: shard_id,
            });
        }
        self.input_dirty.insert(participant_id);
    }

    fn remove_participant(
        &mut self,
        participant_id: &ParticipantId,
        router: &impl CrossShardSend,
    ) -> Option<ParticipantMeta> {
        let mut participant = self.participants.remove(participant_id)?;
        if let Some(room) = self.rooms.get_mut(&participant.room_id) {
            room.members.swap_remove(participant_id);
            if room.members.is_empty() {
                let room_id = participant.room_id;
                let shard_id = self.shard_id;
                router.broadcast(|| CrossShardEvent::RoomMemberLeft {
                    room_id,
                    from_shard_id: shard_id,
                });
                if room.remote_shards.is_empty() {
                    self.rooms.remove(&participant.room_id);
                }
            }
        }
        // Evict audio tracks from room selector.
        if let Some(room) = self.rooms.get_mut(&participant.room_id) {
            let audio_ids: Vec<_> = participant.upstream.audio_track_ids().collect();
            for id in audio_ids {
                room.audio_selector.remove_track((id, None));
            }
        }
        let unsubs = participant.downstream.unsubscribe_all();
        for (stream_id, shard_id) in unsubs {
            handle_participant_topology(
                TopologyEvent::StreamUnsubscribed {
                    shard_id,
                    participant_id: *participant_id,
                    stream_id,
                },
                &mut self.routing,
                router,
            );
        }
        let addrs = self.demuxer.unregister(participant.ufrag().as_bytes());
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
        let _guard = participant.span.enter();
        participant.core.poll(now, &mut queue);
    }
}

pub(super) fn handle_rtp(
    stream_id: StreamId,
    pkt: &RtpPacket,
    routing: &HashMap<StreamId, Routing>,
    participants: &mut HashMap<ParticipantId, ParticipantMeta>,
    dirty: &mut IndexSet<ParticipantId, ahash::RandomState>,
    router: &impl CrossShardSend,
) -> Option<()> {
    let route = routing.get(&stream_id)?;
    match route.kind {
        MediaKind::Video => {
            for participant_id in &route.subscribers {
                let Some(sub) = participants.get_mut(participant_id) else {
                    continue;
                };
                let mut writer = StreamWriter(&mut sub.core.rtc);
                sub.core
                    .downstream
                    .on_forward_rtp(&stream_id, pkt, &mut writer);
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
    let Some(room) = rooms.get_mut(&ev.room_id) else {
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
    for &participant_id in &room.members {
        if participant_id == ev.origin {
            continue;
        }
        let Some(sub) = participants.get_mut(&participant_id) else {
            continue;
        };
        let mut writer = StreamWriter(&mut sub.core.rtc);
        sub.core
            .downstream
            .on_forward_audio_rtp(slot_idx, &ev.pkt, &mut writer);
        dirty.insert(participant_id);
    }
}

pub(super) fn handle_participant_topology(
    ev: TopologyEvent,
    routing: &mut HashMap<StreamId, Routing>,
    router: &impl CrossShardSend,
) -> Option<()> {
    match ev {
        TopologyEvent::StreamSubscribed {
            shard_id,
            participant_id,
            stream_id,
            kind,
        } => {
            let entry = routing.entry(stream_id).or_insert_with(|| Routing {
                kind,
                subscribers: IndexSet::with_capacity(256),
                remote_shards: IndexSet::new(),
            });
            let was_empty = entry.subscribers.is_empty();
            entry.subscribers.insert(participant_id);
            if was_empty {
                router.send(
                    shard_id,
                    CrossShardEvent::StreamSubscribed {
                        stream_id,
                        kind,
                        from_shard_id: router.shard_id(),
                    },
                );
            }
        }
        TopologyEvent::StreamUnsubscribed {
            shard_id,
            participant_id,
            stream_id,
        } => {
            if let Some(entry) = routing.get_mut(&stream_id) {
                entry.subscribers.swap_remove(&participant_id);
                if entry.subscribers.is_empty() {
                    router.send(
                        shard_id,
                        CrossShardEvent::StreamUnsubscribed {
                            stream_id,
                            from_shard_id: router.shard_id(),
                        },
                    );
                    if entry.remote_shards.is_empty() {
                        routing.remove(&stream_id);
                    }
                }
            }
        }
    }

    Some(())
}

pub(super) fn handle_participant_control(
    ev: ControlEvent,
    shard_events: &mut VecDeque<ShardEvent>,
    router: &impl CrossShardSend,
) {
    match ev {
        ControlEvent::TrackPublished(track) => {
            shard_events.push_back(ShardEvent::TrackPublished(track));
        }
        ControlEvent::KeyframeRequested(req) => {
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
        entity::ExternalRoomId, participant::event::TopologyEvent, track::GlobalKeyframeRequest,
    };
    use str0m::media::{KeyframeRequestKind, MediaKind};

    use super::*;

    struct TestRouter {
        shard_id: usize,
        shard_count: usize,
        sent: RefCell<Vec<(usize, CrossShardEvent)>>,
    }

    impl TestRouter {
        fn new(shard_id: usize, shard_count: usize) -> Self {
            Self {
                shard_id,
                shard_count,
                sent: RefCell::new(Vec::new()),
            }
        }

        fn take_sent(&self) -> Vec<(usize, CrossShardEvent)> {
            std::mem::take(&mut *self.sent.borrow_mut())
        }
    }

    impl CrossShardSend for TestRouter {
        fn send(&self, shard_id: usize, ev: CrossShardEvent) {
            self.sent.borrow_mut().push((shard_id, ev));
        }

        fn broadcast<F: Fn() -> CrossShardEvent>(&self, make_ev: F) {
            let mut sent = self.sent.borrow_mut();
            for i in 0..self.shard_count {
                if i != self.shard_id {
                    sent.push((i, make_ev()));
                }
            }
        }

        fn shard_id(&self) -> usize {
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

    fn empty_routing(kind: MediaKind) -> Routing {
        Routing {
            kind,
            subscribers: IndexSet::new(),
            remote_shards: IndexSet::new(),
        }
    }

    #[test]
    fn topology_subscribe_creates_routing_entry() {
        let router = TestRouter::new(1, 3);
        let mut routing: HashMap<StreamId, Routing> = HashMap::default();
        let stream = video_stream(pid());
        let subscriber = pid();

        handle_participant_topology(
            TopologyEvent::StreamSubscribed {
                shard_id: 0,
                participant_id: subscriber,
                stream_id: stream,
                kind: MediaKind::Video,
            },
            &mut routing,
            &router,
        );

        assert!(routing.contains_key(&stream));
        assert!(routing[&stream].subscribers.contains(&subscriber));
    }

    #[test]
    fn topology_first_subscriber_notifies_publisher_shard() {
        let router = TestRouter::new(1, 3);
        let mut routing: HashMap<StreamId, Routing> = HashMap::default();
        let stream = video_stream(pid());

        handle_participant_topology(
            TopologyEvent::StreamSubscribed {
                shard_id: 0,
                participant_id: pid(),
                stream_id: stream,
                kind: MediaKind::Video,
            },
            &mut routing,
            &router,
        );

        let sent = router.take_sent();
        assert_eq!(sent.len(), 1);
        let (target, ev) = &sent[0];
        assert_eq!(*target, 0);
        assert!(
            matches!(
                ev,
                CrossShardEvent::StreamSubscribed {
                    from_shard_id: 1,
                    ..
                }
            ),
            "first subscriber must send StreamSubscribed to publisher shard"
        );
    }

    #[test]
    fn topology_second_subscriber_does_not_notify_publisher() {
        let router = TestRouter::new(1, 3);
        let mut routing: HashMap<StreamId, Routing> = HashMap::default();
        let stream = video_stream(pid());

        handle_participant_topology(
            TopologyEvent::StreamSubscribed {
                shard_id: 0,
                participant_id: pid(),
                stream_id: stream,
                kind: MediaKind::Video,
            },
            &mut routing,
            &router,
        );
        router.take_sent();

        handle_participant_topology(
            TopologyEvent::StreamSubscribed {
                shard_id: 0,
                participant_id: pid(),
                stream_id: stream,
                kind: MediaKind::Video,
            },
            &mut routing,
            &router,
        );

        assert!(
            router.take_sent().is_empty(),
            "second subscriber must not re-notify the publisher"
        );
        assert_eq!(routing[&stream].subscribers.len(), 2);
    }

    #[test]
    fn topology_last_unsubscribe_notifies_publisher_and_removes_entry() {
        let router = TestRouter::new(1, 3);
        let mut routing: HashMap<StreamId, Routing> = HashMap::default();
        let stream = video_stream(pid());
        let subscriber = pid();

        handle_participant_topology(
            TopologyEvent::StreamSubscribed {
                shard_id: 0,
                participant_id: subscriber,
                stream_id: stream,
                kind: MediaKind::Video,
            },
            &mut routing,
            &router,
        );
        router.take_sent();

        handle_participant_topology(
            TopologyEvent::StreamUnsubscribed {
                shard_id: 0,
                participant_id: subscriber,
                stream_id: stream,
            },
            &mut routing,
            &router,
        );

        let sent = router.take_sent();
        assert_eq!(
            sent.len(),
            1,
            "must send exactly one unsubscribe notification"
        );
        assert!(
            matches!(
                &sent[0].1,
                CrossShardEvent::StreamUnsubscribed {
                    from_shard_id: 1,
                    ..
                }
            ),
            "last subscriber must send StreamUnsubscribed to publisher shard"
        );
        assert!(
            !routing.contains_key(&stream),
            "entry must be removed when no subscribers and no remote shards remain"
        );
    }

    #[test]
    fn topology_unsubscribe_with_remaining_subscriber_does_not_notify() {
        let router = TestRouter::new(1, 3);
        let mut routing: HashMap<StreamId, Routing> = HashMap::default();
        let stream = video_stream(pid());
        let s1 = pid();
        let s2 = pid();

        for s in [s1, s2] {
            handle_participant_topology(
                TopologyEvent::StreamSubscribed {
                    shard_id: 0,
                    participant_id: s,
                    stream_id: stream,
                    kind: MediaKind::Video,
                },
                &mut routing,
                &router,
            );
        }
        router.take_sent();

        handle_participant_topology(
            TopologyEvent::StreamUnsubscribed {
                shard_id: 0,
                participant_id: s1,
                stream_id: stream,
            },
            &mut routing,
            &router,
        );

        assert!(
            router.take_sent().is_empty(),
            "must not notify while a local subscriber remains"
        );
        assert_eq!(routing[&stream].subscribers.len(), 1);
    }

    #[test]
    fn topology_unsubscribe_keeps_entry_while_remote_shards_remain() {
        let router = TestRouter::new(1, 3);
        let mut routing: HashMap<StreamId, Routing> = HashMap::default();
        let stream = video_stream(pid());
        let subscriber = pid();

        handle_participant_topology(
            TopologyEvent::StreamSubscribed {
                shard_id: 0,
                participant_id: subscriber,
                stream_id: stream,
                kind: MediaKind::Video,
            },
            &mut routing,
            &router,
        );
        routing.get_mut(&stream).unwrap().remote_shards.insert(2);
        router.take_sent();

        handle_participant_topology(
            TopologyEvent::StreamUnsubscribed {
                shard_id: 0,
                participant_id: subscriber,
                stream_id: stream,
            },
            &mut routing,
            &router,
        );

        assert!(
            routing.contains_key(&stream),
            "entry must remain while remote shards are subscribed"
        );
    }

    #[test]
    fn handle_rtp_sends_to_all_remote_shards() {
        let router = TestRouter::new(0, 3);
        let stream = video_stream(pid());
        let mut routing: HashMap<StreamId, Routing> = HashMap::default();
        let mut route = empty_routing(MediaKind::Video);
        route.remote_shards.insert(1);
        route.remote_shards.insert(2);
        routing.insert(stream, route);

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
        let targets: Vec<usize> = sent.iter().map(|(id, _)| *id).collect();
        assert!(targets.contains(&1));
        assert!(targets.contains(&2));
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
        let mut routing: HashMap<StreamId, Routing> = HashMap::default();
        routing.insert(stream, empty_routing(MediaKind::Video)); // no remote_shards

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
        let routing: HashMap<StreamId, Routing> = HashMap::default();

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
        room.remote_shards.insert(1);
        room.remote_shards.insert(2);
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
    fn cross_shard_stream_subscribed_adds_remote_shard_to_routing() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let stream = video_stream(pid());

        core.pending_cross_shard
            .push_back(CrossShardEvent::StreamSubscribed {
                stream_id: stream,
                kind: MediaKind::Video,
                from_shard_id: 2,
            });
        core.flush_cross_shard(Instant::now(), &router);

        assert!(core.routing.contains_key(&stream));
        assert!(core.routing[&stream].remote_shards.contains(&2));
    }

    #[test]
    fn cross_shard_stream_unsubscribed_removes_entry_when_empty() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let stream = video_stream(pid());

        for ev in [
            CrossShardEvent::StreamSubscribed {
                stream_id: stream,
                kind: MediaKind::Video,
                from_shard_id: 2,
            },
            CrossShardEvent::StreamUnsubscribed {
                stream_id: stream,
                from_shard_id: 2,
            },
        ] {
            core.pending_cross_shard.push_back(ev);
        }
        core.flush_cross_shard(Instant::now(), &router);

        assert!(
            !core.routing.contains_key(&stream),
            "routing entry must be removed when no subscribers and no remote shards remain"
        );
    }

    #[test]
    fn cross_shard_stream_unsubscribed_keeps_entry_with_local_subscribers() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let stream = video_stream(pid());
        let local_sub = pid();

        core.routing.insert(
            stream,
            Routing {
                kind: MediaKind::Video,
                subscribers: {
                    let mut s = IndexSet::new();
                    s.insert(local_sub);
                    s
                },
                remote_shards: {
                    let mut s = IndexSet::new();
                    s.insert(2usize);
                    s
                },
            },
        );

        core.pending_cross_shard
            .push_back(CrossShardEvent::StreamUnsubscribed {
                stream_id: stream,
                from_shard_id: 2,
            });
        core.flush_cross_shard(Instant::now(), &router);

        assert!(
            core.routing.contains_key(&stream),
            "entry must remain while a local subscriber exists"
        );
        assert!(core.routing[&stream].remote_shards.is_empty());
    }

    #[test]
    fn cross_shard_room_member_joined_registers_remote_shard() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let rid = room_id("r1");

        core.pending_cross_shard
            .push_back(CrossShardEvent::RoomMemberJoined {
                room_id: rid,
                from_shard_id: 1,
            });
        core.flush_cross_shard(Instant::now(), &router);

        assert!(core.rooms.contains_key(&rid));
        assert!(core.rooms[&rid].remote_shards.contains(&1));
    }

    #[test]
    fn cross_shard_room_member_left_removes_room_when_empty() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let rid = room_id("r2");

        for ev in [
            CrossShardEvent::RoomMemberJoined {
                room_id: rid,
                from_shard_id: 1,
            },
            CrossShardEvent::RoomMemberLeft {
                room_id: rid,
                from_shard_id: 1,
            },
        ] {
            core.pending_cross_shard.push_back(ev);
        }
        core.flush_cross_shard(Instant::now(), &router);

        assert!(
            !core.rooms.contains_key(&rid),
            "empty room (no local members, no remote shards) must be removed"
        );
    }

    #[test]
    fn cross_shard_room_member_left_keeps_room_with_local_members() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let rid = room_id("r3");

        let mut room = RoomState::new(&mut pulsebeam_runtime::rand::seeded_rng(42));
        room.members.insert(pid());
        room.remote_shards.insert(2);
        core.rooms.insert(rid, room);

        core.pending_cross_shard
            .push_back(CrossShardEvent::RoomMemberLeft {
                room_id: rid,
                from_shard_id: 2,
            });
        core.flush_cross_shard(Instant::now(), &router);

        assert!(
            core.rooms.contains_key(&rid),
            "room must remain while local members exist"
        );
        assert!(core.rooms[&rid].remote_shards.is_empty());
    }

    #[test]
    fn register_remote_participant_populates_shard_and_ufrag_maps() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let participant = pid();

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: 2,
                participant_id: participant,
                ufrag: "testufrag".to_string(),
            }),
            &router,
        );

        assert_eq!(core.participant_shards.get(&participant), Some(&2));
        assert!(core.remote_participant_ufrags.contains_key(&participant));
    }

    #[test]
    fn register_local_shard_participant_is_no_op() {
        let router = TestRouter::new(0, 3);
        let mut core = ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42));
        let participant = pid();

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: 0,
                participant_id: participant,
                ufrag: "localufrag".to_string(),
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

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: 1,
                participant_id: participant,
                ufrag: "removeme".to_string(),
            }),
            &router,
        );

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnregisterParticipant {
                participant_id: participant,
            }),
            &router,
        );

        assert!(!core.participant_shards.contains_key(&participant));
        assert!(!core.remote_participant_ufrags.contains_key(&participant));
        assert!(core.pending_close.is_empty());
    }

    #[test]
    fn control_local_keyframe_request_pushes_shard_event() {
        let router = TestRouter::new(1, 3);
        let mut shard_events: VecDeque<ShardEvent> = VecDeque::new();

        handle_participant_control(
            ControlEvent::KeyframeRequested(GlobalKeyframeRequest {
                shard_id: 1, // same shard → local
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
            ControlEvent::KeyframeRequested(GlobalKeyframeRequest {
                shard_id: 0, // different shard → remote
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
        assert_eq!(sent[0].0, 0, "must be directed to the publisher's shard");
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

    fn make_video_track(origin: ParticipantId, shard_id: usize) -> crate::track::Track {
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
        // Keep the room alive after the last local member leaves so we can
        // inspect room state afterwards.
        core.rooms.get_mut(&r).unwrap().remote_shards.insert(1);

        core.remove_participant(&p, &router);

        let sent = router.take_sent();
        assert_eq!(
            sent.len(),
            2,
            "must broadcast RoomMemberLeft to all other shards"
        );
        for (_, ev) in &sent {
            assert!(
                matches!(ev, CrossShardEvent::RoomMemberLeft { .. }),
                "broadcast must be RoomMemberLeft"
            );
        }
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
        core.rooms.get_mut(&r).unwrap().remote_shards.insert(2);

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
            core.rooms[&r].remote_shards.contains(&2),
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

        core.events
            .push_back(ParticipantEvent::Lifecycle(LifecycleEvent::Exited {
                participant_id: p,
            }));
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
            TopologyEvent::StreamSubscribed {
                shard_id: 1,
                participant_id: subscriber,
                stream_id: stream,
                kind: MediaKind::Video,
            },
            &mut core.routing,
            &router,
        );
        router.take_sent();

        assert!(core.routing[&stream].subscribers.contains(&subscriber));

        // Simulate the downstream emitting StreamUnsubscribed on cleanup, as it does
        // when participant.downstream.unsubscribe_all() returns routes and
        // flush_participant_events processes them.
        core.events.push_back(ParticipantEvent::Topology(
            TopologyEvent::StreamUnsubscribed {
                shard_id: 1,
                participant_id: subscriber,
                stream_id: stream,
            },
        ));
        core.flush_participant_events(&router);

        assert!(
            !core.routing.contains_key(&stream),
            "routing entry must be cleaned up when subscriber unsubscribes"
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
        let a_track = make_video_track(a, 1);
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

        let a_track = make_video_track(a, 1);
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
}

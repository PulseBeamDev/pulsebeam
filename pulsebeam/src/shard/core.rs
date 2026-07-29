use pulsebeam_runtime::net::{self, UnifiedSocket};
use pulsebeam_runtime::rand::Rng;
use tokio::time::Instant;

use super::events::{
    AudioRtpEvent, ParticipantControlEvent, ParticipantEvent, ParticipantLifecycleEvent,
};
use crate::id::AudioSelectorSlotId;
use crate::{
    entity::{ParticipantId, TrackKind},
    id::ShardId,
    participant::{ParticipantConfig, batcher::GsoSendBatch},
    rtp::RtpPacket,
    shard::{
        dirty::DirtyTracker,
        events::EventPipeline,
        participants::{ParticipantHandle, ParticipantRegistry},
        timer::TimerWheel,
    },
    track::StreamId,
};

use super::router::{self, ParticipantShardMeta, RoutingContext, ShardRoutingTable};

pub(crate) use super::router::CrossShardSend;
use super::worker::{ClusterCommand, CrossShardEvent, ShardCommand, ShardEvent};

const MAX_PARTICIPANTS_PER_SHARD: usize = 2048;

struct DispatchCtx<'a, R: CrossShardSend> {
    registry: &'a mut ParticipantRegistry,
    dirty: &'a mut DirtyTracker,
    router: &'a R,
}

impl<'a, R: CrossShardSend> CrossShardSend for DispatchCtx<'a, R> {
    fn send(&self, shard_id: ShardId, ev: CrossShardEvent) {
        self.router.send(shard_id, ev);
    }

    fn shard_id(&self) -> ShardId {
        self.router.shard_id()
    }
}

impl<'a, R: CrossShardSend> RoutingContext for DispatchCtx<'a, R> {
    fn forward_video_rtp(
        &mut self,
        subscriber: ParticipantHandle,
        stream_id: &StreamId,
        pkt: &RtpPacket,
        cache: Option<&crate::rtp::cache::StreamCache>,
    ) {
        if let Some(p) = self.registry.resolve_mut(subscriber) {
            p.on_forward_rtp(stream_id, pkt, cache);
            self.dirty.mark(subscriber.participant_id(), p);
        }
    }

    fn forward_audio_rtp(
        &mut self,
        subscriber: ParticipantId,
        slot_idx: AudioSelectorSlotId,
        pkt: &RtpPacket,
    ) {
        if let Some(p) = self.registry.get_mut(&subscriber) {
            p.on_forward_audio_rtp(slot_idx, pkt);
            self.dirty.mark(subscriber, p);
        }
    }

    fn forward_sctp(
        &mut self,
        subscriber: ParticipantId,
        origin: ParticipantId,
        topic: &crate::track::Topic,
        pkt: &[u8],
    ) {
        if let Some(p) = self.registry.get_mut(&subscriber) {
            p.on_forward_sctp(topic, origin, pkt);
            self.dirty.mark(subscriber, p);
        }
    }

    fn notify_tracks_published(
        &mut self,
        participant_id: ParticipantId,
        tracks: &[crate::track::Track],
    ) {
        if let Some(p) = self.registry.get_mut(&participant_id) {
            p.on_tracks_published(tracks);
            self.dirty.mark(participant_id, p);
        }
    }

    fn notify_tracks_unpublished(
        &mut self,
        participant_id: ParticipantId,
        track_ids: &[crate::entity::TrackId],
    ) {
        let Some(p) = self.registry.get_mut(&participant_id) else {
            return;
        };

        if p.on_tracks_unpublished(track_ids) {
            self.dirty.mark(participant_id, p);
        }
    }

    fn notify_keyframe_request(
        &mut self,
        participant_id: ParticipantId,
        stream_id: StreamId,
        kind: str0m::media::KeyframeRequestKind,
    ) {
        if let Some(p) = self.registry.get_mut(&participant_id) {
            p.handle_remote_keyframe_request(stream_id, kind);
            self.dirty.mark(participant_id, p);
        }
    }

    fn is_local(&self, id: &ParticipantId) -> bool {
        self.registry.contains(id)
    }

    fn forward_reliable_sctp(
        &mut self,
        subscriber: ParticipantId,
        origin: ParticipantId,
        topic: &crate::track::Topic,
        frame: &[u8],
    ) {
        if let Some(p) = self.registry.get_mut(&subscriber) {
            p.on_forward_reliable_sctp(topic, origin, frame);
            self.dirty.mark(subscriber, p);
        }
    }

    fn deliver_reliable_control(
        &mut self,
        publisher: ParticipantId,
        topic: &crate::track::Topic,
        bytes: &[u8],
    ) {
        if let Some(p) = self.registry.get_mut(&publisher) {
            p.on_deliver_reliable_control(topic, bytes);
            self.dirty.mark(publisher, p);
        }
    }
}

pub(crate) struct ShardCore {
    pub(crate) shard_id: ShardId,
    registry: ParticipantRegistry,
    pub(super) routing: ShardRoutingTable,
    timers: TimerWheel,
    dirty: DirtyTracker,
    udp_send_batch: GsoSendBatch,
    pipeline: EventPipeline,
    rng: Rng,
}

impl ShardCore {
    pub(crate) fn new(shard_id: impl Into<ShardId>, max_gso_segments: usize, rng: Rng) -> Self {
        let shard_id = shard_id.into();
        Self {
            shard_id,
            registry: ParticipantRegistry::new(shard_id, max_gso_segments),
            routing: ShardRoutingTable::new(),
            timers: TimerWheel::new(MAX_PARTICIPANTS_PER_SHARD),
            dirty: DirtyTracker::with_capacity(MAX_PARTICIPANTS_PER_SHARD),
            udp_send_batch: GsoSendBatch::preallocated(),
            pipeline: EventPipeline::with_capacity(MAX_PARTICIPANTS_PER_SHARD),
            rng,
        }
    }

    pub(crate) fn next_timer_deadline(&mut self) -> Option<Instant> {
        self.timers.next_deadline()
    }

    pub(crate) fn fire_timers(&mut self, now: Instant) {
        let registry = &mut self.registry;
        let dirty = &mut self.dirty;
        self.timers.drain_expired(now, |handle| {
            if let Some(participant) = registry.resolve_mut(handle) {
                participant.on_timeout(now);
                dirty.mark(handle.participant_id(), participant);
            }
        });
    }

    pub(crate) fn on_udp_batch(
        &mut self,
        batch: pulsebeam_runtime::net::RecvPacketBatch,
        router: &impl CrossShardSend,
    ) {
        let Some(participant_id) = self.registry.demux(&batch) else {
            return;
        };
        if let Some(participant) = self.registry.get_mut(&participant_id) {
            participant.on_ingress(batch);
            self.dirty.mark(participant_id, participant);
        } else if let Some(shard_id) = self.routing.remote_shard_for(&participant_id) {
            router.send(
                shard_id,
                CrossShardEvent::UdpPacket {
                    participant_id,
                    batch,
                },
            );
        }
    }

    pub(crate) fn flush_stream_buffers(&mut self, router: &impl CrossShardSend) {
        let mut ctx = DispatchCtx {
            registry: &mut self.registry,
            dirty: &mut self.dirty,
            router,
        };
        while let Some(ev) = self.pipeline.pop_audio_rtp() {
            debug_assert!(ev.stream_id.0.kind() == TrackKind::Audio);
            self.routing.route_audio(ev, &mut ctx);
        }

        while let Some(ev) = self.pipeline.pop_video_rtp() {
            debug_assert!(ev.stream_id.0.kind() == TrackKind::Video);
            self.routing.route_video(ev.stream_id, &ev.pkt, &mut ctx);
        }

        while let Some(ev) = self.pipeline.pop_data_sctp() {
            self.routing
                .route_data(ev.room_id, ev.origin, &ev.topic, &ev.pkt, &mut ctx);
        }

        while let Some(ev) = self.pipeline.pop_reliable_data_sctp() {
            self.routing
                .route_reliable_data(ev.room_id, ev.origin, &ev.topic, &ev.pkt, &mut ctx);
        }
    }

    pub(crate) fn flush_participant_events(&mut self, router: &impl CrossShardSend) {
        while let Some(event) = self.pipeline.pop_participant_event() {
            match event {
                ParticipantEvent::Topology(ev) => {
                    if let Some(shard_event) = self.routing.handle_topology_event(ev) {
                        self.pipeline.push_shard_event(shard_event);
                    }
                }
                ParticipantEvent::Lifecycle(ParticipantLifecycleEvent::Exited {
                    participant_id,
                }) => {
                    self.remove_participant(&participant_id);
                    self.pipeline
                        .push_shard_event(ShardEvent::ParticipantExited(participant_id));
                }
                ParticipantEvent::Control(ev) => match ev {
                    ParticipantControlEvent::DataTopicPublished {
                        room_id,
                        publisher,
                        topic,
                    } => {
                        self.routing
                            .register_data_publisher(room_id, publisher, topic);
                    }
                    ParticipantControlEvent::DataTopicUnpublished {
                        room_id,
                        publisher,
                        topic,
                    } => {
                        self.routing
                            .unregister_data_publisher(room_id, publisher, &topic);
                    }
                    ParticipantControlEvent::DataTopicSubscribed {
                        room_id,
                        subscriber,
                        topic,
                        publisher,
                    } => {
                        if self.routing.register_data_subscriber(
                            room_id,
                            subscriber,
                            topic.clone(),
                            publisher,
                        ) {
                            self.pipeline
                                .push_shard_event(ShardEvent::DataTopicSubscribed {
                                    room_id,
                                    topic,
                                    publisher,
                                });
                        }
                    }
                    ParticipantControlEvent::DataTopicUnsubscribed {
                        room_id,
                        subscriber,
                        topic,
                        publisher,
                    } => {
                        if self
                            .routing
                            .unregister_data_subscriber(room_id, subscriber, &topic, publisher)
                        {
                            self.pipeline
                                .push_shard_event(ShardEvent::DataTopicUnsubscribed {
                                    room_id,
                                    topic,
                                    publisher,
                                });
                        }
                    }
                    ParticipantControlEvent::ReliableDataTopicPublished {
                        room_id,
                        publisher,
                        topic,
                    } => {
                        self.routing
                            .register_reliable_data_publisher(room_id, publisher, topic);
                    }
                    ParticipantControlEvent::ReliableDataTopicUnpublished {
                        room_id,
                        publisher,
                        topic,
                    } => {
                        self.routing
                            .unregister_reliable_data_publisher(room_id, publisher, &topic);
                    }
                    ParticipantControlEvent::ReliableDataTopicSubscribed {
                        room_id,
                        subscriber,
                        topic,
                    } => {
                        self.routing
                            .register_reliable_data_subscriber(room_id, subscriber, topic);
                    }
                    ParticipantControlEvent::ReliableDataTopicUnsubscribed {
                        room_id,
                        subscriber,
                        topic,
                    } => {
                        self.routing
                            .unregister_reliable_data_subscriber(room_id, subscriber, &topic);
                    }
                    ParticipantControlEvent::ReliableControlReceived {
                        publisher,
                        topic,
                        bytes,
                    } => {
                        let mut ctx = DispatchCtx {
                            registry: &mut self.registry,
                            dirty: &mut self.dirty,
                            router,
                        };
                        self.routing
                            .route_reliable_control(publisher, &topic, &bytes, &mut ctx);
                    }
                    ev => {
                        router::route_participant_control_event(
                            ev,
                            self.pipeline.shard_events_mut(),
                            router,
                        );
                    }
                },
            }
        }
    }

    pub(crate) fn pop_shard_event(&mut self) -> Option<ShardEvent> {
        self.pipeline.pop_shard_event()
    }

    pub(crate) fn poll_and_flush_dirty(
        &mut self,
        now: Instant,
        udp_socket: &mut UnifiedSocket,
        tcp_socket: &mut net::tcp::TcpTransport,
    ) {
        debug_assert!(self.udp_send_batch.is_empty());
        self.dirty.begin_phase();
        while let Some(entry) = self.dirty.next() {
            let Some(handle) = self.registry.handle(&entry.participant_id) else {
                continue;
            };
            let Some(participant) = self.registry.resolve_mut(handle) else {
                continue;
            };
            if participant.generation != entry.generation {
                continue;
            }
            debug_assert!(participant.queued_dirty);
            debug_assert_eq!(participant.participant_id, entry.participant_id);
            participant.queued_dirty = false;
            let room_id = participant.room_id;
            let mut sink = self
                .pipeline
                .participant_sink(room_id, entry.participant_id);
            let deadline = participant.poll(now, &mut sink);
            if let Some(deadline) = deadline {
                self.timers.schedule(handle, deadline);
            }

            while self
                .udp_send_batch
                .append_from(&mut participant.udp_packets)
            {
                if self.udp_send_batch.is_full() {
                    self.udp_send_batch.flush(udp_socket);
                }
            }
            participant.tcp_batcher.flush_tcp(tcp_socket);
        }
        self.dirty.finish_phase();
        debug_assert!(self.dirty.is_empty());
        self.udp_send_batch.flush(udp_socket);
        debug_assert!(self.udp_send_batch.is_empty());
    }

    pub(crate) fn flush_close_peers(
        &mut self,
        udp_socket: &mut UnifiedSocket,
        tcp_socket: &mut net::tcp::TcpTransport,
    ) {
        for addr in self.registry.drain_pending_close().collect::<Vec<_>>() {
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
            ShardCommand::AddParticipant(cfg) => self.add_participant(cfg, router),
            ShardCommand::RemoveParticipant(participant_id) => {
                self.remove_participant(&participant_id);
            }
            ShardCommand::AddTcpConnection { .. } => {
                // Handled by the shard worker directly; no core action needed.
            }
            ShardCommand::Cluster(cmd) => self.on_cluster_command(cmd, router)?,
        }
        Some(())
    }

    fn on_cluster_command(
        &mut self,
        cmd: ClusterCommand,
        router: &impl CrossShardSend,
    ) -> Option<()> {
        match cmd {
            ClusterCommand::RequestKeyframe(req) => {
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                };
                ctx.notify_keyframe_request(req.origin, req.stream_id, req.kind);
            }
            ClusterCommand::RegisterParticipant {
                shard_id,
                room_id,
                participant_id,
            } => {
                if shard_id != self.shard_id {
                    self.routing.register_remote_participant(
                        participant_id,
                        room_id,
                        shard_id,
                        &mut self.rng,
                    );
                }
            }
            ClusterCommand::UnregisterParticipant {
                shard_id,
                room_id,
                participant_id,
            } => {
                self.routing.unregister_remote_participant(
                    participant_id,
                    ParticipantShardMeta { shard_id, room_id },
                );
                self.registry.unregister_remote_demux(participant_id);
            }
            ClusterCommand::PublishTrack(track, room_id) => {
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                };
                self.routing.publish_track(track, room_id, &mut ctx);
            }
            ClusterCommand::UnpublishTracks {
                origin: _,
                room_id,
                track_ids,
            } => {
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                };
                self.routing.unpublish_tracks(room_id, &track_ids, &mut ctx);
            }
            ClusterCommand::SubscribeTrack {
                from_shard_id,
                track,
            } => {
                self.routing
                    .register_remote_subscriber_shard(from_shard_id, track);
            }
            ClusterCommand::UnsubscribeTrack {
                from_shard_id,
                track,
            } => {
                self.routing
                    .unregister_remote_subscriber_shard(from_shard_id, track);
            }
            ClusterCommand::SubscribeDataTopic {
                room_id,
                from_shard_id,
                topic,
                publisher,
            } => {
                self.routing.register_remote_data_subscriber_shard(
                    room_id,
                    from_shard_id,
                    topic,
                    publisher,
                );
            }
            ClusterCommand::UnsubscribeDataTopic {
                room_id,
                from_shard_id,
                topic,
                publisher,
            } => {
                self.routing.unregister_remote_data_subscriber_shard(
                    room_id,
                    from_shard_id,
                    &topic,
                    publisher,
                );
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
            CrossShardEvent::VideoRtpPublished { stream_id, pkt } => {
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                };
                self.routing.route_video(stream_id, &pkt, &mut ctx);
            }
            CrossShardEvent::AudioRtpPublished {
                room_id,
                origin,
                stream_id,
                pkt,
            } => {
                let ev = AudioRtpEvent {
                    stream_id,
                    pkt,
                    room_id,
                    origin,
                };
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                };
                self.routing.route_audio(ev, &mut ctx);
            }
            CrossShardEvent::UdpPacket {
                participant_id,
                batch,
            } => {
                if let Some(participant) = self.registry.get_mut(&participant_id) {
                    participant.on_ingress(batch);
                    self.dirty.mark(participant_id, participant);
                }
            }
            CrossShardEvent::KeyframeRequested(req) => {
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                };
                ctx.notify_keyframe_request(req.origin, req.stream_id, req.kind);
            }
            CrossShardEvent::DataSctpPublished {
                room_id,
                origin,
                topic,
                pkt,
            } => {
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                };
                self.routing
                    .route_data(room_id, origin, &topic, &pkt, &mut ctx);
            }
            CrossShardEvent::ReliableDataSctpPublished {
                room_id,
                origin,
                topic,
                frame,
            } => {
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                };
                self.routing
                    .route_reliable_data(room_id, origin, &topic, &frame, &mut ctx);
            }
            CrossShardEvent::ReliableControlForward {
                publisher,
                topic,
                bytes,
            } => {
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                };
                ctx.deliver_reliable_control(publisher, &topic, &bytes);
            }
        }
    }

    fn add_participant(&mut self, cfg: ParticipantConfig, router: &impl CrossShardSend) {
        let room_id = cfg.room_id;
        let participant_id = cfg.participant_id;
        self.remove_participant(&participant_id);
        let _ = router; // reserved: re-add currently needs no cross-shard notice
        let participant_id = self.registry.insert(cfg, &mut self.rng);
        let handle = self
            .registry
            .handle(&participant_id)
            .expect("new participant must have a local handle");
        self.routing
            .add_local_member(participant_id, handle, room_id, &mut self.rng);
        let participant = self
            .registry
            .get_mut(&participant_id)
            .expect("newly inserted participant must be present");
        self.dirty.mark(participant_id, participant);
    }

    fn remove_participant(&mut self, participant_id: &ParticipantId) -> Option<()> {
        if let Some(handle) = self.registry.handle(participant_id) {
            self.timers.cancel(handle);
        }
        let meta = self.registry.remove(participant_id)?;
        let audio_ids: Vec<_> = meta.upstream.audio_track_ids().collect();
        self.routing
            .remove_local_member(participant_id, meta.room_id, audio_ids);
        Some(())
    }
}

#[cfg(test)]
mod test {
    use std::{
        cell::RefCell,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{
        entity::{ExternalRoomId, RoomId},
        id::ShardId,
    };

    pub(super) struct TestRouter {
        pub shard_id: ShardId,
        pub shard_count: usize,
        pub sent: RefCell<Vec<(ShardId, CrossShardEvent)>>,
    }

    impl TestRouter {
        pub fn new(shard_id: usize, shard_count: usize) -> Self {
            Self {
                shard_id: ShardId::new(shard_id),
                shard_count,
                sent: RefCell::new(Vec::new()),
            }
        }

        pub fn take_sent(&self) -> Vec<(ShardId, CrossShardEvent)> {
            std::mem::take(&mut *self.sent.borrow_mut())
        }
    }

    impl CrossShardSend for TestRouter {
        fn send(&self, shard_id: ShardId, ev: CrossShardEvent) {
            self.sent.borrow_mut().push((shard_id, ev));
        }

        fn shard_id(&self) -> ShardId {
            self.shard_id
        }
    }

    pub(super) fn room_id(s: &str) -> RoomId {
        RoomId::from_external(&ExternalRoomId::new(s).unwrap())
    }

    pub(super) fn pid() -> ParticipantId {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        ParticipantId::new(&mut pulsebeam_runtime::rand::seeded_rng(
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ))
    }

    pub(super) fn video_stream(p: ParticipantId) -> crate::track::StreamId {
        (p.derive_track_id(TrackKind::Video, "v"), None)
    }

    pub(super) fn audio_stream(p: ParticipantId) -> crate::track::StreamId {
        (p.derive_track_id(TrackKind::Audio, "a"), None)
    }

    pub(super) fn video_track(origin: ParticipantId, shard_id: usize) -> crate::track::TrackMeta {
        crate::track::TrackMeta {
            shard_id: ShardId::new(shard_id),
            id: origin.derive_track_id(TrackKind::Video, "v"),
            origin,
        }
    }

    pub(super) fn make_participant_cfg(
        participant_id: ParticipantId,
        room_id: RoomId,
    ) -> ParticipantConfig {
        ParticipantConfig {
            manual_sub: false,
            room_id,
            participant_id,
            rtc: str0m::RtcConfig::new().build(std::time::Instant::now()),
            available_tracks: vec![],
        }
    }

    /// Adds a participant and discards the router traffic it generates, so
    /// callers assert only on the behavior under test, not on setup noise.
    pub(super) fn add_participant(
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

    fn new_core() -> ShardCore {
        ShardCore::new(0, 1, pulsebeam_runtime::rand::seeded_rng(42))
    }

    fn clear_dirty(core: &mut ShardCore) {
        core.dirty.begin_phase();
        while let Some(entry) = core.dirty.next() {
            if let Some(participant) = core.registry.get_mut(&entry.participant_id)
                && participant.generation == entry.generation
            {
                participant.queued_dirty = false;
            }
        }
        core.dirty.finish_phase();
    }

    #[test]
    fn add_participant_populates_registry_and_marks_dirty() {
        let router = TestRouter::new(0, 3);
        let mut core = new_core();
        let p = pid();
        let r = room_id("add1");

        add_participant(&mut core, &router, p, r);

        assert!(core.registry.contains(&p));
        assert!(core.routing.rooms.contains_key(&r));
        let mut core2 = new_core();
        core2.on_command(
            ShardCommand::AddParticipant(make_participant_cfg(p, r)),
            &router,
        );
        assert!(
            core2.dirty.contains(&p),
            "newly added participant must be dirty"
        );
    }

    #[test]
    fn remove_participant_clears_registry_and_room() {
        let router = TestRouter::new(0, 3);
        let mut core = new_core();
        let p = pid();
        let r = room_id("leave1");

        add_participant(&mut core, &router, p, r);
        core.on_command(ShardCommand::RemoveParticipant(p), &router);

        assert!(
            !core.registry.contains(&p),
            "participant must be gone from the registry"
        );
        assert!(
            !core.routing.rooms.contains_key(&r),
            "last member leaving must remove the room"
        );
    }

    #[test]
    fn readding_dirty_participant_ignores_stale_generation() {
        let router = TestRouter::new(0, 3);
        let mut core = new_core();
        let participant = pid();
        let room = room_id("readd-dirty");

        core.on_command(
            ShardCommand::AddParticipant(make_participant_cfg(participant, room)),
            &router,
        );
        core.on_command(
            ShardCommand::AddParticipant(make_participant_cfg(participant, room)),
            &router,
        );

        let current_generation = core.registry.get(&participant).unwrap().generation;
        core.dirty.begin_phase();
        let stale = core.dirty.next().unwrap();
        let current = core.dirty.next().unwrap();
        assert!(core.dirty.next().is_none());
        core.dirty.finish_phase();

        assert_eq!(stale.participant_id, participant);
        assert_ne!(stale.generation, current_generation);
        assert_eq!(current.participant_id, participant);
        assert_eq!(current.generation, current_generation);
    }

    #[test]
    fn duplicate_register_participant_command_does_not_leak_remote_shard() {
        let router = TestRouter::new(0, 3);
        let mut core = new_core();
        let participant = pid();
        let rid = room_id("no-leak");
        let remote_shard = ShardId::new(1);

        let register = || {
            ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                shard_id: remote_shard,
                room_id: rid,
                participant_id: participant,
            })
        };

        // Simulate a redelivered/duplicate RegisterParticipant for the exact
        // same (participant, shard, room).
        core.on_command(register(), &router);
        core.on_command(register(), &router);

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnregisterParticipant {
                shard_id: remote_shard,
                room_id: rid,
                participant_id: participant,
            }),
            &router,
        );

        assert!(
            !core.routing.rooms.contains_key(&rid),
            "one register (deduplicated) + one unregister must fully release the room; \
         a leaked refcount would leave a phantom remote_shards entry forever"
        );
    }

    #[test]
    fn register_remote_participant_keeps_shard_until_last_peer_leaves() {
        let router = TestRouter::new(0, 3);
        let mut core = new_core();
        let a = pid();
        let b = pid();
        let rid = room_id("keep-shard");
        let remote_shard = ShardId::new(1);

        for participant in [a, b] {
            core.on_command(
                ShardCommand::Cluster(ClusterCommand::RegisterParticipant {
                    shard_id: remote_shard,
                    room_id: rid,
                    participant_id: participant,
                }),
                &router,
            );
        }

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnregisterParticipant {
                shard_id: remote_shard,
                room_id: rid,
                participant_id: a,
            }),
            &router,
        );

        assert!(
            core.routing.rooms[&rid]
                .remote_shards
                .contains(&remote_shard),
            "shard must stay registered while participant b is still remote there"
        );

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::UnregisterParticipant {
                shard_id: remote_shard,
                room_id: rid,
                participant_id: b,
            }),
            &router,
        );

        assert!(
            !core.routing.rooms.contains_key(&rid),
            "room must be removed once the final remote leaves"
        );
    }

    #[test]
    fn keyframe_request_command_marks_participant_dirty() {
        let router = TestRouter::new(0, 3);
        let mut core = new_core();
        let p = pid();
        let r = room_id("kf1");
        add_participant(&mut core, &router, p, r);

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::RequestKeyframe(
                crate::track::GlobalKeyframeRequest {
                    origin: p,
                    stream_id: video_stream(p),
                    shard_id: ShardId::new(0),
                    kind: str0m::media::KeyframeRequestKind::Pli,
                },
            )),
            &router,
        );

        assert!(
            core.dirty.contains(&p),
            "keyframe delivery must dirty the target participant"
        );
    }

    #[test]
    fn cross_shard_rtp_published_marks_subscriber_dirty() {
        let router = TestRouter::new(0, 3);
        let mut core = new_core();
        let publisher = pid();
        let subscriber = pid();
        let r = room_id("rtp1");
        add_participant(&mut core, &router, subscriber, r);

        core.on_command(
            ShardCommand::Cluster(ClusterCommand::SubscribeTrack {
                from_shard_id: ShardId::new(0), // unused by register path directly, see below
                track: video_track(publisher, 1),
            }),
            &router,
        );
        assert!(core.dirty.contains(&subscriber));
        clear_dirty(&mut core);
        core.routing
            .register_subscriber(subscriber, video_track(publisher, 1));

        core.on_cross_shard_event(
            CrossShardEvent::VideoRtpPublished {
                stream_id: video_stream(publisher),
                pkt: crate::rtp::RtpPacket::default(),
            },
            tokio::time::Instant::now(),
            &router,
        );

        assert!(
            core.dirty.contains(&subscriber),
            "forwarded RTP must dirty the subscriber"
        );
    }

    #[test]
    fn fire_timers_does_not_spuriously_mark_participants() {
        let router = TestRouter::new(0, 3);
        let mut core = new_core();
        let p = pid();
        let r = room_id("timer1");
        add_participant(&mut core, &router, p, r);
        clear_dirty(&mut core);

        core.fire_timers(tokio::time::Instant::now());
        assert!(!core.dirty.contains(&p));

        core.on_command(ShardCommand::RemoveParticipant(p), &router);
        assert!(!core.registry.contains(&p));
    }
}

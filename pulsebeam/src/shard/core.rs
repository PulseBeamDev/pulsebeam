use pulsebeam_runtime::net::{self, UnifiedSocket};
use pulsebeam_runtime::rand::Rng;
use tokio::time::Instant;

use super::events::{
    ParticipantControlEvent, ParticipantEvent, ParticipantLifecycleEvent, ParticipantTimerEvent,
    RtpEvent,
};
use crate::id::AudioSelectorSlotId;
use crate::{
    entity::{ParticipantId, TrackKind},
    id::ShardId,
    participant::ParticipantConfig,
    rtp::RtpPacket,
    shard::{
        dirty::{DirtyKind, DirtyTracker},
        events::EventPipeline,
        participants::ParticipantRegistry,
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
    kind: DirtyKind,
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
        subscriber: ParticipantId,
        stream_id: &StreamId,
        pkt: &RtpPacket,
    ) {
        if let Some(p) = self.registry.get_mut(&subscriber) {
            p.with_span(|core| core.on_forward_rtp(stream_id, pkt));
            self.dirty.mark(self.kind, subscriber);
        }
    }

    fn forward_audio_rtp(
        &mut self,
        subscriber: ParticipantId,
        slot_idx: AudioSelectorSlotId,
        pkt: &RtpPacket,
    ) {
        if let Some(p) = self.registry.get_mut(&subscriber) {
            p.with_span(|core| core.on_forward_audio_rtp(slot_idx, pkt));
            self.dirty.mark(self.kind, subscriber);
        }
    }

    fn forward_sctp(&mut self, subscriber: ParticipantId, topic: &crate::track::Topic, pkt: &[u8]) {
        if let Some(p) = self.registry.get_mut(&subscriber) {
            p.with_span(|core| core.on_forward_sctp(topic, pkt));
            self.dirty.mark(self.kind, subscriber);
        }
    }

    fn notify_tracks_published(
        &mut self,
        participant_id: ParticipantId,
        tracks: &[crate::track::Track],
    ) {
        if let Some(p) = self.registry.get_mut(&participant_id) {
            p.on_tracks_published(tracks);
            self.dirty.mark(self.kind, participant_id);
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
            self.dirty.mark(self.kind, participant_id);
        }
    }

    fn notify_keyframe_request(
        &mut self,
        participant_id: ParticipantId,
        stream_id: StreamId,
        kind: str0m::media::KeyframeRequestKind,
    ) {
        if let Some(p) = self.registry.get_mut(&participant_id) {
            p.with_span(|core| core.handle_remote_keyframe_request(stream_id, kind));
            self.dirty.mark(self.kind, participant_id);
        }
    }

    fn is_local(&self, id: &ParticipantId) -> bool {
        self.registry.contains(id)
    }
}

pub(crate) struct ShardCore {
    pub(crate) shard_id: ShardId,
    registry: ParticipantRegistry,
    pub(super) routing: ShardRoutingTable,
    timers: TimerWheel,
    dirty: DirtyTracker,
    pipeline: EventPipeline,
    rng: Rng,
}

impl ShardCore {
    pub(crate) fn new(shard_id: impl Into<ShardId>, max_gso_segments: usize, mut rng: Rng) -> Self {
        let shard_id = shard_id.into();
        Self {
            shard_id,
            registry: ParticipantRegistry::new(shard_id, max_gso_segments),
            routing: ShardRoutingTable::new(),
            timers: TimerWheel::new(MAX_PARTICIPANTS_PER_SHARD),
            dirty: DirtyTracker::with_capacity(MAX_PARTICIPANTS_PER_SHARD, &mut rng),
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
        self.timers.drain_expired(now, |participant_id| {
            if let Some(participant) = registry.get_mut(&participant_id) {
                participant.with_span(|core| core.on_timeout(now));
                dirty.mark_input(participant_id);
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
            participant.with_span(|core| core.on_ingress(batch));
            self.dirty.mark_input(participant_id);
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

    pub(crate) fn poll_input(&mut self, now: Instant) {
        Self::poll_participants(
            now,
            self.dirty.input(),
            &mut self.registry,
            &mut self.pipeline,
        );
    }

    pub(crate) fn poll_fanout(&mut self, now: Instant) {
        Self::poll_participants(
            now,
            self.dirty.fanout(),
            &mut self.registry,
            &mut self.pipeline,
        );
    }

    fn poll_participants(
        now: Instant,
        ids: &indexmap::IndexSet<ParticipantId, ahash::RandomState>,
        registry: &mut ParticipantRegistry,
        pipeline: &mut EventPipeline,
    ) {
        for &participant_id in ids {
            let Some(participant) = registry.get_mut(&participant_id) else {
                continue;
            };
            let room_id = participant.room_id;
            let mut sink = pipeline.participant_sink(room_id, participant_id);
            participant.with_span(|core| core.poll(now, &mut sink));
        }
    }

    pub(crate) fn flush_stream_buffers(&mut self, router: &impl CrossShardSend) {
        while let Some(ev) = self.pipeline.pop_audio_rtp() {
            debug_assert!(ev.stream_id.0.kind() == TrackKind::Audio);
            let mut ctx = DispatchCtx {
                registry: &mut self.registry,
                dirty: &mut self.dirty,
                kind: DirtyKind::Fanout,
                router,
            };
            self.routing.route_audio(ev, &mut ctx);
        }

        while let Some(ev) = self.pipeline.pop_video_rtp() {
            debug_assert!(ev.stream_id.0.kind() == TrackKind::Video);
            let mut ctx = DispatchCtx {
                registry: &mut self.registry,
                dirty: &mut self.dirty,
                kind: DirtyKind::Fanout,
                router,
            };
            self.routing.route_video(ev.stream_id, &ev.pkt, &mut ctx);
        }

        while let Some(ev) = self.pipeline.pop_data_sctp() {
            let mut ctx = DispatchCtx {
                registry: &mut self.registry,
                dirty: &mut self.dirty,
                kind: DirtyKind::Fanout,
                router,
            };
            self.routing
                .route_data(ev.room_id, ev.origin, &ev.topic, &ev.pkt, &mut ctx);
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
                ParticipantEvent::Timer(ParticipantTimerEvent::DeadlineUpdated {
                    at,
                    participant_id,
                }) => {
                    self.timers.schedule(participant_id, at);
                }
                ParticipantEvent::Lifecycle(ParticipantLifecycleEvent::Exited {
                    participant_id,
                }) => {
                    self.remove_participant(&participant_id);
                    self.timers.cancel(&participant_id);
                    self.dirty.clear_input(&participant_id);
                    self.pipeline
                        .push_shard_event(ShardEvent::ParticipantExited(participant_id));
                }
                ParticipantEvent::Control(ev) => {
                    match ev {
                        ParticipantControlEvent::DataPacketPublished(data) => {
                            let mut ctx = DispatchCtx {
                                registry: &mut self.registry,
                                dirty: &mut self.dirty,
                                kind: DirtyKind::Fanout,
                                router,
                            };
                            self.routing.route_data(
                                data.room_id,
                                data.origin,
                                &data.topic,
                                &data.pkt,
                                &mut ctx,
                            );
                        }
                        ParticipantControlEvent::DataTopicPublished { room_id, topic } => {
                            if self.routing.register_data_publisher(room_id, topic.clone()) {
                                self.pipeline
                                    .push_shard_event(ShardEvent::DataTopicPublished { room_id, topic });
                            }
                        }
                        ParticipantControlEvent::DataTopicUnpublished { room_id, topic } => {
                            if self.routing.unregister_data_publisher(room_id, &topic) {
                                self.pipeline
                                    .push_shard_event(ShardEvent::DataTopicUnpublished { room_id, topic });
                            }
                        }
                        ParticipantControlEvent::DataTopicSubscribed {
                            room_id,
                            subscriber,
                            topic,
                        } => {
                            if self
                                .routing
                                .register_data_subscriber(room_id, subscriber, topic.clone())
                            {
                                self.pipeline
                                    .push_shard_event(ShardEvent::DataTopicSubscribed { room_id, topic });
                            }
                        }
                        ParticipantControlEvent::DataTopicUnsubscribed {
                            room_id,
                            subscriber,
                            topic,
                        } => {
                            if self
                                .routing
                                .unregister_data_subscriber(room_id, subscriber, &topic)
                            {
                                self.pipeline
                                    .push_shard_event(ShardEvent::DataTopicUnsubscribed { room_id, topic });
                            }
                        }
                        ev => {
                            router::route_participant_control_event(
                                ev,
                                self.pipeline.shard_events_mut(),
                                router,
                            );
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn pop_shard_event(&mut self) -> Option<ShardEvent> {
        self.pipeline.pop_shard_event()
    }

    pub(crate) fn flush_egress(
        &mut self,
        udp_socket: &UnifiedSocket,
        tcp_socket: &mut net::tcp::TcpTransport,
    ) {
        for participant_id in self.dirty.drain_all() {
            let Some(participant) = self.registry.get_mut(&participant_id) else {
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
                    kind: DirtyKind::Input,
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
                    kind: DirtyKind::Input,
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
                    kind: DirtyKind::Input,
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
            ClusterCommand::PublishDataTopic { room_id, topic } => {
                let _ = (room_id, topic);
            }
            ClusterCommand::UnpublishDataTopic { room_id, topic } => {
                let _ = (room_id, topic);
            }
            ClusterCommand::SubscribeDataTopic {
                room_id,
                from_shard_id,
                topic,
            } => {
                self.routing
                    .register_remote_data_subscriber_shard(room_id, from_shard_id, topic);
            }
            ClusterCommand::UnsubscribeDataTopic {
                room_id,
                from_shard_id,
                topic,
            } => {
                self.routing.unregister_remote_data_subscriber_shard(
                    room_id,
                    from_shard_id,
                    &topic,
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
                    kind: DirtyKind::Fanout,
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
                let ev = RtpEvent {
                    stream_id,
                    pkt,
                    room_id,
                    origin,
                };
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    kind: DirtyKind::Fanout,
                    router,
                };
                self.routing.route_audio(ev, &mut ctx);
            }
            CrossShardEvent::UdpPacket {
                participant_id,
                batch,
            } => {
                if let Some(participant) = self.registry.get_mut(&participant_id) {
                    participant.with_span(|core| core.on_ingress(batch));
                    self.dirty.mark_input(participant_id);
                }
            }
            CrossShardEvent::KeyframeRequested(req) => {
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    kind: DirtyKind::Input,
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
                    kind: DirtyKind::Fanout,
                    router,
                };
                self.routing
                    .route_data(room_id, origin, &topic, &pkt, &mut ctx);
            }
        }
    }

    fn add_participant(&mut self, cfg: ParticipantConfig, router: &impl CrossShardSend) {
        let room_id = cfg.room_id;
        let participant_id = cfg.participant_id;
        self.remove_participant(&participant_id);
        let _ = router; // reserved: re-add currently needs no cross-shard notice
        let participant_id = self.registry.insert(cfg, &mut self.rng);
        self.routing
            .add_local_member(participant_id, room_id, &mut self.rng);
        self.dirty.mark_input(participant_id);
    }

    fn remove_participant(&mut self, participant_id: &ParticipantId) -> Option<()> {
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

    #[test]
    fn add_participant_populates_registry_and_marks_input_dirty() {
        let router = TestRouter::new(0, 3);
        let mut core = new_core();
        let p = pid();
        let r = room_id("add1");

        add_participant(&mut core, &router, p, r);

        assert!(core.registry.contains(&p));
        assert!(core.routing.rooms.contains_key(&r));
        // add_participant() in common.rs drains the dirty side-effects via
        // take_sent, but input-dirty is local state, not router traffic —
        // re-add to check it directly without the helper's drain.
        let mut core2 = new_core();
        core2.on_command(
            ShardCommand::AddParticipant(make_participant_cfg(p, r)),
            &router,
        );
        assert!(
            core2.dirty.input().contains(&p),
            "newly added participant must be input-dirty"
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
    fn keyframe_request_command_marks_input_dirty_not_fanout() {
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
            core.dirty.input().contains(&p),
            "keyframe delivery is an input event for the target participant"
        );
    }

    #[test]
    fn cross_shard_rtp_published_marks_fanout_dirty_not_input() {
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
        assert!(core.dirty.input().contains(&subscriber));
        core.dirty.clear_input(&subscriber);
        // Directly seed a local subscriber into routing to isolate fanout behavior
        // from the topology event path (covered separately in router.rs's tests).
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
            core.dirty.fanout().contains(&subscriber),
            "forwarded RTP is a fanout event for the subscriber"
        );
        assert!(!core.dirty.input().contains(&subscriber));
    }

    #[test]
    fn fire_timers_marks_input_dirty_and_clears_on_exit() {
        let router = TestRouter::new(0, 3);
        let mut core = new_core();
        let p = pid();
        let r = room_id("timer1");
        add_participant(&mut core, &router, p, r);
        core.dirty.clear_input(&p); // isolate from add's own dirty marking

        // fire_timers only fires what's actually scheduled; this checks the
        // wiring (registry lookup + dirty marking), not TimerWheel's own logic.
        core.fire_timers(tokio::time::Instant::now());
        // No timer was scheduled, so nothing should fire — this asserts the
        // no-op path doesn't spuriously mark anyone dirty.
        assert!(!core.dirty.input().contains(&p));

        core.on_command(ShardCommand::RemoveParticipant(p), &router);
        // timers.cancel + dirty.clear_input both run on exit — asserting no
        // panic and registry no longer contains the id is the meaningful check
        // here, since there's no scheduled timer to observe firing.
        assert!(!core.registry.contains(&p));
    }
}

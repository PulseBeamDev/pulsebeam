use pulsebeam_runtime::net::{self, UnifiedSocket};
use pulsebeam_runtime::rand::Rng;
use tokio::time::Instant;

use crate::clock::WallAnchor;
use crate::route::{MediaEnvelope, RemoteRoute, RouteAction, RouteEnvelope};

use super::events::{
    AudioRtpEvent, ParticipantControlEvent, ParticipantEvent, ParticipantLifecycleEvent,
};
use crate::id::AudioSelectorSlotId;
use crate::{
    entity::{AudioOrigin, ParticipantId, TrackId, TrackKind},
    id::ShardId,
    participant::{ParticipantConfig, batcher::GsoSendBatch},
    rtp::RtpPacket,
    shard::{
        dirty::DirtyTracker,
        events::EventPipeline,
        participants::{ParticipantKey, ParticipantRegistry},
        timer::TimerWheel,
    },
};
use str0m::media::Rid;

use super::control::ParticipantShardMeta;
use super::router::{self, Origin, RoutingContext, ShardRoutingTable};

pub(crate) use super::router::ShardTransport;
use super::worker::{MediaPayload, Reverse, ShardCommand, ShardEvent, ShardFrame, Topology};

/// A starting hint, not a policy cap: nothing rejects the next participant
/// past this. `TimerWheel`, `DirtyTracker` and `EventPipeline` all grow past
/// it on demand — this only smooths the allocations ramp-up pays.
const PARTICIPANT_CAPACITY_HINT: usize = 64;

struct DispatchCtx<'a, R: ShardTransport> {
    registry: &'a mut ParticipantRegistry,
    dirty: &'a mut DirtyTracker,
    router: &'a R,
    wall: &'a WallAnchor,
}

impl<'a, R: ShardTransport> ShardTransport for DispatchCtx<'a, R> {
    fn send_media(&self, dst: ShardId, env: MediaEnvelope, payload: MediaPayload) {
        self.router.send_media(dst, env, payload);
    }

    fn send_frame(&self, dst: ShardId, ev: ShardFrame) {
        self.router.send_frame(dst, ev);
    }
}

impl<'a, R: ShardTransport> RoutingContext for DispatchCtx<'a, R> {
    fn forward_video_rtp(
        &mut self,
        subscriber: ParticipantKey,
        track_id: TrackId,
        pkt: &RtpPacket,
        cache: Option<&crate::rtp::cache::TrackStreamCache>,
    ) {
        if let Some(p) = self.registry.resolve_mut(subscriber) {
            p.on_forward_rtp(track_id, pkt, cache);
            self.dirty.mark(subscriber, p);
        }
    }

    fn update_layer_states(
        &mut self,
        subscriber: ParticipantKey,
        track_id: TrackId,
        states: &crate::track::TrackStates,
    ) {
        if let Some(p) = self.registry.resolve_mut(subscriber) {
            p.update_layer_states(track_id, states);
        }
    }

    fn forward_audio_rtp(
        &mut self,
        subscriber: ParticipantKey,
        slot_idx: AudioSelectorSlotId,
        origin: AudioOrigin,
        pkt: &RtpPacket,
    ) {
        if let Some(p) = self.registry.resolve_mut(subscriber) {
            p.on_forward_audio_rtp(slot_idx, origin, pkt);
            self.dirty.mark(subscriber, p);
        }
    }

    fn forward_sctp(
        &mut self,
        subscriber: ParticipantKey,
        origin: ParticipantId,
        topic: &crate::track::Topic,
        pkt: &[u8],
    ) {
        if let Some(p) = self.registry.resolve_mut(subscriber) {
            p.on_forward_sctp(topic, origin, pkt);
            self.dirty.mark(subscriber, p);
        }
    }

    fn notify_tracks_published(
        &mut self,
        participant: ParticipantKey,
        tracks: &[crate::track::Track],
    ) {
        if let Some(p) = self.registry.resolve_mut(participant) {
            p.on_tracks_published(tracks);
            self.dirty.mark(participant, p);
        }
    }

    fn notify_tracks_unpublished(
        &mut self,
        participant: ParticipantKey,
        track_ids: &[crate::entity::TrackId],
    ) {
        let Some(p) = self.registry.resolve_mut(participant) else {
            return;
        };

        if p.on_tracks_unpublished(track_ids) {
            self.dirty.mark(participant, p);
        }
    }

    fn notify_keyframe_request(
        &mut self,
        participant_id: ParticipantId,
        track_id: TrackId,
        rid: Option<Rid>,
        kind: str0m::media::KeyframeRequestKind,
    ) {
        if let Some((key, p)) = self.registry.get_mut_with_key(&participant_id) {
            p.handle_remote_keyframe_request((track_id, rid), kind);
            self.dirty.mark(key, p);
        }
    }

    fn is_local(&self, id: &ParticipantId) -> bool {
        self.registry.contains(id)
    }

    fn wall(&self) -> &WallAnchor {
        self.wall
    }

    fn forward_reliable_sctp(
        &mut self,
        subscriber: ParticipantKey,
        origin: ParticipantId,
        topic: &crate::track::Topic,
        frame: &[u8],
    ) {
        if let Some(p) = self.registry.resolve_mut(subscriber) {
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
        if let Some((key, p)) = self.registry.get_mut_with_key(&publisher) {
            p.on_deliver_reliable_control(topic, bytes);
            self.dirty.mark(key, p);
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
    wall: WallAnchor,
}

impl ShardCore {
    pub(crate) fn new(
        shard_id: impl Into<ShardId>,
        max_gso_segments: usize,
        rng: Rng,
        wall: WallAnchor,
    ) -> Self {
        let shard_id = shard_id.into();
        Self {
            shard_id,
            registry: ParticipantRegistry::new(shard_id, max_gso_segments),
            routing: ShardRoutingTable::new(),
            timers: TimerWheel::new(PARTICIPANT_CAPACITY_HINT),
            dirty: DirtyTracker::with_capacity(PARTICIPANT_CAPACITY_HINT),
            udp_send_batch: GsoSendBatch::preallocated(),
            pipeline: EventPipeline::with_capacity(PARTICIPANT_CAPACITY_HINT),
            rng,
            wall,
        }
    }

    /// Put an arriving packet on *this* shard's timeline.
    ///
    /// `Instant` is meaningless outside the process that produced it, so the
    /// sender's values are discarded rather than trusted: playout is rebuilt
    /// from the envelope's portable NTP, and arrival is stamped from our own
    /// clock, which is also the more correct value — every consumer reads it as
    /// "when did this get here".
    ///
    /// Once payloads are bytes, neither field is on the wire at all and this is
    /// the only place they are set.
    fn restamp(&self, pkt: &mut RtpPacket, playout: crate::clock::NtpTime, now: Instant) {
        // While payloads are still typed, the sender's playout is derivable and
        // must agree with the envelope: that proves the wire value correct
        // before it becomes the only source. Same process, so a shared anchor
        // makes the comparison meaningful; cross-node the field will be gone.
        debug_assert!(
            self.wall
                .to_ntp(pkt.playout_time)
                .units_since(playout)
                .unsigned_abs()
                <= 1 << 16,
            "envelope playout disagrees with the payload beyond middle-32 resolution"
        );
        pkt.playout_time = self.wall.to_instant(playout);
        pkt.arrival_ts = now;
        pkt.rehome_extensions();
    }

    /// Deliver a frame addressed to one of this shard's routes.
    ///
    /// The envelope carries no semantic ids: the route entry is the compiled
    /// plan, and the lookup is an array index plus an epoch check.
    fn on_media_frame(
        &mut self,
        env: MediaEnvelope,
        payload: MediaPayload,
        now: Instant,
        router: &impl ShardTransport,
    ) {
        let entry = match self.routing.data.routes.resolve(&env) {
            Ok(entry) => entry,
            Err(err) => {
                // A stale epoch is expected after a teardown and must never
                // reach a recycled route; anything else is a bug.
                tracing::debug!(shard_id = %self.shard_id, ?err, "dropping frame on an unusable route");
                return;
            }
        };
        entry.observe(env.link_seq);
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_cross_shard_media();
        let playout = match entry.expander.expand(env.playout_ntp32) {
            Ok(playout) => playout,
            Err(err) => {
                tracing::warn!(
                    shard_id = %self.shard_id,
                    route = %env.route,
                    stream = %entry.names,
                    ?err,
                    "route timeline is ambiguous; needs a fresh NTP reference"
                );
                return;
            }
        };

        // A plain `Copy` out of the action, not a clone: every variant is a
        // key now, so there is nothing left to allocate on the forwarding
        // path. This is what `Target` (a hand-rolled key-only shadow of
        // `RouteAction`) used to buy; with the action itself `Copy`, it
        // added a second enum for no reason and is gone.
        let action = entry.action;
        if matches!(action, RouteAction::Reverse { .. }) {
            debug_assert!(
                false,
                "no media dispatch for route action {action:?} on {} ({})",
                env.route, entry.names
            );
            return;
        }

        match (action, payload) {
            (RouteAction::Video { local_track }, MediaPayload::Video(mut pkt)) => {
                self.restamp(&mut pkt, playout, now);
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                    wall: &self.wall,
                };
                self.routing.route_video(local_track, pkt, &mut ctx);
            }
            (RouteAction::Audio { room, track }, MediaPayload::Audio(mut pkt)) => {
                self.restamp(&mut pkt, playout, now);
                let Some(room_id) = self.routing.room_id_of(room) else {
                    debug_assert!(false, "an audio route's room key must resolve to a room");
                    return;
                };
                let Some((track_id, _)) = self.routing.track_descriptor(track) else {
                    debug_assert!(false, "an audio route's track key must resolve to a track");
                    return;
                };
                let Some(origin) = self.routing.track_origin(track) else {
                    debug_assert!(false, "an audio route's track key must resolve to a track");
                    return;
                };
                // `room_id` only fills a field the local-origin path still
                // needs to carry; the lookup above resolves through `room`
                // directly, never by hashing it back.
                let ev = AudioRtpEvent {
                    stream_id: (track_id, None),
                    pkt,
                    room_id,
                    origin,
                };
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                    wall: &self.wall,
                };
                self.routing
                    .route_audio(room, track, Origin::Remote, ev, &mut ctx);
            }
            (RouteAction::Data { stream }, MediaPayload::Data(bytes)) => {
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                    wall: &self.wall,
                };
                self.routing
                    .route_data(stream, Origin::Remote, &bytes, &mut ctx);
            }
            (RouteAction::Reliable { stream }, MediaPayload::Data(bytes)) => {
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                    wall: &self.wall,
                };
                self.routing
                    .route_reliable_data(stream, Origin::Remote, &bytes, &mut ctx);
            }
            _ => debug_assert!(false, "payload does not match the route action"),
        }
    }

    /// The node's NTP↔`Instant` mapping, captured once at startup and shared by
    /// every shard so their timelines agree.
    ///
    /// This is a *fallback*. A stream's authoritative NTP reference is its
    /// sender's RTCP Sender Reports, which `Synchronizer` already tracks; this
    /// only covers streams that have not produced one yet. It is deliberately
    /// never refreshed: re-anchoring mid-stream would step playout scheduling,
    /// and reading the wall clock per tick is both a syscall on the packet path
    /// and a source of nondeterminism under simulation.
    #[cfg(test)]
    pub(crate) fn wall(&self) -> &WallAnchor {
        &self.wall
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
                dirty.mark(handle, participant);
            }
        });
    }

    pub(crate) fn on_udp_batch(
        &mut self,
        batch: pulsebeam_runtime::net::RecvPacketBatch,
        router: &impl ShardTransport,
    ) {
        let Some(participant_id) = self.registry.demux(&batch) else {
            return;
        };
        if let Some((key, participant)) = self.registry.get_mut_with_key(&participant_id) {
            participant.on_ingress(batch);
            self.dirty.mark(key, participant);
        } else if let Some(shard_id) = self.routing.remote_shard_for(&participant_id) {
            router.send_frame(
                shard_id,
                ShardFrame::Ingress {
                    participant_id,
                    batch,
                },
            );
        }
    }

    pub(crate) fn flush_stream_buffers(&mut self, router: &impl ShardTransport) {
        let mut ctx = DispatchCtx {
            registry: &mut self.registry,
            dirty: &mut self.dirty,
            router,
            wall: &self.wall,
        };
        while let Some(ev) = self.pipeline.pop_audio_rtp() {
            debug_assert!(ev.stream_id.0.kind() == TrackKind::Audio);
            // A locally published track still costs one lookup to reach its
            // fanout: the publishing participant does not hold the key yet.
            // Same race video already tolerates (TrackPublished may not have
            // drained yet) — a silent skip here self-heals on the next packet.
            let Some(room) = self.routing.control.room_keys.get(&ev.room_id).copied() else {
                continue;
            };
            let Some(track) = self.routing.fanout_of(&ev.stream_id.0) else {
                continue;
            };
            self.routing
                .route_audio(room, track, Origin::Local, ev, &mut ctx);
        }

        while let Some(ev) = self.pipeline.pop_video_rtp() {
            debug_assert!(ev.stream_id.0.kind() == TrackKind::Video);
            // A locally published track still costs one lookup to reach its
            // fanout: the publishing participant does not hold the key yet.
            // Everything downstream of here is index-addressed.
            let Some(fanout) = self.routing.fanout_of(&ev.stream_id.0) else {
                continue;
            };
            self.routing.route_video(fanout, ev.pkt, &mut ctx);
        }

        while let Some(ev) = self.pipeline.pop_data_sctp() {
            let id = crate::shard::control::DataStreamId::new(ev.origin, ev.topic);
            if let Some(stream) = self.routing.data_stream_key(&id) {
                self.routing
                    .route_data(stream, Origin::Local, &ev.pkt, &mut ctx);
            }
        }

        while let Some(ev) = self.pipeline.pop_reliable_data_sctp() {
            let id = crate::shard::control::DataStreamId::new(ev.origin, ev.topic);
            if let Some(stream) = self.routing.reliable_stream_key(&id) {
                self.routing
                    .route_reliable_data(stream, Origin::Local, &ev.pkt, &mut ctx);
            }
        }
    }

    pub(crate) fn flush_participant_events(&mut self, now: Instant, router: &impl ShardTransport) {
        while let Some(event) = self.pipeline.pop_participant_event() {
            match event {
                ParticipantEvent::Topology(ev) => {
                    if let Some(shard_event) =
                        self.routing.handle_topology_event(ev, now, &self.wall)
                    {
                        self.pipeline.push_shard_event(shard_event);
                    }
                }
                ParticipantEvent::Lifecycle(ParticipantLifecycleEvent::Exited {
                    participant_id,
                }) => {
                    self.remove_participant(&participant_id, now);
                    self.pipeline
                        .push_shard_event(ShardEvent::ParticipantExited(participant_id));
                }
                ParticipantEvent::Control(ev) => {
                    match ev {
                        ParticipantControlEvent::DataTopicPublished {
                            room_id,
                            publisher,
                            topic,
                        } => {
                            self.routing
                                .register_data_publisher(room_id, publisher, topic.clone());
                            self.pipeline.push_shard_event(ShardEvent::Relay(
                                Topology::DataTopicPublished {
                                    room_id,
                                    publisher,
                                    topic,
                                },
                            ));
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
                            if let Some(ev) = self.routing.register_data_subscriber(
                                room_id,
                                subscriber,
                                topic.clone(),
                                publisher,
                                now,
                                &self.wall,
                            ) {
                                self.pipeline.push_shard_event(ev);
                            }
                        }
                        ParticipantControlEvent::DataTopicUnsubscribed {
                            room_id,
                            subscriber,
                            topic,
                            publisher,
                        } => {
                            if self.routing.unregister_data_subscriber(
                                room_id, subscriber, &topic, publisher, now,
                            ) {
                                self.pipeline.push_shard_event(ShardEvent::Relay(
                                    Topology::DataTopicUnsubscribed {
                                        room_id,
                                        topic,
                                        publisher,
                                    },
                                ));
                            }
                        }
                        ParticipantControlEvent::ReliableDataTopicPublished {
                            room_id,
                            publisher,
                            topic,
                        } => {
                            let reverse = self.routing.register_reliable_data_publisher(
                                room_id,
                                publisher,
                                topic.clone(),
                                now,
                                &self.wall,
                            );
                            self.pipeline.push_shard_event(ShardEvent::Relay(
                                Topology::ReliableTopicPublished {
                                    room_id,
                                    publisher,
                                    topic,
                                    reverse,
                                },
                            ));
                        }
                        ParticipantControlEvent::ReliableDataTopicUnpublished {
                            room_id,
                            publisher,
                            topic,
                        } => {
                            self.routing.unregister_reliable_data_publisher(
                                room_id, publisher, &topic, now,
                            );
                        }
                        ParticipantControlEvent::ReliableDataTopicSubscribed {
                            room_id,
                            subscriber,
                            topic,
                        } => {
                            if let Some(ev) = self
                                .routing
                                .register_reliable_data_subscriber(room_id, subscriber, topic)
                            {
                                self.pipeline.push_shard_event(ev);
                            }
                        }
                        ParticipantControlEvent::ReliableDataTopicUnsubscribed {
                            room_id,
                            subscriber,
                            topic,
                        } => {
                            if self.routing.unregister_reliable_data_subscriber(
                                room_id, subscriber, &topic, now,
                            ) {
                                self.pipeline.push_shard_event(ShardEvent::Relay(
                                    Topology::ReliableTopicUnsubscribed { room_id, topic },
                                ));
                            }
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
                                wall: &self.wall,
                            };
                            self.routing
                                .route_reliable_control(publisher, &topic, &bytes, &mut ctx);
                        }
                        ParticipantControlEvent::TrackPublished(mut track, states) => {
                            // Register the handles on the node; only the
                            // stateless descriptor continues to the controller.
                            self.routing.publish_local_track(
                                track.meta.id,
                                track.meta.origin,
                                states,
                            );
                            // Open the reverse path now and stamp it on the
                            // descriptor: by the time any shard can subscribe,
                            // it already knows where to ask for a keyframe.
                            track.reverse = self
                                .routing
                                .open_track_reverse_route(&track, now, &self.wall);
                            self.pipeline
                                .push_shard_event(ShardEvent::TrackPublished(track));
                        }
                        // Measurements go straight to the shards holding a
                        // route for the track. The controller has nothing to add
                        // and must not accumulate media state.
                        ParticipantControlEvent::TrackStatsUpdated { track_id, states } => {
                            let mut ctx = DispatchCtx {
                                registry: &mut self.registry,
                                dirty: &mut self.dirty,
                                router,
                                wall: &self.wall,
                            };
                            for (shard_id, env) in
                                self.routing
                                    .publish_stats(track_id, states.clone(), &mut ctx)
                            {
                                router.send_frame(
                                    shard_id,
                                    ShardFrame::Stats {
                                        env,
                                        stats: states.clone(),
                                    },
                                );
                            }
                        }
                        // A keyframe request is upstream feedback, so it goes
                        // straight to the shard that owns the publisher — never
                        // through the controller, which has nothing to add and
                        // would only turn a local request into a round trip.
                        ParticipantControlEvent::KeyframeRequested(req) => {
                            if req.shard_id == self.shard_id {
                                let mut ctx = DispatchCtx {
                                    registry: &mut self.registry,
                                    dirty: &mut self.dirty,
                                    router,
                                    wall: &self.wall,
                                };
                                ctx.notify_keyframe_request(
                                    req.origin,
                                    req.stream_id.0,
                                    req.stream_id.1,
                                    req.kind,
                                );
                            } else if let Some((target, layer)) = self
                                .routing
                                .track_reverse_target(&req.stream_id.0, req.stream_id.1)
                            {
                                router.send_frame(
                                    req.shard_id,
                                    ShardFrame::Reverse {
                                        env: RouteEnvelope::new(target),
                                        body: Reverse::Keyframe {
                                            layer,
                                            kind: req.kind,
                                        },
                                    },
                                );
                            } else {
                                // The reverse route arrives with the track, so a
                                // subscription cannot predate it.
                                debug_assert!(
                                    false,
                                    "no reverse route for a remotely published track"
                                );
                            }
                        }
                        ev => {
                            // The publisher's own shard owns the registry entry,
                            // so it is the only one that may retract it.
                            if let ParticipantControlEvent::TrackUnpublished { track_id, .. } = &ev
                            {
                                self.routing.unpublish_local_track(track_id);
                                self.routing.close_track_reverse_route(track_id, now);
                            }
                            router::route_participant_control_event(
                                ev,
                                self.pipeline.shard_events_mut(),
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

    pub(crate) fn poll_and_flush_dirty(
        &mut self,
        now: Instant,
        udp_socket: &mut UnifiedSocket,
        tcp_socket: &mut net::tcp::TcpTransport,
    ) {
        debug_assert!(self.udp_send_batch.is_empty());
        self.dirty.begin_phase();
        while let Some(handle) = self.dirty.next() {
            let Some(participant) = self.registry.resolve_mut(handle) else {
                continue;
            };
            debug_assert!(participant.queued_dirty);
            participant.queued_dirty = false;
            let room_id = participant.room_id;
            let participant_id = participant.participant_id;
            let mut sink = self.pipeline.participant_sink(room_id, participant_id);
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
        for addr in self.registry.drain_pending_close() {
            udp_socket.close_peer(&addr);
            tcp_socket.close_peer(&addr);
        }
    }

    pub(crate) fn on_command(
        &mut self,
        cmd: ShardCommand,
        now: Instant,
        router: &impl ShardTransport,
    ) -> Option<()> {
        match cmd {
            ShardCommand::AddParticipant(cfg) => self.add_participant(*cfg, now, router),
            ShardCommand::RemoveParticipant(participant_id) => {
                self.remove_participant(&participant_id, now);
            }
            ShardCommand::AddTcpConnection { .. } => {
                // Handled by the shard worker directly; no core action needed.
            }
            cmd => self.on_control_command(cmd, now, router)?,
        }
        Some(())
    }

    fn on_control_command(
        &mut self,
        cmd: ShardCommand,
        now: Instant,
        router: &impl ShardTransport,
    ) -> Option<()> {
        match cmd {
            ShardCommand::AddParticipant(_)
            | ShardCommand::RemoveParticipant(_)
            | ShardCommand::AddTcpConnection { .. } => pulsebeam_runtime::fatal!(
                "a command handled by the outer match reached the inner one; the two have drifted apart"
            ),
            ShardCommand::RegisterParticipant {
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
            ShardCommand::UnregisterParticipant {
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
            ShardCommand::PublishTrack(track, room_id) => {
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                    wall: &self.wall,
                };
                // An audio track published elsewhere makes this shard a
                // destination, so it installs a route and hands back the handle.
                if let Some(ev) = self
                    .routing
                    .publish_track(track, room_id, now, &self.wall, &mut ctx)
                {
                    self.pipeline.push_shard_event(ev);
                }
            }
            ShardCommand::UnpublishTracks {
                origin: _,
                room_id,
                track_ids,
            } => {
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                    wall: &self.wall,
                };
                self.routing
                    .unpublish_tracks(room_id, &track_ids, now, &mut ctx);
            }
            ShardCommand::Relay {
                from_shard_id,
                topology,
            } => self.on_topology(from_shard_id, topology, now)?,
        }
        Some(())
    }

    /// Apply a topology change the controller relayed from `from_shard_id`.
    ///
    /// Everything here is reliable and semantic by construction: it arrived on
    /// the control plane, which is the only place topology travels.
    fn on_topology(
        &mut self,
        from_shard_id: ShardId,
        topology: Topology,
        now: Instant,
    ) -> Option<()> {
        match topology {
            Topology::TrackSubscribed {
                track,
                route,
                epoch,
            } => {
                // The destination allocated and installed this route in its own
                // table; receiving the handle is the acknowledgement that lets
                // media start flowing to it.
                self.routing.register_remote_subscriber_shard(
                    RemoteRoute::new(from_shard_id, route, epoch),
                    track,
                );
            }
            Topology::TrackUnsubscribed {
                track,
                route,
                epoch,
            } => {
                self.routing
                    .unregister_remote_subscriber_shard(from_shard_id, track, route, epoch);
            }
            Topology::DataTopicSubscribed {
                room_id,
                topic,
                publisher,
                route,
                epoch,
            } => {
                let remote = route.map(|r| RemoteRoute::new(from_shard_id, r, epoch));
                // A wildcard destination that arrived after we started
                // publishing needs to hear about those streams to install
                // routes for them.
                let announce = self.routing.register_remote_data_subscriber_shard(
                    room_id,
                    from_shard_id,
                    topic.clone(),
                    publisher,
                    remote,
                );
                for publisher in announce {
                    self.pipeline.push_shard_event(ShardEvent::Relay(
                        Topology::DataTopicPublished {
                            room_id,
                            publisher,
                            topic: topic.clone(),
                        },
                    ));
                }
            }
            Topology::ReliableTopicSubscribed {
                room_id,
                topic,
                publisher,
                route,
                epoch,
            } => {
                let remote = route.map(|r| RemoteRoute::new(from_shard_id, r, epoch));
                let announce = self.routing.register_remote_reliable_subscriber_shard(
                    room_id,
                    from_shard_id,
                    topic.clone(),
                    publisher,
                    remote,
                );
                for publisher in announce {
                    let reverse = self.routing.topic_reverse_handle(publisher, &topic);
                    self.pipeline.push_shard_event(ShardEvent::Relay(
                        Topology::ReliableTopicPublished {
                            room_id,
                            publisher,
                            topic: topic.clone(),
                            reverse,
                        },
                    ));
                }
            }
            Topology::ReliableTopicUnsubscribed { room_id, topic } => {
                self.routing.unregister_remote_reliable_subscriber_shard(
                    room_id,
                    from_shard_id,
                    &topic,
                    None,
                );
            }
            Topology::ReliableTopicPublished {
                room_id,
                publisher,
                topic,
                reverse,
            } => {
                self.routing
                    .learn_topic_reverse_target(publisher, &topic, reverse);
                if let Some(ev) = self
                    .routing
                    .on_remote_reliable_publisher(room_id, publisher, &topic, now, &self.wall)
                {
                    self.pipeline.push_shard_event(ev);
                }
            }
            Topology::DataTopicPublished {
                room_id,
                publisher,
                topic,
            } => {
                if let Some(ev) = self
                    .routing
                    .on_remote_data_publisher(room_id, publisher, &topic, now, &self.wall)
                {
                    self.pipeline.push_shard_event(ev);
                }
            }
            Topology::DataTopicUnsubscribed {
                room_id,
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

    pub fn on_shard_frame(&mut self, ev: ShardFrame, now: Instant, router: &impl ShardTransport) {
        match ev {
            ShardFrame::Media { env, payload } => {
                self.on_media_frame(env, payload, now, router);
            }
            ShardFrame::Ingress {
                participant_id,
                batch,
            } => {
                if let Some((key, participant)) = self.registry.get_mut_with_key(&participant_id) {
                    participant.on_ingress(batch);
                    self.dirty.mark(key, participant);
                }
            }
            ShardFrame::Reverse { env, body } => {
                self.on_reverse_frame(env, body, router);
            }
            ShardFrame::Stats { env, stats } => {
                let Some(RouteAction::Video { local_track, .. }) = self
                    .routing
                    .data
                    .routes
                    .resolve_action(env.route, env.epoch)
                else {
                    // The route was retired while this was in flight.
                    return;
                };
                let fanout = *local_track;
                let mut ctx = DispatchCtx {
                    registry: &mut self.registry,
                    dirty: &mut self.dirty,
                    router,
                    wall: &self.wall,
                };
                self.routing.apply_stats(fanout, stats, &mut ctx);
            }
        }
    }

    /// Act on a frame travelling back toward one of this shard's publishers.
    ///
    /// The route is the whole address: it names the publisher and the stream,
    /// and for a track it carries the encoding order, so the frame itself only
    /// has to say which layer and what it wants.
    fn on_reverse_frame(
        &mut self,
        env: RouteEnvelope,
        body: Reverse,
        router: &impl ShardTransport,
    ) {
        let (route, epoch) = (env.route, env.epoch);
        use crate::route::ReverseTarget;

        // Resolve fully before touching the registry: the target borrows the
        // route table, and dispatch needs the rest of `self` mutably.
        enum Act {
            Keyframe(TrackId, Option<Rid>, str0m::media::KeyframeRequestKind),
            Data(crate::track::Topic, Vec<u8>),
        }
        let (origin, act) = {
            let Some((origin, target)) = self.routing.resolve_reverse(route, epoch) else {
                // The stream was unpublished while this was in flight, or the
                // slot has been recycled. Both are expected under teardown.
                tracing::debug!(%route, "dropping a reverse frame on an unusable route");
                return;
            };
            let act = match (*target, body) {
                (ReverseTarget::Track { track }, Reverse::Keyframe { layer, kind }) => {
                    let Some((track_id, encodings)) = self.routing.track_descriptor(track) else {
                        debug_assert!(false, "a reverse frame's fanout key must resolve");
                        return;
                    };
                    let Some(rid) = encodings.get(usize::from(layer)).copied() else {
                        debug_assert!(false, "a reverse frame named an encoding the track lacks");
                        return;
                    };
                    Act::Keyframe(track_id, rid, kind)
                }
                (ReverseTarget::Track { .. }, Reverse::Nack { .. }) => {
                    // Nothing raises these yet; the route resolves, so the only
                    // missing piece is the retransmission path itself.
                    return;
                }
                (ReverseTarget::Topic { stream }, Reverse::DataAck(bytes)) => {
                    let Some(entry) = self.routing.reliable_stream(stream) else {
                        debug_assert!(false, "a reverse frame's fanout key must resolve");
                        return;
                    };
                    Act::Data(entry.id.topic.clone(), bytes)
                }
                (target, _) => {
                    debug_assert!(false, "reverse body does not match a {target:?} route");
                    return;
                }
            };
            (origin, act)
        };

        let mut ctx = DispatchCtx {
            registry: &mut self.registry,
            dirty: &mut self.dirty,
            router,
            wall: &self.wall,
        };
        match act {
            Act::Keyframe(track_id, rid, kind) => {
                ctx.notify_keyframe_request(origin, track_id, rid, kind);
            }
            Act::Data(topic, bytes) => {
                ctx.deliver_reliable_control(origin, &topic, &bytes);
            }
        }
    }

    fn add_participant(
        &mut self,
        cfg: ParticipantConfig,
        now: Instant,
        router: &impl ShardTransport,
    ) {
        let room_id = cfg.room_id;
        let participant_id = cfg.participant_id;
        self.remove_participant(&participant_id, now);
        let _ = router; // reserved: re-add currently needs no cross-shard notice
        let known_tracks = cfg.available_tracks.clone();
        let key = self.registry.insert(cfg, &mut self.rng);
        self.routing
            .add_local_member(participant_id, key, room_id, &mut self.rng);
        let Some(participant) = self.registry.get_mut(&participant_id) else {
            pulsebeam_runtime::fatal!(
                "registry accepted participant {participant_id} but cannot resolve it"
            )
        };
        self.dirty.mark(key, participant);

        // Tracks already published when this member arrived never went through
        // `publish_track` here, so their audio routes are installed now.
        let registry = &self.registry;
        let events = self.routing.adopt_known_tracks(
            room_id,
            &known_tracks,
            &|id| registry.contains(id),
            now,
            &self.wall,
        );
        for ev in events {
            self.pipeline.push_shard_event(ev);
        }
    }

    fn remove_participant(&mut self, participant_id: &ParticipantId, now: Instant) -> Option<()> {
        if let Some(key) = self.registry.key_of(participant_id) {
            self.timers.cancel(key);
        }
        let meta = self.registry.remove(participant_id)?;
        let audio_ids: Vec<_> = meta.upstream.audio_track_ids().collect();
        self.routing
            .remove_local_member(participant_id, meta.room_id, audio_ids, now);
        Some(())
    }
}

#[cfg(test)]
mod test {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
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
        pub sent: RefCell<Vec<(ShardId, ShardFrame)>>,
    }

    impl TestRouter {
        pub fn new() -> Self {
            Self {
                sent: RefCell::new(Vec::new()),
            }
        }

        pub fn take_sent(&self) -> Vec<(ShardId, ShardFrame)> {
            std::mem::take(&mut *self.sent.borrow_mut())
        }
    }

    impl ShardTransport for TestRouter {
        fn send_media(&self, dst: ShardId, env: MediaEnvelope, payload: MediaPayload) {
            self.sent
                .borrow_mut()
                .push((dst, ShardFrame::Media { env, payload }));
        }

        fn send_frame(&self, dst: ShardId, ev: ShardFrame) {
            self.sent.borrow_mut().push((dst, ev));
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
            ShardCommand::AddParticipant(Box::new(make_participant_cfg(participant_id, room_id))),
            now(),
            router,
        );
        router.take_sent();
    }

    fn now() -> Instant {
        Instant::now()
    }

    fn new_core() -> ShardCore {
        ShardCore::new(
            0,
            1,
            pulsebeam_runtime::rand::seeded_rng(42),
            WallAnchor::new(std::time::SystemTime::now(), Instant::now()),
        )
    }

    fn clear_dirty(core: &mut ShardCore) {
        core.dirty.begin_phase();
        while let Some(key) = core.dirty.next() {
            if let Some(participant) = core.registry.resolve_mut(key) {
                participant.queued_dirty = false;
            }
        }
        core.dirty.finish_phase();
    }

    #[test]
    fn add_participant_populates_registry_and_marks_dirty() {
        let router = TestRouter::new();
        let mut core = new_core();
        let p = pid();
        let r = room_id("add1");

        add_participant(&mut core, &router, p, r);

        assert!(core.registry.contains(&p));
        assert!(core.routing.has_room(&r));
        let mut core2 = new_core();
        core2.on_command(
            ShardCommand::AddParticipant(Box::new(make_participant_cfg(p, r))),
            now(),
            &router,
        );
        let key = core2.registry.key_of(&p).unwrap();
        assert!(
            core2.dirty.contains(key),
            "newly added participant must be dirty"
        );
    }

    #[test]
    fn remove_participant_clears_registry_and_room() {
        let router = TestRouter::new();
        let mut core = new_core();
        let p = pid();
        let r = room_id("leave1");

        add_participant(&mut core, &router, p, r);
        core.on_command(ShardCommand::RemoveParticipant(p), now(), &router);

        assert!(
            !core.registry.contains(&p),
            "participant must be gone from the registry"
        );
        assert!(
            !core.routing.has_room(&r),
            "last member leaving must remove the room"
        );
    }

    /// A stale dirty entry for a participant's *previous* incarnation must
    /// not be silently mistaken for the current one when both are queued at
    /// once — the property `removed_handle_never_resolves_to_replacement_with_same_id`
    /// pins for the registry directly, exercised here through the path that
    /// actually queues dirty entries.
    #[test]
    fn readding_dirty_participant_does_not_resolve_the_stale_incarnation() {
        let router = TestRouter::new();
        let mut core = new_core();
        let participant = pid();
        let room = room_id("readd-dirty");

        core.on_command(
            ShardCommand::AddParticipant(Box::new(make_participant_cfg(participant, room))),
            now(),
            &router,
        );
        core.on_command(
            ShardCommand::AddParticipant(Box::new(make_participant_cfg(participant, room))),
            now(),
            &router,
        );

        let current_key = core.registry.key_of(&participant).unwrap();
        core.dirty.begin_phase();
        let stale = core.dirty.next().unwrap();
        let current = core.dirty.next().unwrap();
        assert!(core.dirty.next().is_none());
        core.dirty.finish_phase();

        assert_ne!(stale, current, "the two incarnations must be distinct keys");
        assert_eq!(current, current_key);
        assert!(
            core.registry.resolve_mut(stale).is_none(),
            "the first incarnation's key must not resolve to the replacement"
        );
        assert!(core.registry.resolve_mut(current).is_some());
    }

    #[test]
    fn duplicate_register_participant_command_does_not_leak_remote_shard() {
        let router = TestRouter::new();
        let mut core = new_core();
        let participant = pid();
        let rid = room_id("no-leak");
        let remote_shard = ShardId::new(1);

        let register = || ShardCommand::RegisterParticipant {
            shard_id: remote_shard,
            room_id: rid,
            participant_id: participant,
        };

        // Simulate a redelivered/duplicate RegisterParticipant for the exact
        // same (participant, shard, room).
        core.on_command(register(), now(), &router);
        core.on_command(register(), now(), &router);

        core.on_command(
            ShardCommand::UnregisterParticipant {
                shard_id: remote_shard,
                room_id: rid,
                participant_id: participant,
            },
            now(),
            &router,
        );

        assert!(
            !core.routing.has_room(&rid),
            "one register (deduplicated) + one unregister must fully release the room; \
         a leaked refcount would leave a phantom remote_shards entry forever"
        );
    }

    #[test]
    fn register_remote_participant_keeps_shard_until_last_peer_leaves() {
        let router = TestRouter::new();
        let mut core = new_core();
        let a = pid();
        let b = pid();
        let rid = room_id("keep-shard");
        let remote_shard = ShardId::new(1);

        for participant in [a, b] {
            core.on_command(
                ShardCommand::RegisterParticipant {
                    shard_id: remote_shard,
                    room_id: rid,
                    participant_id: participant,
                },
                now(),
                &router,
            );
        }

        core.on_command(
            ShardCommand::UnregisterParticipant {
                shard_id: remote_shard,
                room_id: rid,
                participant_id: a,
            },
            now(),
            &router,
        );

        assert!(
            core.routing
                .room(&rid)
                .unwrap()
                .remote_shards
                .contains(&remote_shard),
            "shard must stay registered while participant b is still remote there"
        );

        core.on_command(
            ShardCommand::UnregisterParticipant {
                shard_id: remote_shard,
                room_id: rid,
                participant_id: b,
            },
            now(),
            &router,
        );

        assert!(
            !core.routing.has_room(&rid),
            "room must be removed once the final remote leaves"
        );
    }

    #[test]
    fn keyframe_request_from_a_peer_shard_marks_participant_dirty() {
        // Feedback reaches the publisher's shard directly, never through the
        // controller, so this is the only path a remote keyframe request takes.
        let router = TestRouter::new();
        let mut core = new_core();
        let p = pid();
        let r = room_id("kf1");
        add_participant(&mut core, &router, p, r);

        // The publisher's shard opens the reverse route; the frame carries only
        // that route, so nothing on the wire names the participant or track.
        let meta = video_track(p, 0);
        let descriptor = crate::track::Track {
            meta: meta.clone(),
            layers: vec![crate::track::TrackLayer {
                meta,
                rid: None,
                quality: crate::track::LayerQuality::High,
            }],
            reverse: None,
        };
        let target = core
            .routing
            .open_track_reverse_route(&descriptor, now(), &core.wall)
            .expect("a published track opens a reverse route");

        core.on_shard_frame(
            ShardFrame::Reverse {
                env: RouteEnvelope::new(target),
                body: Reverse::Keyframe {
                    layer: 0,
                    kind: str0m::media::KeyframeRequestKind::Pli,
                },
            },
            now(),
            &router,
        );

        assert!(
            core.dirty.contains(core.registry.key_of(&p).unwrap()),
            "keyframe delivery must dirty the target participant"
        );
    }

    #[test]
    fn cross_shard_rtp_published_marks_subscriber_dirty() {
        let router = TestRouter::new();
        let mut core = new_core();
        let publisher = pid();
        let subscriber = pid();
        let r = room_id("rtp1");
        add_participant(&mut core, &router, subscriber, r);

        core.on_command(
            ShardCommand::Relay {
                from_shard_id: ShardId::new(0),
                topology: Topology::TrackSubscribed {
                    track: video_track(publisher, 1),
                    route: crate::route::RouteId::new(0),
                    epoch: 0,
                },
            },
            now(),
            &router,
        );
        assert!(
            core.dirty
                .contains(core.registry.key_of(&subscriber).unwrap())
        );
        clear_dirty(&mut core);
        let subscribed = core
            .routing
            .register_subscriber(
                subscriber,
                video_track(publisher, 1),
                tokio::time::Instant::now(),
                &WallAnchor::new(std::time::SystemTime::now(), Instant::now()),
            )
            .expect("first subscriber installs a route");
        let ShardEvent::Relay(Topology::TrackSubscribed { route, epoch, .. }) = subscribed else {
            panic!("expected TrackSubscribed");
        };

        // Address the frame the way a remote publisher would: by the route the
        // destination just handed out, with no semantic ids on the wire, and
        // with playout stamped from the sender's NTP timeline.
        let pkt = crate::rtp::RtpPacket::default();
        let env = MediaEnvelope {
            epoch,
            route,
            link_seq: 0,
            playout_ntp32: core.wall().to_ntp(pkt.playout_time).middle32(),
        };
        core.on_shard_frame(
            ShardFrame::Media {
                env,
                payload: MediaPayload::Video(pkt),
            },
            tokio::time::Instant::now(),
            &router,
        );

        assert!(
            core.dirty
                .contains(core.registry.key_of(&subscriber).unwrap()),
            "forwarded RTP must dirty the subscriber"
        );
    }

    /// A sender's `Instant` values mean nothing on the receiving shard, and
    /// cross-node they will not be on the wire at all. The destination must
    /// rebuild both from what it owns: playout from the envelope's NTP, arrival
    /// from its own clock.
    #[test]
    fn an_arriving_frame_is_restamped_onto_the_destination_timeline() {
        let core = new_core();
        let mut pkt = crate::rtp::RtpPacket::default();

        // A plausible sender playout, and arrival/playout Instants that are
        // deliberately wrong for this shard.
        let playout = core
            .wall()
            .ntp()
            .wrapping_add(std::time::Duration::from_millis(120));
        let bogus = Instant::now() - std::time::Duration::from_secs(3600);
        pkt.playout_time = core.wall().to_instant(playout);
        pkt.arrival_ts = bogus;

        let now = Instant::now();
        core.restamp(&mut pkt, playout, now);

        assert_eq!(pkt.arrival_ts, now, "arrival must be the destination's own");
        assert_ne!(
            pkt.arrival_ts, bogus,
            "the sender's arrival must be discarded"
        );
        // Playout survives the NTP round trip within middle-32 resolution.
        let drift = core
            .wall()
            .to_ntp(pkt.playout_time)
            .units_since(playout)
            .unsigned_abs();
        assert!(
            drift <= 1 << 16,
            "playout must be rebuilt from the envelope, drifted {drift} units"
        );
    }

    #[test]
    fn fire_timers_does_not_spuriously_mark_participants() {
        let router = TestRouter::new();
        let mut core = new_core();
        let p = pid();
        let r = room_id("timer1");
        add_participant(&mut core, &router, p, r);
        clear_dirty(&mut core);

        core.fire_timers(tokio::time::Instant::now());
        assert!(!core.dirty.contains(core.registry.key_of(&p).unwrap()));

        core.on_command(ShardCommand::RemoveParticipant(p), now(), &router);
        assert!(!core.registry.contains(&p));
    }
}

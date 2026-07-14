use std::collections::VecDeque;

use ahash::{HashMap, HashMapExt};
use indexmap::IndexSet;
use pulsebeam_runtime::rand;
use str0m::media::KeyframeRequestKind;

use super::events::{ParticipantControlEvent, ParticipantTopologyEvent, RtpEvent};
use crate::audio_selector::TopNAudioSelector;
use crate::entity::{ParticipantId, RoomId, TrackId, TrackKind};
use crate::id::{AudioSelectorSlotId, ShardId};
use crate::rtp::RtpPacket;
use crate::track::{StreamId, Track, TrackMeta};

use super::worker::{CrossShardEvent, ShardEvent};

type FastIndexSet<T> = IndexSet<T, ahash::RandomState>;

fn fast_set<T>() -> FastIndexSet<T> {
    IndexSet::with_hasher(ahash::RandomState::default())
}

fn fast_set_with_capacity<T>(cap: usize) -> FastIndexSet<T> {
    IndexSet::with_capacity_and_hasher(cap, ahash::RandomState::default())
}

/// Abstraction over the cross-shard message bus. Implemented by the shard
/// worker's real router; faked in tests.
pub(crate) trait CrossShardSend {
    fn send(&self, shard_id: ShardId, ev: CrossShardEvent);
    fn shard_id(&self) -> ShardId;
}

pub(crate) trait RoutingContext: CrossShardSend {
    fn forward_video_rtp(
        &mut self,
        subscriber: ParticipantId,
        stream_id: &StreamId,
        pkt: &RtpPacket,
    );
    fn forward_audio_rtp(
        &mut self,
        subscriber: ParticipantId,
        slot_idx: AudioSelectorSlotId,
        pkt: &RtpPacket,
    );
    fn notify_tracks_published(&mut self, participant_id: ParticipantId, tracks: &[Track]);
    fn notify_tracks_unpublished(&mut self, participant_id: ParticipantId, track_ids: &[TrackId]);
    fn notify_keyframe_request(
        &mut self,
        participant_id: ParticipantId,
        stream_id: StreamId,
        kind: KeyframeRequestKind,
    );
    fn is_local(&self, id: &ParticipantId) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParticipantShardMeta {
    pub shard_id: ShardId,
    pub room_id: RoomId,
}

pub(crate) struct ShardRoomContext {
    pub members: FastIndexSet<ParticipantId>,
    pub remote_shards: FastIndexSet<ShardId>,
    pub audio_selector: TopNAudioSelector,
}

impl ShardRoomContext {
    fn new(rng: &mut impl rand::RngCore) -> Self {
        Self {
            members: fast_set(),
            remote_shards: fast_set(),
            audio_selector: TopNAudioSelector::new(rng),
        }
    }
}

pub(crate) struct TrackRoute {
    pub kind: TrackKind,
    pub subscribers: FastIndexSet<ParticipantId>,
    pub remote_shards: FastIndexSet<ShardId>,
}

impl TrackRoute {
    fn new(kind: TrackKind) -> Self {
        Self {
            kind,
            subscribers: fast_set_with_capacity(256),
            remote_shards: fast_set(),
        }
    }
}

/// Pure pub/sub state for a shard: which participants are in which rooms,
/// which shards subscribe to which tracks, and where remote participants
/// live.
pub(crate) struct ShardRoutingTable {
    pub rooms: HashMap<RoomId, ShardRoomContext>,
    pub tracks: HashMap<TrackId, TrackRoute>,
    participant_shards: HashMap<ParticipantId, ParticipantShardMeta>,
    remote_participant_counts: HashMap<(RoomId, ShardId), usize>,
}

impl ShardRoutingTable {
    pub fn new() -> Self {
        Self {
            rooms: HashMap::new(),
            tracks: HashMap::new(),
            participant_shards: HashMap::new(),
            remote_participant_counts: HashMap::new(),
        }
    }

    // -- local room membership -------------------------------------------

    pub fn add_local_member(
        &mut self,
        participant_id: ParticipantId,
        room_id: RoomId,
        rng: &mut impl rand::RngCore,
    ) {
        self.rooms
            .entry(room_id)
            .or_insert_with(|| ShardRoomContext::new(rng))
            .members
            .insert(participant_id);
    }

    /// Removes a local participant from its room and evicts its audio
    /// tracks from the room's selector. Cleans up the room entry if it's
    /// now empty of both local and remote members.
    pub fn remove_local_member(
        &mut self,
        participant_id: &ParticipantId,
        room_id: RoomId,
        audio_track_ids: impl IntoIterator<Item = TrackId>,
    ) {
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return;
        };
        room.members.swap_remove(participant_id);
        for id in audio_track_ids {
            room.audio_selector.remove_track((id, None));
        }
        if room.members.is_empty() && room.remote_shards.is_empty() {
            self.rooms.remove(&room_id);
        }
    }

    pub fn remote_shard_for(&self, participant_id: &ParticipantId) -> Option<ShardId> {
        self.participant_shards
            .get(participant_id)
            .map(|m| m.shard_id)
    }

    // -- remote participant membership (refcounted per room/shard) -------

    /// Idempotent: re-registering a participant with the same (room, shard)
    /// it's already registered under is a no-op and does NOT bump the
    /// refcount. This matters — a duplicate/redelivered register message
    /// must not desync the count from the number of real registrations,
    /// or `unregister` can never bring it back to zero.
    pub fn register_remote_participant(
        &mut self,
        participant_id: ParticipantId,
        room_id: RoomId,
        shard_id: ShardId,
        rng: &mut impl rand::RngCore,
    ) {
        let meta = ParticipantShardMeta { shard_id, room_id };

        if self.participant_shards.get(&participant_id).copied() == Some(meta) {
            return;
        }

        if let Some(previous) = self.participant_shards.remove(&participant_id) {
            self.release_remote_count(previous);
        }

        self.participant_shards.insert(participant_id, meta);
        self.rooms
            .entry(room_id)
            .or_insert_with(|| ShardRoomContext::new(rng))
            .remote_shards
            .insert(shard_id);
        *self
            .remote_participant_counts
            .entry((room_id, shard_id))
            .or_insert(0) += 1;
    }

    pub fn unregister_remote_participant(
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
        self.release_remote_count(current);
    }

    fn release_remote_count(&mut self, meta: ParticipantShardMeta) {
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

        if !should_remove_shard {
            return;
        }

        if let Some(room) = self.rooms.get_mut(&meta.room_id) {
            room.remote_shards.swap_remove(&meta.shard_id);
            if room.members.is_empty() && room.remote_shards.is_empty() {
                self.rooms.remove(&meta.room_id);
            }
        }
    }

    // -- track subscription topology (local subscribers) -----------------

    /// Registers a local subscriber for `track`. Returns a `ShardEvent` iff
    /// this is the *first* subscriber, so the caller can notify the
    /// publisher shard to start forwarding.
    pub fn register_subscriber(
        &mut self,
        subscriber: ParticipantId,
        track: TrackMeta,
    ) -> Option<ShardEvent> {
        let entry = self
            .tracks
            .entry(track.id)
            .or_insert_with(|| TrackRoute::new(track.id.kind()));
        let was_empty = entry.subscribers.is_empty();
        entry.subscribers.insert(subscriber);
        was_empty.then_some(ShardEvent::TrackSubscribed(track))
    }

    /// Returns a `ShardEvent` iff this was the *last* local subscriber, so
    /// the caller can tell the publisher shard to stop forwarding.
    pub fn unregister_subscriber(
        &mut self,
        subscriber: ParticipantId,
        track: TrackMeta,
    ) -> Option<ShardEvent> {
        let entry = self.tracks.get_mut(&track.id)?;
        entry.subscribers.swap_remove(&subscriber);
        entry
            .subscribers
            .is_empty()
            .then_some(ShardEvent::TrackUnsubscribed(track))
    }

    pub fn handle_topology_event(&mut self, ev: ParticipantTopologyEvent) -> Option<ShardEvent> {
        match ev {
            ParticipantTopologyEvent::TrackSubscribed { track, subscriber } => {
                self.register_subscriber(subscriber, track)
            }
            ParticipantTopologyEvent::TrackUnsubscribed { track, subscriber } => {
                self.unregister_subscriber(subscriber, track)
            }
        }
    }

    // -- track subscription topology (remote shards) ---------------------

    pub fn register_remote_subscriber_shard(&mut self, from_shard_id: ShardId, track: TrackMeta) {
        self.tracks
            .entry(track.id)
            .or_insert_with(|| TrackRoute::new(track.id.kind()))
            .remote_shards
            .insert(from_shard_id);
    }

    pub fn unregister_remote_subscriber_shard(&mut self, from_shard_id: ShardId, track: TrackMeta) {
        if let Some(route) = self.tracks.get_mut(&track.id) {
            route.remote_shards.swap_remove(&from_shard_id);
        }
    }

    // -- track publish / unpublish ----------------------------------------

    pub fn publish_track(&self, track: Track, room_id: RoomId, ctx: &mut impl RoutingContext) {
        let publisher = track.meta.origin;
        let Some(room) = self.rooms.get(&room_id) else {
            tracing::debug!(%room_id, "publish_track: room missing on this shard");
            return;
        };
        let tracks = std::slice::from_ref(&track);
        for &participant_id in &room.members {
            if participant_id == publisher {
                continue;
            }
            ctx.notify_tracks_published(participant_id, tracks);
        }
    }

    pub fn unpublish_tracks(
        &mut self,
        room_id: RoomId,
        track_ids: &[TrackId],
        ctx: &mut impl RoutingContext,
    ) {
        if let Some(room) = self.rooms.get_mut(&room_id) {
            for &track_id in track_ids {
                room.audio_selector.remove_track((track_id, None));
            }
        }
        let Some(room) = self.rooms.get(&room_id) else {
            tracing::debug!(%room_id, "unpublish_tracks: room missing on this shard");
            return;
        };
        for &participant_id in &room.members {
            ctx.notify_tracks_unpublished(participant_id, track_ids);
        }
    }

    // -- hot-path packet fanout --------------------------------------------

    #[inline]
    pub fn route_video(&self, stream_id: StreamId, pkt: &RtpPacket, ctx: &mut impl RoutingContext) {
        let Some(route) = self.tracks.get(&stream_id.0) else {
            return;
        };
        for &subscriber_id in &route.subscribers {
            ctx.forward_video_rtp(subscriber_id, &stream_id, pkt);
        }
        for &shard_id in &route.remote_shards {
            ctx.send(
                shard_id,
                CrossShardEvent::VideoRtpPublished {
                    stream_id,
                    pkt: pkt.deep_clone(),
                },
            );
        }
    }

    #[inline]
    pub fn route_audio(&mut self, mut ev: RtpEvent, ctx: &mut impl RoutingContext) {
        tracing::trace!(
            target: crate::log::TARGET_AUDIO,
            room_id = %ev.room_id,
            origin = %ev.origin,
            stream_id = %ev.stream_id.0,
            seq_no = %ev.pkt.seq_no,
            "audio packet entered shard audio fanout"
        );

        let Some(room) = self.rooms.get_mut(&ev.room_id) else {
            tracing::warn!(target: crate::log::TARGET_AUDIO, room_id = %ev.room_id, "audio packet dropped: room missing");
            return;
        };

        if ctx.is_local(&ev.origin) {
            for &shard_id in &room.remote_shards {
                ctx.send(
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

        let Some(slot_idx) = room.audio_selector.filter(ev.stream_id, &mut ev.pkt) else {
            return;
        };
        for &participant_id in &room.members {
            if participant_id == ev.origin {
                continue;
            }
            ctx.forward_audio_rtp(participant_id, slot_idx, &ev.pkt);
        }
    }
}

// -- participant-originated control-event routing -------------------------

/// Routes an event a participant raised about itself (published a track,
/// wants a keyframe, ...) either into the local `shard_events` queue or
/// across the cluster bus, depending on where it needs to land.
pub(crate) fn route_participant_control_event(
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
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet as StdHashSet;

    use crate::entity::ExternalRoomId;

    /// A `RoutingContext` fake that just records calls. No `ParticipantCore`,
    /// no tracing spans, no `ShardCore` — this is the whole point of the
    /// trait boundary.
    #[derive(Default)]
    struct RecordingCtx {
        shard_id: ShardId,
        local: StdHashSet<ParticipantId>,
        sent: RefCell<Vec<(ShardId, CrossShardEvent)>>,
        forwarded_video: RefCell<Vec<ParticipantId>>,
        forwarded_audio: RefCell<Vec<(ParticipantId, AudioSelectorSlotId)>>,
        published: RefCell<Vec<ParticipantId>>,
        unpublished: RefCell<Vec<ParticipantId>>,
        keyframed: RefCell<Vec<ParticipantId>>,
    }

    impl CrossShardSend for RecordingCtx {
        fn send(&self, shard_id: ShardId, ev: CrossShardEvent) {
            self.sent.borrow_mut().push((shard_id, ev));
        }
        fn shard_id(&self) -> ShardId {
            self.shard_id
        }
    }

    impl RoutingContext for RecordingCtx {
        fn forward_video_rtp(
            &mut self,
            subscriber: ParticipantId,
            _stream_id: &StreamId,
            _pkt: &RtpPacket,
        ) {
            self.forwarded_video.borrow_mut().push(subscriber);
        }
        fn forward_audio_rtp(
            &mut self,
            subscriber: ParticipantId,
            slot_idx: AudioSelectorSlotId,
            _pkt: &RtpPacket,
        ) {
            self.forwarded_audio
                .borrow_mut()
                .push((subscriber, slot_idx));
        }
        fn notify_tracks_published(&mut self, participant_id: ParticipantId, _tracks: &[Track]) {
            self.published.borrow_mut().push(participant_id);
        }
        fn notify_tracks_unpublished(
            &mut self,
            participant_id: ParticipantId,
            _track_ids: &[TrackId],
        ) {
            self.unpublished.borrow_mut().push(participant_id);
        }
        fn notify_keyframe_request(
            &mut self,
            participant_id: ParticipantId,
            _stream_id: StreamId,
            _kind: KeyframeRequestKind,
        ) {
            self.keyframed.borrow_mut().push(participant_id);
        }
        fn is_local(&self, id: &ParticipantId) -> bool {
            self.local.contains(id)
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

    // -- the bug this refactor exists to prevent recurring ------------------

    #[test]
    fn duplicate_register_remote_participant_does_not_leak_refcount() {
        let mut table = ShardRoutingTable::new();
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let participant = pid();
        let room = room_id("r1");
        let shard = ShardId::new(1);

        table.register_remote_participant(participant, room, shard, &mut rng);
        // Redelivered / duplicate register for the exact same (room, shard).
        table.register_remote_participant(participant, room, shard, &mut rng);

        // A single unregister must be enough to fully release the shard —
        // if the duplicate register above had bumped the refcount, this
        // would leave a phantom `remote_shards` entry forever.
        table.unregister_remote_participant(
            participant,
            ParticipantShardMeta {
                shard_id: shard,
                room_id: room,
            },
        );

        assert!(
            !table.rooms.contains_key(&room),
            "room must be fully cleaned up after one register + one unregister"
        );
    }

    #[test]
    fn moving_remote_participant_releases_the_old_shard() {
        let mut table = ShardRoutingTable::new();
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let participant = pid();
        let room = room_id("r2");
        let old_shard = ShardId::new(1);
        let new_shard = ShardId::new(2);

        table.register_remote_participant(participant, room, old_shard, &mut rng);
        table.register_remote_participant(participant, room, new_shard, &mut rng);

        assert!(!table.rooms[&room].remote_shards.contains(&old_shard));
        assert!(table.rooms[&room].remote_shards.contains(&new_shard));
    }

    // -- topology ------------------------------------------------------------

    #[test]
    fn first_subscriber_notifies_publisher_shard() {
        let mut table = ShardRoutingTable::new();
        let track = TrackMeta {
            shard_id: ShardId::new(1),
            id: pid().derive_track_id(TrackKind::Video, "v"),
            origin: pid(),
        };

        let ev = table.register_subscriber(pid(), track.clone());
        assert!(matches!(ev, Some(ShardEvent::TrackSubscribed(t)) if t == track));

        let ev2 = table.register_subscriber(pid(), track);
        assert!(ev2.is_none(), "second subscriber must not re-notify");
    }

    // -- fanout ---------------------------------------------------------------

    #[test]
    fn route_video_forwards_to_subscribers_and_remote_shards() {
        let mut table = ShardRoutingTable::new();
        let track_id = pid().derive_track_id(TrackKind::Video, "v");
        let stream_id: StreamId = (track_id, None);
        let subscriber = pid();

        table.register_subscriber(
            subscriber,
            TrackMeta {
                shard_id: ShardId::new(0),
                id: track_id,
                origin: pid(),
            },
        );
        table
            .tracks
            .get_mut(&track_id)
            .unwrap()
            .remote_shards
            .insert(ShardId::new(3));

        let mut ctx = RecordingCtx {
            shard_id: ShardId::new(0),
            ..Default::default()
        };
        table.route_video(stream_id, &RtpPacket::default(), &mut ctx);

        assert_eq!(ctx.forwarded_video.borrow().as_slice(), &[subscriber]);
        assert_eq!(ctx.sent.borrow().len(), 1);
    }
}

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};

use pulsebeam_runtime::net::RecvPacketBatch;
use slotmap::SecondaryMap;

use crate::{
    id::ShardId,
    participant::{ParticipantConfig, ParticipantCore},
    route::TransportHandle,
    shard::demux::Demuxer,
};

pub(crate) use crate::keys::ParticipantKey;

pub(crate) struct ParticipantMeta {
    core: ParticipantCore,
    pub(super) queued_dirty: bool,
    pub(super) ingress: TransportHandle,
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

pub(crate) struct ParticipantRegistry {
    shard_id: ShardId,
    max_gso_segments: usize,
    /// Boxed, and that is the point.
    ///
    /// `SecondaryMap` is a dense `Vec` indexed by the key, so its element size
    /// is its stride. `ParticipantMeta` is ~10.9KB — three quarters of it
    /// str0m's `Rtc` — and growing the map `Vec::extend`s, which reallocates
    /// and memcpies every participant already in it. On a shard filling to 500
    /// that is ~2.7MB copied in one go, on a `SCHED_FIFO` thread, while media
    /// is flowing. A pointer costs 16 bytes of stride instead of 10,904, so the
    /// same growth moves 8KB.
    ///
    /// The indirection is free where it matters: resolving a participant always
    /// missed on a 10.9KB object anyway, and the pointer array it now goes
    /// through is dense enough to stay resident (500 participants = 8KB).
    participants: SecondaryMap<ParticipantKey, Box<ParticipantMeta>>,
    demuxer: Demuxer,
    pending_close: VecDeque<SocketAddr>,
}

impl ParticipantRegistry {
    pub fn len(&self) -> usize {
        self.participants.len()
    }

    pub fn new(shard_id: ShardId, max_gso_segments: usize, shard_count: u16) -> Self {
        debug_assert!(shard_count > 0);
        Self {
            shard_id,
            max_gso_segments,
            participants: SecondaryMap::new(),
            demuxer: Demuxer::for_node(0, 0, shard_count),
            pending_close: VecDeque::new(),
        }
    }

    pub fn insert(
        &mut self,
        key: ParticipantKey,
        cfg: ParticipantConfig,
        ingress: TransportHandle,
    ) -> bool {
        debug_assert_eq!(ingress.shard(), self.shard_id);
        let participant_id = cfg.participant_id;
        let core = ParticipantCore::new(cfg, self.shard_id, self.max_gso_segments, 1);
        if self.participants.contains_key(key) {
            debug_assert!(false, "duplicate participant materialization");
            return false;
        }
        let previous = self.participants.insert(
            key,
            Box::new(ParticipantMeta {
                core,
                queued_dirty: false,
                ingress,
            }),
        );
        debug_assert!(previous.is_none());
        tracing::info!(%participant_id, "participant added to shard");
        true
    }

    pub fn remove_key(&mut self, key: ParticipantKey) -> Option<Box<ParticipantMeta>> {
        let meta = self.participants.remove(key)?;
        let addrs = self.demuxer.unregister(meta.ingress.route);
        self.pending_close.extend(addrs);
        Some(meta)
    }

    pub fn resolve_mut(&mut self, key: ParticipantKey) -> Option<&mut ParticipantMeta> {
        self.participants.get_mut(key).map(Box::as_mut)
    }

    pub fn publish_track_to(&mut self, track: &crate::track::Track, audience: &[ParticipantKey]) {
        for &key in audience {
            if let Some(participant) = self.participants.get_mut(key) {
                participant.on_tracks_published(std::slice::from_ref(track));
            }
        }
    }

    pub fn unpublish_track_to(
        &mut self,
        track_id: &crate::entity::TrackId,
        audience: &[ParticipantKey],
    ) {
        for &key in audience {
            if let Some(participant) = self.participants.get_mut(key) {
                participant.on_tracks_unpublished(std::slice::from_ref(track_id));
            }
        }
    }

    pub fn bind_subscribed_track(
        &mut self,
        participant: ParticipantKey,
        track_id: crate::entity::TrackId,
        fanout: crate::shard::router::TrackKey,
    ) {
        if let Some(participant) = self.resolve_mut(participant) {
            participant.bind_subscribed_track(track_id, fanout);
        }
    }

    pub fn unbind_subscribed_track(
        &mut self,
        participant: ParticipantKey,
        track_id: crate::entity::TrackId,
        fanout: crate::shard::router::TrackKey,
    ) {
        if let Some(participant) = self.resolve_mut(participant) {
            participant.unbind_subscribed_track(track_id, fanout);
        }
    }

    pub fn demux(&mut self, batch: &RecvPacketBatch) -> Option<TransportHandle> {
        self.demuxer.demux(batch)
    }

    /// Cache an address a sibling shard resolved on this shard's behalf.
    ///
    /// Steering is a cache, and populating it moves a flow from the shard the
    /// tuple hash picked to the shard that owns the route — which has never
    /// seen the flow's STUN and so cannot classify anything that follows it.
    /// Learning the address while forwarding is still happening is what makes
    /// that handover lossless.
    pub fn learn_addr(&mut self, src: SocketAddr, handle: TransportHandle) {
        self.demuxer.learn(src, handle);
    }

    pub fn authenticate_addr(&mut self, src: SocketAddr, handle: TransportHandle) {
        self.demuxer.authenticate(src, handle);
    }

    /// The route a participant's authenticated address belongs to.
    ///
    /// Reports the route handle so the shard can tell control which flow to
    /// pin in the steering map.
    pub fn authenticated_handle(&self, key: ParticipantKey) -> Option<TransportHandle> {
        let Some(meta) = self.participants.get(key) else {
            debug_assert!(false, "authenticated participant must still be registered");
            return None;
        };
        Some(meta.ingress)
    }

    pub fn drain_pending_close(&mut self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.pending_close.drain(..)
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;

    fn value_size<K: slotmap::Key, V>(_: &SecondaryMap<K, V>) -> usize {
        std::mem::size_of::<V>()
    }

    /// The registry's element stride is a pointer, not a participant.
    ///
    /// `SecondaryMap` is a dense `Vec` indexed by the key, so whatever it holds
    /// is what gets memcpied every time the map grows — on the shard's
    /// `SCHED_FIFO` thread, with media flowing. Storing the participant inline
    /// made that ~2.7MB in one go at 500 participants; a pointer makes it 8KB.
    ///
    /// This is a stride check rather than a style check: if `ParticipantMeta`
    /// ever shrinks to something a `Vec` can carry, the indirection can go and
    /// this assertion is the place to reconsider it.
    #[test]
    fn the_registry_holds_participants_behind_a_pointer() {
        let registry = ParticipantRegistry::new(ShardId::new(0), 1, 1);
        assert_eq!(
            value_size(&registry.participants),
            std::mem::size_of::<usize>(),
            "the participant registry must store a pointer per slot"
        );
        assert!(
            std::mem::size_of::<ParticipantMeta>() > 4096,
            "a participant is small enough to inline now; revisit the Box and this test"
        );
    }
}

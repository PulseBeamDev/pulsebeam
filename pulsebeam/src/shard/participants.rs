use std::collections::VecDeque;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};

use ahash::HashMap;
use pulsebeam_runtime::net::RecvPacketBatch;
use pulsebeam_runtime::rand::{Rng, RngCore, SeedableRng};
use slotmap::{SlotMap, new_key_type};

use crate::{
    entity::ParticipantId,
    id::ShardId,
    participant::{ParticipantConfig, ParticipantCore},
    shard::demux::Demuxer,
};

new_key_type! {
    pub(crate) struct LocalParticipantKey;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ParticipantHandle {
    key: LocalParticipantKey,
    participant_id: ParticipantId,
    generation: u64,
}

impl ParticipantHandle {
    pub(super) fn new(
        key: LocalParticipantKey,
        participant_id: ParticipantId,
        generation: u64,
    ) -> Self {
        Self {
            key,
            participant_id,
            generation,
        }
    }

    pub(super) fn key(self) -> LocalParticipantKey {
        self.key
    }

    pub fn participant_id(self) -> ParticipantId {
        self.participant_id
    }

    pub fn generation(self) -> u64 {
        self.generation
    }
}

pub(crate) struct ParticipantMeta {
    core: ParticipantCore,
    pub(super) queued_dirty: bool,
    pub(super) generation: u64,
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
    participants: SlotMap<LocalParticipantKey, ParticipantMeta>,
    participant_keys: HashMap<ParticipantId, LocalParticipantKey>,
    next_generation: u64,
    demuxer: Demuxer,
    /// Addresses freed by a removal/unregister, waiting for the worker to
    /// actually close the sockets during the output phase.
    pending_close: VecDeque<SocketAddr>,
}

impl ParticipantRegistry {
    pub fn new(shard_id: ShardId, max_gso_segments: usize) -> Self {
        Self {
            shard_id,
            max_gso_segments,
            participants: SlotMap::with_key(),
            participant_keys: HashMap::default(),
            next_generation: 1,
            demuxer: Demuxer::new(shard_id.into()),
            pending_close: VecDeque::new(),
        }
    }

    pub fn insert(&mut self, cfg: ParticipantConfig, rng: &mut Rng) -> ParticipantId {
        let participant_id = cfg.participant_id;
        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("participant generation exhausted");
        debug_assert_ne!(generation, 0);
        let mut participant_rng = Rng::seed_from_u64(rng.next_u64());
        let core = ParticipantCore::new(
            cfg,
            self.shard_id,
            self.max_gso_segments,
            1,
            &mut participant_rng,
        );
        let previous = self.participant_keys.get(&participant_id);
        debug_assert!(
            previous.is_none(),
            "duplicate participant registry insertion"
        );
        let key = self.participants.insert(ParticipantMeta {
            core,
            queued_dirty: false,
            generation,
        });
        let previous = self.participant_keys.insert(participant_id, key);
        debug_assert!(previous.is_none());
        tracing::info!(%participant_id, "participant added to shard");
        participant_id
    }

    /// Removes a local participant and queues its addresses for closing.
    /// Returns the removed state so the caller can read final fields
    /// (room_id, upstream track ids) before it's dropped.
    pub fn remove(&mut self, id: &ParticipantId) -> Option<ParticipantMeta> {
        let key = self.participant_keys.remove(id)?;
        let meta = self
            .participants
            .remove(key)
            .expect("participant ID mapped to a missing local slot");
        debug_assert_eq!(meta.participant_id, *id);
        let addrs = self.demuxer.unregister(*id);
        self.pending_close.extend(addrs);
        Some(meta)
    }

    /// Frees demux entries for a participant that lives on a *different*
    /// shard (used when a remote registration is torn down) — there's no
    /// local `ParticipantMeta` to remove, just stale routing state.
    pub fn unregister_remote_demux(&mut self, id: ParticipantId) {
        let addrs = self.demuxer.unregister(id);
        self.pending_close.extend(addrs);
    }

    pub fn get_mut(&mut self, id: &ParticipantId) -> Option<&mut ParticipantMeta> {
        let key = *self.participant_keys.get(id)?;
        let participant = self.participants.get_mut(key)?;
        debug_assert_eq!(participant.participant_id, *id);
        Some(participant)
    }

    pub fn get_mut_with_handle(
        &mut self,
        id: &ParticipantId,
    ) -> Option<(ParticipantHandle, &mut ParticipantMeta)> {
        let key = *self.participant_keys.get(id)?;
        let participant = self.participants.get_mut(key)?;
        debug_assert_eq!(participant.participant_id, *id);
        let handle = ParticipantHandle::new(key, *id, participant.generation);
        Some((handle, participant))
    }

    #[cfg(test)]
    pub fn get(&self, id: &ParticipantId) -> Option<&ParticipantMeta> {
        let key = *self.participant_keys.get(id)?;
        let participant = self.participants.get(key)?;
        debug_assert_eq!(participant.participant_id, *id);
        Some(participant)
    }

    pub fn contains(&self, id: &ParticipantId) -> bool {
        self.participant_keys.contains_key(id)
    }

    pub fn handle(&self, id: &ParticipantId) -> Option<ParticipantHandle> {
        let key = *self.participant_keys.get(id)?;
        let participant = self.participants.get(key)?;
        debug_assert_eq!(participant.participant_id, *id);
        Some(ParticipantHandle::new(key, *id, participant.generation))
    }

    pub fn resolve_mut(&mut self, handle: ParticipantHandle) -> Option<&mut ParticipantMeta> {
        let participant = self.participants.get_mut(handle.key)?;
        if participant.participant_id != handle.participant_id
            || participant.generation != handle.generation
        {
            debug_assert!(false, "stale local participant handle");
            return None;
        }
        Some(participant)
    }

    pub fn demux(&mut self, batch: &RecvPacketBatch) -> Option<ParticipantId> {
        self.demuxer.demux(batch)
    }

    pub fn drain_pending_close(&mut self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.pending_close.drain(..)
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core and a fixture may read the host clock.
    // See docs/thread-per-core.md.
    #![allow(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::float_cmp
    )]
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::entity::ExternalRoomId;

    fn pid() -> ParticipantId {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        ParticipantId::new(&mut pulsebeam_runtime::rand::seeded_rng(
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ))
    }

    fn room_id(s: &str) -> crate::entity::RoomId {
        crate::entity::RoomId::from_external(&ExternalRoomId::new(s).unwrap())
    }

    fn cfg(participant_id: ParticipantId, room_id: crate::entity::RoomId) -> ParticipantConfig {
        ParticipantConfig {
            manual_sub: false,
            room_id,
            participant_id,
            rtc: str0m::RtcConfig::new().build(std::time::Instant::now()),
            available_tracks: vec![],
        }
    }

    #[test]
    fn insert_then_contains() {
        let mut registry = ParticipantRegistry::new(ShardId::new(0), 1);
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let p = pid();

        let returned_id = registry.insert(cfg(p, room_id("r1")), &mut rng);

        assert_eq!(returned_id, p);
        assert!(registry.contains(&p));
        assert!(registry.get_mut(&p).is_some());
    }

    #[test]
    fn remove_missing_participant_returns_none() {
        let mut registry = ParticipantRegistry::new(ShardId::new(0), 1);
        assert!(registry.remove(&pid()).is_none());
    }

    #[test]
    fn remove_present_participant_clears_contains() {
        let mut registry = ParticipantRegistry::new(ShardId::new(0), 1);
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let p = pid();
        registry.insert(cfg(p, room_id("r2")), &mut rng);

        let removed = registry.remove(&p);

        assert!(
            removed.is_some(),
            "must return the removed participant's state"
        );
        assert!(
            !registry.contains(&p),
            "participant must be gone after remove"
        );
        assert!(registry.get_mut(&p).is_none());
    }

    #[test]
    fn remove_is_idempotent_second_call_is_none() {
        let mut registry = ParticipantRegistry::new(ShardId::new(0), 1);
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let p = pid();
        registry.insert(cfg(p, room_id("r3")), &mut rng);

        assert!(registry.remove(&p).is_some());
        assert!(
            registry.remove(&p).is_none(),
            "removing an already-removed participant must be a safe no-op, not panic"
        );
    }

    #[test]
    fn removed_handle_never_resolves_to_replacement_with_same_id() {
        let mut registry = ParticipantRegistry::new(ShardId::new(0), 1);
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let participant_id = pid();
        registry.insert(cfg(participant_id, room_id("r5")), &mut rng);
        let removed_handle = registry.handle(&participant_id).unwrap();

        assert!(registry.remove(&participant_id).is_some());
        registry.insert(cfg(participant_id, room_id("r5")), &mut rng);
        let replacement_handle = registry.handle(&participant_id).unwrap();

        assert_ne!(removed_handle, replacement_handle);
        assert!(registry.resolve_mut(removed_handle).is_none());
        assert!(registry.resolve_mut(replacement_handle).is_some());
    }

    #[test]
    fn unregister_remote_demux_does_not_touch_local_participants() {
        // A remote-registration teardown has no local ParticipantMeta to
        // remove — this must be a pure demux-table operation and must not
        // panic or affect any locally-registered participant.
        let mut registry = ParticipantRegistry::new(ShardId::new(0), 1);
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let local = pid();
        let remote = pid();
        registry.insert(cfg(local, room_id("r4")), &mut rng);

        registry.unregister_remote_demux(remote);

        assert!(
            registry.contains(&local),
            "unrelated local participant must be unaffected"
        );
        assert!(
            !registry.contains(&remote),
            "remote id was never local to begin with"
        );
    }
}

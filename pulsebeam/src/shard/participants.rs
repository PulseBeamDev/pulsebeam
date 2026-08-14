use std::collections::VecDeque;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};

use ahash::HashMap;
use pulsebeam_runtime::net::RecvPacketBatch;
use pulsebeam_runtime::rand::{Rng, RngCore, SeedableRng};
use slotmap::{SlotMap, new_key_type};

use crate::shard::router::RoomKey;
use crate::{
    entity::ParticipantId,
    id::ShardId,
    participant::{ParticipantConfig, ParticipantCore},
    route::{TransportHandle, TransportRoute},
    shard::demux::Demuxer,
};

new_key_type! {
    /// A local participant's slot on this shard. Dense, `Copy`, and
    /// meaningless outside the shard that issued it — the same rule every
    /// other arena key in `shard/` follows.
    ///
    /// Bare: a slotmap key already carries a version distinguishing a slot's
    /// current occupant from whoever held it before, so there is nothing left
    /// to pack alongside it. The 32-byte `ParticipantHandle` wrapper this
    /// replaced re-implemented that version as its own `generation: u64` and
    /// carried `participant_id` for lookups that never needed a name —
    /// `resolve_mut` and friends check the key against the arena, not a
    /// cached copy of who used to be there.
    pub struct ParticipantKey;
}

pub(crate) struct ParticipantMeta {
    core: ParticipantCore,
    pub(super) queued_dirty: bool,
    /// This connection's ICE association, so teardown can retire the route
    /// and free the demuxer's cache entries for it. Route and key share a
    /// lifetime by construction, but the route table and the demuxer are
    /// reached separately, so both need the value.
    pub(super) ingress_route: TransportRoute,
    /// This participant's room, already compiled. Set when it joins, so
    /// nothing on the packet path hashes a `RoomId` to find the fanout it
    /// belongs to.
    pub(super) room_key: RoomKey,
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
    /// `None` between `reserve` and `populate` — an ingress route may already
    /// point at the key while connection setup (ICE/DTLS negotiation) is
    /// still in flight, so the slot has to exist before `ParticipantCore`
    /// does. Every lookup here treats a reserved-but-unpopulated slot the
    /// same as a missing one.
    participants: SlotMap<ParticipantKey, Option<ParticipantMeta>>,
    participant_keys: HashMap<ParticipantId, ParticipantKey>,
    /// The ingress route for a key between `reserve` and `populate` — the
    /// route is installed (via `install_ingress_route`) in a separate step
    /// from `reserve`, itself separate from `populate`, because the ufrag
    /// built from the route has to exist before negotiation can produce the
    /// `ParticipantConfig` `populate` needs. Entries here never outlive a
    /// reservation: `populate` consumes one, `release_reserved` drops it.
    pending_ingress: HashMap<ParticipantKey, TransportHandle>,
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
            pending_ingress: HashMap::default(),
            demuxer: Demuxer::new(),
            pending_close: VecDeque::new(),
        }
    }

    /// Reserve a slot for a connection whose ICE/DTLS setup is still in
    /// flight. The key is real immediately — `install_ingress_route` can
    /// address it — but `resolve_mut` returns `None` until `populate` fills
    /// it in, or the reservation is abandoned via `release_reserved`.
    pub fn reserve(&mut self, participant_id: ParticipantId) -> ParticipantKey {
        debug_assert!(
            !self.participant_keys.contains_key(&participant_id),
            "duplicate participant registry reservation"
        );
        let key = self.participants.insert(None);
        let previous = self.participant_keys.insert(participant_id, key);
        debug_assert!(previous.is_none());
        key
    }

    /// Record the route `install_ingress_route` installed for a reserved
    /// key, so `populate` can carry it onto the finished `ParticipantMeta`
    /// once negotiation completes.
    pub fn stash_ingress(&mut self, key: ParticipantKey, handle: TransportHandle) {
        debug_assert!(
            self.participants.get(key).is_some_and(Option::is_none),
            "stash_ingress called on a key that isn't a bare reservation"
        );
        self.pending_ingress.insert(key, handle);
    }

    /// Fill in a slot `reserve` minted, once negotiation completes. Consumes
    /// the route `stash_ingress` recorded; a key populated without ever
    /// having a route stashed for it (test-only, local-only creation) falls
    /// back to an unaddressable placeholder.
    pub fn populate(&mut self, key: ParticipantKey, cfg: ParticipantConfig, rng: &mut Rng) {
        let participant_id = cfg.participant_id;
        let handle = self
            .pending_ingress
            .remove(&key)
            .unwrap_or(TransportHandle::new(TransportRoute::from_raw(0), 0));
        let ingress_route = handle.route;
        let mut participant_rng = Rng::seed_from_u64(rng.next_u64());
        let core = ParticipantCore::new(
            cfg,
            self.shard_id,
            self.max_gso_segments,
            1,
            &mut participant_rng,
        );
        let Some(slot) = self.participants.get_mut(key) else {
            pulsebeam_runtime::fatal!("populate called on a key the registry does not hold")
        };
        debug_assert!(
            slot.is_none(),
            "populate called on an already-populated slot"
        );
        *slot = Some(ParticipantMeta {
            core,
            queued_dirty: false,
            ingress_route,
            // Filled in by `join_room` once the room's arena entry exists,
            // which is a step later than this one.
            room_key: RoomKey::default(),
        });
        tracing::info!(%participant_id, "participant added to shard");
    }

    /// `reserve` immediately followed by `populate`, for callers that have
    /// no real connection to address — tests, and any local participant
    /// creation that doesn't route through connection setup.
    pub fn insert(&mut self, cfg: ParticipantConfig, rng: &mut Rng) -> ParticipantKey {
        let key = self.reserve(cfg.participant_id);
        self.populate(key, cfg, rng);
        key
    }

    /// Record the room this participant joined, already compiled. The room's
    /// arena entry is created after `populate`, so this is a second step
    /// rather than a constructor argument.
    pub fn join_room(&mut self, key: ParticipantKey, room_key: RoomKey) {
        let Some(Some(meta)) = self.participants.get_mut(key) else {
            debug_assert!(false, "join_room called on a key with no participant");
            return;
        };
        meta.room_key = room_key;
    }

    /// Free a reservation that never got to `populate` — negotiation failed,
    /// or the connection was abandoned before setup completed.
    pub fn release_reserved(&mut self, key: ParticipantKey) {
        let slot = self.participants.remove(key);
        debug_assert!(
            !matches!(slot, Some(Some(_))),
            "release_reserved called on an already-populated slot"
        );
        self.pending_ingress.remove(&key);
        self.participant_keys.retain(|_, k| *k != key);
    }

    /// Removes a local participant and queues its addresses for closing.
    /// Returns the removed state so the caller can read final fields
    /// (room_id, upstream track ids) before it's dropped.
    pub fn remove(&mut self, id: &ParticipantId) -> Option<ParticipantMeta> {
        let key = self.participant_keys.remove(id)?;
        let Some(slot) = self.participants.remove(key) else {
            pulsebeam_runtime::fatal!(
                "participant {id} is keyed to a slot the registry does not hold"
            )
        };
        let meta = slot?;
        debug_assert_eq!(meta.participant_id, *id);
        let addrs = self.demuxer.unregister(meta.ingress_route);
        self.pending_close.extend(addrs);
        Some(meta)
    }

    pub fn get_mut(&mut self, id: &ParticipantId) -> Option<&mut ParticipantMeta> {
        let key = *self.participant_keys.get(id)?;
        let participant = self.participants.get_mut(key)?.as_mut()?;
        debug_assert_eq!(participant.participant_id, *id);
        Some(participant)
    }

    pub fn get_mut_with_key(
        &mut self,
        id: &ParticipantId,
    ) -> Option<(ParticipantKey, &mut ParticipantMeta)> {
        let key = *self.participant_keys.get(id)?;
        let participant = self.participants.get_mut(key)?.as_mut()?;
        debug_assert_eq!(participant.participant_id, *id);
        Some((key, participant))
    }

    /// True once the participant is fully populated — a bare reservation
    /// does not count, since nothing downstream can act on it yet.
    pub fn contains(&self, id: &ParticipantId) -> bool {
        self.participant_keys
            .get(id)
            .and_then(|&key| self.participants.get(key))
            .is_some_and(Option::is_some)
    }

    pub fn key_of(&self, id: &ParticipantId) -> Option<ParticipantKey> {
        self.participant_keys.get(id).copied()
    }

    pub fn resolve_mut(&mut self, key: ParticipantKey) -> Option<&mut ParticipantMeta> {
        self.participants.get_mut(key)?.as_mut()
    }

    pub fn demux(&mut self, batch: &RecvPacketBatch) -> Option<TransportHandle> {
        self.demuxer.demux(batch)
    }

    /// The route `stash_ingress` recorded for a reservation that never
    /// reached `populate` — read (without consuming) by the cancellation
    /// path so it can retire the route before calling `release_reserved`.
    pub fn pending_ingress_of(&self, key: ParticipantKey) -> Option<TransportHandle> {
        self.pending_ingress.get(&key).copied()
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

        registry.insert(cfg(p, room_id("r1")), &mut rng);

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
        let removed_key = registry.key_of(&participant_id).unwrap();

        assert!(registry.remove(&participant_id).is_some());
        registry.insert(cfg(participant_id, room_id("r5")), &mut rng);
        let replacement_key = registry.key_of(&participant_id).unwrap();

        assert_ne!(removed_key, replacement_key);
        assert!(registry.resolve_mut(removed_key).is_none());
        assert!(registry.resolve_mut(replacement_key).is_some());
    }

    #[test]
    fn a_reserved_key_resolves_only_after_populate() {
        let mut registry = ParticipantRegistry::new(ShardId::new(0), 1);
        let mut rng = pulsebeam_runtime::rand::seeded_rng(1);
        let p = pid();

        let key = registry.reserve(p);
        assert!(
            registry.resolve_mut(key).is_none(),
            "a bare reservation must not resolve to a participant"
        );
        assert!(
            !registry.contains(&p),
            "a bare reservation must not count as present"
        );

        registry.stash_ingress(key, TransportHandle::new(TransportRoute::from_raw(1), 3));
        registry.populate(key, cfg(p, room_id("r6")), &mut rng);
        assert!(
            registry.resolve_mut(key).is_some(),
            "the same key must resolve once populated"
        );
        assert!(registry.contains(&p));
    }

    #[test]
    fn releasing_a_reservation_frees_its_name_and_key() {
        let mut registry = ParticipantRegistry::new(ShardId::new(0), 1);
        let p = pid();

        let key = registry.reserve(p);
        registry.release_reserved(key);

        assert!(
            !registry.contains(&p),
            "a released reservation must not be resolvable by name"
        );
        assert!(registry.resolve_mut(key).is_none());
        assert!(
            registry.key_of(&p).is_none(),
            "the name index must be freed too, or a retry can never reserve again"
        );
    }
}

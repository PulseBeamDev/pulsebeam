use std::collections::VecDeque;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};

use ahash::HashMap;
use pulsebeam_runtime::net::RecvPacketBatch;
use pulsebeam_runtime::rand::{Rng, RngCore, SeedableRng};

use crate::{
    entity::ParticipantId,
    id::ShardId,
    participant::{ParticipantConfig, ParticipantCore},
    shard::demux::Demuxer,
};

pub(crate) struct ParticipantMeta {
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
    pub(super) fn with_span<R>(&mut self, f: impl FnOnce(&mut ParticipantCore) -> R) -> R {
        let _guard = self.span.enter();
        f(&mut self.core)
    }
}

pub(crate) struct ParticipantRegistry {
    shard_id: ShardId,
    max_gso_segments: usize,
    participants: HashMap<ParticipantId, ParticipantMeta>,
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
            participants: HashMap::default(),
            demuxer: Demuxer::new(shard_id.into()),
            pending_close: VecDeque::new(),
        }
    }

    pub fn insert(&mut self, cfg: ParticipantConfig, rng: &mut Rng) -> ParticipantId {
        let participant_id = cfg.participant_id;
        let room_id = cfg.room_id;
        let span = tracing::info_span!("participant", %room_id, %participant_id);
        let mut participant_rng = Rng::seed_from_u64(rng.next_u64());
        let core = ParticipantCore::new(
            cfg,
            self.shard_id,
            self.max_gso_segments,
            1,
            &mut participant_rng,
        );
        self.participants
            .insert(participant_id, ParticipantMeta { core, span });
        tracing::info!(%participant_id, "participant added to shard");
        participant_id
    }

    /// Removes a local participant and queues its addresses for closing.
    /// Returns the removed state so the caller can read final fields
    /// (room_id, upstream track ids) before it's dropped.
    pub fn remove(&mut self, id: &ParticipantId) -> Option<ParticipantMeta> {
        let meta = self.participants.remove(id)?;
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
        self.participants.get_mut(id)
    }

    pub fn contains(&self, id: &ParticipantId) -> bool {
        self.participants.contains_key(id)
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

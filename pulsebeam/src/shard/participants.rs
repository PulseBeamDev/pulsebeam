use std::collections::VecDeque;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};

use pulsebeam_runtime::net::RecvPacketBatch;
use pulsebeam_runtime::rand::{Rng, RngCore, SeedableRng};
use slotmap::SecondaryMap;

use crate::{
    id::ShardId,
    participant::{ParticipantConfig, ParticipantCore},
    route::{TransportHandle, TransportRoute},
    shard::demux::Demuxer,
};

pub(crate) use crate::keys::ParticipantKey;

pub(crate) struct ParticipantMeta {
    core: ParticipantCore,
    pub(super) queued_dirty: bool,
    pub(super) ingress_route: TransportRoute,
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
    participants: SecondaryMap<ParticipantKey, ParticipantMeta>,
    demuxer: Demuxer,
    pending_close: VecDeque<SocketAddr>,
}

impl ParticipantRegistry {
    pub fn len(&self) -> usize {
        self.participants.len()
    }

    pub fn new(shard_id: ShardId, max_gso_segments: usize) -> Self {
        Self {
            shard_id,
            max_gso_segments,
            participants: SecondaryMap::new(),
            demuxer: Demuxer::new(),
            pending_close: VecDeque::new(),
        }
    }

    pub fn insert(
        &mut self,
        key: ParticipantKey,
        cfg: ParticipantConfig,
        ingress: TransportHandle,
        rng: &mut Rng,
    ) -> bool {
        debug_assert_eq!(ingress.shard(), self.shard_id);
        let participant_id = cfg.participant_id;
        let mut participant_rng = Rng::seed_from_u64(rng.next_u64());
        let core = ParticipantCore::new(
            cfg,
            self.shard_id,
            self.max_gso_segments,
            1,
            &mut participant_rng,
        );
        if self.participants.contains_key(key) {
            debug_assert!(false, "duplicate participant materialization");
            return false;
        }
        let previous = self.participants.insert(
            key,
            ParticipantMeta {
                core,
                queued_dirty: false,
                ingress_route: ingress.route,
            },
        );
        debug_assert!(previous.is_none());
        tracing::info!(%participant_id, "participant added to shard");
        true
    }

    pub fn remove_key(&mut self, key: ParticipantKey) -> Option<ParticipantMeta> {
        let meta = self.participants.remove(key)?;
        let addrs = self.demuxer.unregister(meta.ingress_route);
        self.pending_close.extend(addrs);
        Some(meta)
    }

    #[allow(
        clippy::manual_find,
        reason = "participant lookup is the shard-owned boundary before a keyed route is selected"
    )]
    pub fn get_mut(&mut self, id: &crate::entity::ParticipantId) -> Option<&mut ParticipantMeta> {
        for (_, participant) in &mut self.participants {
            if participant.participant_id == *id {
                return Some(participant);
            }
        }
        None
    }

    pub fn get_mut_with_key(
        &mut self,
        id: &crate::entity::ParticipantId,
    ) -> Option<(ParticipantKey, &mut ParticipantMeta)> {
        let mut key = None;
        for (candidate, participant) in &mut self.participants {
            if participant.participant_id == *id {
                key = Some(candidate);
                break;
            }
        }
        let key = key?;
        let participant = self.participants.get_mut(key)?;
        Some((key, participant))
    }

    pub fn resolve_mut(&mut self, key: ParticipantKey) -> Option<&mut ParticipantMeta> {
        self.participants.get_mut(key)
    }

    pub fn publish_track(&mut self, track: &crate::track::Track) {
        for (_, participant) in &mut self.participants {
            participant.on_tracks_published(std::slice::from_ref(track));
        }
    }

    pub fn unpublish_track(&mut self, track_id: &crate::entity::TrackId) {
        for (_, participant) in &mut self.participants {
            participant.on_tracks_unpublished(std::slice::from_ref(track_id));
        }
    }

    pub fn demux(&mut self, batch: &RecvPacketBatch) -> Option<TransportHandle> {
        self.demuxer.demux(batch)
    }

    pub fn drain_pending_close(&mut self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.pending_close.drain(..)
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use pulsebeam_core::identity::{ConnectionId, ParticipantId};
use pulsebeam_runtime::time::Instant;

use super::{ControllerError, ownership};
use crate::control::registry::RoomRegistry;
use crate::{entity::ConnectionScope, id::ShardId};

const MAX_PER_SHARD: usize = 64;
const LIFETIME: Duration = Duration::from_secs(10);
pub(super) const VALID_FOR_MS: u32 = 10_000;

#[derive(Default)]
pub(super) struct Placements {
    leases: BTreeMap<ownership::Proof, Entry>,
    expires: BTreeSet<(Instant, ownership::Proof)>,
    counts: BTreeMap<ShardId, usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Target {
    pub scope: ConnectionScope,
    pub participant: ParticipantId,
    pub shard: ShardId,
}

pub(super) struct Lease {
    pub target: Target,
    pub connection: ConnectionId,
    pub partition: Option<usize>,
    pub successor: Option<Vec<u8>>,
}

struct Entry {
    lease: Lease,
    expiry: Instant,
}

impl Placements {
    pub fn deadline(&self) -> Option<Instant> {
        self.expires.first().map(|(deadline, _)| *deadline)
    }

    pub fn has_capacity(&self, shard: ShardId) -> bool {
        self.counts.get(&shard).copied().unwrap_or_default() < MAX_PER_SHARD
    }

    pub fn insert(
        &mut self,
        proof: ownership::Proof,
        lease: Lease,
        registry: &mut RoomRegistry,
        now: Instant,
    ) -> Result<(), ControllerError> {
        let Target {
            scope,
            participant,
            shard,
        } = lease.target;
        if !self.has_capacity(shard)
            && !self
                .leases
                .get(&proof)
                .is_some_and(|entry| entry.lease.target.shard == shard)
        {
            return Err(ControllerError::ServiceUnavailable);
        }
        let expiry = now
            .checked_add(LIFETIME)
            .ok_or(ControllerError::ServiceUnavailable)?;
        // Reserve the replacement before releasing the prior pin, so there is
        // no instant where an occupied partition is free to move.
        if let (Some(room), Some(index)) = (scope.joined_room(), lease.partition) {
            registry.reserve_partition(room, participant, lease.connection, shard, index);
        }
        self.remove(proof, registry);
        let count = self.counts.entry(shard).or_default();
        *count = count.saturating_add(1);
        self.expires.insert((expiry, proof));
        self.leases.insert(proof, Entry { lease, expiry });
        Ok(())
    }

    pub fn get(
        &self,
        proof: ownership::Proof,
        target: Target,
        registry: &RoomRegistry,
        now: Instant,
    ) -> Option<&Lease> {
        let Entry { lease, expiry } = self.leases.get(&proof)?;
        if *expiry <= now || lease.target != target {
            return None;
        }
        let Target {
            scope,
            participant,
            shard,
        } = target;
        match (scope.joined_room(), lease.partition) {
            (Some(room), Some(index))
                if registry.partition_reserved(
                    room,
                    participant,
                    lease.connection,
                    shard,
                    index,
                ) =>
            {
                Some(lease)
            }
            (None, None) => Some(lease),
            _ => None,
        }
    }

    /// Transfer the reservation to pending materialization without releasing its room pin.
    pub fn take(&mut self, proof: ownership::Proof) -> Option<Lease> {
        let Entry { lease, expiry } = self.leases.remove(&proof)?;
        self.expires.remove(&(expiry, proof));
        let shard = lease.target.shard;
        if let Some(count) = self.counts.get_mut(&shard) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.counts.remove(&shard);
            }
        }
        Some(lease)
    }

    pub fn remove(&mut self, proof: ownership::Proof, registry: &mut RoomRegistry) {
        if let Some(lease) = self.take(proof)
            && let Some(room) = lease.target.scope.joined_room()
        {
            registry.release_partition(room, lease.target.participant, lease.connection);
        }
    }

    pub fn expire(&mut self, registry: &mut RoomRegistry, now: Instant, budget: usize) {
        for _ in 0..budget {
            let Some(&(expiry, proof)) = self.expires.first() else {
                break;
            };
            if expiry > now {
                break;
            }
            self.remove(proof, registry);
        }
    }
}

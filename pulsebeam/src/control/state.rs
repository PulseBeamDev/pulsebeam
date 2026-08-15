//! The canonical lifecycle state, and the transaction that moves it.
//!
//! Only the control plane mutates this. A shard never reads it — a shard
//! reads its own [`ShardView`](crate::view::ShardView), which is *derived*
//! from here, not a second independently maintained model.
//!
//! The unit of change is a [`LifecycleTransaction`]: stage allocator and arena
//! mutations, compile one view delta per affected shard, publish each once,
//! then commit. Shards apply queued deltas on their next tick; a packet that
//! arrives in that small window is dropped and counted rather than blocking a
//! media loop on control-plane progress.
#![deny(clippy::arithmetic_side_effects)]

use slotmap::SlotMap;
use tokio::time::Instant;

use crate::id::ShardId;
use crate::route::{
    PackedRoute, RouteError, RouteHandle, RouteId, SlotAllocator, TransportHandle, TransportRoute,
};

/// One shard's slot namespace for one route family.
///
/// The allocator lives here rather than on the shard because the shard bits
/// of a route are decided by placement, which is a control-plane decision —
/// asking the destination to mint its own address is what forced the old
/// round trip before ICE credentials could be produced.
#[derive(Debug)]
pub(crate) struct PerShardAllocator {
    shards: Vec<SlotAllocator>,
}

impl PerShardAllocator {
    pub fn new(shard_count: usize) -> Self {
        Self {
            shards: (0..shard_count)
                .map(|idx| {
                    SlotAllocator::with_max_slots(
                        ShardId::new(idx),
                        PackedRoute::MAX_SLOT.saturating_add(1),
                    )
                })
                .collect(),
        }
    }

    fn shard_mut(&mut self, shard: ShardId) -> Option<&mut SlotAllocator> {
        self.shards.get_mut(shard.index())
    }

    pub fn allocate(&mut self, shard: ShardId, now: Instant) -> Result<(u32, u16), RouteError> {
        let Some(alloc) = self.shard_mut(shard) else {
            debug_assert!(false, "allocating on a shard outside the node");
            return Err(RouteError::Exhausted { max_slots: 0 });
        };
        alloc.allocate(now)
    }

    pub fn retire(&mut self, shard: ShardId, slot: u32, now: Instant) {
        let Some(alloc) = self.shard_mut(shard) else {
            debug_assert!(false, "retiring on a shard outside the node");
            return;
        };
        alloc.retire(slot, now);
    }
}

/// A route allocated but not yet committed. Held until the whole generation
/// commits or aborts, so an abandoned transaction cannot leak an address.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RouteReservation {
    pub shard_id: ShardId,
    pub slot: u32,
    pub family: RouteFamily,
}

#[derive(Debug)]
pub(crate) struct ParticipantRecord {
    pub id: crate::entity::ParticipantId,
}

#[derive(Debug)]
pub(crate) struct TrackRecord {
    pub id: crate::entity::TrackId,
    pub origin: crate::entity::ParticipantId,
}

#[derive(Debug)]
pub(crate) struct StreamRecord {
    pub id: crate::shard::router::DataStreamId,
}

#[derive(Debug)]
pub(crate) struct ShardArenas {
    pub participants: SlotMap<crate::keys::ParticipantKey, ParticipantRecord>,
    pub tracks: SlotMap<crate::keys::TrackKey, TrackRecord>,
    pub data: SlotMap<crate::keys::DataStreamKey, StreamRecord>,
    pub reliable: SlotMap<crate::keys::ReliableStreamKey, StreamRecord>,
}

impl ShardArenas {
    fn new() -> Self {
        Self {
            participants: SlotMap::with_key(),
            tracks: SlotMap::with_key(),
            data: SlotMap::with_key(),
            reliable: SlotMap::with_key(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum RouteFamily {
    Transport,
    Endpoint,
}

#[derive(Debug)]
pub(crate) struct LifecycleTransaction {
    pub generation: u64,
    pub reservations: Vec<RouteReservation>,
    pub participants: Vec<(ShardId, crate::keys::ParticipantKey)>,
    pub tracks: Vec<(ShardId, crate::keys::TrackKey)>,
    pub data: Vec<(ShardId, crate::keys::DataStreamKey)>,
    pub reliable: Vec<(ShardId, crate::keys::ReliableStreamKey)>,
}

impl LifecycleTransaction {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            reservations: Vec::new(),
            participants: Vec::new(),
            tracks: Vec::new(),
            data: Vec::new(),
            reliable: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransactionError {
    /// Another transaction is already staged. Lifecycle changes serialise
    /// through one actor, so this is a programming error, not backpressure.
    Busy,
    /// No transaction is staged.
    Idle,
    Allocation(RouteError),
}

/// Owns the canonical state, the allocators, and the one staged transaction.
///
/// Deliberately holds no view writers: publication is the caller's step, so
/// that "one publish per affected shard per generation" is enforceable by
/// looking at one function rather than trusting fifty call sites.
#[derive(Debug)]
pub(crate) struct ControlPlaneState {
    pending: Option<LifecycleTransaction>,
    detached_participants: Vec<(ShardId, crate::keys::ParticipantKey)>,
    detached_tracks: Vec<(ShardId, crate::keys::TrackKey)>,
    detached_data: Vec<(ShardId, crate::keys::DataStreamKey)>,
    detached_reliable: Vec<(ShardId, crate::keys::ReliableStreamKey)>,
    pub arenas: Vec<ShardArenas>,
    pub transport: PerShardAllocator,
    pub endpoint: PerShardAllocator,
    generation: u64,
}

impl ControlPlaneState {
    pub fn new(shard_count: usize) -> Self {
        Self {
            pending: None,
            detached_participants: Vec::new(),
            detached_tracks: Vec::new(),
            detached_data: Vec::new(),
            detached_reliable: Vec::new(),
            arenas: (0..shard_count).map(|_| ShardArenas::new()).collect(),
            transport: PerShardAllocator::new(shard_count),
            endpoint: PerShardAllocator::new(shard_count),
            generation: 0,
        }
    }

    pub fn mint_participant(
        &mut self,
        shard: ShardId,
        id: crate::entity::ParticipantId,
    ) -> Option<crate::keys::ParticipantKey> {
        let key = self
            .arenas
            .get_mut(shard.index())
            .map(|arena| arena.participants.insert(ParticipantRecord { id }))?;
        if let Some(tx) = self.pending.as_mut() {
            tx.participants.push((shard, key));
        } else {
            self.detached_participants.push((shard, key));
        }
        Some(key)
    }

    pub fn mint_track(
        &mut self,
        shard: ShardId,
        id: crate::entity::TrackId,
        origin: crate::entity::ParticipantId,
    ) -> Option<crate::keys::TrackKey> {
        let key = self
            .arenas
            .get_mut(shard.index())
            .map(|arena| arena.tracks.insert(TrackRecord { id, origin }))?;
        if let Some(tx) = self.pending.as_mut() {
            tx.tracks.push((shard, key));
        } else {
            self.detached_tracks.push((shard, key));
        }
        Some(key)
    }

    pub fn mint_data(
        &mut self,
        shard: ShardId,
        id: crate::shard::router::DataStreamId,
    ) -> Option<crate::keys::DataStreamKey> {
        let key = self
            .arenas
            .get_mut(shard.index())
            .map(|arena| arena.data.insert(StreamRecord { id }))?;
        if let Some(tx) = self.pending.as_mut() {
            tx.data.push((shard, key));
        } else {
            self.detached_data.push((shard, key));
        }
        Some(key)
    }

    pub fn mint_reliable(
        &mut self,
        shard: ShardId,
        id: crate::shard::router::DataStreamId,
    ) -> Option<crate::keys::ReliableStreamKey> {
        let key = self
            .arenas
            .get_mut(shard.index())
            .map(|arena| arena.reliable.insert(StreamRecord { id }))?;
        if let Some(tx) = self.pending.as_mut() {
            tx.reliable.push((shard, key));
        } else {
            self.detached_reliable.push((shard, key));
        }
        Some(key)
    }

    pub fn remove_participant(&mut self, shard: ShardId, key: crate::keys::ParticipantKey) {
        let record = self
            .arenas
            .get_mut(shard.index())
            .and_then(|arena| arena.participants.remove(key));
        if let Some(record) = record {
            debug_assert!(!record.id.as_str().is_empty());
        }
    }

    pub fn remove_track(&mut self, shard: ShardId, key: crate::keys::TrackKey) {
        let record = self
            .arenas
            .get_mut(shard.index())
            .and_then(|arena| arena.tracks.remove(key));
        if let Some(record) = record {
            debug_assert!(!record.id.as_str().is_empty());
            debug_assert!(!record.origin.as_str().is_empty());
        }
    }

    pub fn remove_data(&mut self, shard: ShardId, key: crate::keys::DataStreamKey) {
        let record = self
            .arenas
            .get_mut(shard.index())
            .and_then(|arena| arena.data.remove(key));
        if let Some(record) = record {
            debug_assert!(!record.id.topic.as_ref().is_empty());
        }
    }

    pub fn remove_reliable(&mut self, shard: ShardId, key: crate::keys::ReliableStreamKey) {
        let record = self
            .arenas
            .get_mut(shard.index())
            .and_then(|arena| arena.reliable.remove(key));
        if let Some(record) = record {
            debug_assert!(!record.id.topic.as_ref().is_empty());
        }
    }

    pub fn pending(&self) -> Option<&LifecycleTransaction> {
        self.pending.as_ref()
    }

    /// Open the next generation. One at a time: the control plane is a single
    /// actor, and a second concurrent staging would make "generation N+1" a
    /// lie.
    pub fn begin(&mut self) -> Result<(), TransactionError> {
        if self.pending.is_some() {
            return Err(TransactionError::Busy);
        }
        let generation = self.generation.saturating_add(1);
        let mut tx = LifecycleTransaction::new(generation);
        tx.participants.append(&mut self.detached_participants);
        tx.tracks.append(&mut self.detached_tracks);
        tx.data.append(&mut self.detached_data);
        tx.reliable.append(&mut self.detached_reliable);
        self.pending = Some(tx);
        Ok(())
    }

    fn tx_mut(&mut self) -> Result<&mut LifecycleTransaction, TransactionError> {
        self.pending.as_mut().ok_or(TransactionError::Idle)
    }

    /// Reserve a transport route on an already-chosen shard.
    ///
    /// The shard is an input, not an output: placement decided it before this
    /// was called, which is precisely what lets the ufrag be built without
    /// asking the destination for an address first.
    pub fn reserve_transport(
        &mut self,
        shard_id: ShardId,
        now: Instant,
    ) -> Result<TransportHandle, TransactionError> {
        let (slot, epoch) = self
            .transport
            .allocate(shard_id, now)
            .map_err(TransactionError::Allocation)?;
        let handle = TransportHandle::new(TransportRoute::new(shard_id, slot), epoch);
        debug_assert_eq!(
            handle.shard(),
            shard_id,
            "a reserved route must carry the shard it was reserved on"
        );
        let tx = self.tx_mut()?;
        tx.reservations.push(RouteReservation {
            shard_id,
            slot,
            family: RouteFamily::Transport,
        });
        Ok(handle)
    }

    /// Reserve an endpoint route on the shard that asked for it. Placement is
    /// not a decision here — the requesting shard is the destination.
    pub fn reserve_endpoint(
        &mut self,
        shard_id: ShardId,
        now: Instant,
    ) -> Result<RouteHandle, TransactionError> {
        let (slot, epoch) = self
            .endpoint
            .allocate(shard_id, now)
            .map_err(TransactionError::Allocation)?;
        let handle = RouteHandle::new(RouteId::new(shard_id, slot), epoch);
        let tx = self.tx_mut()?;
        tx.reservations.push(RouteReservation {
            shard_id,
            slot,
            family: RouteFamily::Endpoint,
        });
        Ok(handle)
    }

    /// Return an endpoint slot to its allocator, once its route is absent from
    /// the published view.
    pub fn release_endpoint(&mut self, shard_id: ShardId, slot: u32, now: Instant) {
        self.endpoint.retire(shard_id, slot, now);
    }

    /// Return a transport slot to its allocator.
    ///
    /// Only after the route is absent from the staged view; the allocator's
    /// quarantine then keeps the slot out of circulation for as long as a
    /// stale datagram could take to arrive.
    pub fn release_transport(&mut self, shard_id: ShardId, slot: u32, now: Instant) {
        self.transport.retire(shard_id, slot, now);
    }

    /// Commit the staged generation after its deltas have been queued.
    pub fn commit(&mut self) -> Result<LifecycleTransaction, TransactionError> {
        let tx = self.pending.take().ok_or(TransactionError::Idle)?;
        debug_assert_eq!(
            tx.generation,
            self.generation.saturating_add(1),
            "generations commit in order"
        );
        self.generation = tx.generation;
        Ok(tx)
    }

    /// Abandon the staged generation, walking every staged mutation back and
    /// releasing every reservation it took.
    pub fn abort(&mut self, now: Instant) -> Option<LifecycleTransaction> {
        let tx = self.pending.take()?;
        for reservation in &tx.reservations {
            match reservation.family {
                RouteFamily::Transport => {
                    self.transport
                        .retire(reservation.shard_id, reservation.slot, now);
                }
                RouteFamily::Endpoint => {
                    self.endpoint
                        .retire(reservation.shard_id, reservation.slot, now);
                }
            }
        }
        for &(shard, key) in &tx.participants {
            self.remove_participant(shard, key);
        }
        for &(shard, key) in &tx.tracks {
            self.remove_track(shard, key);
        }
        for &(shard, key) in &tx.data {
            self.remove_data(shard, key);
        }
        for &(shard, key) in &tx.reliable {
            self.remove_reliable(shard, key);
        }
        Some(tx)
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_reserved_route_carries_the_shard_placement_chose() {
        let mut state = ControlPlaneState::new(8);
        state.begin().unwrap();
        let handle = state
            .reserve_transport(ShardId::new(5), Instant::now())
            .unwrap();
        assert_eq!(handle.shard(), ShardId::new(5));
    }

    #[tokio::test(start_paused = true)]
    async fn the_two_families_allocate_from_separate_namespaces() {
        let mut state = ControlPlaneState::new(2);
        let now = Instant::now();
        let shard = ShardId::new(1);
        let (transport_slot, _) = state.transport.allocate(shard, now).unwrap();
        let (endpoint_slot, _) = state.endpoint.allocate(shard, now).unwrap();
        assert_eq!(
            transport_slot, endpoint_slot,
            "both namespaces start at zero, which is why they must not share a table"
        );
    }

    /// An abandoned transaction must not leak the address it took, or a
    /// reconnect storm would exhaust the namespace one failed join at a time.
    #[tokio::test(start_paused = true)]
    async fn an_aborted_reservation_returns_to_the_allocator() {
        let mut state = ControlPlaneState::new(2);
        let shard = ShardId::new(0);

        state.begin().unwrap();
        let first = state.reserve_transport(shard, Instant::now()).unwrap();
        state.abort(Instant::now());

        tokio::time::advance(crate::route::ROUTE_QUARANTINE).await;

        state.begin().unwrap();
        let second = state.reserve_transport(shard, Instant::now()).unwrap();
        assert_eq!(second.route, first.route, "the slot came back");
        assert_ne!(second.epoch, first.epoch, "as a new incarnation");
    }

    #[tokio::test(start_paused = true)]
    async fn only_one_transaction_stages_at_a_time() {
        let mut state = ControlPlaneState::new(2);
        state.begin().unwrap();
        assert_eq!(state.begin(), Err(TransactionError::Busy));
    }

    #[tokio::test(start_paused = true)]
    async fn abort_removes_every_minted_runtime_key() {
        let mut state = ControlPlaneState::new(1);
        let shard = ShardId::new(0);
        state.begin().unwrap();
        let participant_id = crate::entity::ParticipantId::from_bytes([1; 16]);
        let participant = state.mint_participant(shard, participant_id).unwrap();
        let track = state
            .mint_track(
                shard,
                participant_id.derive_track_id(crate::entity::TrackKind::Video, "track"),
                participant_id,
            )
            .unwrap();
        let stream = crate::shard::router::DataStreamId::new(
            crate::entity::RoomId::from_external(
                &crate::entity::ExternalRoomId::new("room").unwrap(),
            ),
            participant_id,
            crate::track::Topic::for_test("topic"),
        );
        let data = state.mint_data(shard, stream.clone()).unwrap();
        let reliable = state.mint_reliable(shard, stream).unwrap();
        state.abort(Instant::now());
        assert!(state.arenas[0].participants.get(participant).is_none());
        assert!(state.arenas[0].tracks.get(track).is_none());
        assert!(state.arenas[0].data.get(data).is_none());
        assert!(state.arenas[0].reliable.get(reliable).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn removed_track_key_does_not_resolve_after_reissue() {
        let mut state = ControlPlaneState::new(1);
        let shard = ShardId::new(0);
        let origin = crate::entity::ParticipantId::from_bytes([2; 16]);
        let track_id = origin.derive_track_id(crate::entity::TrackKind::Video, "track");

        state.begin().unwrap();
        let old_key = state.mint_track(shard, track_id, origin).unwrap();
        state.commit().unwrap();
        state.remove_track(shard, old_key);

        state.begin().unwrap();
        let new_key = state.mint_track(shard, track_id, origin).unwrap();
        assert!(state.arenas[0].tracks.get(old_key).is_none());
        assert!(state.arenas[0].tracks.get(new_key).is_some());
        assert_ne!(old_key, new_key);
    }
}

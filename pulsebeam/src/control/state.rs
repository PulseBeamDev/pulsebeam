//! The canonical lifecycle state, and the transaction that moves it.
//!
//! Only the control plane mutates this. A shard never reads it — a shard
//! reads its own [`ShardView`](crate::view::ShardView), which is *derived*
//! from here, not a second independently maintained model.
//!
//! The unit of change is a [`LifecycleTransaction`]: stage, allocate, ask the
//! owning shards to prepare their runtime bindings, compile one view delta per
//! affected shard, publish each once, wait for the generation barrier, then
//! commit and advertise. Nothing is externally visible before the barrier
//! clears, which is what stops a sender ever holding a route that resolves to
//! a participant, room or track the owning shard has not built yet.
#![deny(clippy::arithmetic_side_effects)]

use ahash::{HashMap, HashMapExt};
use tokio::time::Instant;

use crate::entity::{ParticipantId, RoomId};
use crate::id::ShardId;
use crate::route::{
    PackedRoute, RouteError, RouteHandle, RouteId, SlotAllocator, TransportHandle, TransportRoute,
};
use crate::shard::participants::ParticipantKey;

/// Identifies one in-flight transaction, so a late acknowledgement for a
/// transaction that has already been abandoned is recognisable as stale
/// rather than applied to whatever is in flight now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct TransactionId(u64);

impl std::fmt::Display for TransactionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tx{}", self.0)
    }
}

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

/// What the control plane knows about one participant.
///
/// `binding` is opaque: it is the owning shard's own arena key, stored only
/// long enough to compile into that shard's next view. The control plane
/// never dereferences it and it is meaningless on any other shard.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParticipantRecord {
    pub shard_id: ShardId,
    pub room_id: RoomId,
    pub transport: TransportHandle,
    pub binding: Option<ParticipantKey>,
}

/// The canonical mutable source for lifecycle facts. Never read by a packet
/// handler.
#[derive(Debug, Default)]
pub(crate) struct ControlState {
    pub participants: HashMap<ParticipantId, ParticipantRecord>,
}

impl ControlState {
    pub fn new() -> Self {
        Self {
            participants: HashMap::new(),
        }
    }
}

/// How to undo one staged mutation.
///
/// The staged generation mutates `committed` in ordinary memory and records
/// how to walk back, rather than cloning the whole state per transaction: a
/// join would otherwise cost a copy of every participant on the node. Nothing
/// staged is *visible* until the views publish and the barrier clears, which
/// is where the plan's "leaves the previously committed views valid" actually
/// lives.
#[derive(Debug)]
enum Undo {
    RemoveParticipant(ParticipantId),
    RestoreParticipant(ParticipantId, Box<ParticipantRecord>),
}

/// A route allocated but not yet committed. Held until the whole generation
/// commits or aborts, so an abandoned transaction cannot leak an address.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RouteReservation {
    pub shard_id: ShardId,
    pub slot: u32,
}

/// A runtime binding an owning shard reported having prepared.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PreparedBinding {
    pub transaction: TransactionId,
    pub shard_id: ShardId,
    pub participant: ParticipantKey,
}

#[derive(Debug)]
pub(crate) struct LifecycleTransaction {
    pub id: TransactionId,
    pub generation: u64,
    undo: Vec<Undo>,
    pub reservations: Vec<RouteReservation>,
    pub prepared: Vec<PreparedBinding>,
    affected: Vec<ShardId>,
}

impl LifecycleTransaction {
    fn new(id: TransactionId, generation: u64) -> Self {
        Self {
            id,
            generation,
            undo: Vec::new(),
            reservations: Vec::new(),
            prepared: Vec::new(),
            affected: Vec::new(),
        }
    }

    pub fn affected(&self) -> &[ShardId] {
        &self.affected
    }

    fn touch(&mut self, shard: ShardId) {
        if !self.affected.contains(&shard) {
            self.affected.push(shard);
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
    pub committed: ControlState,
    pending: Option<LifecycleTransaction>,
    pub transport: PerShardAllocator,
    pub endpoint: PerShardAllocator,
    generation: u64,
    next_transaction: u64,
}

impl ControlPlaneState {
    pub fn new(shard_count: usize) -> Self {
        Self {
            committed: ControlState::new(),
            pending: None,
            transport: PerShardAllocator::new(shard_count),
            endpoint: PerShardAllocator::new(shard_count),
            generation: 0,
            next_transaction: 0,
        }
    }

    pub fn committed_generation(&self) -> u64 {
        self.generation
    }

    pub fn pending(&self) -> Option<&LifecycleTransaction> {
        self.pending.as_ref()
    }

    /// Open the next generation. One at a time: the control plane is a single
    /// actor, and a second concurrent staging would make "generation N+1" a
    /// lie.
    pub fn begin(&mut self) -> Result<TransactionId, TransactionError> {
        if self.pending.is_some() {
            return Err(TransactionError::Busy);
        }
        let id = TransactionId(self.next_transaction);
        self.next_transaction = self.next_transaction.saturating_add(1);
        let generation = self.generation.saturating_add(1);
        self.pending = Some(LifecycleTransaction::new(id, generation));
        Ok(id)
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
        tx.reservations.push(RouteReservation { shard_id, slot });
        tx.touch(shard_id);
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
        tx.reservations.push(RouteReservation { shard_id, slot });
        tx.touch(shard_id);
        Ok(handle)
    }

    /// Return an endpoint slot to its allocator, once its route is absent from
    /// the published view.
    pub fn release_endpoint(&mut self, shard_id: ShardId, slot: u32, now: Instant) {
        self.endpoint.retire(shard_id, slot, now);
    }

    /// Stage a participant. Not visible to anything until the transaction
    /// commits.
    pub fn stage_participant(
        &mut self,
        participant_id: ParticipantId,
        record: ParticipantRecord,
    ) -> Result<(), TransactionError> {
        let previous = self.committed.participants.insert(participant_id, record);
        let tx = self.tx_mut()?;
        tx.touch(record.shard_id);
        tx.undo.push(match previous {
            Some(old) => Undo::RestoreParticipant(participant_id, Box::new(old)),
            None => Undo::RemoveParticipant(participant_id),
        });
        Ok(())
    }

    /// Record the binding key an owning shard reported. A duplicate report for
    /// the same participant is idempotent — a retry after a lost
    /// acknowledgement must not create a second endpoint.
    pub fn record_binding(
        &mut self,
        participant_id: ParticipantId,
        binding: PreparedBinding,
    ) -> Result<(), TransactionError> {
        let Some(record) = self.committed.participants.get_mut(&participant_id) else {
            return Ok(());
        };
        debug_assert_eq!(
            record.shard_id, binding.shard_id,
            "only the owning shard prepares a participant's binding"
        );
        if let Some(existing) = record.binding {
            debug_assert_eq!(
                existing, binding.participant,
                "a repeated prepare must report the same binding"
            );
            return Ok(());
        }
        record.binding = Some(binding.participant);
        let tx = self.tx_mut()?;
        debug_assert_eq!(
            tx.id, binding.transaction,
            "a binding prepared for an abandoned transaction must not be applied to this one"
        );
        tx.prepared.push(binding);
        Ok(())
    }

    /// Stage a participant's removal.
    pub fn forget_participant(
        &mut self,
        participant_id: ParticipantId,
    ) -> Result<(), TransactionError> {
        let previous = self.committed.participants.remove(&participant_id);
        let tx = self.tx_mut()?;
        if let Some(old) = previous {
            tx.touch(old.shard_id);
            tx.undo
                .push(Undo::RestoreParticipant(participant_id, Box::new(old)));
        }
        Ok(())
    }

    /// Return a transport slot to its allocator.
    ///
    /// Only after the route is absent from the published view and that
    /// generation is acknowledged — the allocator's own quarantine then keeps
    /// the slot out of circulation for as long as a stale datagram could take
    /// to arrive.
    pub fn release_transport(&mut self, shard_id: ShardId, slot: u32, now: Instant) {
        self.transport.retire(shard_id, slot, now);
    }

    /// The room a participant was placed in, for authorization checks that
    /// must not re-derive it from anything the data plane touched.
    pub fn room_of(&self, participant_id: &ParticipantId) -> Option<RoomId> {
        self.committed
            .participants
            .get(participant_id)
            .map(|record| record.room_id)
    }

    /// Commit the staged generation. Callers must only reach here once every
    /// affected shard has acknowledged the published generation.
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
        let mut tx = self.pending.take()?;
        while let Some(undo) = tx.undo.pop() {
            match undo {
                Undo::RemoveParticipant(id) => {
                    self.committed.participants.remove(&id);
                }
                Undo::RestoreParticipant(id, record) => {
                    self.committed.participants.insert(id, *record);
                }
            }
        }
        for reservation in &tx.reservations {
            self.transport
                .retire(reservation.shard_id, reservation.slot, now);
        }
        Some(tx)
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::entity::ExternalRoomId;
    use slotmap::KeyData;

    fn pid(seed: u8) -> ParticipantId {
        ParticipantId::from_bytes([seed; 16])
    }

    fn room() -> RoomId {
        RoomId::from_external(&ExternalRoomId::new("room1").unwrap())
    }

    fn participant_key(n: u32) -> ParticipantKey {
        ParticipantKey::from(KeyData::from_ffi(u64::from(n) | (1 << 32)))
    }

    fn record(shard: ShardId, transport: TransportHandle) -> ParticipantRecord {
        ParticipantRecord {
            shard_id: shard,
            room_id: room(),
            transport,
            binding: None,
        }
    }

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

    #[tokio::test(start_paused = true)]
    async fn a_committed_transaction_advances_exactly_one_generation() {
        let mut state = ControlPlaneState::new(2);
        assert_eq!(state.committed_generation(), 0);

        state.begin().unwrap();
        let handle = state
            .reserve_transport(ShardId::new(0), Instant::now())
            .unwrap();
        state
            .stage_participant(pid(1), record(ShardId::new(0), handle))
            .unwrap();
        let tx = state.commit().unwrap();

        assert_eq!(tx.generation, 1);
        assert_eq!(state.committed_generation(), 1);
        assert!(state.committed.participants.contains_key(&pid(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn aborting_leaves_no_participant_and_no_generation() {
        let mut state = ControlPlaneState::new(2);
        state.begin().unwrap();
        let handle = state
            .reserve_transport(ShardId::new(0), Instant::now())
            .unwrap();
        state
            .stage_participant(pid(1), record(ShardId::new(0), handle))
            .unwrap();

        state.abort(Instant::now());

        assert!(!state.committed.participants.contains_key(&pid(1)));
        assert_eq!(state.committed_generation(), 0, "an aborted generation never lands");
        assert!(state.pending().is_none());
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

    /// A retry after a lost acknowledgement must not create a second binding.
    #[tokio::test(start_paused = true)]
    async fn a_repeated_prepare_report_is_idempotent() {
        let mut state = ControlPlaneState::new(2);
        let shard = ShardId::new(0);
        let id = state.begin().unwrap();
        let handle = state.reserve_transport(shard, Instant::now()).unwrap();
        state.stage_participant(pid(1), record(shard, handle)).unwrap();

        let binding = PreparedBinding {
            transaction: id,
            shard_id: shard,
            participant: participant_key(7),
        };
        state.record_binding(pid(1), binding).unwrap();
        state.record_binding(pid(1), binding).unwrap();

        assert_eq!(
            state.committed.participants.get(&pid(1)).and_then(|r| r.binding),
            Some(participant_key(7)),
        );
        assert_eq!(
            state.pending().map(|tx| tx.prepared.len()),
            Some(1),
            "the second report must not add a second binding"
        );
    }

    /// A binding report for a participant the transaction abandoned is a
    /// no-op, not a resurrection.
    #[tokio::test(start_paused = true)]
    async fn a_late_binding_report_for_an_unknown_participant_is_harmless() {
        let mut state = ControlPlaneState::new(2);
        let id = state.begin().unwrap();
        state
            .record_binding(
                pid(9),
                PreparedBinding {
                    transaction: id,
                    shard_id: ShardId::new(0),
                    participant: participant_key(1),
                },
            )
            .unwrap();
        assert!(state.committed.participants.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn only_one_transaction_stages_at_a_time() {
        let mut state = ControlPlaneState::new(2);
        state.begin().unwrap();
        assert_eq!(state.begin(), Err(TransactionError::Busy));
    }

    #[tokio::test(start_paused = true)]
    async fn a_transaction_records_every_shard_it_touches() {
        let mut state = ControlPlaneState::new(4);
        state.begin().unwrap();
        state.reserve_transport(ShardId::new(2), Instant::now()).unwrap();
        state.reserve_transport(ShardId::new(2), Instant::now()).unwrap();
        state.reserve_transport(ShardId::new(3), Instant::now()).unwrap();

        let affected = state.pending().map(LifecycleTransaction::affected).unwrap();
        assert_eq!(affected.len(), 2, "two distinct shards, however many routes");
        assert!(affected.contains(&ShardId::new(2)));
        assert!(affected.contains(&ShardId::new(3)));
    }
}

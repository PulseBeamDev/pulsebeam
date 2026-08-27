use slotmap::SlotMap;
use tokio::time::Instant;

use crate::{
    control::{controller::ParticipantState, registry::RoomRegistry},
    entity::{ParticipantId, RoomId},
    id::ShardId,
    participant::ParticipantConfig,
    route::{PackedRoute, SlotAllocator, TransportHandle, TransportRoute},
    shard::participants::ParticipantKey,
};

pub const DEFAULT_ROOM_SHARD_SLOT: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomPlacement {
    Hashed,
    RoundRobin,
}

struct TransportAllocators {
    shards: Vec<SlotAllocator>,
}

impl TransportAllocators {
    fn new(shard_count: usize) -> Self {
        Self {
            shards: (0..shard_count)
                .map(|index| {
                    SlotAllocator::with_max_slots(
                        ShardId::new(index),
                        PackedRoute::MAX_SLOT.saturating_add(1),
                    )
                })
                .collect(),
        }
    }

    #[allow(
        clippy::expect_used,
        reason = "a transport allocation for an unconfigured shard is a controller invariant violation"
    )]
    fn allocate(&mut self, shard: ShardId, now: Instant) -> TransportHandle {
        let allocator = self
            .shards
            .get_mut(shard.index())
            .expect("transport allocation must target a configured shard");
        let (slot, epoch) = allocator.allocate_transport(now);
        TransportHandle::new(TransportRoute::new(shard, slot), epoch)
    }

    fn retire(&mut self, handle: TransportHandle, now: Instant) {
        let Some(allocator) = self.shards.get_mut(handle.shard().index()) else {
            debug_assert!(false, "transport retirement targeted an unknown shard");
            return;
        };
        allocator.retire(handle.route.slot(), now);
    }
}

pub struct ControllerCore {
    pub(crate) registry: RoomRegistry,
    room_shard_slot: usize,
    placement: RoomPlacement,
    transport: TransportAllocators,
    participants: Vec<SlotMap<ParticipantKey, ParticipantId>>,
}

impl ControllerCore {
    pub fn with_placement(room_shard_slot: usize, placement: RoomPlacement) -> Self {
        debug_assert!(room_shard_slot > 0);
        Self {
            registry: RoomRegistry::new(),
            room_shard_slot,
            placement,
            transport: TransportAllocators::new(0),
            participants: Vec::new(),
        }
    }

    pub fn with_shards(
        shard_count: usize,
        room_shard_slot: usize,
        placement: RoomPlacement,
    ) -> Self {
        debug_assert!(shard_count > 0);
        let mut core = Self::with_placement(room_shard_slot, placement);
        core.transport = TransportAllocators::new(shard_count);
        core.participants = (0..shard_count).map(|_| SlotMap::with_key()).collect();
        core
    }

    pub fn room_slot(&self, room_id: &RoomId) -> (usize, RoomPlacement) {
        let count = self
            .registry
            .get_room(room_id)
            .map(super::room::Room::participant_count)
            .unwrap_or_default();
        (
            count.checked_div(self.room_shard_slot).unwrap_or(0),
            self.placement,
        )
    }

    pub fn reserve_transport(&mut self, shard: ShardId, now: Instant) -> TransportHandle {
        self.transport.allocate(shard, now)
    }

    pub fn release_transport(&mut self, handle: TransportHandle, now: Instant) {
        self.transport.retire(handle, now);
    }

    pub fn mint_participant(
        &mut self,
        shard: ShardId,
        participant: ParticipantId,
    ) -> Option<ParticipantKey> {
        let key = self
            .participants
            .get_mut(shard.index())?
            .insert(participant);
        Some(key)
    }

    pub fn remove_participant_key(&mut self, shard: ShardId, key: ParticipantKey) {
        let Some(arena) = self.participants.get_mut(shard.index()) else {
            debug_assert!(false, "participant key targeted an unknown shard");
            return;
        };
        let removed = arena.remove(key);
        debug_assert!(removed.is_some(), "participant key must be live at removal");
    }

    #[allow(clippy::too_many_arguments, reason = "this control-plane boundary materializes one complete participant configuration")]
    pub fn create_participant(
        &mut self,
        connection_id: pulsebeam_rtc::ConnectionId,
        session: pulsebeam_rtc::NegotiatedSession,
        local: pulsebeam_rtc::LocalTransport,
        state: ParticipantState,
        shard: ShardId,
        transport: TransportHandle,
        key: ParticipantKey,
    ) -> ParticipantConfig {
        self.registry
            .add_participant(state.participant_id, state.room_id, shard, Some(transport));
        self.registry
            .set_connection_id(&state.participant_id, state.connection_id);
        self.registry.bind_participant(&state.participant_id, key);
        ParticipantConfig {
            manual_sub: state.manual_sub,
            room_id: state.room_id,
            participant_id: state.participant_id,
            connection_id,
            session,
            local,
        }
    }

    pub fn delete_participant(&mut self, participant: &ParticipantId) -> Option<ParticipantMeta> {
        let meta = self.registry.get_participant(participant)?;
        let result = self.participant_meta(meta);
        self.registry.remove_participant(participant);
        Some(result)
    }

    pub fn disconnect_participant(
        &mut self,
        participant: &ParticipantId,
    ) -> Option<ParticipantMeta> {
        let meta = self.registry.get_participant(participant)?;
        let result = self.participant_meta(meta);
        let _ = self.registry.disconnect_participant(participant);
        Some(result)
    }

    fn participant_meta(&self, meta: &super::registry::ParticipantMeta) -> ParticipantMeta {
        ParticipantMeta {
            shard: meta.shard_id,
            binding: meta.binding,
            transport: meta.transport,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ParticipantMeta {
    pub shard: ShardId,
    pub binding: Option<ParticipantKey>,
    pub transport: Option<TransportHandle>,
}

use tokio::time::Instant;

use crate::{
    control::{controller::ParticipantState, registry::RoomRegistry},
    entity::{ParticipantId, RoomId},
    id::ShardId,
    participant::ParticipantConfig,
    route::{NodeTransportAddress, PackedRoute, SlotAllocator, TransportRoute},
};
use str0m::Rtc;

pub const DEFAULT_ROOM_SHARD_SLOT: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomPlacement {
    Hashed,
    RoundRobin,
}

struct TransportAddressAllocator {
    shards: Vec<SlotAllocator>,
}

impl TransportAddressAllocator {
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
    fn allocate(&mut self, shard: ShardId, now: Instant) -> NodeTransportAddress {
        let allocator = self
            .shards
            .get_mut(shard.index())
            .expect("transport allocation must target a configured shard");
        let (slot, epoch) = allocator.allocate_transport(now);
        NodeTransportAddress::new(TransportRoute::new(shard, slot), epoch)
    }

    fn retire(&mut self, address: NodeTransportAddress, now: Instant) {
        let Some(allocator) = self.shards.get_mut(address.shard().index()) else {
            debug_assert!(false, "transport retirement targeted an unknown shard");
            return;
        };
        allocator.retire(address.route.slot(), now);
    }
}

pub struct ControllerCore {
    pub(crate) registry: RoomRegistry,
    room_shard_slot: usize,
    placement: RoomPlacement,
    transport: TransportAddressAllocator,
}

impl ControllerCore {
    pub fn with_placement(room_shard_slot: usize, placement: RoomPlacement) -> Self {
        debug_assert!(room_shard_slot > 0);
        Self {
            registry: RoomRegistry::new(),
            room_shard_slot,
            placement,
            transport: TransportAddressAllocator::new(0),
        }
    }

    pub fn with_shards(
        shard_count: usize,
        room_shard_slot: usize,
        placement: RoomPlacement,
    ) -> Self {
        debug_assert!(shard_count > 0);
        let mut core = Self::with_placement(room_shard_slot, placement);
        core.transport = TransportAddressAllocator::new(shard_count);
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

    pub fn reserve_transport(&mut self, shard: ShardId, now: Instant) -> NodeTransportAddress {
        self.transport.allocate(shard, now)
    }

    pub fn release_transport(&mut self, address: NodeTransportAddress, now: Instant) {
        self.transport.retire(address, now);
    }

    pub fn create_participant(
        &mut self,
        rtc: Rtc,
        state: ParticipantState,
        shard: ShardId,
        transport: NodeTransportAddress,
    ) -> ParticipantConfig {
        self.registry
            .add_participant(state.participant_id, state.room_id, shard, Some(transport));
        self.registry
            .set_connection_id(&state.participant_id, state.connection_id);
        ParticipantConfig {
            manual_sub: state.manual_sub,
            room_id: state.room_id,
            participant_id: state.participant_id,
            rtc,
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
            transport: meta.transport,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ParticipantMeta {
    pub shard: ShardId,
    pub transport: Option<NodeTransportAddress>,
}

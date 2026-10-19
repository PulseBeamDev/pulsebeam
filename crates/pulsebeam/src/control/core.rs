use tokio::time::Instant;

use crate::{
    control::{
        controller::ParticipantState, negotiator::NegotiatedResources, registry::RoomRegistry,
    },
    entity::{ParticipantId, RoomId},
    id::ShardId,
    participant::ParticipantConfig,
    route::{NodeTransportAddress, PackedRoute, SlotAllocator, TransportRoute},
};
use str0m::Rtc;

pub const DEFAULT_ROOM_PARTITION_SIZE: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomPartitionPlacement {
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
    room_partition_size: usize,
    placement: RoomPartitionPlacement,
    transport: TransportAddressAllocator,
}

impl ControllerCore {
    pub fn with_placement(room_partition_size: usize, placement: RoomPartitionPlacement) -> Self {
        debug_assert!(room_partition_size > 0);
        Self {
            registry: RoomRegistry::new(),
            room_partition_size,
            placement,
            transport: TransportAddressAllocator::new(0),
        }
    }

    pub fn with_shards(
        shard_count: usize,
        room_partition_size: usize,
        placement: RoomPartitionPlacement,
    ) -> Self {
        debug_assert!(shard_count > 0);
        let mut core = Self::with_placement(room_partition_size, placement);
        core.transport = TransportAddressAllocator::new(shard_count);
        core
    }

    pub fn next_room_partition(
        &self,
        room_id: &RoomId,
        participant_id: &ParticipantId,
    ) -> (usize, RoomPartitionPlacement) {
        let index = self
            .registry
            .get_room_shard(room_id)
            .map(|room| room.next_partition_index(participant_id, self.room_partition_size))
            .unwrap_or(0);
        (index, self.placement)
    }

    pub fn reserve_transport(&mut self, shard: ShardId, now: Instant) -> NodeTransportAddress {
        self.transport.allocate(shard, now)
    }

    pub fn release_transport(&mut self, address: NodeTransportAddress, now: Instant) {
        self.transport.retire(address, now);
    }

    pub fn prepare_participant(
        &self,
        rtc: Rtc,
        resources: NegotiatedResources,
        state: ParticipantState,
    ) -> ParticipantConfig {
        ParticipantConfig {
            manual_sub: state.manual_sub,
            room_id: state.room_id,
            participant_id: state.participant_id,
            participant_external_id: state.participant_external_id,
            connection_id: state.connection_id,
            profile: state.profile,
            authorization_expiry: state
                .authorization
                .map(super::controller::AuthorizationLease::expiry),
            rtc,
            resources,
        }
    }

    pub fn remove_incarnation(
        &mut self,
        participant: &ParticipantId,
        connection_id: crate::entity::ConnectionId,
    ) -> bool {
        self.registry
            .remove_incarnation(participant, connection_id)
            .is_some()
    }
}

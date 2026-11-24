use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::ops::{Deref, DerefMut};

use pulsebeam_proto::data_control::{
    publisher_observation::Record as PublisherRecord,
    subscriber_observation::Record as SubscriberRecord,
};
use pulsebeam_runtime::net::RecvPacketBatch;
use slotmap::SlotMap;

use crate::{
    entity::{ConnectionId, EndpointKey},
    id::ShardId,
    participant::{Connection, ConnectionConfig, data_fabric::SubscriberLease},
    route::NodeTransportAddress,
    shard::{
        demux::Demuxer,
        router::{FabricLocalBranch, FabricSourceHandle},
    },
};

pub(crate) use crate::keys::ParticipantHandle;

pub(crate) struct ParticipantMeta {
    connection: Connection,
    pub(super) queued_dirty: bool,
    pub(super) ingress: NodeTransportAddress,
}

impl Deref for ParticipantMeta {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

impl DerefMut for ParticipantMeta {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.connection
    }
}

/// Carrier-independent identities for live shard owners.
#[derive(Default)]
struct ParticipantIndex {
    by_endpoint: HashMap<EndpointKey, ParticipantHandle>,
    by_connection: HashMap<ConnectionId, ParticipantHandle>,
    transports: Vec<Option<(NodeTransportAddress, ParticipantHandle)>>,
}

impl ParticipantIndex {
    fn install(
        &mut self,
        connection: ConnectionId,
        endpoint: Option<EndpointKey>,
        transport: NodeTransportAddress,
        handle: ParticipantHandle,
    ) {
        if let Some(endpoint) = endpoint {
            self.by_endpoint.insert(endpoint, handle);
        }
        let prior = self.by_connection.insert(connection, handle);
        debug_assert!(prior.is_none(), "a connection must be materialized once");
        self.install_transport(transport, handle);
    }

    fn remove(
        &mut self,
        connection: ConnectionId,
        endpoint: Option<EndpointKey>,
        transport: NodeTransportAddress,
        handle: ParticipantHandle,
    ) {
        if let Some(endpoint) = endpoint
            && self.resolve_endpoint(endpoint) == Some(handle)
        {
            self.by_endpoint.remove(&endpoint);
        }
        if self.resolve_connection(connection) == Some(handle) {
            self.by_connection.remove(&connection);
        }
        if self.resolve_transport(transport) == Some(handle) {
            self.retire_transport(transport);
        }
    }

    fn resolve_endpoint(&self, endpoint: EndpointKey) -> Option<ParticipantHandle> {
        self.by_endpoint.get(&endpoint).copied()
    }

    fn resolve_connection(&self, connection_id: ConnectionId) -> Option<ParticipantHandle> {
        self.by_connection.get(&connection_id).copied()
    }

    fn resolve_transport(&self, address: NodeTransportAddress) -> Option<ParticipantHandle> {
        match self.transports.get(address.route.index()) {
            Some(Some((installed, handle))) if *installed == address => Some(*handle),
            _ => None,
        }
    }

    fn install_transport(&mut self, address: NodeTransportAddress, handle: ParticipantHandle) {
        let index = address.route.index();
        if index >= self.transports.len() {
            self.transports
                .resize_with(index.saturating_add(1), || None);
        }
        let Some(slot) = self.transports.get_mut(index) else {
            debug_assert!(false, "transport slot must exist after resize");
            return;
        };
        *slot = Some((address, handle));
    }

    fn retire_transport(&mut self, address: NodeTransportAddress) {
        let Some(slot) = self.transports.get_mut(address.route.index()) else {
            return;
        };
        if slot.is_some_and(|(installed, _)| installed == address) {
            *slot = None;
        }
    }
}

enum ShardOwner {
    Rtc(Box<ParticipantMeta>),
    Quic(QuicOwner),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct QuicGrant {
    pub(crate) revision: u64,
    pub(crate) expiry: pulsebeam_core::auth::AuthorizationExpiry,
}

struct QuicOwner {
    connection: ConnectionId,
    ingress: NodeTransportAddress,
    association: NonZeroU64,
    grant: QuicGrant,
    data_receivers: BTreeMap<FabricLocalBranch, SubscriberLease>,
    data_control: VecDeque<(SubscriberLease, SubscriberRecord)>,
    publisher_control: VecDeque<(FabricSourceHandle, PublisherRecord)>,
    publisher_refresh: VecDeque<(FabricSourceHandle, u32, pulsebeam_runtime::time::Instant)>,
}

impl ShardOwner {
    fn ingress(&self) -> NodeTransportAddress {
        match self {
            Self::Rtc(meta) => meta.ingress,
            Self::Quic(owner) => owner.ingress,
        }
    }
}

pub(crate) struct ParticipantRegistry {
    shard_id: ShardId,
    max_gso_segments: usize,
    /// Boxing keeps `SlotMap` growth from copying full participants on the
    /// shard's real-time media thread.
    participants: SlotMap<ParticipantHandle, Box<ShardOwner>>,
    index: ParticipantIndex,
    demuxer: Demuxer,
    pending_close: VecDeque<(NodeTransportAddress, Vec<SocketAddr>)>,
}

impl ParticipantRegistry {
    pub fn len(&self) -> usize {
        self.participants.len()
    }

    pub fn new(shard_id: ShardId, max_gso_segments: usize, shard_count: u16) -> Self {
        debug_assert!(shard_count > 0);
        Self {
            shard_id,
            max_gso_segments,
            participants: SlotMap::with_key(),
            index: ParticipantIndex::default(),
            demuxer: Demuxer::for_node(0, 0, shard_count),
            pending_close: VecDeque::new(),
        }
    }

    pub fn insert(
        &mut self,
        cfg: ConnectionConfig,
        ingress: NodeTransportAddress,
    ) -> ParticipantHandle {
        debug_assert_eq!(ingress.shard(), self.shard_id);
        let endpoint = cfg.scope.endpoint(cfg.participant_id);
        let connection_id = cfg.connection_id;
        if let Some(previous) = endpoint.and_then(|endpoint| self.resolve_endpoint(endpoint)) {
            let _ = self.remove_handle(previous);
        }
        let connection = Connection::new(cfg, self.shard_id, self.max_gso_segments);
        let handle =
            self.participants
                .insert(Box::new(ShardOwner::Rtc(Box::new(ParticipantMeta {
                    connection,
                    queued_dirty: false,
                    ingress,
                }))));
        self.index.install(connection_id, endpoint, ingress, handle);
        tracing::info!(%connection_id, "participant added to shard");
        handle
    }

    pub(super) fn insert_quic(
        &mut self,
        connection: ConnectionId,
        ingress: NodeTransportAddress,
        association: NonZeroU64,
        grant: QuicGrant,
    ) -> bool {
        if grant.revision == 0
            || ingress.shard() != self.shard_id
            || self.resolve_connection(connection).is_some()
            || self
                .index
                .transports
                .get(ingress.route.index())
                .is_some_and(Option::is_some)
        {
            return false;
        }
        let handle = self
            .participants
            .insert(Box::new(ShardOwner::Quic(QuicOwner {
                connection,
                ingress,
                association,
                grant,
                data_receivers: BTreeMap::new(),
                data_control: VecDeque::new(),
                publisher_control: VecDeque::new(),
                publisher_refresh: VecDeque::new(),
            })));
        self.index.install(connection, None, ingress, handle);
        true
    }

    pub(super) fn connection_id(&self, handle: ParticipantHandle) -> Option<ConnectionId> {
        match self.participants.get(handle)?.as_ref() {
            ShardOwner::Rtc(owner) => Some(owner.connection_id),
            ShardOwner::Quic(owner) => Some(owner.connection),
        }
    }

    pub(super) fn is_quic_owner(&self, handle: ParticipantHandle) -> bool {
        matches!(
            self.participants.get(handle).map(Box::as_ref),
            Some(ShardOwner::Quic(_))
        )
    }

    pub(super) fn bind_quic_data_receiver(
        &mut self,
        handle: ParticipantHandle,
        lease: SubscriberLease,
    ) -> bool {
        let Some(ShardOwner::Quic(owner)) = self.participants.get_mut(handle).map(Box::as_mut)
        else {
            return false;
        };
        match owner.data_receivers.entry(lease.branch) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(lease);
                true
            }
            std::collections::btree_map::Entry::Occupied(_) => false,
        }
    }

    pub(super) fn unbind_quic_data_receiver(
        &mut self,
        handle: ParticipantHandle,
        branch: FabricLocalBranch,
    ) {
        if let Some(ShardOwner::Quic(owner)) = self.participants.get_mut(handle).map(Box::as_mut) {
            owner.data_receivers.remove(&branch);
            owner
                .data_control
                .retain(|(lease, _)| lease.branch != branch);
        }
    }

    pub(super) fn enqueue_quic_subscriber_control(
        &mut self,
        handle: ParticipantHandle,
        lease: SubscriberLease,
        record: SubscriberRecord,
    ) -> bool {
        const MAX_PENDING: usize = 64;
        let Some(ShardOwner::Quic(owner)) = self.participants.get_mut(handle).map(Box::as_mut)
        else {
            return false;
        };
        if owner.data_control.len() >= MAX_PENDING
            || owner.data_receivers.get(&lease.branch) != Some(&lease)
        {
            return false;
        }
        owner.data_control.push_back((lease, record));
        true
    }

    pub(super) fn pop_quic_subscriber_control(
        &mut self,
        handle: ParticipantHandle,
    ) -> Option<(SubscriberLease, SubscriberRecord)> {
        let ShardOwner::Quic(owner) = self.participants.get_mut(handle)?.as_mut() else {
            return None;
        };
        owner.data_control.pop_front()
    }

    pub(super) fn enqueue_quic_publisher_control(
        &mut self,
        handle: ParticipantHandle,
        source: FabricSourceHandle,
        record: PublisherRecord,
    ) -> bool {
        const MAX_PENDING: usize = 64;
        let Some(ShardOwner::Quic(owner)) = self.participants.get_mut(handle).map(Box::as_mut)
        else {
            return false;
        };
        if owner.publisher_control.len() >= MAX_PENDING {
            return false;
        }
        owner.publisher_control.push_back((source, record));
        true
    }

    pub(super) fn pop_quic_publisher_control(
        &mut self,
        handle: ParticipantHandle,
    ) -> Option<(FabricSourceHandle, PublisherRecord)> {
        let ShardOwner::Quic(owner) = self.participants.get_mut(handle)?.as_mut() else {
            return None;
        };
        owner.publisher_control.pop_front()
    }

    pub(super) fn enqueue_quic_publisher_refresh(
        &mut self,
        handle: ParticipantHandle,
        source: FabricSourceHandle,
        ssrc: u32,
        now: pulsebeam_runtime::time::Instant,
    ) -> bool {
        let Some(ShardOwner::Quic(owner)) = self.participants.get_mut(handle).map(Box::as_mut)
        else {
            return false;
        };
        let pending = &mut owner.publisher_refresh;
        pending.retain(|(key, _, at)| {
            *key != source && now.saturating_duration_since(*at) < crate::track::KEYFRAME_DEBOUNCE
        });
        if pending.len() >= 8 {
            return false;
        }
        pending.push_back((source, ssrc, now));
        true
    }

    pub(super) fn pop_quic_publisher_refresh(
        &mut self,
        handle: ParticipantHandle,
    ) -> Option<(FabricSourceHandle, u32, pulsebeam_runtime::time::Instant)> {
        let ShardOwner::Quic(owner) = self.participants.get_mut(handle)?.as_mut() else {
            return None;
        };
        owner.publisher_refresh.pop_front()
    }

    pub(super) fn quic_receiver_lease(
        &self,
        handle: ParticipantHandle,
        owned: u64,
    ) -> Option<SubscriberLease> {
        let ShardOwner::Quic(owner) = self.participants.get(handle)?.as_ref() else {
            return None;
        };
        owner
            .data_receivers
            .values()
            .find(|lease| lease.owned == owned)
            .copied()
    }

    pub(super) fn reliable_receiver(
        &self,
        handle: ParticipantHandle,
        branch: FabricLocalBranch,
        unix_now: Option<u64>,
    ) -> Option<SubscriberLease> {
        match self.participants.get(handle)?.as_ref() {
            ShardOwner::Rtc(owner) => owner.reliable_subscriber_branch(branch),
            ShardOwner::Quic(owner)
                if self.quic_owner_authorized(handle, owner.association, unix_now?) =>
            {
                owner.data_receivers.get(&branch).copied()
            }
            ShardOwner::Quic(_) => None,
        }
    }

    pub(super) fn current_quic_owner_authorized(
        &self,
        handle: ParticipantHandle,
        now: u64,
    ) -> bool {
        let Some(ShardOwner::Quic(owner)) = self.participants.get(handle).map(Box::as_ref) else {
            return false;
        };
        self.quic_owner_authorized(handle, owner.association, now)
    }

    pub(super) fn quic_owner_authorized(
        &self,
        handle: ParticipantHandle,
        association: NonZeroU64,
        now: u64,
    ) -> bool {
        matches!(self.participants.get(handle).map(Box::as_ref),
            Some(ShardOwner::Quic(owner)) if owner.association == association
                && self.resolve_transport(owner.ingress) == Some(handle)
                && !owner.grant.expiry.is_expired_at(now))
    }

    pub(super) fn quic_grant(
        &self,
        connection: ConnectionId,
        ingress: NodeTransportAddress,
        association: NonZeroU64,
        now: u64,
    ) -> Option<QuicGrant> {
        let handle = self.resolve_transport(ingress)?;
        let ShardOwner::Quic(owner) = self.participants.get(handle)?.as_ref() else {
            return None;
        };
        (owner.connection == connection
            && owner.association == association
            && !owner.grant.expiry.is_expired_at(now))
        .then_some(owner.grant)
    }

    pub(super) fn update_quic_grant(&mut self, connection: ConnectionId, grant: QuicGrant) {
        let Some(handle) = self.resolve_connection(connection) else {
            return;
        };
        if let Some(ShardOwner::Quic(owner)) = self.participants.get_mut(handle).map(Box::as_mut)
            && owner.grant.revision.checked_add(1) == Some(grant.revision)
        {
            owner.grant = grant;
        }
    }

    pub(super) fn retire_quic(
        &mut self,
        connection: ConnectionId,
        ingress: NodeTransportAddress,
        association: NonZeroU64,
    ) {
        let Some(handle) = self.resolve_connection(connection) else {
            return;
        };
        if matches!(self.participants.get(handle).map(Box::as_ref),
            Some(ShardOwner::Quic(owner)) if owner.ingress == ingress && owner.association == association)
        {
            self.remove_quic_owner(handle, connection, ingress);
        }
    }

    fn remove_quic_owner(
        &mut self,
        handle: ParticipantHandle,
        connection: ConnectionId,
        ingress: NodeTransportAddress,
    ) {
        self.participants.remove(handle);
        self.index.remove(connection, None, ingress, handle);
    }

    pub fn remove(
        &mut self,
        connection: ConnectionId,
        address: NodeTransportAddress,
    ) -> Option<Box<ParticipantMeta>> {
        let handle = self.resolve_connection(connection)?;
        let owner = self.participants.get(handle)?.as_ref();
        if owner.ingress() != address {
            return None;
        }
        if matches!(owner, ShardOwner::Quic(_)) {
            self.remove_quic_owner(handle, connection, address);
            return None;
        }
        self.remove_handle(handle)
    }

    fn remove_handle(&mut self, handle: ParticipantHandle) -> Option<Box<ParticipantMeta>> {
        if !matches!(self.participants.get(handle)?.as_ref(), ShardOwner::Rtc(_)) {
            return None;
        }
        let ShardOwner::Rtc(meta) = *self.participants.remove(handle)? else {
            return None;
        };
        self.index.remove(
            meta.connection_id,
            meta.scope.endpoint(meta.participant_id),
            meta.ingress,
            handle,
        );
        let addrs = self.demuxer.unregister(meta.ingress.route);
        self.pending_close.push_back((meta.ingress, addrs));
        Some(meta)
    }

    pub fn resolve_endpoint(&self, endpoint: EndpointKey) -> Option<ParticipantHandle> {
        self.index.resolve_endpoint(endpoint)
    }

    pub fn resolve_connection(&self, connection_id: ConnectionId) -> Option<ParticipantHandle> {
        self.index.resolve_connection(connection_id)
    }

    pub fn resolve_mut(&mut self, key: ParticipantHandle) -> Option<&mut ParticipantMeta> {
        match self.participants.get_mut(key)?.as_mut() {
            ShardOwner::Rtc(meta) => Some(meta.as_mut()),
            ShardOwner::Quic(_) => None,
        }
    }

    pub fn resolve_transport(&self, address: NodeTransportAddress) -> Option<ParticipantHandle> {
        self.index.resolve_transport(address)
    }

    pub fn retire_transport(&mut self, address: NodeTransportAddress) {
        self.index.retire_transport(address);
    }

    pub fn demux(&mut self, batch: &RecvPacketBatch) -> Option<NodeTransportAddress> {
        self.demuxer.demux(batch)
    }

    /// Cache an address a sibling shard resolved on this shard's behalf.
    ///
    /// Steering is a cache, and populating it moves a flow from the shard the
    /// tuple hash picked to the shard that owns the route — which has never
    /// seen the flow's STUN and so cannot classify anything that follows it.
    /// Learning the address while forwarding is still happening is what makes
    /// that handover lossless.
    pub fn learn_addr(&mut self, src: SocketAddr, address: NodeTransportAddress) {
        self.demuxer.learn(src, address);
    }

    pub fn authenticate_addr(&mut self, src: SocketAddr, address: NodeTransportAddress) {
        self.demuxer.authenticate(src, address);
    }

    /// The route a participant's authenticated address belongs to.
    ///
    /// Reports the route address so the shard can tell control which flow to
    /// pin in the steering map.
    pub fn authenticated_address(&self, key: ParticipantHandle) -> Option<NodeTransportAddress> {
        let Some(meta) = self.participants.get(key) else {
            debug_assert!(false, "authenticated participant must still be registered");
            return None;
        };
        Some(meta.ingress())
    }

    pub fn drain_pending_close(
        &mut self,
    ) -> impl Iterator<Item = (NodeTransportAddress, Vec<SocketAddr>)> + '_ {
        self.pending_close.drain(..)
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;

    fn value_size<K: slotmap::Key, V>(_: &SlotMap<K, V>) -> usize {
        std::mem::size_of::<V>()
    }

    /// The registry's element stride is a pointer, not a participant.
    ///
    /// `SlotMap` is a dense `Vec` indexed by the handle, so whatever it holds
    /// is what gets memcpied every time the map grows — on the shard's
    /// `SCHED_FIFO` thread, with media flowing. Storing the participant inline
    /// made that ~2.7MB in one go at 500 participants; a pointer makes it 8KB.
    ///
    /// This is a stride check rather than a style check: if `ParticipantMeta`
    /// ever shrinks to something a `Vec` can carry, the indirection can go and
    /// this assertion is the place to reconsider it.
    #[test]
    fn the_registry_holds_participants_behind_a_pointer() {
        let registry = ParticipantRegistry::new(ShardId::new(0), 1, 1);
        assert_eq!(
            value_size(&registry.participants),
            std::mem::size_of::<usize>(),
            "the participant registry must store a pointer per slot"
        );
        assert!(
            std::mem::size_of::<ParticipantMeta>() > 4096,
            "a participant is small enough to inline now; revisit the Box and this test"
        );
    }

    #[test]
    fn unjoined_transports_do_not_claim_or_replace_an_endpoint() {
        use crate::control::{NegotiatedResources, controller::SignalingProtocol};
        use crate::entity::{ConnectionScope, ProjectRoomKey, RoomId};
        use crate::route::TransportRoute;

        let shard = ShardId::new(0);
        let room = ProjectRoomKey::fixture(RoomId::new("stage").unwrap());
        let participant = crate::entity::fixture_participant_id();
        let mut registry = ParticipantRegistry::new(shard, 1, 1);
        let mut install = |scope, index| {
            let connection_id = ConnectionId::new();
            let transport = NodeTransportAddress::new(TransportRoute::new(shard, index), 1);
            let handle = registry.insert(
                ConnectionConfig {
                    scope,
                    participant_id: participant,
                    connection_id,
                    profile: SignalingProtocol::State,
                    authorization_expiry: None,
                    rtc: crate::control::application_only_test_connection(),
                    resources: NegotiatedResources::empty_for_test(),
                },
                transport,
            );
            (connection_id, transport, handle)
        };
        let roomless = ConnectionScope {
            project: room.project,
            room: None,
        };
        let (first, first_transport, first_handle) = install(roomless, 1);
        let (_, _, joined_handle) = install(room.into(), 2);
        let (second, second_transport, second_handle) = install(roomless, 3);
        let endpoint = EndpointKey { room, participant };
        assert_eq!(registry.len(), 3);
        assert_eq!(registry.resolve_endpoint(endpoint), Some(joined_handle));
        assert_eq!(registry.resolve_connection(first), Some(first_handle));
        assert_eq!(registry.resolve_connection(second), Some(second_handle));
        assert!(registry.remove(first, first_transport).is_some());
        assert!(registry.resolve_connection(first).is_none());
        assert_eq!(
            registry.resolve_transport(second_transport),
            Some(second_handle)
        );
        assert_eq!(registry.resolve_endpoint(endpoint), Some(joined_handle));
    }

    #[test]
    fn quic_owner_uses_shared_indexes_and_requires_exact_retirement() {
        let shard = ShardId::new(0);
        let mut registry = ParticipantRegistry::new(shard, 1, 1);
        let connection = ConnectionId::from_bytes([1; 16]);
        let route = crate::route::TransportRoute::new(shard, 7);
        let address = NodeTransportAddress::new(route, 1);
        let serial = NonZeroU64::MIN;
        let grant = QuicGrant {
            revision: 1,
            expiry: pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds(100),
        };
        assert!(registry.insert_quic(connection, address, serial, grant));
        let handle = registry.resolve_connection(connection).unwrap();
        assert_eq!(registry.resolve_transport(address), Some(handle));
        assert!(registry.resolve_mut(handle).is_none());
        assert!(!registry.insert_quic(connection, address, serial, grant));
        assert!(
            registry
                .quic_grant(connection, address, serial, 100)
                .is_none()
        );
        let next = QuicGrant {
            revision: 2,
            expiry: pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds(200),
        };
        registry.update_quic_grant(
            connection,
            QuicGrant {
                revision: 3,
                ..next
            },
        );
        assert!(
            registry
                .quic_grant(connection, address, serial, 100)
                .is_none()
        );
        registry.update_quic_grant(connection, next);
        registry.update_quic_grant(connection, grant);
        assert_eq!(
            registry
                .quic_grant(connection, address, serial, 100)
                .unwrap()
                .revision,
            2
        );
        registry.retire_quic(connection, address, NonZeroU64::new(2).unwrap());
        assert_eq!(registry.resolve_connection(connection), Some(handle));
        registry.retire_quic(connection, address, serial);
        assert_eq!(registry.resolve_connection(connection), None);
        assert_eq!(registry.resolve_transport(address), None);
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn stale_transport_retirement_cannot_remove_a_replacement_handle() {
        let shard = ShardId::new(0);
        let route = crate::route::TransportRoute::new(shard, 7);
        let old_address = NodeTransportAddress::new(route, 1);
        let replacement_address = NodeTransportAddress::new(route, 2);
        let mut handles = SlotMap::<ParticipantHandle, ()>::with_key();
        let old_handle = handles.insert(());
        let replacement_handle = handles.insert(());
        let mut index = ParticipantIndex::default();

        index.install_transport(old_address, old_handle);
        index.install_transport(replacement_address, replacement_handle);
        index.retire_transport(old_address);

        assert_eq!(
            index.resolve_transport(replacement_address),
            Some(replacement_handle)
        );
    }
}

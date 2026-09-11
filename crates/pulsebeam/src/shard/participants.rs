use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};

use pulsebeam_runtime::net::RecvPacketBatch;
use slotmap::SlotMap;

use crate::{
    entity::ParticipantId,
    id::ShardId,
    participant::{ParticipantConfig, ParticipantCore},
    route::NodeTransportAddress,
    shard::demux::Demuxer,
};

pub(crate) use crate::keys::ParticipantHandle;

pub(crate) struct ParticipantMeta {
    core: ParticipantCore,
    pub(super) queued_dirty: bool,
    pub(super) ingress: NodeTransportAddress,
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
    /// Boxed, and that is the point.
    ///
    /// `SlotMap` is a dense `Vec` indexed by the handle, so its element size
    /// is its stride. `ParticipantMeta` is ~10.9KB — three quarters of it
    /// str0m's `Rtc` — and growing the map `Vec::extend`s, which reallocates
    /// and memcpies every participant already in it. On a shard filling to 500
    /// that is ~2.7MB copied in one go, on a `SCHED_FIFO` thread, while media
    /// is flowing. A pointer costs 16 bytes of stride instead of 10,904, so the
    /// same growth moves 8KB.
    ///
    /// The indirection is free where it matters: resolving a participant always
    /// missed on a 10.9KB object anyway, and the pointer array it now goes
    /// through is dense enough to stay resident (500 participants = 8KB).
    participants: SlotMap<ParticipantHandle, Box<ParticipantMeta>>,
    by_id: HashMap<ParticipantId, ParticipantHandle>,
    transports: Vec<Option<(NodeTransportAddress, ParticipantHandle)>>,
    demuxer: Demuxer,
    pending_close: VecDeque<SocketAddr>,
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
            by_id: HashMap::new(),
            transports: Vec::new(),
            demuxer: Demuxer::for_node(0, 0, shard_count),
            pending_close: VecDeque::new(),
        }
    }

    pub fn insert(
        &mut self,
        cfg: ParticipantConfig,
        ingress: NodeTransportAddress,
    ) -> ParticipantHandle {
        debug_assert_eq!(ingress.shard(), self.shard_id);
        let participant_id = cfg.participant_id;
        if let Some(previous) = self.by_id.get(&participant_id).copied() {
            let _ = self.remove_handle(previous);
        }
        let core = ParticipantCore::new(cfg, self.shard_id, self.max_gso_segments, 1);
        let handle = self.participants.insert(Box::new(ParticipantMeta {
            core,
            queued_dirty: false,
            ingress,
        }));
        self.by_id.insert(participant_id, handle);
        self.install_transport(ingress, handle);
        tracing::info!(%participant_id, "participant added to shard");
        handle
    }

    pub fn remove(&mut self, address: NodeTransportAddress) -> Option<Box<ParticipantMeta>> {
        let handle = self.resolve_transport(address)?;
        self.remove_handle(handle)
    }

    fn remove_handle(&mut self, handle: ParticipantHandle) -> Option<Box<ParticipantMeta>> {
        let meta = self.participants.remove(handle)?;
        if self.by_id.get(&meta.participant_id) == Some(&handle) {
            self.by_id.remove(&meta.participant_id);
        }
        self.retire_transport(meta.ingress);
        let addrs = self.demuxer.unregister(meta.ingress.route);
        self.pending_close.extend(addrs);
        Some(meta)
    }

    pub fn resolve(&self, participant_id: &ParticipantId) -> Option<ParticipantHandle> {
        self.by_id.get(participant_id).copied()
    }

    pub fn resolve_mut(&mut self, key: ParticipantHandle) -> Option<&mut ParticipantMeta> {
        self.participants.get_mut(key).map(Box::as_mut)
    }

    pub fn resolve_transport(&self, address: NodeTransportAddress) -> Option<ParticipantHandle> {
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

    pub fn retire_transport(&mut self, address: NodeTransportAddress) {
        let Some(slot) = self.transports.get_mut(address.route.index()) else {
            return;
        };
        if slot.is_some_and(|(installed, _)| installed == address) {
            *slot = None;
        }
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
        Some(meta.ingress)
    }

    pub fn drain_pending_close(&mut self) -> impl Iterator<Item = SocketAddr> + '_ {
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
    fn stale_transport_retirement_cannot_remove_a_replacement_handle() {
        let shard = ShardId::new(0);
        let route = crate::route::TransportRoute::new(shard, 7);
        let old_address = NodeTransportAddress::new(route, 1);
        let replacement_address = NodeTransportAddress::new(route, 2);
        let mut handles = SlotMap::<ParticipantHandle, ()>::with_key();
        let old_handle = handles.insert(());
        let replacement_handle = handles.insert(());
        let mut registry = ParticipantRegistry::new(shard, 1, 1);

        registry.install_transport(old_address, old_handle);
        registry.install_transport(replacement_address, replacement_handle);
        registry.retire_transport(old_address);

        assert_eq!(
            registry.resolve_transport(replacement_address),
            Some(replacement_handle)
        );
    }
}

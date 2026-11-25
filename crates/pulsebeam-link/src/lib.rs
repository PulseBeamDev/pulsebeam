#![forbid(unsafe_code)]

//! Bounded delivery between independently owned execution contexts.

use std::collections::HashMap;
use std::num::NonZeroUsize;

mod control;
mod fabric_keys;
pub mod io;
mod network;
pub mod wan;
mod wire;
pub use control::{CONTROL_FRAME_BYTES, PeerControl, StreamControl, StreamInbound, StreamOutbound};
pub use network::{
    CodecError, FabricCodec, FabricPath, FabricPeer, LinkError, NetworkClass, NetworkId,
    NetworkPeer, NodeRouteKey, PeerFingerprint, PeerRole, SecretCredential, WireCodec,
};
pub use pulsebeam_transport::{
    PeerConfig, QuicCredential, QuicPeer, QuicTuning, ReceiveTime, Time as LinkTime,
};

// Only provisioning writes these independent lifecycle words. Packet paths read
// them without locks or RMWs; queued packets carry indices, not shared refcounts.
#[allow(
    clippy::disallowed_types,
    reason = "cold-owned lifecycle table; independent atomic fences, no packet-rate writes or reference-count operations"
)]
mod lifecycle {
    pub(super) use std::sync::Arc;
    pub(super) use std::sync::atomic::{AtomicU64, Ordering};
    pub(super) type Shared<T> = Arc<T>;
    pub(super) type Fence = AtomicU64;
}
use lifecycle::{Fence, Ordering, Shared};

const QUEUE_CAPACITY: NonZeroUsize = const { NonZeroUsize::new(1024).unwrap() };
const MAX_BINDINGS: usize = 65_536;
const MAX_PORTS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveryTag(u64);

impl DeliveryTag {
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortId {
    owner: u64,
    slot: usize,
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BindingId {
    owner: u64,
    slot: usize,
    generation: u64,
}

pub enum Destination {
    Local {
        port: PortId,
        tag: DeliveryTag,
    },
    Network {
        peer: NetworkId,
        address: NodeRouteKey,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindError {
    Closed,
    Capacity,
    UnsupportedProfile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendErrorKind {
    Full,
    Closed,
    Stale,
    NotReady,
    TooLarge,
    Unsupported,
}

#[derive(Debug)]
pub struct SendError<P> {
    pub kind: SendErrorKind,
    pub packet: P,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FabricPurpose {
    Live,
    RepairOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FabricIngress {
    pub association: NetworkId,
    pub key: NodeRouteKey,
    pub purpose: FabricPurpose,
}

#[derive(Debug)]
pub struct Received<P> {
    pub tag: DeliveryTag,
    pub packet: P,
    pub received_at: ReceiveTime,
    pub fabric: Option<FabricIngress>,
}

struct Pending<P> {
    binding: BindingId,
    parent: Option<BindingId>,
    expires_at: LinkTime,
    received: Received<P>,
}

struct Table {
    bindings: Box<[Fence]>,
    ports: Box<[Fence]>,
}

impl Table {
    fn live(words: &[Fence], slot: usize, generation: u64) -> bool {
        words
            .get(slot)
            .is_some_and(|word| word.load(Ordering::Acquire) == generation)
    }

    fn binding_live(&self, binding: BindingId) -> bool {
        Self::live(&self.bindings, binding.slot, binding.generation)
    }

    fn port_live(&self, port: PortId) -> bool {
        Self::live(&self.ports, port.slot, port.generation)
    }

    fn allocate(words: &[Fence]) -> Option<(usize, u64)> {
        words.iter().enumerate().find_map(|(slot, word)| {
            let previous = word.load(Ordering::Relaxed);
            if previous % 2 != 0 {
                return None;
            }
            let generation = previous.checked_add(1)?;
            // Leave the final generation retired permanently, never wrap.
            if generation == u64::MAX {
                return None;
            }
            word.store(generation, Ordering::Release);
            Some((slot, generation))
        })
    }

    fn retire(words: &[Fence], slot: usize, generation: u64) {
        if let Some(word) = words.get(slot) {
            let _ = word.compare_exchange(
                generation,
                generation.saturating_add(1),
                Ordering::Release,
                Ordering::Relaxed,
            );
        }
    }
}

/// A generation-specific fence that a shard can apply at its lifecycle cutoff.
/// LinkControl still owns the binding record and its cleanup.
#[derive(Clone)]
pub struct BindingCutoff {
    table: Shared<Table>,
    binding: BindingId,
}

impl std::fmt::Debug for BindingCutoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BindingCutoff")
            .field("binding", &self.binding)
            .finish()
    }
}

impl BindingCutoff {
    pub fn id(&self) -> BindingId {
        self.binding
    }

    pub fn is_live(&self) -> bool {
        self.table.binding_live(self.binding)
    }

    pub fn fence(&self) {
        Table::retire(
            &self.table.bindings,
            self.binding.slot,
            self.binding.generation,
        );
    }
}

/// One provisioning owner. The clock closure must use the host's injected
/// monotonic domain, in production and simulation alike.
pub struct LinkControl<P> {
    owner: u64,
    table: Shared<Table>,
    clock: Shared<dyn Fn() -> LinkTime + Send + Sync>,
    ports: HashMap<usize, pulsebeam_queue::Sender<Pending<P>>>,
    bindings: HashMap<BindingId, BindingTarget>,
    networks: HashMap<NetworkId, network::Entry<P>>,
    next_network: u64,
}

impl<P> LinkControl<P> {
    pub fn new(clock: impl Fn() -> LinkTime + Send + Sync + 'static) -> Result<Self, BindError> {
        // Process-local identity allocation is cold-path only. Exhaustion fails
        // rather than making an old control's handles valid on another control.
        static NEXT_OWNER: Fence = Fence::new(1);
        let owner = NEXT_OWNER
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| BindError::Capacity)?;
        Ok(Self {
            owner,
            table: Shared::new(Table {
                bindings: (0..MAX_BINDINGS).map(|_| Fence::new(0)).collect(),
                ports: (0..MAX_PORTS).map(|_| Fence::new(0)).collect(),
            }),
            clock: Shared::new(clock),
            ports: HashMap::new(),
            bindings: HashMap::new(),
            networks: HashMap::new(),
            next_network: 0,
        })
    }

    pub fn create_port(&mut self) -> Result<(PortId, LinkRx<P>), BindError> {
        let (slot, generation) = Table::allocate(&self.table.ports).ok_or(BindError::Capacity)?;
        let port = PortId {
            owner: self.owner,
            slot,
            generation,
        };
        let (sender, receiver) = pulsebeam_queue::bounded(QUEUE_CAPACITY);
        self.ports.insert(slot, sender);
        Ok((
            port,
            LinkRx {
                port,
                table: self.table.clone(),
                clock: self.clock.clone(),
                receiver,
                drops: DropCounts::default(),
            },
        ))
    }

    pub fn bind_send(&mut self, to: Destination) -> Result<(BindingId, LinkTx<P>), BindError> {
        let (destination, target) = match to {
            Destination::Local { port, tag } => {
                if port.owner != self.owner || !self.table.port_live(port) {
                    return Err(BindError::Closed);
                }
                let sender = self.ports.get(&port.slot).ok_or(BindError::Closed)?.clone();
                (
                    TxDestination::Local { port, tag, sender },
                    BindingTarget::Local(port),
                )
            }
            Destination::Network { peer, address } => {
                if address.to_bytes() == control::CONTEXT {
                    return Err(BindError::Closed);
                }
                let entry = self.networks.get(&peer).ok_or(BindError::Closed)?;
                if entry.state.load(Ordering::Acquire) == network::CLOSED {
                    return Err(BindError::Closed);
                }
                (
                    TxDestination::Network(entry.tx(address)),
                    BindingTarget::NetworkSend(peer),
                )
            }
        };
        let binding = self.allocate_binding()?;
        self.bindings.insert(binding, target);
        Ok((
            binding,
            LinkTx {
                binding,
                parent: None,
                destination,
                table: self.table.clone(),
                clock: self.clock.clone(),
            },
        ))
    }

    fn allocate_binding(&self) -> Result<BindingId, BindError> {
        let (slot, generation) =
            Table::allocate(&self.table.bindings).ok_or(BindError::Capacity)?;
        Ok(BindingId {
            owner: self.owner,
            slot,
            generation,
        })
    }

    pub fn unbind(&mut self, binding: BindingId) {
        if let Some(target) = self.bindings.remove(&binding) {
            target.retire(&self.table, binding);
            if let BindingTarget::NetworkReceive {
                peer,
                fabric: Some(route),
                ..
            } = target
                && let Some(entry) = self.networks.get_mut(&peer)
            {
                entry.retire_fabric_key((self.clock)(), route.key);
            }
        }
    }

    pub fn close_port(&mut self, port: PortId) {
        if port.owner == self.owner && self.table.port_live(port) {
            Table::retire(&self.table.ports, port.slot, port.generation);
            let networks = &mut self.networks;
            let now = (self.clock)();
            self.bindings.retain(|binding, target| {
                let keep = match target {
                    BindingTarget::Local(target) => *target != port,
                    BindingTarget::NetworkReceive {
                        destinations,
                        fabric: Some(_),
                        ..
                    } => !destinations.iter().any(|&(target, _)| target == port),
                    BindingTarget::NetworkReceive { destinations, .. } => {
                        retire_receive_port(&self.table, destinations, port);
                        !destinations.is_empty()
                    }
                    BindingTarget::NetworkSend(_) => true,
                };
                if !keep {
                    target.retire(&self.table, *binding);
                    if let BindingTarget::NetworkReceive {
                        peer,
                        fabric: Some(route),
                        ..
                    } = target
                        && let Some(entry) = networks.get_mut(peer)
                    {
                        entry.retire_fabric_key(now, route.key);
                    }
                }
                keep
            });
            if let Some(sender) = self.ports.remove(&port.slot) {
                sender.close();
            }
        }
    }
}

impl<P> Drop for LinkControl<P> {
    fn drop(&mut self) {
        for word in self.table.ports.iter().chain(self.table.bindings.iter()) {
            let value = word.load(Ordering::Relaxed);
            if value % 2 != 0 {
                word.store(value.saturating_add(1), Ordering::Release);
            }
        }
        for sender in self.ports.values() {
            sender.close();
        }
        for entry in self.networks.values() {
            entry.close();
        }
    }
}

enum BindingTarget {
    Local(PortId),
    NetworkSend(NetworkId),
    NetworkReceive {
        peer: NetworkId,
        destinations: Vec<(PortId, BindingId)>,
        fabric: Option<network::StagedFabricRoute>,
    },
}

fn retire_receive_port(table: &Table, destinations: &mut Vec<(PortId, BindingId)>, port: PortId) {
    if let Some(index) = destinations.iter().position(|&(target, _)| target == port) {
        let (_, child) = destinations.swap_remove(index);
        Table::retire(&table.bindings, child.slot, child.generation);
    }
}

impl BindingTarget {
    fn retire(&self, table: &Table, binding: BindingId) {
        Table::retire(&table.bindings, binding.slot, binding.generation);
        if let Self::NetworkReceive { destinations, .. } = self {
            for (_, destination) in destinations {
                Table::retire(&table.bindings, destination.slot, destination.generation);
            }
        }
    }
}

enum TxDestination<P> {
    Local {
        port: PortId,
        tag: DeliveryTag,
        sender: pulsebeam_queue::Sender<Pending<P>>,
    },
    Network(network::NetworkTx<P>),
}

pub struct LinkTx<P> {
    binding: BindingId,
    parent: Option<BindingId>,
    table: Shared<Table>,
    clock: Shared<dyn Fn() -> LinkTime + Send + Sync>,
    destination: TxDestination<P>,
}

impl<P> std::fmt::Debug for LinkTx<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkTx")
            .field("binding", &self.binding)
            .finish_non_exhaustive()
    }
}

impl<P> PartialEq for LinkTx<P> {
    fn eq(&self, other: &Self) -> bool {
        self.binding == other.binding
    }
}
impl<P> Eq for LinkTx<P> {}

impl<P> Clone for LinkTx<P> {
    fn clone(&self) -> Self {
        Self {
            binding: self.binding,
            parent: self.parent,
            table: self.table.clone(),
            clock: self.clock.clone(),
            destination: match &self.destination {
                TxDestination::Local { port, tag, sender } => TxDestination::Local {
                    port: *port,
                    tag: *tag,
                    sender: sender.clone(),
                },
                TxDestination::Network(tx) => TxDestination::Network(tx.clone()),
            },
        }
    }
}

impl<P> LinkTx<P> {
    pub fn is_live(&self) -> bool {
        self.table.binding_live(self.binding)
            && self
                .parent
                .is_none_or(|parent| self.table.binding_live(parent))
    }

    pub fn local_cutoff(&self) -> Option<BindingCutoff> {
        if self.local_delivery_tag().is_none() || !self.is_live() {
            return None;
        }
        Some(BindingCutoff {
            table: self.table.clone(),
            binding: self.binding,
        })
    }

    pub fn local_delivery_tag(&self) -> Option<DeliveryTag> {
        match &self.destination {
            TxDestination::Local { tag, .. } => Some(*tag),
            TxDestination::Network(_) => None,
        }
    }

    pub fn try_send(&self, packet: P, expires_at: LinkTime) -> Result<(), SendError<P>> {
        self.try_send_received(
            packet,
            ReceiveTime {
                monotonic: (self.clock)(),
                wall: None,
            },
            expires_at,
        )
    }

    /// Preserve the full logical position to the Fabric network codec. The
    /// historical wire still carries only its existing sequence fields.
    pub fn try_send_fabric(
        &self,
        packet: P,
        source_position: u64,
        expires_at: LinkTime,
    ) -> Result<(), SendError<P>> {
        let TxDestination::Network(tx) = &self.destination else {
            return Err(SendError {
                kind: SendErrorKind::Unsupported,
                packet,
            });
        };
        self.try_send_network(tx, packet, Some(source_position), expires_at)
    }

    fn try_send_network(
        &self,
        tx: &network::NetworkTx<P>,
        packet: P,
        source_position: Option<u64>,
        expires_at: LinkTime,
    ) -> Result<(), SendError<P>> {
        if !self.is_live() {
            return Err(SendError {
                kind: SendErrorKind::Stale,
                packet,
            });
        }
        tx.try_send(
            self.binding,
            self.parent,
            packet,
            source_position,
            expires_at,
        )
    }

    /// Retain the original host observation through a local ownership handoff.
    pub fn try_send_received(
        &self,
        packet: P,
        received_at: ReceiveTime,
        expires_at: LinkTime,
    ) -> Result<(), SendError<P>> {
        let (port, tag, sender) = match &self.destination {
            TxDestination::Network(tx) => {
                return self.try_send_network(tx, packet, None, expires_at);
            }
            TxDestination::Local { port, tag, sender } => (*port, *tag, sender),
        };
        if !self.table.port_live(port) {
            return Err(SendError {
                kind: SendErrorKind::Closed,
                packet,
            });
        }
        if !self.is_live() {
            return Err(SendError {
                kind: SendErrorKind::Stale,
                packet,
            });
        }
        sender
            .try_send(Pending {
                binding: self.binding,
                parent: None,
                expires_at,
                received: Received {
                    tag,
                    packet,
                    received_at,
                    fabric: None,
                },
            })
            .map_err(|error| {
                let (kind, pending) = match error {
                    pulsebeam_queue::TrySendError::Full(value) => (SendErrorKind::Full, value),
                    pulsebeam_queue::TrySendError::Closed(value) => (SendErrorKind::Closed, value),
                };
                SendError {
                    kind,
                    packet: pending.received.packet,
                }
            })
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropCounts {
    pub expired: u64,
    pub stale: u64,
    pub closed: u64,
}

pub struct LinkRx<P> {
    port: PortId,
    table: Shared<Table>,
    clock: Shared<dyn Fn() -> LinkTime + Send + Sync>,
    receiver: pulsebeam_queue::Receiver<Pending<P>>,
    drops: DropCounts,
}

impl<P> LinkRx<P> {
    /// Examines at most max queued items, including discarded items. Returns the
    /// number appended, preserving the caller's work bound under stale floods.
    pub fn try_recv_batch(&mut self, out: &mut Vec<Received<P>>, max: usize) -> usize {
        let before = out.len();
        let now = (self.clock)();
        for _ in 0..max {
            let Ok(pending) = self.receiver.try_recv() else {
                break;
            };
            if !self.table.port_live(self.port) {
                self.drops.closed = self.drops.closed.saturating_add(1);
            } else if !self.table.binding_live(pending.binding)
                || pending
                    .parent
                    .is_some_and(|parent| !self.table.binding_live(parent))
            {
                self.drops.stale = self.drops.stale.saturating_add(1);
            } else if pending.expires_at <= now {
                self.drops.expired = self.drops.expired.saturating_add(1);
            } else {
                out.push(pending.received);
            }
        }
        out.len().saturating_sub(before)
    }

    pub async fn readable(&mut self) -> bool {
        if !self.table.port_live(self.port) {
            return false;
        }
        self.receiver.readable().await && self.table.port_live(self.port)
    }

    pub fn drop_counts(&self) -> DropCounts {
        self.drops
    }
}

#[cfg(test)]
mod network_tests;
#[cfg(test)]
mod tests;

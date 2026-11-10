#![forbid(unsafe_code)]

//! Bounded delivery between independently owned execution contexts.

use std::collections::HashMap;
use std::num::NonZeroUsize;

pub mod io;
mod network;
mod wire;
pub use network::{
    CodecError, LinkError, NetworkClass, NetworkId, NetworkPeer, NodeRouteKey, PeerFingerprint,
    PeerRole, SecretCredential, WireCodec,
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

#[derive(Debug)]
pub struct Received<P> {
    pub tag: DeliveryTag,
    pub packet: P,
    pub received_at: ReceiveTime,
}

struct Pending<P> {
    binding: BindingId,
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
        if let Some(word) = words.get(slot)
            && word.load(Ordering::Relaxed) == generation
        {
            word.store(generation.saturating_add(1), Ordering::Release);
        }
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
                let entry = self.networks.get(&peer).ok_or(BindError::Closed)?;
                if entry.state.load(Ordering::Acquire) == network::CLOSED {
                    return Err(BindError::Closed);
                }
                (
                    TxDestination::Network(entry.tx(address)),
                    BindingTarget::Network(peer),
                )
            }
        };
        let binding = self.allocate_binding()?;
        self.bindings.insert(binding, target);
        Ok((
            binding,
            LinkTx {
                binding,
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
        if binding.owner == self.owner {
            Table::retire(&self.table.bindings, binding.slot, binding.generation);
            self.bindings.remove(&binding);
        }
    }

    pub fn close_port(&mut self, port: PortId) {
        if port.owner == self.owner && self.table.port_live(port) {
            Table::retire(&self.table.ports, port.slot, port.generation);
            self.bindings.retain(|binding, destination| {
                if *destination == BindingTarget::Local(port) {
                    Table::retire(&self.table.bindings, binding.slot, binding.generation);
                    false
                } else {
                    true
                }
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum BindingTarget {
    Local(PortId),
    Network(NetworkId),
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

    /// Retain the original host observation through a local ownership handoff.
    pub fn try_send_received(
        &self,
        packet: P,
        received_at: ReceiveTime,
        expires_at: LinkTime,
    ) -> Result<(), SendError<P>> {
        if let TxDestination::Local { port, .. } = &self.destination
            && !self.table.port_live(*port)
        {
            return Err(SendError {
                kind: SendErrorKind::Closed,
                packet,
            });
        }
        if !self.table.binding_live(self.binding) {
            return Err(SendError {
                kind: SendErrorKind::Stale,
                packet,
            });
        }
        match &self.destination {
            TxDestination::Network(tx) => tx.try_send(self.binding, packet, expires_at),
            TxDestination::Local { tag, sender, .. } => sender
                .try_send(Pending {
                    binding: self.binding,
                    expires_at,
                    received: Received {
                        tag: *tag,
                        packet,
                        received_at,
                    },
                })
                .map_err(|error| {
                    let (kind, pending) = match error {
                        pulsebeam_queue::TrySendError::Full(value) => (SendErrorKind::Full, value),
                        pulsebeam_queue::TrySendError::Closed(value) => {
                            (SendErrorKind::Closed, value)
                        }
                    };
                    SendError {
                        kind,
                        packet: pending.received.packet,
                    }
                }),
        }
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
            } else if !self.table.binding_live(pending.binding) {
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

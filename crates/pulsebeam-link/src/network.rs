use std::net::SocketAddr;

use super::*;

pub(crate) const NOT_READY: u64 = 0;
pub(crate) const READY: u64 = 1;
pub(crate) const CLOSED: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NetworkId {
    owner: u64,
    serial: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeRouteKey(pub(crate) u32);

impl NodeRouteKey {
    pub const fn to_bytes(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }
    pub const fn from_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_be_bytes(bytes))
    }
}

pub use pulsebeam_transport::{NetworkClass, PeerFingerprint, PeerRole};

#[derive(Clone)]
pub struct SecretCredential(pulsebeam_transport::Credential);

impl SecretCredential {
    pub fn generate() -> Result<Self, LinkError> {
        pulsebeam_transport::Credential::generate()
            .map(Self)
            .map_err(|_| LinkError::Security)
    }

    pub fn fingerprint(&self) -> PeerFingerprint {
        self.0.fingerprint()
    }
}

pub struct NetworkPeer {
    pub endpoint: SocketAddr,
    pub credential: SecretCredential,
    pub expected_peer: PeerFingerprint,
    pub role: PeerRole,
    pub class: NetworkClass,
}

impl NetworkPeer {
    pub(crate) fn into_transport(self) -> pulsebeam_transport::Peer {
        pulsebeam_transport::Peer {
            endpoint: self.endpoint,
            credential: self.credential.0,
            expected_peer: self.expected_peer,
            role: self.role,
            class: self.class,
        }
    }
}

impl From<NetworkPeer> for pulsebeam_transport::PeerConfig {
    fn from(peer: NetworkPeer) -> Self {
        peer.into_transport().into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkError {
    UnsupportedProfile,
    Capacity,
    Security,
    Closed,
    AlreadyOpen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecError {
    Unsupported,
    Malformed,
    TooLarge,
}

/// Network-only constructor dependency. Local packets need no codec or Clone.
/// Multi-port network fanout requires shallow cloning of immutable payloads.
pub trait WireCodec<P>: Send + Sync {
    fn encoded_len(&self, packet: &P) -> Result<usize, CodecError>;
    fn encode(&self, packet: &P, dst: &mut [u8]) -> Result<usize, CodecError>;
    fn decode(&self, wire: &[u8]) -> Result<P, CodecError>;
}

pub(crate) struct Outbound<P> {
    pub binding: BindingId,
    pub address: NodeRouteKey,
    pub expires_at: LinkTime,
    pub packet: P,
}

pub(crate) struct LocalDestination<P> {
    pub port: PortId,
    pub tag: DeliveryTag,
    pub sender: pulsebeam_queue::Sender<Pending<P>>,
}

pub(crate) struct ReceiveBinding<P> {
    pub key: NodeRouteKey,
    pub binding: BindingId,
    pub destinations: Vec<LocalDestination<P>>,
}

pub(crate) struct Entry<P> {
    pub state: Shared<Fence>,
    peer: PeerFingerprint,
    sender: pulsebeam_queue::Sender<Outbound<P>>,
    commands: pulsebeam_queue::Sender<ReceiveBinding<P>>,
    codec: Shared<dyn WireCodec<P>>,
    next_route: u32,
    driver: Option<io::NetworkDriver<P>>,
}

impl<P> Entry<P> {
    pub fn close(&self) {
        self.state.store(CLOSED, Ordering::Release);
        self.sender.close();
        self.commands.close();
    }

    pub fn tx(&self, address: NodeRouteKey) -> NetworkTx<P> {
        NetworkTx {
            address,
            state: self.state.clone(),
            sender: self.sender.clone(),
            codec: self.codec.clone(),
        }
    }
}

pub(crate) struct NetworkTx<P> {
    address: NodeRouteKey,
    state: Shared<Fence>,
    sender: pulsebeam_queue::Sender<Outbound<P>>,
    codec: Shared<dyn WireCodec<P>>,
}

impl<P> Clone for NetworkTx<P> {
    fn clone(&self) -> Self {
        Self {
            address: self.address,
            state: self.state.clone(),
            sender: self.sender.clone(),
            codec: self.codec.clone(),
        }
    }
}

impl<P> NetworkTx<P> {
    pub fn try_send(
        &self,
        binding: BindingId,
        packet: P,
        expires_at: LinkTime,
    ) -> Result<(), SendError<P>> {
        let kind = match self.state.load(Ordering::Acquire) {
            CLOSED => Some(SendErrorKind::Closed),
            NOT_READY => Some(SendErrorKind::NotReady),
            _ => match self.codec.encoded_len(&packet) {
                Ok(len) if len <= wire::PAYLOAD_BUDGET => None,
                Ok(_) | Err(CodecError::TooLarge) => Some(SendErrorKind::TooLarge),
                Err(_) => Some(SendErrorKind::Unsupported),
            },
        };
        if let Some(kind) = kind {
            return Err(SendError { kind, packet });
        }
        self.sender
            .try_send(Outbound {
                binding,
                address: self.address,
                expires_at,
                packet,
            })
            .map_err(|error| {
                let (kind, value) = match error {
                    pulsebeam_queue::TrySendError::Full(value) => (SendErrorKind::Full, value),
                    pulsebeam_queue::TrySendError::Closed(value) => (SendErrorKind::Closed, value),
                };
                SendError {
                    kind,
                    packet: value.packet,
                }
            })
    }
}

impl<P: Clone + Send + 'static> LinkControl<P> {
    pub fn open_network<C: WireCodec<P> + 'static>(
        &mut self,
        peer: impl Into<pulsebeam_transport::PeerConfig>,
        codec: C,
    ) -> Result<NetworkId, LinkError> {
        let peer = peer.into();
        if self.networks.len() >= MAX_PORTS {
            return Err(LinkError::Capacity);
        }
        if self.networks.values().any(|entry| {
            entry.peer == peer.expected_peer() && entry.state.load(Ordering::Acquire) != CLOSED
        }) {
            return Err(LinkError::AlreadyOpen);
        }
        let expected_peer = peer.expected_peer();
        let serial = self
            .next_network
            .checked_add(1)
            .ok_or(LinkError::Capacity)?;
        self.next_network = serial;
        let id = NetworkId {
            owner: self.owner,
            serial,
        };
        let state = Shared::new(Fence::new(NOT_READY));
        let codec: Shared<dyn WireCodec<P>> = Shared::new(codec);
        let (sender, receiver) = pulsebeam_queue::bounded(QUEUE_CAPACITY);
        let (commands, command_rx) = pulsebeam_queue::bounded(QUEUE_CAPACITY);
        let driver = io::NetworkDriver::new(
            peer,
            codec.clone(),
            state.clone(),
            self.table.clone(),
            receiver,
            command_rx,
        )?;
        self.networks.insert(
            id,
            Entry {
                state,
                peer: expected_peer,
                sender,
                commands,
                codec,
                next_route: 0,
                driver: Some(driver),
            },
        );
        Ok(id)
    }

    pub fn bind_receive(
        &mut self,
        from: NetworkId,
        destinations: &[(PortId, DeliveryTag)],
    ) -> Result<(BindingId, NodeRouteKey), BindError> {
        if destinations.is_empty() || destinations.len() > MAX_PORTS {
            return Err(BindError::Capacity);
        }
        let mut fanout = Vec::with_capacity(destinations.len());
        for &(port, tag) in destinations {
            if port.owner != self.owner || !self.table.port_live(port) {
                return Err(BindError::Closed);
            }
            if fanout
                .iter()
                .any(|destination: &LocalDestination<P>| destination.port == port)
            {
                return Err(BindError::Capacity);
            }
            fanout.push(LocalDestination {
                port,
                tag,
                sender: self.ports.get(&port.slot).ok_or(BindError::Closed)?.clone(),
            });
        }
        let binding = self.allocate_binding()?;
        let result = (|| {
            let entry = self.networks.get_mut(&from).ok_or(BindError::Closed)?;
            if entry.state.load(Ordering::Acquire) == CLOSED {
                return Err(BindError::Closed);
            }
            // Never recycle a route key within this authenticated association.
            let raw = entry.next_route.checked_add(1).ok_or(BindError::Capacity)?;
            let key = NodeRouteKey(raw);
            entry
                .commands
                .try_send(ReceiveBinding {
                    key,
                    binding,
                    destinations: fanout,
                })
                .map_err(|_| BindError::Capacity)?;
            entry.next_route = raw;
            Ok(key)
        })();
        match result {
            Ok(key) => {
                self.bindings.insert(binding, BindingTarget::Network(from));
                Ok((binding, key))
            }
            Err(error) => {
                Table::retire(&self.table.bindings, binding.slot, binding.generation);
                Err(error)
            }
        }
    }
}

impl<P> LinkControl<P> {
    /// Transfer the single association owner to the host I/O adapter exactly once.
    pub fn take_network_driver(&mut self, peer: NetworkId) -> Option<io::NetworkDriver<P>> {
        self.networks.get_mut(&peer)?.driver.take()
    }

    pub fn close_network(&mut self, peer: NetworkId) {
        if let Some(entry) = self.networks.remove(&peer) {
            entry.close();
        }
        self.bindings.retain(|binding, target| {
            if *target == BindingTarget::Network(peer) {
                Table::retire(&self.table.bindings, binding.slot, binding.generation);
                false
            } else {
                true
            }
        });
    }
}

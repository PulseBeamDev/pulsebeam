use std::{net::SocketAddr, num::NonZeroU16};

use crate::fabric_keys::FabricKeys;

use super::*;

pub(crate) const NOT_READY: u64 = 0;
pub(crate) const READY: u64 = 1;
pub(crate) const CLOSED: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NetworkId {
    owner: u64,
    serial: u64,
}

impl NetworkId {
    /// Association epoch within the Link host. Pair with the node incarnation
    /// for diagnostics; it is not a remote identity or routing key.
    pub const fn epoch(self) -> u64 {
        self.serial
    }
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

pub use pulsebeam_transport::{FabricPath, FabricPeer, NetworkClass, PeerFingerprint, PeerRole};

#[derive(Clone)]
pub struct SecretCredential(pulsebeam_transport::Credential);

impl SecretCredential {
    pub fn generate() -> Result<Self, LinkError> {
        pulsebeam_transport::Credential::generate()
            .map(Self)
            .map_err(|_| LinkError::Security)
    }

    pub fn from_der(certificate: Vec<u8>, private_key: Vec<u8>) -> Result<Self, LinkError> {
        pulsebeam_transport::Credential::from_der(certificate, private_key)
            .map(Self)
            .map_err(|_| LinkError::Security)
    }

    pub fn fingerprint(&self) -> PeerFingerprint {
        self.0.fingerprint()
    }
}

#[derive(Clone)]
pub struct NetworkPeer {
    pub endpoint: SocketAddr,
    pub credential: SecretCredential,
    pub expected_peer: PeerFingerprint,
    pub role: PeerRole,
    pub class: NetworkClass,
}

impl NetworkPeer {
    pub fn with_fabric_path(self, path: FabricPath) -> FabricPeer {
        FabricPeer {
            dtls: self.into_transport(),
            path,
        }
    }

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

/// An image codec for the negotiated Fabric association. The image carries
/// the destination key; Link checks its binding before full image decoding.
pub trait FabricCodec<P>: Send + Sync {
    fn encoded_len(&self, packet: &P) -> Result<usize, CodecError>;
    fn encode(&self, packet: &P, key: NodeRouteKey, dst: &mut [u8]) -> Result<usize, CodecError>;
    fn encode_positioned(
        &self,
        packet: &P,
        key: NodeRouteKey,
        _source_position: Option<u64>,
        dst: &mut [u8],
    ) -> Result<usize, CodecError> {
        self.encode(packet, key, dst)
    }
    fn route_key(&self, image: &[u8]) -> Result<NodeRouteKey, CodecError>;
    fn decode(&self, image: &[u8]) -> Result<P, CodecError>;
}

pub(crate) enum NetworkCodec<P> {
    Legacy(Shared<dyn WireCodec<P>>),
    Fabric(Shared<dyn FabricCodec<P>>),
}

impl<P> Clone for NetworkCodec<P> {
    fn clone(&self) -> Self {
        match self {
            Self::Legacy(codec) => Self::Legacy(codec.clone()),
            Self::Fabric(codec) => Self::Fabric(codec.clone()),
        }
    }
}

impl<P> NetworkCodec<P> {
    pub(crate) fn is_fabric(&self) -> bool {
        matches!(self, Self::Fabric(_))
    }

    pub(crate) fn payload_limit(&self) -> usize {
        match self {
            Self::Legacy(_) => wire::PAYLOAD_BUDGET,
            Self::Fabric(_) => wire::FABRIC_IMAGE,
        }
    }

    fn encoded_len(&self, packet: &P) -> Result<usize, CodecError> {
        match self {
            Self::Legacy(codec) => codec.encoded_len(packet),
            Self::Fabric(codec) => codec.encoded_len(packet),
        }
    }

    pub(crate) fn encode(
        &self,
        packet: &P,
        key: NodeRouteKey,
        source_position: Option<u64>,
        dst: &mut [u8],
    ) -> Result<usize, CodecError> {
        match self {
            Self::Legacy(codec) => codec.encode(packet, dst),
            Self::Fabric(codec) => {
                let len = codec.encode_positioned(packet, key, source_position, dst)?;
                let image = dst.get(..len).ok_or(CodecError::TooLarge)?;
                (key.0 != 0 && codec.route_key(image)? == key)
                    .then_some(len)
                    .ok_or(CodecError::Malformed)
            }
        }
    }

    pub(crate) fn route_key(
        &self,
        context: Option<[u8; 4]>,
        image: &[u8],
    ) -> Result<NodeRouteKey, CodecError> {
        match self {
            Self::Legacy(_) => context
                .map(NodeRouteKey::from_bytes)
                .ok_or(CodecError::Malformed),
            Self::Fabric(codec) => {
                if context.is_some() {
                    return Err(CodecError::Malformed);
                }
                let key = codec.route_key(image)?;
                (key.0 != 0).then_some(key).ok_or(CodecError::Malformed)
            }
        }
    }

    pub(crate) fn decode(&self, bytes: &[u8]) -> Result<P, CodecError> {
        match self {
            Self::Legacy(codec) => codec.decode(bytes),
            Self::Fabric(codec) => codec.decode(bytes),
        }
    }
}

pub(crate) struct Outbound<P> {
    pub binding: BindingId,
    pub parent: Option<BindingId>,
    pub address: NodeRouteKey,
    pub source_position: Option<u64>,
    pub expires_at: LinkTime,
    pub packet: P,
}

pub(crate) struct LocalDestination<P> {
    pub port: PortId,
    pub binding: BindingId,
    pub tag: DeliveryTag,
    pub sender: pulsebeam_queue::Sender<Pending<P>>,
}

pub(crate) struct ReceiveBinding<P> {
    pub key: NodeRouteKey,
    pub binding: BindingId,
    pub destinations: Vec<LocalDestination<P>>,
    pub fabric: Option<FabricReceive<P>>,
}

type FabricAdmission<P> = Box<dyn Fn(&P) -> bool + Send + Sync>;

struct FabricStage<P> {
    shard: u8,
    purpose: FabricPurpose,
    admits: FabricAdmission<P>,
}

pub(crate) struct FabricReceive<P> {
    pub association: NetworkId,
    pub purpose: FabricPurpose,
    active: Shared<Fence>,
    pub admits: FabricAdmission<P>,
}

impl<P> FabricReceive<P> {
    pub(super) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire) == READY
    }
}

pub(super) struct StagedFabricRoute {
    pub key: NodeRouteKey,
    purpose: FabricPurpose,
    active: Shared<Fence>,
}

pub(crate) struct Entry<P> {
    pub state: Shared<Fence>,
    peer: PeerFingerprint,
    sender: pulsebeam_queue::Sender<Outbound<P>>,
    commands: pulsebeam_queue::Sender<ReceiveBinding<P>>,
    codec: NetworkCodec<P>,
    next_route: u32,
    fabric_keys: Option<FabricKeys>,
    driver: Option<io::NetworkDriver<P>>,
}

impl<P> Entry<P> {
    pub(super) fn retire_fabric_key(&mut self, now: LinkTime, key: NodeRouteKey) {
        if let Some(keys) = &mut self.fabric_keys {
            keys.retire(now, key);
        }
    }

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
    codec: NetworkCodec<P>,
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
        parent: Option<BindingId>,
        packet: P,
        source_position: Option<u64>,
        expires_at: LinkTime,
    ) -> Result<(), SendError<P>> {
        let kind = match self.state.load(Ordering::Acquire) {
            CLOSED => Some(SendErrorKind::Closed),
            NOT_READY => Some(SendErrorKind::NotReady),
            _ if source_position.is_some() && !self.codec.is_fabric() => {
                Some(SendErrorKind::Unsupported)
            }
            _ => match self.codec.encoded_len(&packet) {
                Ok(len)
                    if len <= self.codec.payload_limit()
                        && (len != 0 || !self.codec.is_fabric()) =>
                {
                    None
                }
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
                parent,
                address: self.address,
                source_position,
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
        self.open_network_inner(
            peer.into(),
            NetworkCodec::Legacy(Shared::new(codec)),
            Shared::new(Fence::new(NOT_READY)),
            None,
        )
    }

    pub fn open_network_with_control<C: WireCodec<P> + 'static>(
        &mut self,
        peer: impl Into<pulsebeam_transport::PeerConfig>,
        codec: C,
    ) -> Result<(NetworkId, PeerControl), LinkError> {
        let state = Shared::new(Fence::new(NOT_READY));
        let (control, host) = control::pair(state.clone());
        let id = self.open_network_inner(
            peer.into(),
            NetworkCodec::Legacy(Shared::new(codec)),
            state,
            Some(host),
        )?;
        Ok((id, control))
    }

    pub fn open_fabric_network<C: FabricCodec<P> + 'static>(
        &mut self,
        peer: FabricPeer,
        codec: C,
    ) -> Result<NetworkId, LinkError> {
        self.open_fabric_inner(peer, codec, Shared::new(Fence::new(NOT_READY)), None)
    }

    pub fn open_fabric_network_with_control<C: FabricCodec<P> + 'static>(
        &mut self,
        peer: FabricPeer,
        codec: C,
    ) -> Result<(NetworkId, PeerControl), LinkError> {
        let state = Shared::new(Fence::new(NOT_READY));
        let (control, host) = control::pair(state.clone());
        let id = self.open_fabric_inner(peer, codec, state, Some(host))?;
        Ok((id, control))
    }

    fn open_fabric_inner<C: FabricCodec<P> + 'static>(
        &mut self,
        peer: FabricPeer,
        codec: C,
        state: Shared<Fence>,
        host: Option<control::HostLane>,
    ) -> Result<NetworkId, LinkError> {
        if !peer.path.qualifies() {
            return Err(LinkError::UnsupportedProfile);
        }
        self.open_network_inner(
            peer.into(),
            NetworkCodec::Fabric(Shared::new(codec)),
            state,
            host,
        )
    }

    fn open_network_inner(
        &mut self,
        peer: pulsebeam_transport::PeerConfig,
        codec: NetworkCodec<P>,
        state: Shared<Fence>,
        host: Option<control::HostLane>,
    ) -> Result<NetworkId, LinkError> {
        if matches!(peer, pulsebeam_transport::PeerConfig::Fabric(_)) != codec.is_fabric() {
            return Err(LinkError::UnsupportedProfile);
        }
        if self.networks.len() >= MAX_PORTS {
            return Err(LinkError::Capacity);
        }
        if self.networks.values().any(|entry| {
            entry.peer == peer.expected_peer() && entry.state.load(Ordering::Acquire) != CLOSED
        }) {
            return Err(LinkError::AlreadyOpen);
        }
        let expected_peer = peer.expected_peer();
        let id = self.reserve_network_id()?;
        let (sender, receiver) = pulsebeam_queue::bounded(QUEUE_CAPACITY);
        let (commands, command_rx) = pulsebeam_queue::bounded(QUEUE_CAPACITY);
        let driver = io::NetworkDriver::new(
            peer,
            codec.clone(),
            state.clone(),
            self.table.clone(),
            receiver,
            command_rx,
            host,
        )?;
        let fabric_keys = if codec.is_fabric() {
            Some(FabricKeys::new(
                NonZeroU16::new(4096).ok_or(LinkError::Capacity)?,
            ))
        } else {
            None
        };
        self.networks.insert(
            id,
            Entry {
                state,
                peer: expected_peer,
                sender,
                commands,
                codec,
                next_route: 0,
                fabric_keys,
                driver: Some(driver),
            },
        );
        Ok(id)
    }

    /// Reserve an epoch for an explicitly provisioned host-owned WAN driver.
    /// It cannot bind routes on the historical datagram driver.
    pub fn reserve_network_id(&mut self) -> Result<NetworkId, LinkError> {
        let serial = self
            .next_network
            .checked_add(1)
            .ok_or(LinkError::Capacity)?;
        self.next_network = serial;
        Ok(NetworkId {
            owner: self.owner,
            serial,
        })
    }

    pub fn bind_receive(
        &mut self,
        from: NetworkId,
        destinations: &[(PortId, DeliveryTag)],
    ) -> Result<(BindingId, NodeRouteKey), BindError> {
        self.bind_receive_mode(from, None, destinations)
    }

    /// Reserve a key and bind an immutable policy and exact fanout. The caller
    /// installs the key at every child before explicitly activating it here.
    pub fn stage_fabric_receive(
        &mut self,
        from: NetworkId,
        shard: u8,
        destinations: &[(PortId, DeliveryTag)],
        admits: impl Fn(&P) -> bool + Send + Sync + 'static,
    ) -> Result<(BindingId, NodeRouteKey), BindError> {
        self.bind_receive_mode(
            from,
            Some(FabricStage {
                shard,
                purpose: FabricPurpose::Live,
                admits: Box::new(admits),
            }),
            destinations,
        )
    }

    /// Repairs never fan out: one destination receives the image for validation
    /// against its own exact demand and final handoff fences.
    pub fn stage_fabric_repair_receive(
        &mut self,
        from: NetworkId,
        shard: u8,
        destination: (PortId, DeliveryTag),
        admits: impl Fn(&P) -> bool + Send + Sync + 'static,
    ) -> Result<(BindingId, NodeRouteKey), BindError> {
        self.bind_receive_mode(
            from,
            Some(FabricStage {
                shard,
                purpose: FabricPurpose::RepairOnly,
                admits: Box::new(admits),
            }),
            &[destination],
        )
    }

    fn bind_receive_mode(
        &mut self,
        from: NetworkId,
        fabric: Option<FabricStage<P>>,
        destinations: &[(PortId, DeliveryTag)],
    ) -> Result<(BindingId, NodeRouteKey), BindError> {
        if destinations.is_empty() || destinations.len() > MAX_PORTS {
            return Err(BindError::Capacity);
        }
        let entry = self.networks.get(&from).ok_or(BindError::Closed)?;
        let is_fabric = fabric.is_some();
        if entry.codec.is_fabric() != is_fabric
            || (is_fabric && entry.state.load(Ordering::Acquire) != READY)
        {
            return Err(BindError::UnsupportedProfile);
        }
        let purpose = fabric.as_ref().map(|stage| stage.purpose);
        let active = is_fabric.then(|| Shared::new(Fence::new(NOT_READY)));
        let now = (self.clock)();
        let binding = self.allocate_binding()?;
        let mut children = Vec::with_capacity(destinations.len());
        let result = (|| {
            let mut fanout = Vec::with_capacity(destinations.len());
            for &(port, tag) in destinations {
                if port.owner != self.owner || !self.table.port_live(port) {
                    return Err(BindError::Closed);
                }
                if children.iter().any(|&(existing, _)| existing == port) {
                    return Err(BindError::Capacity);
                }
                let sender = self.ports.get(&port.slot).ok_or(BindError::Closed)?.clone();
                let child = self.allocate_binding()?;
                children.push((port, child));
                fanout.push(LocalDestination {
                    port,
                    binding: child,
                    tag,
                    sender,
                });
            }
            let entry = self.networks.get_mut(&from).ok_or(BindError::Closed)?;
            if entry.state.load(Ordering::Acquire) == CLOSED {
                return Err(BindError::Closed);
            }
            if let Some((stage, active)) = fabric.zip(active.as_ref()) {
                let fabric = FabricReceive {
                    association: from,
                    purpose: stage.purpose,
                    active: active.clone(),
                    admits: stage.admits,
                };
                let keys = entry
                    .fabric_keys
                    .as_mut()
                    .ok_or(BindError::UnsupportedProfile)?;
                let commands = &entry.commands;
                return keys
                    .install(now, stage.shard, |key| {
                        commands
                            .try_send(ReceiveBinding {
                                key,
                                binding,
                                destinations: fanout,
                                fabric: Some(fabric),
                            })
                            .is_ok()
                    })
                    .ok_or(BindError::Capacity);
            }
            let raw = entry.next_route.checked_add(1).ok_or(BindError::Capacity)?;
            let key = NodeRouteKey(raw);
            entry
                .commands
                .try_send(ReceiveBinding {
                    key,
                    binding,
                    destinations: fanout,
                    fabric: None,
                })
                .map_err(|_| BindError::Capacity)?;
            entry.next_route = raw;
            Ok(key)
        })();
        let target = BindingTarget::NetworkReceive {
            peer: from,
            destinations: children,
            fabric: result.as_ref().ok().copied().zip(active).zip(purpose).map(
                |((key, active), purpose)| StagedFabricRoute {
                    key,
                    active,
                    purpose,
                },
            ),
        };
        match result {
            Ok(key) => {
                self.bindings.insert(binding, target);
                Ok((binding, key))
            }
            Err(error) => {
                target.retire(&self.table, binding);
                Err(error)
            }
        }
    }
}

impl<P> LinkControl<P> {
    pub fn is_fabric_network(&self, peer: NetworkId) -> bool {
        self.networks
            .get(&peer)
            .is_some_and(|entry| entry.codec.is_fabric())
    }

    /// Bind a receiver-issued Fabric key on an already negotiated association.
    /// The provisioning owner supplies the key from its downstream acknowledgment.
    pub fn bind_fabric_send(
        &mut self,
        peer: NetworkId,
        address: NodeRouteKey,
    ) -> Result<(BindingId, LinkTx<P>), BindError> {
        let entry = self.networks.get(&peer).ok_or(BindError::Closed)?;
        match entry.state.load(Ordering::Acquire) {
            CLOSED => return Err(BindError::Closed),
            READY if entry.codec.is_fabric() => {}
            _ => return Err(BindError::UnsupportedProfile),
        }
        if address.to_bytes() == [0; 4] {
            return Err(BindError::Closed);
        }
        self.bind_send(Destination::Network { peer, address })
    }

    /// Compile a Fabric next hop whose lifetime also depends on an active input route.
    /// The caller owns the authoritative acknowledgment of the downstream key.
    pub fn bind_fabric_relay(
        &mut self,
        upstream: BindingId,
        downstream: NetworkId,
        key: NodeRouteKey,
    ) -> Result<(BindingId, LinkTx<P>), BindError> {
        self.bind_fabric_relay_for(upstream, downstream, key, FabricPurpose::Live)
    }

    pub fn bind_fabric_repair_relay(
        &mut self,
        upstream: BindingId,
        downstream: NetworkId,
        key: NodeRouteKey,
    ) -> Result<(BindingId, LinkTx<P>), BindError> {
        self.bind_fabric_relay_for(upstream, downstream, key, FabricPurpose::RepairOnly)
    }

    fn bind_fabric_relay_for(
        &mut self,
        upstream: BindingId,
        downstream: NetworkId,
        key: NodeRouteKey,
        purpose: FabricPurpose,
    ) -> Result<(BindingId, LinkTx<P>), BindError> {
        let Some(BindingTarget::NetworkReceive {
            fabric: Some(route),
            ..
        }) = self.bindings.get(&upstream)
        else {
            return Err(BindError::Closed);
        };
        if !self.table.binding_live(upstream) || route.active.load(Ordering::Acquire) != READY {
            return Err(BindError::Closed);
        }
        if route.purpose != purpose {
            return Err(BindError::UnsupportedProfile);
        }
        let (binding, mut tx) = self.bind_fabric_send(downstream, key)?;
        tx.parent = Some(upstream);
        Ok((binding, tx))
    }

    /// Publish a still-live staged Fabric binding after all children have acknowledged it.
    pub fn activate_fabric_receive(&mut self, binding: BindingId) -> Result<(), BindError> {
        let Some(BindingTarget::NetworkReceive {
            peer,
            fabric: Some(route),
            ..
        }) = self.bindings.get(&binding)
        else {
            return Err(BindError::Closed);
        };
        if !self.table.binding_live(binding) {
            return Err(BindError::Closed);
        }
        let entry = self.networks.get(peer).ok_or(BindError::Closed)?;
        if entry.state.load(Ordering::Acquire) != READY {
            return Err(BindError::UnsupportedProfile);
        }
        route.active.store(READY, Ordering::Release);
        Ok(())
    }

    /// A publisher repair label is distinct, but its send dies with the exact live send.
    pub fn bind_fabric_publisher_repair(
        &mut self,
        parent: BindingId,
        peer: NetworkId,
        address: NodeRouteKey,
    ) -> Result<(BindingId, LinkTx<P>), BindError> {
        if !matches!(self.bindings.get(&parent), Some(BindingTarget::NetworkSend(owner)) if *owner == peer)
            || !self.table.binding_live(parent)
        {
            return Err(BindError::Closed);
        }
        let (binding, mut tx) = self.bind_fabric_send(peer, address)?;
        tx.parent = Some(parent);
        Ok((binding, tx))
    }

    /// Allocate a cutoff for a route whose WAN socket has a separate owner.
    /// The resulting binding cannot send through the historical Link driver.
    pub fn reserve_wan_send(&mut self, peer: NetworkId) -> Result<BindingCutoff, BindError> {
        if peer.owner != self.owner || self.networks.contains_key(&peer) {
            return Err(BindError::Closed);
        }
        let binding = self.allocate_binding()?;
        self.bindings
            .insert(binding, BindingTarget::NetworkSend(peer));
        Ok(BindingCutoff {
            table: self.table.clone(),
            binding,
        })
    }

    /// Give the publisher shard a fence for one compiled Fabric send binding.
    pub fn fabric_send_cutoff(&self, binding: BindingId) -> Option<BindingCutoff> {
        let BindingTarget::NetworkSend(peer) = self.bindings.get(&binding)? else {
            return None;
        };
        (self.table.binding_live(binding) && self.is_fabric_network(*peer)).then(|| BindingCutoff {
            table: self.table.clone(),
            binding,
        })
    }

    /// Get a fence for an exact destination. A Fabric route's membership is indivisible.
    pub fn receive_port_cutoff(&self, binding: BindingId, port: PortId) -> Option<BindingCutoff> {
        let BindingTarget::NetworkReceive {
            destinations,
            fabric,
            ..
        } = self.bindings.get(&binding)?
        else {
            return None;
        };
        let (_, child) = destinations.iter().find(|&&(target, _)| target == port)?;
        Some(BindingCutoff {
            table: self.table.clone(),
            binding: if fabric.is_some() { binding } else { *child },
        })
    }

    /// Fence one receive port, including payloads already queued to it. The
    /// wire route remains live until its last destination is removed.
    pub fn unbind_receive_port(&mut self, binding: BindingId, port: PortId) {
        let Some(BindingTarget::NetworkReceive {
            destinations,
            fabric,
            ..
        }) = self.bindings.get_mut(&binding)
        else {
            return;
        };
        if fabric.is_some() {
            if destinations.iter().any(|&(target, _)| target == port) {
                self.unbind(binding);
            }
            return;
        }
        retire_receive_port(&self.table, destinations, port);
        if destinations.is_empty() {
            self.unbind(binding);
        }
    }

    /// Transfer the single association owner to the host I/O adapter exactly once.
    pub fn take_network_driver(&mut self, peer: NetworkId) -> Option<io::NetworkDriver<P>> {
        self.networks.get_mut(&peer)?.driver.take()
    }

    pub fn close_network(&mut self, peer: NetworkId) {
        if let Some(entry) = self.networks.remove(&peer) {
            entry.close();
        }
        self.bindings.retain(|binding, target| {
            if matches!(target, BindingTarget::NetworkSend(owner) | BindingTarget::NetworkReceive { peer: owner, .. } if *owner == peer) {
                target.retire(&self.table, *binding);
                false
            } else {
                true
            }
        });
    }
}

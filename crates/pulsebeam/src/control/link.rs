use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::task::{Context, Poll};

use pulsebeam_core::identity::NodeId;
use pulsebeam_link::{
    BindError, BindingId, Destination, FabricIngress, LinkControl, NetworkId, PortId,
};
use pulsebeam_runtime::sync::oneshot;
use pulsebeam_runtime::time::Instant;

use crate::control::controller::peer_ownership::relay::fabric::{LiveRouteAck, WireLiveRouteAck};
use crate::route::fabric::{
    CompiledLiveRelay, CompiledPublisherSend, LiveNetworkChild, LiveNextHop, LiveRelayChange,
    PublisherSendChange, RelayRetirement,
    local::Emission,
    network_repair::{Demand, Interest, RepairChild, RepairRoute},
};
use crate::shard::worker::ShardFrame;
use crate::shard_update::{CompiledRoute, TrackPlan, TrackPlanUpdate, TrackRouteAction};
use crate::{link::fabric_tx::FabricTx, route::NodeRouteAddress};
pub(super) mod wan;

use crate::{
    entity::{ProjectRoomKey, PublicationKey},
    id::ShardId,
    log::{RelayEvidence, RelayNode},
};

struct SendBranch {
    binding: BindingId,
    peer: pulsebeam_link::NetworkId,
    source: (ShardId, PublicationKey),
    route: CompiledRoute,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct RouteScope {
    pub(super) room: ProjectRoomKey,
    pub(super) ingress: Option<NetworkId>,
}

#[derive(Clone, Copy)]
struct ReceiveAction {
    route: TrackRouteAction,
    scope: Option<RouteScope>,
}

pub(super) struct LiveReceiveSpec {
    pub peer: NetworkId,
    pub room: ProjectRoomKey,
    pub publication: PublicationKey,
    pub incarnation: std::num::NonZeroU64,
    pub sequence_anchor: u64,
    pub major: u8,
    pub revision: u8,
    pub emission: Emission,
    pub addresses: Vec<NodeRouteAddress>,
}

type ChildReceipt = oneshot::Receiver<Option<LiveNetworkChild>>;
type ReceiveResult = Result<crate::link::ReceiveRoute, BindError>;

pub(super) struct StagedLiveReceive {
    pub binding: BindingId,
    pub receipt: oneshot::Receiver<ReceiveResult>,
}

struct PendingLiveReceive {
    spec: LiveReceiveSpec,
    route: crate::link::ReceiveRoute,
    receipts: Vec<(LiveNetworkChild, ChildReceipt)>,
    failed: bool,
    reply: oneshot::Sender<ReceiveResult>,
}

impl PendingLiveReceive {
    fn receipt(
        &mut self,
        mut poll: impl FnMut(&mut ChildReceipt) -> Poll<Option<LiveNetworkChild>>,
    ) -> bool {
        self.receipts
            .retain_mut(|(child, receipt)| match poll(receipt) {
                Poll::Pending => true,
                Poll::Ready(value) => {
                    self.failed |= value != Some(*child);
                    false
                }
            });
        self.failed || self.receipts.is_empty()
    }
}

struct RepairReceiveSpec {
    pub parent: BindingId,
    pub destination: NodeRouteAddress,
    pub receiver: crate::entity::ConnectionId,
    pub branch: NonZeroU64,
    pub demand: Demand,
}

struct RepairReceive {
    parent: BindingId,
    child: RepairChild,
    route: crate::link::ReceiveRoute,
}

struct PendingRepairReceive {
    receive: RepairReceive,
    receipt: oneshot::Receiver<Option<RepairChild>>,
    received: Option<bool>,
    reply: oneshot::Sender<ReceiveResult>,
}

impl PendingRepairReceive {
    fn observe(&mut self, outcome: Poll<Option<RepairChild>>) {
        if let Poll::Ready(receipt) = outcome {
            self.received = Some(receipt == Some(self.receive.child));
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct FabricRelayPermit {
    pub upstream: BindingId,
    pub destination: NodeRouteAddress,
    pub peer: NetworkId,
    pub expected: LiveRouteAck,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct PublisherSendPermit {
    pub shard: ShardId,
    pub publisher: crate::shard_update::PublisherAnchor,
    pub ack: LiveRouteAck,
}

struct FabricPublisherSend {
    binding: BindingId,
    permit: PublisherSendPermit,
    compiled: CompiledPublisherSend,
}

struct FabricRelayOwner {
    shard: ShardId,
    publisher: Option<crate::shard_update::PublisherAnchor>,
    expected: LiveRouteAck,
    lease: FabricRelayLease,
    withdrawal_seen: bool,
    cutoff_failed: bool,
}

enum FabricRelayLease {
    Awaiting,
    Acknowledged(Instant),
    Retired,
}

struct RetiringFabricRelay {
    binding: BindingId,
    next: LiveNextHop,
    reason: RelayRetirement,
    phase: FabricRelayCutoff,
}

enum FabricRelayCutoff {
    NeedsShard(CompiledLiveRelay),
    Staging,
    Waiting(oneshot::Receiver<bool>),
}

struct RepairRelaySend {
    binding: BindingId,
    live_binding: BindingId,
    next: LiveNextHop,
    expires: Instant,
}

struct FabricRelaySend {
    binding: BindingId,
    upstream: BindingId,
    expected: LiveRouteAck,
    compiled: CompiledLiveRelay,
}

impl FabricRelaySend {
    fn authorized_by(&self, permit: FabricRelayPermit) -> bool {
        self.upstream == permit.upstream
            && self.compiled.input.destination == permit.destination
            && self.compiled.next.peer == permit.peer
            && self.expected == permit.expected
    }
}

struct ReceiveBranch {
    peer: NetworkId,
    fabric: Option<LiveRouteAck>,
    addresses: Vec<NodeRouteAddress>,
    live: usize,
    evidence: Option<RelayEvidence>,
}

impl ReceiveBranch {
    fn record(&self, event: &'static str, destinations: usize) {
        if let Some(evidence) = self.evidence {
            evidence.record(event, destinations);
        }
    }
}

enum Retirement {
    Binding(BindingId),
    ReceivePort(BindingId, PortId),
}

impl Retirement {
    fn binding(&self) -> BindingId {
        match *self {
            Self::Binding(binding) | Self::ReceivePort(binding, _) => binding,
        }
    }
}

pub(super) struct Links {
    control: LinkControl<ShardFrame>,
    host: Option<pulsebeam_queue::Sender<crate::link::host::Command>>,
    wan: HashMap<NetworkId, wan::WanPeer>,
    peers: HashMap<NetworkId, std::net::SocketAddr>,
    peer_ids: HashMap<NetworkId, NodeId>,
    node: Option<RelayNode>,
    ports: Vec<PortId>,
    clock_origin: Instant,
    routes: HashMap<NodeRouteAddress, (BindingId, CompiledRoute)>,
    plans: HashMap<(ShardId, PublicationKey), TrackPlan>,
    sends: Vec<SendBranch>,
    installed: HashSet<NodeRouteAddress>,
    route_actions: HashMap<NodeRouteAddress, ReceiveAction>,
    installations: HashMap<NodeRouteAddress, (oneshot::Receiver<()>, Option<bool>)>,
    receives: HashMap<BindingId, ReceiveBranch>,
    fabric_installations: Vec<PendingLiveReceive>,
    repair_installations: Vec<PendingRepairReceive>,
    repair_receives: Vec<RepairReceive>,
    repair_frontiers: HashMap<(BindingId, NonZeroU64), NonZeroU64>,
    fabric_relays: Vec<FabricRelaySend>,
    fabric_publisher_sends: Vec<FabricPublisherSend>,
    repair_sends: Vec<RepairRelaySend>,
    fabric_permits: Vec<FabricRelayPermit>,
    fabric_relay_owners: HashMap<LiveNextHop, FabricRelayOwner>,
    fabric_relay_cutoffs: Vec<RetiringFabricRelay>,
    fabric_issued: HashMap<LiveNextHop, BindingId>,
    retirements: Vec<(Vec<Retirement>, oneshot::Receiver<()>, bool)>,
}

impl Links {
    pub(super) fn new(
        control: LinkControl<ShardFrame>,
        ports: Vec<PortId>,
        clock_origin: Instant,
    ) -> Self {
        Self {
            control,
            host: None,
            wan: HashMap::new(),
            peers: HashMap::new(),
            peer_ids: HashMap::new(),
            node: None,
            ports,
            clock_origin,
            routes: HashMap::new(),
            plans: HashMap::new(),
            sends: Vec::new(),
            installed: HashSet::new(),
            route_actions: HashMap::new(),
            installations: HashMap::new(),
            receives: HashMap::new(),
            fabric_installations: Vec::new(),
            repair_installations: Vec::new(),
            repair_receives: Vec::new(),
            repair_frontiers: HashMap::new(),
            fabric_relays: Vec::new(),
            fabric_publisher_sends: Vec::new(),
            repair_sends: Vec::new(),
            fabric_permits: Vec::new(),
            fabric_relay_owners: HashMap::new(),
            fabric_relay_cutoffs: Vec::new(),
            fabric_issued: HashMap::new(),
            retirements: Vec::new(),
        }
    }

    pub(super) fn set_host(&mut self, host: pulsebeam_queue::Sender<crate::link::host::Command>) {
        self.host = Some(host);
    }

    pub(super) fn set_node(&mut self, node: RelayNode) {
        self.node = Some(node);
    }

    pub(super) fn identify_peer(&mut self, link: NetworkId, node: NodeId) {
        self.peer_ids.insert(link, node);
    }

    pub(super) fn inspect(
        &self,
        reply: oneshot::Sender<Result<crate::link::HostHealth, pulsebeam_link::LinkError>>,
    ) {
        let Some(host) = &self.host else {
            let _ = reply.send(Err(pulsebeam_link::LinkError::Closed));
            return;
        };
        if let Err(error) = host.try_send(crate::link::host::Command::Inspect(reply)) {
            let (failure, command) = match error {
                pulsebeam_queue::TrySendError::Full(command) => {
                    (pulsebeam_link::LinkError::Capacity, command)
                }
                pulsebeam_queue::TrySendError::Closed(command) => {
                    (pulsebeam_link::LinkError::Closed, command)
                }
            };
            if let crate::link::host::Command::Inspect(reply) = command {
                let _ = reply.send(Err(failure));
            }
        }
    }

    pub(super) fn open_peer(
        &mut self,
        peer: impl Into<pulsebeam_link::PeerConfig>,
    ) -> Result<pulsebeam_link::NetworkId, pulsebeam_link::LinkError> {
        self.open_association(peer.into(), None, |control, peer| {
            match peer {
                pulsebeam_link::PeerConfig::Fabric(peer) => {
                    control.open_fabric_network(peer, crate::link::fabric_codec::FabricWireCodec)
                }
                _ => control.open_network(peer, crate::link::ReverseCodec),
            }
            .map(|id| (id, ()))
        })
        .map(|(id, ())| id)
    }

    pub(super) fn open_reverse_control_peer(
        &mut self,
        peer: pulsebeam_link::NetworkPeer,
        node: NodeId,
    ) -> Result<(NetworkId, pulsebeam_link::PeerControl), pulsebeam_link::LinkError> {
        self.open_association(peer.into(), Some(node), |control, peer| {
            control.open_network_with_control(peer, crate::link::ReverseCodec)
        })
    }

    #[cfg(all(test, feature = "sim"))]
    pub(super) fn open_control_peer(
        &mut self,
        peer: impl Into<pulsebeam_link::PeerConfig>,
        node: NodeId,
    ) -> Result<(NetworkId, pulsebeam_link::PeerControl), pulsebeam_link::LinkError> {
        self.open_association(peer.into(), Some(node), |control, peer| match peer {
            pulsebeam_link::PeerConfig::Fabric(peer) => control
                .open_fabric_network_with_control(peer, crate::link::fabric_codec::FabricWireCodec),
            _ => control.open_network_with_control(peer, crate::link::ReverseCodec),
        })
    }

    fn open_association<T>(
        &mut self,
        peer: pulsebeam_link::PeerConfig,
        peer_node: Option<NodeId>,
        open: impl FnOnce(
            &mut LinkControl<ShardFrame>,
            pulsebeam_link::PeerConfig,
        ) -> Result<(NetworkId, T), pulsebeam_link::LinkError>,
    ) -> Result<(NetworkId, T), pulsebeam_link::LinkError> {
        let host = self
            .host
            .as_ref()
            .ok_or(pulsebeam_link::LinkError::Closed)?;
        if self
            .peers
            .values()
            .any(|endpoint| *endpoint == peer.endpoint())
        {
            return Err(pulsebeam_link::LinkError::AlreadyOpen);
        }
        if self.peers.len() >= crate::link::host::PEERS {
            return Err(pulsebeam_link::LinkError::Capacity);
        }
        let endpoint = peer.endpoint();
        let (id, lane) = open(&mut self.control, peer)?;
        let Some(driver) = self.control.take_network_driver(id) else {
            self.control.close_network(id);
            return Err(pulsebeam_link::LinkError::Closed);
        };
        if host
            .try_send(crate::link::host::Command::Install(Box::new(
                crate::link::host::Install {
                    id,
                    endpoint,
                    driver,
                    node: self.node,
                    peer_node,
                },
            )))
            .is_err()
        {
            self.control.close_network(id);
            return Err(pulsebeam_link::LinkError::Capacity);
        }
        self.peers.insert(id, endpoint);
        Ok((id, lane))
    }

    pub(super) fn clock_origin(&self) -> Instant {
        self.clock_origin
    }

    pub(super) fn close_peer(
        &mut self,
        peer: pulsebeam_link::NetworkId,
    ) -> Vec<(ShardId, TrackPlanUpdate)> {
        self.control.close_network(peer);
        if let Some(wan) = self.wan.remove(&peer) {
            wan.retire(&mut self.control);
        }
        self.peers.remove(&peer);
        self.peer_ids.remove(&peer);
        self.fabric_relay_owners.retain(|hop, _| hop.peer != peer);
        self.repair_sends.retain(|repair| repair.next.peer != peer);
        self.fabric_publisher_sends
            .retain(|send| send.compiled.next.peer != peer);
        self.fabric_issued.retain(|hop, _| hop.peer != peer);
        self.fabric_installations
            .retain(|pending| pending.spec.peer != peer);
        self.retain_fabric_relays(|relay| {
            relay.input.ingress.association != peer && relay.next.peer != peer
        });
        self.receives.retain(|_, branch| {
            if branch.peer != peer {
                return true;
            }
            branch.record("relay_removed", 0);
            false
        });
        self.fabric_permits
            .retain(|permit| permit.peer != peer && self.receives.contains_key(&permit.upstream));
        let mut updates = Vec::new();
        let mut touched = HashSet::new();
        self.sends.retain(|branch| {
            if branch.peer != peer {
                return true;
            }
            if let Some(plan) = self.plans.get(&branch.source)
                && touched.insert(branch.source)
            {
                updates.push((
                    branch.source.0,
                    TrackPlanUpdate {
                        publication_id: branch.source.1,
                        plan: Some(plan.clone()),
                    },
                ));
            }
            false
        });
        updates
    }

    pub(super) fn bind_reverse(
        &mut self,
        shard: ShardId,
        track: PublicationKey,
        peer: pulsebeam_link::NetworkId,
        address: pulsebeam_link::NodeRouteKey,
    ) -> Result<(BindingId, TrackPlanUpdate), pulsebeam_link::BindError> {
        if self.fabric_peer(peer) {
            return Err(pulsebeam_link::BindError::UnsupportedProfile);
        }
        if !self.peers.contains_key(&peer) {
            return Err(pulsebeam_link::BindError::Closed);
        }
        let source = (shard, track);
        let plan = self
            .plans
            .get(&source)
            .ok_or(pulsebeam_link::BindError::Closed)?
            .clone();
        if self.sends.iter().any(|branch| branch.source == source) {
            return Err(pulsebeam_link::BindError::Capacity);
        }
        let (binding, tx) = self
            .control
            .bind_send(Destination::Network { peer, address })?;
        self.sends.push(SendBranch {
            binding,
            peer,
            source,
            route: CompiledRoute {
                // The network codec omits this local-only field. The receiver's
                // authenticated binding supplies its own destination DeliveryTag.
                address: NodeRouteAddress::new(crate::route::RouteId::from_raw(0), 0),
                tx,
                clock_origin: self.clock_origin,
            },
        });
        Ok((
            binding,
            TrackPlanUpdate {
                publication_id: track,
                plan: Some(plan),
            },
        ))
    }

    pub(super) fn remove_send(&mut self, binding: BindingId) -> Option<(ShardId, TrackPlanUpdate)> {
        let index = self
            .sends
            .iter()
            .position(|branch| branch.binding == binding)?;
        let branch = self.sends.remove(index);
        let Some(plan) = self.plans.get(&branch.source).cloned() else {
            self.control.unbind(binding);
            return None;
        };
        Some((
            branch.source.0,
            TrackPlanUpdate {
                publication_id: branch.source.1,
                plan: Some(plan),
            },
        ))
    }

    pub(super) fn prepare_plan(
        &mut self,
        shard: ShardId,
        update: &mut TrackPlanUpdate,
    ) -> Vec<BindingId> {
        let source = (shard, update.publication_id);
        match &mut update.plan {
            Some(plan) => {
                let mut base = plan.clone();
                base.compiled_remote.clear();
                base.compiled_reverse = None;
                base.compiled_network_reverse = None;
                if self.network_enabled() {
                    self.plans.insert(source, base);
                }
                plan.compiled_remote = plan
                    .remote
                    .iter()
                    .map(|address| self.compile(*address))
                    .collect();
                plan.compiled_reverse = plan.reverse_route.map(|address| self.compile(address));
                plan.compiled_network_reverse = self
                    .sends
                    .iter()
                    .find(|branch| branch.source == source)
                    .map(|branch| branch.route.clone());
                Vec::new()
            }
            None => {
                self.plans.remove(&source);
                let mut bindings = Vec::new();
                self.sends.retain(|branch| {
                    if branch.source == source {
                        bindings.push(branch.binding);
                        false
                    } else {
                        true
                    }
                });
                bindings
            }
        }
    }

    pub(super) fn retire_bindings_after(
        &mut self,
        bindings: Vec<BindingId>,
        ack: oneshot::Receiver<()>,
    ) {
        if !bindings.is_empty() {
            self.retirements.push((
                bindings.into_iter().map(Retirement::Binding).collect(),
                ack,
                false,
            ));
        }
    }

    pub(super) fn network_enabled(&self) -> bool {
        self.host.is_some() || !self.wan.is_empty()
    }

    pub(super) fn has_peer(&self, peer: pulsebeam_link::NetworkId) -> bool {
        self.peers.contains_key(&peer) || self.wan.contains_key(&peer)
    }

    pub(super) fn send_live(&self, binding: BindingId) -> bool {
        self.sends.iter().any(|branch| branch.binding == binding)
    }

    pub(super) fn relay_targets(
        &mut self,
        peer: NetworkId,
        limit: usize,
    ) -> Option<BTreeMap<super::controller::peer_ownership::relay::Key, Vec<NodeRouteAddress>>>
    {
        use super::controller::peer_ownership::relay::Key;
        self.reap();
        let mut targets = BTreeMap::<_, Vec<_>>::new();
        for address in &self.installed {
            let Some(action) = self.route_actions.get(address) else {
                continue;
            };
            let Some(scope) = action.scope else { continue };
            let reverse = matches!(action.route, TrackRouteAction::Reverse { .. });
            if (reverse && scope.ingress.is_none()) || (!reverse && scope.ingress == Some(peer)) {
                let key = Key::new(scope.room, action.route.publication_id(), reverse);
                if targets.len() >= limit && !targets.contains_key(&key) {
                    return None;
                }
                targets.entry(key).or_default().push(*address);
            }
        }
        for addresses in targets.values_mut() {
            addresses.sort_by_key(|address| (address.route.get(), address.epoch));
        }
        Some(targets)
    }

    pub(super) fn destinations(
        &mut self,
        track: PublicationKey,
        ingress_peer: Option<pulsebeam_link::NetworkId>,
    ) -> Vec<crate::link::ReceiveDestination> {
        self.reap();
        let mut destinations: Vec<_> = self
            .installed
            .iter()
            .filter_map(|address| {
                let action = self.route_actions.get(address)?;
                if action.route.publication_id() != track
                    || action.scope.and_then(|scope| scope.ingress) != ingress_peer
                {
                    return None;
                }
                let kind = match action.route {
                    TrackRouteAction::Forward { .. } => crate::link::ReceiveKind::Forward,
                    TrackRouteAction::Reverse { .. } => crate::link::ReceiveKind::Reverse,
                };
                Some(crate::link::ReceiveDestination {
                    address: *address,
                    kind,
                })
            })
            .collect();
        destinations.sort_by_key(|destination| {
            (destination.address.route.get(), destination.address.epoch)
        });
        destinations
    }

    pub(super) fn install_after(
        &mut self,
        address: NodeRouteAddress,
        action: TrackRouteAction,
        scope: Option<RouteScope>,
        ack: oneshot::Receiver<()>,
    ) {
        self.installed.remove(&address);
        self.route_actions.insert(
            address,
            ReceiveAction {
                route: action,
                scope,
            },
        );
        self.installations.insert(address, (ack, None));
    }

    pub(super) fn bind_receive(
        &mut self,
        peer: pulsebeam_link::NetworkId,
        addresses: Vec<NodeRouteAddress>,
    ) -> Result<crate::link::ReceiveRoute, pulsebeam_link::BindError> {
        self.reap();
        if self.fabric_peer(peer) {
            return Err(pulsebeam_link::BindError::UnsupportedProfile);
        }
        if !self.peers.contains_key(&peer) {
            return Err(pulsebeam_link::BindError::Closed);
        }
        if addresses.is_empty() || addresses.len() > self.ports.len() {
            return Err(pulsebeam_link::BindError::Capacity);
        }
        let mut destinations = Vec::with_capacity(addresses.len());
        let mut expected = None;
        for address in &addresses {
            if !self.installed.contains(address) {
                return Err(pulsebeam_link::BindError::Closed);
            }
            let action = self
                .route_actions
                .get(address)
                .ok_or(pulsebeam_link::BindError::Closed)?;
            if action
                .scope
                .and_then(|scope| scope.ingress)
                .is_some_and(|origin| origin != peer)
            {
                return Err(pulsebeam_link::BindError::Closed);
            }
            let TrackRouteAction::Reverse { publication_id } = action.route else {
                return Err(pulsebeam_link::BindError::Closed);
            };
            let identity = (publication_id, action.scope);
            if expected.is_some_and(|expected| expected != identity) {
                return Err(pulsebeam_link::BindError::Closed);
            }
            expected = Some(identity);
            let port = *self
                .ports
                .get(address.shard().index())
                .ok_or(pulsebeam_link::BindError::Closed)?;
            destinations.push((port, crate::link::delivery_tag(*address)));
        }
        let (binding, address) = self.control.bind_receive(peer, &destinations)?;
        let evidence = expected.and_then(|(publication, scope)| {
            Some(RelayEvidence {
                local: self.node?,
                peer: self.peer_ids.get(&peer).copied(),
                link: peer,
                label: address,
                project: scope?.room.project,
                publication,
                reverse: true,
            })
        });
        let branch = ReceiveBranch {
            peer,
            fabric: None,
            live: addresses.len(),
            addresses,
            evidence,
        };
        branch.record("relay_installed", branch.live);
        self.receives.insert(binding, branch);
        Ok(crate::link::ReceiveRoute { binding, address })
    }

    fn allows_live_receive(&self, spec: &LiveReceiveSpec, address: NodeRouteAddress) -> bool {
        self.allows_live_scope(spec.peer, spec.room, spec.publication, address)
    }

    fn allows_live_scope(
        &self,
        peer: NetworkId,
        room: ProjectRoomKey,
        track: PublicationKey,
        address: NodeRouteAddress,
    ) -> bool {
        self.installed.contains(&address)
            && self.route_actions.get(&address).is_some_and(|action| {
                matches!(action.route, TrackRouteAction::Forward { publication_id } if publication_id == track)
                    && action.scope == Some(RouteScope { room, ingress: Some(peer) })
            })
    }

    pub(super) fn stage_live_receive(
        &mut self,
        spec: LiveReceiveSpec,
        mut stage_child: impl FnMut(LiveNetworkChild, pulsebeam_link::BindingCutoff) -> ChildReceipt,
    ) -> Result<StagedLiveReceive, BindError> {
        self.reap();
        if !self.peers.contains_key(&spec.peer) {
            return Err(BindError::Closed);
        }
        let Some(first) = spec.addresses.first() else {
            return Err(BindError::Capacity);
        };
        if spec.addresses.len() > self.ports.len()
            || self
                .fabric_issued
                .len()
                .saturating_add(self.fabric_installations.len())
                >= crate::control::controller::peer_ownership::relay::LIMIT
        {
            return Err(BindError::Capacity);
        }
        let mut destinations = Vec::with_capacity(spec.addresses.len());
        for &address in &spec.addresses {
            if !self.allows_live_receive(&spec, address) {
                return Err(BindError::Closed);
            }
            let port = *self
                .ports
                .get(address.shard().index())
                .ok_or(BindError::Closed)?;
            destinations.push((port, crate::link::delivery_tag(address)));
        }
        let (binding, address) = crate::link::fabric_codec::stage_receive(
            &mut self.control,
            spec.peer,
            u8::try_from(first.shard().index()).map_err(|_| BindError::Capacity)?,
            &destinations,
            (spec.major, spec.revision),
            spec.emission,
        )?;
        let mut receipts = Vec::with_capacity(destinations.len());
        for (&destination, &(port, _)) in spec.addresses.iter().zip(&destinations) {
            let Some(lifetime) = self.control.receive_port_cutoff(binding, port) else {
                self.control.unbind(binding);
                return Err(BindError::Closed);
            };
            let child = LiveNetworkChild {
                ingress: FabricIngress {
                    association: spec.peer,
                    key: address,
                    purpose: pulsebeam_link::FabricPurpose::Live,
                },
                destination,
                publication: spec.publication,
                incarnation: spec.incarnation,
                sequence_anchor: spec.sequence_anchor,
                major: spec.major,
                revision: spec.revision,
                emission: spec.emission,
                window: None,
            };
            receipts.push((child, stage_child(child, lifetime)));
        }
        let (reply, result) = oneshot::channel();
        self.fabric_installations.push(PendingLiveReceive {
            spec,
            route: crate::link::ReceiveRoute { binding, address },
            receipts,
            failed: false,
            reply,
        });
        Ok(StagedLiveReceive {
            binding,
            receipt: result,
        })
    }

    pub(super) fn fabric_receive_live(&self, binding: BindingId) -> bool {
        self.receives
            .get(&binding)
            .filter(|receive| receive.fabric.is_some())
            .is_some_and(|receive| {
                receive.addresses.iter().all(|address| {
                    self.ports
                        .get(address.shard().index())
                        .and_then(|port| self.control.receive_port_cutoff(binding, *port))
                        .is_some_and(|cutoff| cutoff.is_live())
                })
            })
    }

    pub(super) fn fabric_peer(&self, peer: NetworkId) -> bool {
        self.control.is_fabric_network(peer)
    }

    pub(super) fn stage_located_repair_receive(
        &mut self,
        interest: Interest,
        sequence: u64,
        stage_child: impl FnOnce(
            RepairChild,
            pulsebeam_link::BindingCutoff,
        ) -> oneshot::Receiver<Option<RepairChild>>,
    ) -> Result<oneshot::Receiver<ReceiveResult>, BindError> {
        let parent = self
            .repair_interest_parent(interest)
            .ok_or(BindError::Closed)?;
        self.stage_repair_receive(
            RepairReceiveSpec {
                parent,
                destination: interest.parent.destination,
                receiver: interest.receiver,
                branch: interest.branch,
                demand: Demand {
                    request: interest.request,
                    sequence,
                    expires: interest.expires,
                },
            },
            stage_child,
        )
    }

    pub(super) fn allows_repair_interest(&self, interest: Interest, now: Instant) -> bool {
        if interest.parent.window.is_some() {
            return self.allows_wan_repair_interest(interest, now);
        }
        if now >= interest.expires {
            return false;
        }
        let Some(parent) = self.repair_interest_parent(interest) else {
            return false;
        };
        self.ports
            .get(interest.parent.destination.shard().index())
            .and_then(|port| self.control.receive_port_cutoff(parent, *port))
            .is_some_and(|cutoff| cutoff.is_live())
    }

    fn repair_interest_parent(&self, interest: Interest) -> Option<BindingId> {
        let parent = interest.parent;
        let hop = LiveNextHop {
            peer: parent.ingress.association,
            key: parent.ingress.key,
        };
        let binding = *self.fabric_issued.get(&hop)?;
        let receive = self.receives.get(&binding)?;
        let live = receive.fabric?;
        (parent.ingress.purpose == pulsebeam_link::FabricPurpose::Live
            && receive.peer == hop.peer
            && live.key == hop.key
            && receive.addresses.contains(&parent.destination)
            && live.publication == parent.publication
            && live.incarnation == parent.incarnation
            && live.sequence_anchor == parent.sequence_anchor
            && live.emission == parent.emission
            && live.major == parent.major
            && live.revision == parent.revision)
            .then_some(binding)
    }

    pub(super) fn cancel_repair_receive(&mut self, interest: Interest) {
        let Some(parent) = self.repair_interest_parent(interest) else {
            return;
        };
        let matches = |receive: &RepairReceive| {
            receive.parent == parent
                && receive.child.receiver == interest.receiver
                && receive.child.destination == interest.parent.destination
                && receive.child.route.branch == interest.branch
                && receive.child.demand.request == interest.request
        };
        self.repair_installations.retain(|pending| {
            if !matches(&pending.receive) {
                return true;
            }
            self.control.unbind(pending.receive.route.binding);
            false
        });
        self.repair_receives.retain(|receive| {
            if !matches(receive) {
                return true;
            }
            self.control.unbind(receive.route.binding);
            false
        });
    }

    pub(super) fn repair_receive_live(
        &self,
        route: crate::link::ReceiveRoute,
        now: Instant,
    ) -> bool {
        self.repair_receives
            .iter()
            .any(|receive| receive.route == route && self.repair_parent_live(receive, now))
    }

    fn stage_repair_receive(
        &mut self,
        spec: RepairReceiveSpec,
        stage_child: impl FnOnce(
            RepairChild,
            pulsebeam_link::BindingCutoff,
        ) -> oneshot::Receiver<Option<RepairChild>>,
    ) -> Result<oneshot::Receiver<ReceiveResult>, BindError> {
        self.reap();
        let now = Instant::now();
        let Some(latest_deadline) = now.checked_add(std::time::Duration::from_secs(2)) else {
            return Err(BindError::Closed);
        };
        if now >= spec.demand.expires || spec.demand.expires > latest_deadline {
            return Err(BindError::Closed);
        }
        if self
            .repair_installations
            .len()
            .saturating_add(self.repair_receives.len())
            >= crate::control::controller::peer_ownership::relay::LIMIT
        {
            return Err(BindError::Capacity);
        }
        let parent = self.receives.get(&spec.parent).ok_or(BindError::Closed)?;
        let live = parent.fabric.ok_or(BindError::UnsupportedProfile)?;
        if live.emission.profile() != pulsebeam_core::fabric::Profile::Data
            || !parent.addresses.contains(&spec.destination)
        {
            return Err(BindError::UnsupportedProfile);
        }
        let port = *self
            .ports
            .get(spec.destination.shard().index())
            .ok_or(BindError::Closed)?;
        if !self
            .control
            .receive_port_cutoff(spec.parent, port)
            .is_some_and(|lifetime| lifetime.is_live())
        {
            return Err(BindError::Closed);
        }
        let peer = parent.peer;
        let frontier = (spec.parent, spec.branch);
        if let Some(last) = self.repair_frontiers.get(&frontier) {
            if spec.demand.request.id <= *last {
                return Err(BindError::Closed);
            }
        } else if self.repair_frontiers.len()
            >= crate::control::controller::peer_ownership::relay::LIMIT
        {
            return Err(BindError::Capacity);
        }
        let (binding, address) = self.control.stage_fabric_repair_receive(
            peer,
            u8::try_from(spec.destination.shard().index()).map_err(|_| BindError::Capacity)?,
            (port, crate::link::delivery_tag(spec.destination)),
            move |frame| matches!(frame, ShardFrame::FabricImage(image) if live.emission.accepts(image)),
        )?;
        let child = RepairChild {
            parent: FabricIngress {
                association: peer,
                key: live.key,
                purpose: pulsebeam_link::FabricPurpose::Live,
            },
            route: RepairRoute {
                ingress: FabricIngress {
                    association: peer,
                    key: address,
                    purpose: pulsebeam_link::FabricPurpose::RepairOnly,
                },
                publication: live.publication,
                source: live.incarnation,
                branch: spec.branch,
                major: live.major,
                revision: live.revision,
                emission: live.emission,
            },
            destination: spec.destination,
            receiver: spec.receiver,
            demand: spec.demand,
            window: None,
        };
        let Some(lifetime) = self.control.receive_port_cutoff(binding, port) else {
            self.control.unbind(binding);
            return Err(BindError::Closed);
        };
        self.repair_frontiers
            .insert(frontier, spec.demand.request.id);
        let receipt = stage_child(child, lifetime);
        let (reply, ack) = oneshot::channel();
        self.repair_installations.push(PendingRepairReceive {
            receive: RepairReceive {
                parent: spec.parent,
                child,
                route: crate::link::ReceiveRoute { binding, address },
            },
            receipt,
            received: None,
            reply,
        });
        Ok(ack)
    }

    fn repair_parent_live(&self, receive: &RepairReceive, now: Instant) -> bool {
        let child = receive.child;
        let Some(port) = self.ports.get(child.destination.shard().index()) else {
            return false;
        };
        now < child.demand.expires
            && self.receives.get(&receive.parent).is_some_and(|parent| {
                parent.peer == child.route.ingress.association
                    && parent.addresses.contains(&child.destination)
                    && parent.fabric.is_some_and(|live| {
                        live.publication == child.route.publication
                            && live.incarnation == child.route.source
                            && live.key == child.parent.key
                    })
            })
            && self
                .control
                .receive_port_cutoff(receive.parent, *port)
                .is_some_and(|cutoff| cutoff.is_live())
    }

    fn reap_repair_receives(&mut self, now: Instant) {
        self.repair_frontiers
            .retain(|(parent, _), _| self.receives.contains_key(parent));
        let mut index = 0_usize;
        while let Some(pending) = self.repair_installations.get(index) {
            let parent_live = self.repair_parent_live(&pending.receive, now);
            let Some(pending) = self.repair_installations.get_mut(index) else {
                break;
            };
            if pending.received.is_none() {
                let receipt = match pending.receipt.try_recv() {
                    Ok(receipt) => Poll::Ready(receipt),
                    Err(oneshot::error::TryRecvError::Empty) => Poll::Pending,
                    Err(oneshot::error::TryRecvError::Closed) => Poll::Ready(None),
                };
                pending.observe(receipt);
            }
            if parent_live && pending.received.is_none() {
                index = index.saturating_add(1);
                continue;
            }
            let pending = self.repair_installations.swap_remove(index);
            let binding = pending.receive.route.binding;
            let result = if parent_live && pending.received == Some(true) {
                self.control
                    .activate_fabric_receive(binding)
                    .map(|()| pending.receive.route)
            } else {
                Err(BindError::Closed)
            };
            let accepted = result.is_ok();
            if pending.reply.send(result).is_ok() && accepted {
                self.repair_receives.push(pending.receive);
            } else {
                self.control.unbind(binding);
            }
        }
        let mut index = 0_usize;
        while let Some(receive) = self.repair_receives.get(index) {
            if self.repair_parent_live(receive, now) {
                index = index.saturating_add(1);
            } else {
                let receive = self.repair_receives.swap_remove(index);
                self.control.unbind(receive.route.binding);
            }
        }
    }

    pub(super) fn allows_relay_data_expectation(
        &self,
        expectation: &crate::route::fabric::relay::RelayExpectation,
        now: Instant,
    ) -> bool {
        let route = &expectation.route;
        now < expectation.expires
            && route.tx.is_live()
            && self.fabric_relays.iter().any(|relay| {
                relay.compiled.tx == route.tx
                    && relay.compiled.input == route.input
                    && relay.compiled.next == route.next
                    && relay.compiled.expires_at > now
            })
    }

    pub(super) fn allows_publisher_data_expectation(
        &self,
        expectation: &crate::route::fabric::PublisherExpectation,
        now: Instant,
    ) -> bool {
        use crate::route::fabric::hop_expectations::Path;
        now < expectation.expires
            && expectation.route.publisher.emission == Emission::DATA
            && self.fabric_publisher_sends.iter().any(|send| {
                send.compiled.matches(&expectation.route)
                    && now < send.compiled.expires_at
                    && send.compiled.tx.is_live()
            })
    }

    pub(super) fn peer_publisher_data_observations(
        &self,
        peer: NetworkId,
        frame: crate::control::controller::peer_ownership::fabric_data::Frame,
        now: Instant,
    ) -> Vec<crate::route::fabric::PublisherObservation> {
        use crate::control::controller::peer_ownership::fabric_data::Action;
        let Action::Observed(outcome) = frame.action else {
            return Vec::new();
        };
        self.fabric_publisher_sends
            .iter()
            .map(|send| &send.compiled)
            .filter(|route| {
                route.publisher.emission == Emission::DATA
                    && route.next.peer == peer
                    && route.next.key == frame.scope.key
                    && route.publisher.incarnation == frame.scope.source
                    && now < route.expires_at
                    && route.tx.is_live()
            })
            .map(|route| crate::route::fabric::PublisherObservation {
                route: route.clone(),
                unit: frame.unit,
                outcome,
            })
            .collect()
    }

    pub(super) fn peer_data_observations(
        &self,
        peer: NetworkId,
        frame: crate::control::controller::peer_ownership::fabric_data::Frame,
        now: Instant,
    ) -> Vec<crate::route::fabric::relay::RelayObservation> {
        use crate::control::controller::peer_ownership::fabric_data::Action;
        let Action::Observed(outcome) = frame.action else {
            return Vec::new();
        };
        self.current_data_relay_paths(peer, frame.scope, now)
            .map(|route| crate::route::fabric::relay::RelayObservation {
                route: route.clone(),
                unit: frame.unit,
                outcome,
            })
            .collect()
    }

    pub(super) fn bind_peer_repair_send(
        &mut self,
        located: crate::route::fabric::relay::LocatedRepair,
        label: pulsebeam_link::NodeRouteKey,
        now: Instant,
    ) -> Result<(BindingId, crate::route::fabric::relay::PeerRepairHandoff), BindError> {
        use crate::route::fabric::{
            hop_expectations::Path,
            relay::{PeerRepairHandoff, PeerRepairLookup},
        };
        self.reap_fabric_relays(now);
        let (live_next, live_binding, upstream, deadline) = match located.lookup.as_ref() {
            PeerRepairLookup::Relay(lookup) => {
                let route = &lookup.route;
                let live = self
                    .fabric_relays
                    .iter()
                    .find(|live| live.compiled.matches(route))
                    .ok_or(BindError::Closed)?;
                (
                    route.next,
                    live.binding,
                    Some(live.upstream),
                    live.compiled.expires_at,
                )
            }
            PeerRepairLookup::Publisher(lookup) => {
                let route = &lookup.route;
                let live = self
                    .fabric_publisher_sends
                    .iter()
                    .find(|live| live.compiled.matches(route) && live.compiled.tx.is_live())
                    .ok_or(BindError::Closed)?;
                (route.next, live.binding, None, live.compiled.expires_at)
            }
            PeerRepairLookup::Media(_) => return Err(BindError::UnsupportedProfile),
        };
        let next = LiveNextHop {
            peer: live_next.peer,
            key: label,
        };
        let expires = deadline.min(located.expires);
        if now >= expires
            || next == live_next
            || self
                .fabric_relay_cutoffs
                .iter()
                .any(|retired| retired.next == next)
            || self.repair_sends.iter().any(|repair| repair.next == next)
            || self.fabric_relay_owners.contains_key(&next)
        {
            return Err(BindError::Closed);
        }
        if self.repair_sends.len() >= crate::control::controller::peer_ownership::relay::LIMIT {
            return Err(BindError::Capacity);
        }
        let (binding, tx) = if let Some(upstream) = upstream {
            self.control.bind_fabric_relay(upstream, next.peer, label)?
        } else {
            self.control
                .bind_fabric_publisher_repair(live_binding, next.peer, label)?
        };
        self.repair_sends.push(RepairRelaySend {
            binding,
            live_binding,
            next,
            expires,
        });
        Ok((
            binding,
            PeerRepairHandoff {
                located,
                tx: FabricTx::legacy(tx, self.clock_origin),
            },
        ))
    }

    fn fence_repair_sends(&mut self, parent: BindingId) {
        self.repair_sends.retain(|repair| {
            if repair.live_binding != parent {
                return true;
            }
            self.control.unbind(repair.binding);
            false
        });
    }

    pub(super) fn cancel_relay_repair_send(&mut self, binding: BindingId) {
        if let Some(index) = self
            .repair_sends
            .iter()
            .position(|repair| repair.binding == binding)
        {
            self.repair_sends.swap_remove(index);
            self.control.unbind(binding);
        }
    }

    fn current_data_relay_paths(
        &self,
        peer: NetworkId,
        scope: crate::control::controller::peer_ownership::fabric_data::Scope,
        now: Instant,
    ) -> impl Iterator<Item = &CompiledLiveRelay> {
        self.fabric_relays
            .iter()
            .map(|bound| &bound.compiled)
            .filter(move |route| {
                route.next.peer == peer
                    && route.next.key == scope.key
                    && route.input.incarnation == scope.source
                    && route.input.emission.profile() == pulsebeam_core::fabric::Profile::Data
                    && now < route.expires_at
                    && route.tx.is_live()
            })
    }

    pub(super) fn peer_repair_lookups(
        &self,
        peer: NetworkId,
        frame: crate::control::controller::peer_ownership::fabric_data::RepairFrame,
        now: Instant,
    ) -> Vec<crate::route::fabric::relay::RelayRepairLookup> {
        use crate::control::controller::peer_ownership::fabric_data::RepairAction;
        if frame.action != RepairAction::Lookup {
            return Vec::new();
        }
        self.current_data_relay_paths(peer, frame.scope, now)
            .map(|route| crate::route::fabric::relay::RelayRepairLookup {
                route: route.clone(),
                branch: frame.branch,
                request: frame.request,
            })
            .collect()
    }

    pub(super) fn peer_publisher_repair_lookups(
        &self,
        peer: NetworkId,
        frame: crate::control::controller::peer_ownership::fabric_data::RepairFrame,
        now: Instant,
    ) -> impl Iterator<Item = crate::route::fabric::relay::PublisherRepairLookup> {
        use crate::control::controller::peer_ownership::fabric_data::RepairAction;
        self.fabric_publisher_sends
            .iter()
            .map(|send| &send.compiled)
            .filter(move |route| {
                frame.action == RepairAction::Lookup
                    && route.publisher.emission == Emission::DATA
                    && route.next.peer == peer
                    && route.next.key == frame.scope.key
                    && route.publisher.incarnation == frame.scope.source
                    && now < route.expires_at
                    && route.tx.is_live()
            })
            .map(
                move |route| crate::route::fabric::relay::PublisherRepairLookup {
                    route: route.clone(),
                    branch: frame.branch,
                    request: frame.request,
                },
            )
    }

    pub(super) fn peer_data_expectations(
        &self,
        peer: NetworkId,
        frame: crate::control::controller::peer_ownership::fabric_data::Frame,
        now: Instant,
    ) -> Option<Vec<crate::route::fabric::NetworkDataExpectation>> {
        if self.wan.contains_key(&peer) {
            return self.wan_data_expectations(peer, frame, now);
        }
        use crate::route::fabric::NetworkDataExpectation;
        let expires = now.checked_add(frame.lease()?)?;
        let hop = LiveNextHop {
            peer,
            key: frame.scope.key,
        };
        let binding = self.fabric_issued.get(&hop)?;
        let receive = self.receives.get(binding)?;
        let live = receive.fabric?;
        if receive.peer != peer
            || live.key != frame.scope.key
            || live.incarnation != frame.scope.source
            || live.emission.profile() != pulsebeam_core::fabric::Profile::Data
        {
            return None;
        }
        receive
            .addresses
            .iter()
            .map(|&destination| {
                let port = *self.ports.get(destination.shard().index())?;
                if !self
                    .control
                    .receive_port_cutoff(*binding, port)
                    .is_some_and(|cutoff| cutoff.is_live())
                {
                    return None;
                }
                Some(NetworkDataExpectation {
                    parent: LiveNetworkChild {
                        ingress: FabricIngress {
                            association: peer,
                            key: live.key,
                            purpose: pulsebeam_link::FabricPurpose::Live,
                        },
                        destination,
                        publication: live.publication,
                        incarnation: live.incarnation,
                        sequence_anchor: live.sequence_anchor,
                        major: live.major,
                        revision: live.revision,
                        emission: live.emission,
                        window: None,
                    },
                    unit: frame.unit,
                    expires,
                })
            })
            .collect()
    }

    pub(super) fn fabric_offers(&self, peer: NetworkId) -> Vec<WireLiveRouteAck> {
        let mut offers: Vec<_> = self
            .receives
            .iter()
            .filter_map(|(&binding, branch)| {
                if branch.peer != peer {
                    return None;
                }
                let address = branch.addresses.first()?;
                let port = *self.ports.get(address.shard().index())?;
                let ack = branch.fabric?;
                self.control
                    .receive_port_cutoff(binding, port)?
                    .is_live()
                    .then_some(ack)
            })
            .take(crate::control::controller::peer_ownership::relay::LIMIT.saturating_add(1))
            .collect();
        offers.sort_unstable_by_key(|ack| ack.key.to_bytes());
        offers.into_iter().map(WireLiveRouteAck::from).collect()
    }

    /// Record a locally authorized edge. A peer acknowledgment alone never grants relay access.
    pub(super) fn permit_fabric_relay(
        &mut self,
        permit: FabricRelayPermit,
    ) -> Result<(), BindError> {
        let input = self
            .receives
            .get(&permit.upstream)
            .ok_or(BindError::Closed)?;
        if input.peer == permit.peer
            || !self.fabric_peer(permit.peer)
            || !input.addresses.contains(&permit.destination)
            || !input
                .fabric
                .is_some_and(|source| source.same_source(permit.expected))
        {
            return Err(BindError::Closed);
        }
        if self.fabric_permits.contains(&permit) {
            return Ok(());
        }
        if self.fabric_permits.len() >= crate::control::controller::peer_ownership::relay::LIMIT {
            return Err(BindError::Capacity);
        }
        let next = LiveNextHop {
            peer: permit.peer,
            key: permit.expected.key,
        };
        let shard = permit.destination.shard();
        if let Some(owner) = self.fabric_relay_owners.get(&next) {
            if owner.publisher.is_some()
                || owner.shard != shard
                || owner.expected != permit.expected
                || matches!(owner.lease, FabricRelayLease::Retired)
            {
                return Err(BindError::Closed);
            }
        } else {
            if self.fabric_relay_owners.len()
                >= crate::control::controller::peer_ownership::relay::LIMIT
            {
                return Err(BindError::Capacity);
            }
            self.fabric_relay_owners.insert(
                next,
                FabricRelayOwner {
                    shard,
                    publisher: None,
                    expected: permit.expected,
                    lease: FabricRelayLease::Awaiting,
                    withdrawal_seen: false,
                    cutoff_failed: false,
                },
            );
        }
        self.fabric_permits.push(permit);
        Ok(())
    }

    pub(super) fn revoke_fabric_relay(&mut self, permit: FabricRelayPermit) -> bool {
        self.fabric_permits.retain(|entry| *entry != permit);
        let binding = self
            .fabric_relays
            .iter()
            .find(|relay| relay.authorized_by(permit))
            .map(|relay| relay.binding);
        binding
            .is_some_and(|binding| self.retire_fabric_relay_with(binding, RelayRetirement::Revoked))
    }

    pub(super) fn reconcile_publisher_sends(
        &mut self,
        peer: NetworkId,
        desired: &[PublisherSendPermit],
        expires_at: Instant,
        now: Instant,
    ) -> Vec<(BindingId, PublisherSendChange)> {
        let mut changes = Vec::new();
        let mut index = 0;
        while let Some(send) = self.fabric_publisher_sends.get(index) {
            if send.compiled.next.peer == peer
                && (now >= send.compiled.expires_at
                    || now >= expires_at
                    || !send.compiled.tx.is_live()
                    || !desired.contains(&send.permit))
            {
                let send = self.fabric_publisher_sends.swap_remove(index);
                changes.push((send.binding, self.fence_publisher_send(send)));
            } else {
                index = index.saturating_add(1);
            }
        }
        if now >= expires_at {
            return changes;
        }
        for &permit in desired {
            if self.fabric_publisher_sends.iter().any(|send| {
                send.compiled.next.peer == peer
                    && send.permit == permit
                    && send.compiled.expires_at == expires_at
            }) {
                continue;
            }
            if let Ok(change) = self.compile_publisher_send(peer, permit, expires_at, now) {
                changes.push(change);
            }
        }
        changes
    }

    fn compile_publisher_send(
        &mut self,
        peer: NetworkId,
        permit: PublisherSendPermit,
        expires_at: Instant,
        now: Instant,
    ) -> Result<(BindingId, PublisherSendChange), BindError> {
        let ack = permit.ack;
        use crate::entity::TrackKind;
        use pulsebeam_core::fabric::Profile;
        let profile_allowed = match (ack.publication.kind(), ack.emission.profile()) {
            (TrackKind::Data, Profile::Data) => ack.emission == Emission::DATA,
            (TrackKind::Audio, Profile::Opus) => cfg!(feature = "sim") && self.fabric_peer(peer),
            _ => false,
        };
        let next = LiveNextHop { peer, key: ack.key };
        if now >= expires_at
            || !profile_allowed
            || ack.publication != permit.publisher.publication
            || ack.incarnation != permit.publisher.incarnation
            || ack.emission != permit.publisher.emission
            || !ack.emission.fits_registry(ack.major, ack.revision)
            || self.repair_sends.iter().any(|repair| repair.next == next)
        {
            return Err(BindError::UnsupportedProfile);
        }
        if let Some(owner) = self.fabric_relay_owners.get(&next) {
            if owner.publisher != Some(permit.publisher)
                || owner.shard != permit.shard
                || owner.expected != ack
                || matches!(owner.lease, FabricRelayLease::Retired)
            {
                return Err(BindError::Closed);
            }
        } else if self.fabric_relay_owners.len()
            >= crate::control::controller::peer_ownership::relay::LIMIT
        {
            return Err(BindError::Capacity);
        }
        if let Some(send) = self
            .fabric_publisher_sends
            .iter_mut()
            .find(|send| send.compiled.next == next)
        {
            let lifetime = self
                .control
                .fabric_send_cutoff(send.binding)
                .ok_or(BindError::Closed)?;
            send.compiled.expires_at = expires_at;
            return Ok((
                send.binding,
                PublisherSendChange::Install {
                    route: send.compiled.clone(),
                    lifetime,
                },
            ));
        }
        let (binding, tx) = self.control.bind_fabric_send(peer, ack.key)?;
        let Some(lifetime) = self.control.fabric_send_cutoff(binding) else {
            self.control.unbind(binding);
            return Err(BindError::Closed);
        };
        let compiled = CompiledPublisherSend {
            publisher: permit.publisher,
            shard: permit.shard,
            next,
            expires_at,
            tx: FabricTx::legacy(tx, self.clock_origin),
        };
        self.fabric_relay_owners.insert(
            next,
            FabricRelayOwner {
                shard: permit.shard,
                publisher: Some(permit.publisher),
                expected: ack,
                lease: FabricRelayLease::Acknowledged(expires_at),
                withdrawal_seen: false,
                cutoff_failed: false,
            },
        );
        self.fabric_publisher_sends.push(FabricPublisherSend {
            binding,
            permit,
            compiled: compiled.clone(),
        });
        Ok((
            binding,
            PublisherSendChange::Install {
                route: compiled,
                lifetime,
            },
        ))
    }

    pub(super) fn retire_publisher_send(
        &mut self,
        binding: BindingId,
    ) -> Option<PublisherSendChange> {
        let index = self
            .fabric_publisher_sends
            .iter()
            .position(|send| send.binding == binding)?;
        let send = self.fabric_publisher_sends.swap_remove(index);
        Some(self.fence_publisher_send(send))
    }

    fn fence_publisher_send(&mut self, send: FabricPublisherSend) -> PublisherSendChange {
        let binding = send.binding;
        self.fence_repair_sends(binding);
        self.control.unbind(binding);
        let next = send.compiled.next;
        if let Some(owner) = self.fabric_relay_owners.get_mut(&next) {
            owner.lease = FabricRelayLease::Retired;
        }
        self.fabric_relay_cutoffs.push(RetiringFabricRelay {
            binding,
            next,
            reason: RelayRetirement::Unconfirmed,
            phase: FabricRelayCutoff::Staging,
        });
        PublisherSendChange::Retire(send.compiled)
    }

    pub(super) fn reconcile_fabric_relays(
        &mut self,
        peer: NetworkId,
        acknowledged: &[LiveRouteAck],
        expires_at: Instant,
        now: Instant,
    ) -> Vec<(BindingId, LiveRelayChange)> {
        for (next, owner) in &mut self.fabric_relay_owners {
            if next.peer != peer {
                continue;
            }
            let present = expires_at > now && acknowledged.contains(&owner.expected);
            if expires_at > now
                && !matches!(owner.lease, FabricRelayLease::Awaiting)
                && !acknowledged.iter().any(|ack| ack.key == next.key)
            {
                owner.withdrawal_seen = true;
            }
            owner.lease = match owner.lease {
                FabricRelayLease::Retired => FabricRelayLease::Retired,
                FabricRelayLease::Acknowledged(previous) if previous <= now || !present => {
                    FabricRelayLease::Retired
                }
                _ if present => FabricRelayLease::Acknowledged(expires_at),
                _ => FabricRelayLease::Awaiting,
            };
        }
        self.fabric_permits.retain(|permit| {
            self.fabric_relay_owners
                .get(&LiveNextHop {
                    peer: permit.peer,
                    key: permit.expected.key,
                })
                .is_some_and(|owner| !matches!(owner.lease, FabricRelayLease::Retired))
        });
        let desired: Vec<_> = self
            .fabric_permits
            .iter()
            .copied()
            .filter(|permit| {
                permit.peer == peer && expires_at > now && acknowledged.contains(&permit.expected)
            })
            .collect();
        let withdrawn: Vec<_> = self
            .fabric_relays
            .iter()
            .filter_map(|relay| {
                if relay.compiled.next.peer != peer {
                    return None;
                }
                let reason = if expires_at > now && !acknowledged.contains(&relay.expected) {
                    RelayRetirement::Revoked
                } else if relay.compiled.expires_at <= now
                    || !relay.compiled.tx.is_live()
                    || !desired.iter().any(|permit| relay.authorized_by(*permit))
                {
                    RelayRetirement::Unconfirmed
                } else {
                    return None;
                };
                Some((relay.binding, reason))
            })
            .collect();
        for (binding, reason) in withdrawn {
            self.retire_fabric_relay_with(binding, reason);
        }
        let mut updates = self.take_fabric_retirements(peer);
        for permit in desired {
            if !self.fabric_permits.contains(&permit) {
                continue;
            }
            if self.fabric_relays.iter().any(|relay| {
                relay.authorized_by(permit)
                    && relay.compiled.tx.is_live()
                    && relay.compiled.expires_at == expires_at
            }) {
                continue;
            }
            if let Ok((binding, relay)) = self.compile_fabric_relay(
                permit.upstream,
                permit.destination,
                peer,
                permit.expected,
                expires_at,
                now,
            ) {
                updates.push((binding, LiveRelayChange::Install(relay)));
            }
        }
        updates
    }

    /// Match an acknowledgment from `peer` against one installed local input.
    /// Only the authenticated ownership receiver can supply the acknowledgment.
    fn compile_fabric_relay(
        &mut self,
        upstream: BindingId,
        destination: NodeRouteAddress,
        peer: NetworkId,
        ack: LiveRouteAck,
        expires_at: Instant,
        now: Instant,
    ) -> Result<(BindingId, CompiledLiveRelay), BindError> {
        if expires_at <= now || !ack.emission.fits_registry(ack.major, ack.revision) {
            return Err(BindError::UnsupportedProfile);
        }
        let receive = self.receives.get(&upstream).ok_or(BindError::Closed)?;
        let input = receive.fabric.ok_or(BindError::UnsupportedProfile)?;
        if peer == receive.peer
            || !receive.addresses.contains(&destination)
            || !input.same_source(ack)
        {
            return Err(BindError::Closed);
        }
        let input = LiveNetworkChild {
            ingress: FabricIngress {
                association: receive.peer,
                key: input.key,
                purpose: pulsebeam_link::FabricPurpose::Live,
            },
            destination,
            publication: input.publication,
            incarnation: input.incarnation,
            sequence_anchor: input.sequence_anchor,
            major: input.major,
            revision: input.revision,
            emission: input.emission,
            window: None,
        };
        let next = LiveNextHop { peer, key: ack.key };
        self.reap_fabric_relays(now);
        if self.repair_sends.iter().any(|repair| repair.next == next) {
            return Err(BindError::Closed);
        }
        if let Some(existing) = self
            .fabric_relays
            .iter_mut()
            .find(|relay| relay.upstream == upstream && relay.compiled.next == next)
        {
            if existing.compiled.input != input || existing.expected != ack {
                return Err(BindError::Closed);
            }
            existing.compiled.expires_at = expires_at;
            return Ok((existing.binding, existing.compiled.clone()));
        }
        if self
            .fabric_relays
            .len()
            .saturating_add(self.fabric_relay_cutoffs.len())
            >= crate::control::controller::peer_ownership::relay::LIMIT
        {
            return Err(BindError::Capacity);
        }
        let (binding, tx) = self.control.bind_fabric_relay(upstream, peer, next.key)?;
        let compiled = CompiledLiveRelay {
            input,
            next,
            expires_at,
            tx: FabricTx::legacy(tx, self.clock_origin),
        };
        self.fabric_relays.push(FabricRelaySend {
            binding,
            upstream,
            expected: ack,
            compiled: compiled.clone(),
        });
        Ok((binding, compiled))
    }

    fn retain_fabric_relays(&mut self, keep: impl Fn(&CompiledLiveRelay) -> bool) {
        let retired: Vec<_> = self
            .fabric_relays
            .iter()
            .filter_map(|relay| (!keep(&relay.compiled)).then_some(relay.binding))
            .collect();
        for binding in retired {
            self.retire_fabric_relay(binding);
        }
    }

    pub(super) fn retire_fabric_relay(&mut self, binding: BindingId) -> bool {
        self.retire_fabric_relay_with(binding, RelayRetirement::Unconfirmed)
    }

    fn retire_fabric_relay_with(&mut self, binding: BindingId, reason: RelayRetirement) -> bool {
        let Some(index) = self
            .fabric_relays
            .iter()
            .position(|relay| relay.binding == binding)
        else {
            return false;
        };
        let relay = self.fabric_relays.swap_remove(index);
        self.fence_repair_sends(binding);
        self.control.unbind(binding);
        self.fabric_permits
            .retain(|permit| !relay.authorized_by(*permit));
        self.fabric_relay_cutoffs.push(RetiringFabricRelay {
            binding,
            next: relay.compiled.next,
            reason,
            phase: FabricRelayCutoff::NeedsShard(relay.compiled),
        });
        true
    }

    pub(super) fn take_fabric_retirements(
        &mut self,
        peer: NetworkId,
    ) -> Vec<(BindingId, LiveRelayChange)> {
        let mut changes = Vec::new();
        for cutoff in &mut self.fabric_relay_cutoffs {
            if cutoff.next.peer == peer
                && matches!(cutoff.phase, FabricRelayCutoff::NeedsShard(_))
                && let FabricRelayCutoff::NeedsShard(relay) =
                    std::mem::replace(&mut cutoff.phase, FabricRelayCutoff::Staging)
            {
                changes.push((
                    cutoff.binding,
                    LiveRelayChange::Retire(relay, cutoff.reason),
                ));
            }
        }
        changes
    }

    pub(super) fn record_fabric_retirement(
        &mut self,
        binding: BindingId,
        receipt: Option<oneshot::Receiver<bool>>,
    ) {
        let Some(index) = self.fabric_relay_cutoffs.iter().position(|cutoff| {
            cutoff.binding == binding && matches!(cutoff.phase, FabricRelayCutoff::Staging)
        }) else {
            return;
        };
        if let Some(receipt) = receipt {
            if let Some(cutoff) = self.fabric_relay_cutoffs.get_mut(index) {
                cutoff.phase = FabricRelayCutoff::Waiting(receipt);
            }
        } else {
            let cutoff = self.fabric_relay_cutoffs.swap_remove(index);
            if let Some(owner) = self.fabric_relay_owners.get_mut(&cutoff.next) {
                owner.cutoff_failed = true;
            }
        }
    }

    /// Accept only an exact issued key on the authenticated association, after local cutoff.
    pub(super) fn accept_fabric_retirements(&mut self, peer: NetworkId, labels: &[[u8; 4]]) {
        for &label in labels {
            let key = LiveNextHop {
                peer,
                key: pulsebeam_link::NodeRouteKey::from_bytes(label),
            };
            if self
                .fabric_issued
                .get(&key)
                .is_some_and(|binding| !self.receives.contains_key(binding))
            {
                self.fabric_issued.remove(&key);
            }
        }
    }

    pub(super) fn fabric_retired_labels(&mut self, peer: NetworkId) -> Vec<[u8; 4]> {
        self.reap_fabric_retirement_receipts();
        let mut labels: Vec<_> = self
            .fabric_relay_owners
            .iter()
            .filter_map(|(next, owner)| {
                (next.peer == peer
                    && owner.withdrawal_seen
                    && !owner.cutoff_failed
                    && !self
                        .fabric_relay_cutoffs
                        .iter()
                        .any(|cutoff| cutoff.next == *next)
                    && !self
                        .fabric_relays
                        .iter()
                        .any(|relay| relay.compiled.next == *next)
                    && !self
                        .fabric_publisher_sends
                        .iter()
                        .any(|send| send.compiled.next == *next))
                .then_some(next.key.to_bytes())
            })
            .collect();
        labels.sort_unstable();
        labels
    }

    fn reap_fabric_retirement_receipts(&mut self) {
        self.fabric_relay_cutoffs.retain_mut(|cutoff| {
            let FabricRelayCutoff::Waiting(receipt) = &mut cutoff.phase else {
                return true;
            };
            match receipt.try_recv() {
                Err(oneshot::error::TryRecvError::Empty) => true,
                result => {
                    if result != Ok(true)
                        && let Some(owner) = self.fabric_relay_owners.get_mut(&cutoff.next)
                    {
                        owner.cutoff_failed = true;
                    }
                    false
                }
            }
        });
    }

    fn reap_fabric_relays(&mut self, now: Instant) {
        self.reap_fabric_retirement_receipts();
        self.retain_fabric_relays(|relay| relay.expires_at > now && relay.tx.is_live());
        self.repair_sends.retain(|repair| {
            if now < repair.expires {
                return true;
            }
            self.control.unbind(repair.binding);
            false
        });
    }

    pub(super) fn unbind_receive(&mut self, binding: BindingId) {
        let removed = self.receives.remove(&binding);
        let mut owned = removed.is_some();
        self.fabric_installations.retain(|pending| {
            if pending.route.binding == binding {
                owned = true;
                false
            } else {
                true
            }
        });
        for (bindings, _, _) in &mut self.retirements {
            bindings.retain(|pending| {
                if pending.binding() == binding {
                    owned = true;
                    false
                } else {
                    true
                }
            });
        }
        if owned {
            self.control.unbind(binding);
        }
        if let Some(branch) = removed {
            branch.record("relay_removed", 0);
        }
        self.fabric_permits
            .retain(|permit| permit.upstream != binding);
        self.reap_fabric_relays(Instant::now());
    }

    pub(super) fn compile(&mut self, address: NodeRouteAddress) -> CompiledRoute {
        if let Some((_, route)) = self.routes.get(&address) {
            return route.clone();
        }
        let Some(&port) = self.ports.get(address.shard().index()) else {
            pulsebeam_runtime::fatal!("compiled route must name a provisioned Link port");
        };
        let (binding, tx) = self
            .control
            .bind_send(Destination::Local {
                port,
                tag: crate::link::delivery_tag(address),
            })
            .unwrap_or_else(|error| {
                pulsebeam_runtime::fatal!("Link route capacity exhausted: {error:?}")
            });
        let route = CompiledRoute {
            address,
            tx,
            clock_origin: self.clock_origin,
        };
        self.routes.insert(address, (binding, route.clone()));
        route
    }

    pub(super) fn contains(&self, address: NodeRouteAddress) -> bool {
        self.routes.contains_key(&address)
            || self.installed.contains(&address)
            || self.installations.contains_key(&address)
    }

    pub(super) fn retire_after(
        &mut self,
        address: NodeRouteAddress,
        stage: impl FnOnce(Vec<pulsebeam_link::BindingCutoff>) -> oneshot::Receiver<()>,
    ) {
        self.installed.remove(&address);
        self.route_actions.remove(&address);
        self.installations.remove(&address);
        self.fabric_installations.retain(|pending| {
            let keep = !pending.spec.addresses.contains(&address);
            if !keep {
                self.control.unbind(pending.route.binding);
            }
            keep
        });
        let revoked: Vec<_> = self
            .fabric_relays
            .iter()
            .filter_map(|relay| {
                self.receives
                    .get(&relay.upstream)
                    .is_some_and(|parent| parent.addresses.contains(&address))
                    .then_some(relay.binding)
            })
            .collect();
        for binding in revoked {
            self.retire_fabric_relay_with(binding, RelayRetirement::Revoked);
        }
        self.receives.retain(|binding, branch| {
            if branch.fabric.is_some() && branch.addresses.contains(&address) {
                self.control.unbind(*binding);
                branch.record("relay_removed", 0);
                false
            } else {
                true
            }
        });
        let now = Instant::now();
        self.reap_fabric_relays(now);
        self.reap_repair_receives(now);
        let mut retirements = Vec::new();
        if let Some((binding, _)) = self.routes.remove(&address) {
            retirements.push(Retirement::Binding(binding));
        }
        if let Some(&port) = self.ports.get(address.shard().index()) {
            for (binding, branch) in &mut self.receives {
                if let Some(index) = branch
                    .addresses
                    .iter()
                    .position(|target| *target == address)
                {
                    branch.addresses.swap_remove(index);
                    retirements.push(Retirement::ReceivePort(*binding, port));
                }
            }
        }
        if !retirements.is_empty() {
            let bindings = retirements
                .iter()
                .filter_map(|action| match *action {
                    Retirement::ReceivePort(binding, port) => {
                        self.control.receive_port_cutoff(binding, port)
                    }
                    Retirement::Binding(_) => None,
                })
                .collect();
            self.retirements.push((retirements, stage(bindings), false));
        }
    }

    pub(super) fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let mut ready = false;
        for (ack, outcome) in self.installations.values_mut() {
            if outcome.is_none() {
                if let Poll::Ready(result) = Pin::new(ack).poll(cx) {
                    *outcome = Some(result.is_ok());
                }
            }
            ready |= outcome.is_some();
        }
        for pending in &mut self.fabric_installations {
            ready |= pending
                .receipt(|receipt| Pin::new(receipt).poll(cx).map(|value| value.ok().flatten()));
        }
        for pending in &mut self.repair_installations {
            if pending.received.is_none() {
                let result = Pin::new(&mut pending.receipt)
                    .poll(cx)
                    .map(|value| value.ok().flatten());
                pending.observe(result);
            }
            ready |= pending.received.is_some();
        }
        for (_, ack, done) in &mut self.retirements {
            if !*done {
                *done = Pin::new(ack).poll(cx).is_ready();
            }
            ready |= *done;
        }
        if ready {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    pub(super) fn reap(&mut self) {
        let now = Instant::now();
        for wan in self.wan.values_mut() {
            wan.reap(&mut self.control, now);
        }
        self.reap_fabric_relays(now);
        self.reap_repair_receives(now);
        self.installations.retain(|address, (ack, outcome)| {
            if outcome.is_none() {
                *outcome = match ack.try_recv() {
                    Ok(()) => Some(true),
                    Err(oneshot::error::TryRecvError::Closed) => Some(false),
                    Err(oneshot::error::TryRecvError::Empty) => None,
                };
            }
            if *outcome == Some(true) {
                self.installed.insert(*address);
            } else if *outcome == Some(false) {
                self.route_actions.remove(address);
            }
            outcome.is_none()
        });
        let mut index = 0_usize;
        while let Some(pending) = self.fabric_installations.get_mut(index) {
            if !pending.receipt(|receipt| match receipt.try_recv() {
                Ok(value) => Poll::Ready(value),
                Err(oneshot::error::TryRecvError::Empty) => Poll::Pending,
                Err(oneshot::error::TryRecvError::Closed) => Poll::Ready(None),
            }) {
                index = index.saturating_add(1);
                continue;
            }
            let pending = self.fabric_installations.swap_remove(index);
            let binding = pending.route.binding;
            let result = if pending.failed
                || !pending
                    .spec
                    .addresses
                    .iter()
                    .all(|&address| self.allows_live_receive(&pending.spec, address))
            {
                Err(BindError::Closed)
            } else {
                self.control
                    .activate_fabric_receive(binding)
                    .map(|()| pending.route)
            };
            let accepted = result.is_ok();
            if pending.reply.send(result).is_ok() && accepted {
                self.fabric_issued.insert(
                    LiveNextHop {
                        peer: pending.spec.peer,
                        key: pending.route.address,
                    },
                    binding,
                );
                let branch = ReceiveBranch {
                    peer: pending.spec.peer,
                    fabric: Some(LiveRouteAck {
                        room: pending.spec.room,
                        publication: pending.spec.publication,
                        incarnation: pending.spec.incarnation,
                        sequence_anchor: pending.spec.sequence_anchor,
                        major: pending.spec.major,
                        revision: pending.spec.revision,
                        emission: pending.spec.emission,
                        key: pending.route.address,
                    }),
                    live: pending.spec.addresses.len(),
                    addresses: pending.spec.addresses,
                    evidence: self.node.map(|local| RelayEvidence {
                        local,
                        peer: self.peer_ids.get(&pending.spec.peer).copied(),
                        link: pending.spec.peer,
                        label: pending.route.address,
                        project: pending.spec.room.project,
                        publication: pending.spec.publication,
                        reverse: false,
                    }),
                };
                branch.record("relay_installed", branch.live);
                self.receives.insert(binding, branch);
            } else {
                self.control.unbind(binding);
            }
        }
        self.retirements.retain_mut(|(bindings, ack, done)| {
            let retired =
                *done || !matches!(ack.try_recv(), Err(oneshot::error::TryRecvError::Empty));
            if retired {
                for action in bindings {
                    match *action {
                        Retirement::Binding(binding) => self.control.unbind(binding),
                        Retirement::ReceivePort(binding, port) => {
                            self.control.unbind_receive_port(binding, port);
                            if let Some(branch) = self.receives.get_mut(&binding) {
                                branch.live = branch.live.checked_sub(1).unwrap_or_else(|| {
                                    pulsebeam_runtime::fatal!(
                                        "receive branch lost live placement accounting"
                                    )
                                });
                                if branch.live == 0 {
                                    branch.record("relay_removed", 0);
                                    self.receives.remove(&binding);
                                } else {
                                    branch.record("relay_changed", branch.live);
                                }
                            }
                        }
                    }
                }
            }
            !retired
        });
    }
}

#[cfg(all(test, feature = "sim"))]
#[path = "link_simulation.rs"]
mod simulation;

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_link::{LinkTime, SendErrorKind};

    #[test]
    fn network_provisioning_requires_a_host_and_rolls_back_full_host_admission() {
        use pulsebeam_link::{LinkError, NetworkClass, NetworkPeer, PeerRole, SecretCredential};
        let credential = SecretCredential::generate().unwrap();
        let peer = || NetworkPeer {
            endpoint: "127.0.0.1:17001".parse().unwrap(),
            expected_peer: credential.fingerprint(),
            credential: credential.clone(),
            role: PeerRole::Initiator,
            class: NetworkClass::Lan,
        };
        let mut control = LinkControl::new(|| LinkTime::from_nanos(0)).unwrap();
        let (port, _rx) = control.create_port().unwrap();
        let (second_port, _second_rx) = control.create_port().unwrap();
        let mut links = Links::new(control, vec![port, second_port], Instant::now());
        assert_eq!(links.open_peer(peer()), Err(LinkError::Closed));
        let (sender, mut installs) = pulsebeam_queue::bounded(std::num::NonZeroUsize::MIN);
        links.set_host(sender);
        let first = links.open_peer(peer()).unwrap();
        assert_eq!(links.open_peer(peer()), Err(LinkError::AlreadyOpen));
        links.close_peer(first);
        // The closed driver's pending host command still occupies the one slot.
        assert_eq!(links.open_peer(peer()), Err(LinkError::Capacity));
        assert!(links.peers.is_empty());
        drop(installs.try_recv().unwrap());
        let replacement = links.open_peer(peer()).unwrap();
        assert_ne!(first, replacement);

        let address =
            NodeRouteAddress::new(crate::route::RouteId::new(crate::id::ShardId::new(0), 1), 1);
        assert_eq!(
            links.bind_receive(replacement, vec![address]),
            Err(pulsebeam_link::BindError::Closed)
        );
        let track = crate::entity::ConnectionId::from_bytes([5; 16])
            .fixture_publication(crate::entity::TrackKind::Video, "network-source");
        let action = TrackRouteAction::Reverse {
            publication_id: track,
        };
        let (installed, wait) = oneshot::channel();
        links.install_after(address, action, None, wait);
        assert!(links.destinations(track, None).is_empty());
        assert_eq!(
            links.bind_receive(replacement, vec![address]),
            Err(pulsebeam_link::BindError::Closed)
        );
        installed.send(()).unwrap();
        let receive = links.bind_receive(replacement, vec![address]).unwrap();
        assert!(links.receives.contains_key(&receive.binding));
        let other_address =
            NodeRouteAddress::new(crate::route::RouteId::new(ShardId::new(1), 2), 1);
        let other_track = crate::entity::ConnectionId::from_bytes([5; 16])
            .fixture_publication(crate::entity::TrackKind::Video, "other-source");
        for other_action in [
            TrackRouteAction::Forward {
                publication_id: track,
            },
            TrackRouteAction::Reverse {
                publication_id: other_track,
            },
        ] {
            let (ack, wait) = oneshot::channel();
            links.install_after(other_address, other_action, None, wait);
            ack.send(()).unwrap();
            assert_eq!(
                links.bind_receive(replacement, vec![address, other_address]),
                Err(pulsebeam_link::BindError::Closed)
            );
            let (ack, wait) = oneshot::channel();
            links.retire_after(other_address, |_| wait);
            let _ = ack.send(());
            links.reap();
        }

        assert_eq!(
            links.destinations(track, None),
            vec![crate::link::ReceiveDestination {
                address,
                kind: crate::link::ReceiveKind::Reverse,
            }]
        );
        let shard = ShardId::new(0);
        let mut source = TrackPlanUpdate {
            publication_id: track,
            plan: Some(TrackPlan {
                reverse_route: Some(address),
                ..Default::default()
            }),
        };
        assert!(links.prepare_plan(shard, &mut source).is_empty());
        let (reverse_binding, mut reverse_plan) = links
            .bind_reverse(shard, track, replacement, receive.address)
            .unwrap();
        assert!(matches!(
            links.bind_reverse(shard, track, replacement, receive.address),
            Err(pulsebeam_link::BindError::Capacity)
        ));
        assert!(links.prepare_plan(shard, &mut reverse_plan).is_empty());
        let reverse_tx = reverse_plan
            .plan
            .as_ref()
            .unwrap()
            .compiled_network_reverse
            .as_ref()
            .unwrap()
            .tx
            .clone();
        let (_, mut restored) = links.remove_send(reverse_binding).unwrap();
        assert!(links.prepare_plan(shard, &mut restored).is_empty());
        let restored = restored.plan.unwrap();
        assert!(restored.compiled_network_reverse.is_none());
        assert!(restored.compiled_reverse.is_some());
        assert!(restored.compiled_remote.is_empty());
        let (reverse_cutoff, wait) = oneshot::channel();
        links.retire_bindings_after(vec![reverse_binding], wait);
        reverse_cutoff.send(()).unwrap();
        links.reap();
        let reverse_frame = ShardFrame::Reverse {
            address,
            packet: crate::participant::reverse::ReversePacket::keyframe(
                None,
                crate::media::KeyframeRequestKind::Pli,
            ),
        };
        assert_eq!(
            reverse_tx
                .try_send(reverse_frame, LinkTime::from_nanos(u64::MAX))
                .unwrap_err()
                .kind,
            SendErrorKind::Stale
        );
        let (next_binding, mut next_plan) = links
            .bind_reverse(shard, track, replacement, receive.address)
            .unwrap();
        assert!(links.prepare_plan(shard, &mut next_plan).is_empty());
        let tx = next_plan.plan.unwrap().compiled_network_reverse.unwrap().tx;
        let mut withdrawal = TrackPlanUpdate {
            publication_id: track,
            plan: None,
        };
        let bindings = links.prepare_plan(shard, &mut withdrawal);
        assert_eq!(bindings, vec![next_binding]);
        let (cleared, wait) = oneshot::channel();
        links.retire_bindings_after(bindings, wait);
        links.reap();
        let frame = || ShardFrame::Reverse {
            address,
            packet: crate::participant::reverse::ReversePacket::keyframe(
                None,
                crate::media::KeyframeRequestKind::Pli,
            ),
        };
        assert_eq!(
            tx.try_send(frame(), LinkTime::from_nanos(u64::MAX))
                .unwrap_err()
                .kind,
            SendErrorKind::NotReady
        );
        cleared.send(()).unwrap();
        links.reap();
        assert_eq!(
            tx.try_send(frame(), LinkTime::from_nanos(u64::MAX))
                .unwrap_err()
                .kind,
            SendErrorKind::Stale
        );
        assert!(matches!(
            links.bind_reverse(shard, track, replacement, receive.address),
            Err(pulsebeam_link::BindError::Closed)
        ));

        let (retired, wait) = oneshot::channel();
        links.retire_after(address, |_| wait);
        links.reap();
        assert_eq!(
            links.bind_receive(replacement, vec![address]),
            Err(pulsebeam_link::BindError::Closed)
        );
        assert!(links.retirements.iter().any(|(bindings, _, _)| {
            bindings
                .iter()
                .any(|action| action.binding() == receive.binding)
        }));
        retired.send(()).unwrap();
        links.reap();
        assert!(links.retirements.is_empty());
        assert!(links.destinations(track, None).is_empty());
        let (failed, wait) = oneshot::channel();
        links.install_after(address, action, None, wait);
        drop(failed);
        assert_eq!(
            links.bind_receive(replacement, vec![address]),
            Err(pulsebeam_link::BindError::Closed)
        );
        assert!(!links.installed.contains(&address));
        links.close_peer(replacement);
    }

    #[test]
    fn late_installation_ack_cannot_resurrect_a_retired_destination() {
        let control = LinkControl::<ShardFrame>::new(|| LinkTime::from_nanos(0)).unwrap();
        let mut links = Links::new(control, Vec::new(), Instant::now());
        let address = NodeRouteAddress::new(crate::route::RouteId::new(ShardId::new(0), 7), 1);
        let track = crate::entity::ConnectionId::from_bytes([9; 16])
            .fixture_publication(crate::entity::TrackKind::Audio, "retired");
        let (installed, installation) = oneshot::channel();
        links.install_after(
            address,
            TrackRouteAction::Forward {
                publication_id: track,
            },
            None,
            installation,
        );
        assert!(links.contains(address));
        let (retired, retirement) = oneshot::channel();
        links.retire_after(address, |_| retirement);
        // Dropping the obsolete observer cancels the old installation, even if
        // that shard's queued generation applies after retirement was staged.
        assert!(installed.send(()).is_err());
        let _ = retired.send(());
        links.reap();
        assert!(!links.contains(address));
        assert!(!links.installed.contains(&address));
        assert!(links.installations.is_empty());
        assert!(links.destinations(track, None).is_empty());
        assert!(links.route_actions.is_empty());
    }

    #[test]
    fn route_binding_retires_only_after_destination_cutoff_and_fences_queued_work() {
        let mut control = LinkControl::new(|| LinkTime::from_nanos(0)).unwrap();
        let (port, mut rx) = control.create_port().unwrap();
        let mut links = Links::new(control, vec![port], Instant::now());
        let address =
            NodeRouteAddress::new(crate::route::RouteId::new(crate::id::ShardId::new(0), 1), 1);
        let compiled = links.compile(address);
        assert_eq!(compiled.tx, links.compile(address).tx);
        let frame = || ShardFrame::Reverse {
            address,
            packet: crate::participant::reverse::ReversePacket::keyframe(
                None,
                crate::media::KeyframeRequestKind::Pli,
            ),
        };
        let (ack, wait) = oneshot::channel();
        links.retire_after(address, |_| wait);
        links.reap();
        assert!(
            compiled
                .tx
                .try_send(frame(), LinkTime::from_nanos(u64::MAX))
                .is_ok()
        );
        ack.send(()).unwrap();
        links.reap();
        assert!(
            matches!(compiled.tx.try_send(frame(), LinkTime::from_nanos(u64::MAX)), Err(error) if error.kind == SendErrorKind::Stale)
        );
        let replacement = links.compile(address);
        assert_ne!(compiled.tx, replacement.tx);
        assert!(
            replacement
                .tx
                .try_send(frame(), LinkTime::from_nanos(u64::MAX))
                .is_ok()
        );
        let mut batch = Vec::new();
        assert_eq!(rx.try_recv_batch(&mut batch, 2), 1);
        assert_eq!(rx.drop_counts().stale, 1);
    }
}

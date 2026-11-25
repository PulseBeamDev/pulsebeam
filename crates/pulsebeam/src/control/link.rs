use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::task::{Context, Poll};

use pulsebeam_core::identity::NodeId;
use pulsebeam_link::{BindingId, Destination, LinkControl, NetworkId, PortId};
use pulsebeam_runtime::sync::oneshot;
use pulsebeam_runtime::time::Instant;

use crate::route::NodeRouteAddress;
use crate::route::fabric::local::Emission;
use crate::shard::worker::ShardFrame;
use crate::shard_update::{CompiledRoute, TrackPlan, TrackPlanUpdate, TrackRouteAction};
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

struct ReceiveBranch {
    peer: NetworkId,
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
        let peer = peer.into();
        if matches!(peer, pulsebeam_link::PeerConfig::Fabric(_)) {
            return Err(pulsebeam_link::LinkError::UnsupportedProfile);
        }
        self.open_association(peer, None, |control, peer| {
            control
                .open_network(peer, crate::link::ReverseCodec)
                .map(|id| (id, ()))
        })
        .map(|(id, ())| id)
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
        self.receives.retain(|_, branch| {
            if branch.peer != peer {
                return true;
            }
            branch.record("relay_removed", 0);
            false
        });
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

    pub(super) fn accepts_ownership(&self, peer: NetworkId) -> bool {
        self.peers.contains_key(&peer)
            || self
                .wan
                .get(&peer)
                .is_some_and(|wan| wan.carriage == wan::Carriage::Ownership)
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
            live: addresses.len(),
            addresses,
            evidence,
        };
        branch.record("relay_installed", branch.live);
        self.receives.insert(binding, branch);
        Ok(crate::link::ReceiveRoute { binding, address })
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

    pub(super) fn unbind_receive(&mut self, binding: BindingId) {
        let removed = self.receives.remove(&binding);
        let mut owned = removed.is_some();
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
        let obsolete = peer().with_fabric_path(pulsebeam_link::FabricPath {
            udp_payload_limit: 1200,
            additional_overhead: 0,
        });
        assert_eq!(
            links.open_peer(obsolete),
            Err(LinkError::UnsupportedProfile)
        );
        assert!(links.peers.is_empty());
        assert!(installs.try_recv().is_err());
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

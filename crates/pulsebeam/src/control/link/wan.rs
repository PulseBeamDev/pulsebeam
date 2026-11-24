use std::num::NonZeroU32;

use pulsebeam_core::fabric::{
    self,
    wan::{FEEDBACK_SSRC, Window},
};
use pulsebeam_link::{BindError, BindingCutoff, FabricIngress, LinkTx};

use super::*;
use crate::{
    control::controller::peer_ownership::relay::{
        fabric::WanLiveRouteAck, wan_source::WanSourceWindow,
    },
    link::wan_host::{self, ChildInstall, Commands, ReceiveCommand},
    route::fabric::{
        LiveNetworkChild,
        network_repair::{
            Demand, Interest, MAX_REPAIR_LIFETIME, MediaRepairChild, RepairChild, RepairRoute,
        },
    },
};

const RECEIVE_LIMIT: usize = crate::control::controller::peer_ownership::relay::LIMIT;

#[cfg(all(test, target_os = "linux", not(feature = "sim")))]
mod tests;

pub(in crate::control) struct WanLiveReceiveSpec {
    pub live: LiveReceiveSpec,
    pub canonical_ingress: NetworkId,
    pub start: u64,
    pub forms: u8,
    pub expires: Instant,
    pub timing: Option<crate::route::fabric::WindowTiming>,
}

pub(in crate::control) struct RepairSendPermit {
    pub live: pulsebeam_link::NodeRouteKey,
    pub source: NonZeroU64,
    pub position: u64,
    pub window: Window,
}

impl From<crate::control::controller::peer_ownership::fabric_data::WanRepairReady>
    for RepairSendPermit
{
    fn from(
        ready: crate::control::controller::peer_ownership::fabric_data::WanRepairReady,
    ) -> Self {
        Self {
            live: ready.scope.key,
            source: ready.scope.source,
            position: ready.sequence,
            window: ready.window,
        }
    }
}

pub(super) struct WanPeer {
    commands: Commands,
    next_label: u32,
    receives: HashMap<NonZeroU32, Receive>,
}

struct Receive {
    expires: Instant,
    bindings: Vec<Local>,
    window: Window,
    pending: Option<oneshot::Receiver<Result<Window, wan_host::Error>>>,
    live: Option<LiveAdvertisement>,
    repair: Option<ReceiveRepair>,
}

enum ReceiveRepair {
    Data(Box<(Interest, RepairChild)>),
    Media(MediaRepairChild),
}

struct LiveAdvertisement {
    ack: WanLiveRouteAck,
    canonical_ingress: NetworkId,
    children: Vec<LiveNetworkChild>,
    evidence: Option<RelayEvidence>,
}

impl Receive {
    fn record_removal(&self) {
        if self.pending.is_none()
            && let Some(evidence) = self.live.as_ref().and_then(|live| live.evidence)
        {
            evidence.record("relay_removed", 0);
        }
    }

    fn reap(&mut self, now: Instant) -> bool {
        if let Some(report) = &mut self.pending {
            match report.try_recv() {
                Ok(Ok(window)) if window == self.window => {
                    self.pending = None;
                    if let Some(live) = &self.live
                        && let Some(evidence) = live.evidence
                        && now < self.expires
                        && self.bindings.len() == live.children.len()
                        && self.bindings.iter().all(|(_, tx)| tx.is_live())
                    {
                        evidence.record("relay_installed", live.children.len());
                    }
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
                _ => return false,
            }
        }
        true
    }
}

type Local = (BindingId, LinkTx<ShardFrame>);

impl WanPeer {
    fn allocate_label(&mut self) -> Result<NonZeroU32, BindError> {
        let mut label = self.next_label.checked_add(1).ok_or(BindError::Capacity)?;
        if label == FEEDBACK_SSRC {
            label = label.checked_add(1).ok_or(BindError::Capacity)?;
        }
        self.next_label = label;
        NonZeroU32::new(label).ok_or(BindError::Capacity)
    }

    pub(super) fn retire(self, control: &mut LinkControl<ShardFrame>) {
        for receive in self.receives.into_values() {
            receive.record_removal();
            unbind(control, receive.bindings);
        }
    }

    pub(super) fn reap(&mut self, control: &mut LinkControl<ShardFrame>, now: Instant) {
        self.receives.retain(|_, receive| {
            if !receive.reap(now) {
                unbind(control, std::mem::take(&mut receive.bindings));
                return false;
            }
            receive.bindings.retain(|(binding, tx)| {
                if now < receive.expires && tx.is_live() {
                    true
                } else {
                    control.unbind(*binding);
                    false
                }
            });
            if receive.bindings.is_empty() {
                receive.record_removal();
                false
            } else {
                true
            }
        });
    }
}

fn unbind(control: &mut LinkControl<ShardFrame>, bindings: Vec<Local>) {
    for (binding, _) in bindings {
        control.unbind(binding);
    }
}

impl Links {
    pub(in crate::control) fn reserve_wan_peer(
        &mut self,
    ) -> Result<NetworkId, pulsebeam_link::LinkError> {
        self.control.reserve_network_id()
    }

    pub(in crate::control) fn attach_wan_host(
        &mut self,
        commands: Commands,
    ) -> Result<(), BindError> {
        if self.wan.len() >= crate::link::host::PEERS || self.wan.contains_key(&commands.peer()) {
            return Err(BindError::Capacity);
        }
        self.wan.insert(
            commands.peer(),
            WanPeer {
                commands,
                next_label: 0,
                receives: HashMap::new(),
            },
        );
        Ok(())
    }

    pub(in crate::control) fn stage_wan_publisher_transport(
        &mut self,
        peer: NetworkId,
        source: WanSourceWindow,
        ack: WanLiveRouteAck,
        expires: Instant,
        now: Instant,
    ) -> Result<(BindingCutoff, wan_host::PendingTx), BindError> {
        if now >= expires || !source.matches_ack(ack) {
            return Err(BindError::Closed);
        }
        let ssrc = ack.window.ssrc();
        let expected = Window::new(
            source.publisher.emission.profile(),
            source.window_start,
            source.forms(),
            ssrc,
            source.publisher.emission.wire_profile().1,
        )
        .map_err(|_| BindError::UnsupportedProfile)?;
        let spec = pulsebeam_link::wan::WindowSpec {
            authority: source.publisher.incarnation,
            direction: pulsebeam_link::wan::Direction::Send,
            start: source.window_start,
            ssrc,
            purpose: pulsebeam_link::wan::Purpose::Live,
            expires: expires.into_std(),
        };
        self.stage_wan_send(peer, expected, ack.window, spec, None)
    }

    pub(in crate::control) fn is_wan_peer(&self, peer: NetworkId) -> bool {
        self.wan.contains_key(&peer)
    }

    pub(in crate::control) fn wan_transit_input(
        &self,
        upstream: NetworkId,
        source: WanSourceWindow,
        shard: ShardId,
        now: Instant,
    ) -> Option<(LiveNetworkChild, Instant)> {
        self.wan
            .get(&upstream)?
            .receives
            .values()
            .find_map(|receive| {
                let live = receive.live.as_ref()?;
                if receive.pending.is_some()
                    || now >= receive.expires
                    || !source.matches_ack(live.ack)
                {
                    return None;
                }
                live.children.iter().find_map(|child| {
                    let tag = crate::link::delivery_tag(child.destination);
                    (child.destination.shard() == shard
                        && receive
                            .bindings
                            .iter()
                            .any(|(_, tx)| tx.is_live() && tx.local_delivery_tag() == Some(tag)))
                    .then_some((*child, receive.expires))
                })
            })
    }

    /// Bind an already-authorized outgoing branch to the exact live receive
    /// child installed on another association. The ACK cannot grant transit.
    pub(in crate::control) fn stage_wan_relay_transport(
        &mut self,
        peer: NetworkId,
        input: LiveNetworkChild,
        ack: WanLiveRouteAck,
        expires: Instant,
        now: Instant,
    ) -> Result<(BindingCutoff, wan_host::PendingTx), BindError> {
        self.reap();
        let upstream = input.ingress.association;
        let window = input.window.ok_or(BindError::Closed)?;
        let receive = self
            .wan
            .get(&upstream)
            .and_then(|peer| peer.receives.get(&window.ssrc()))
            .filter(|receive| receive.pending.is_none() && receive.window == window)
            .ok_or(BindError::Closed)?;
        let live = receive.live.as_ref().ok_or(BindError::Closed)?;
        if peer == upstream
            || !live.children.contains(&input)
            || ack.room != live.ack.room
            || ack.publication != input.publication
            || ack.incarnation != input.incarnation
            || ack.sequence_anchor != input.sequence_anchor
        {
            return Err(BindError::Closed);
        }
        let expires = expires.min(receive.expires);
        if now >= expires {
            return Err(BindError::Closed);
        }
        let tag = crate::link::delivery_tag(input.destination);
        let parent = receive
            .bindings
            .iter()
            .find_map(|(_, tx)| {
                (tx.is_live() && tx.local_delivery_tag() == Some(tag))
                    .then(|| tx.local_cutoff())
                    .flatten()
            })
            .ok_or(BindError::Closed)?;
        let expected = Window::new(
            window.profile(),
            window.start(),
            window.forms(),
            ack.window.ssrc(),
            window.emission(),
        )
        .map_err(|_| BindError::UnsupportedProfile)?;
        if expected != ack.window {
            return Err(BindError::Closed);
        }
        let spec = pulsebeam_link::wan::WindowSpec {
            authority: input.incarnation,
            direction: pulsebeam_link::wan::Direction::Send,
            start: window.start(),
            ssrc: expected.ssrc(),
            purpose: pulsebeam_link::wan::Purpose::Live,
            expires: expires.into_std(),
        };
        self.stage_wan_send(
            peer,
            expected,
            ack.window,
            spec,
            Some(wan_host::SendParent::Transit {
                cutoff: parent,
                window,
            }),
        )
    }

    pub(in crate::control) fn stage_wan_repair_transport(
        &mut self,
        peer: NetworkId,
        parent: wan_host::ParentSend,
        permit: impl Into<RepairSendPermit>,
        expires: Instant,
        now: Instant,
    ) -> Result<(BindingCutoff, wan_host::PendingTx), BindError> {
        let permit = permit.into();
        if now >= expires
            || !parent.cutoff.is_live()
            || parent.window.ssrc().get().to_be_bytes() != permit.live.to_bytes()
        {
            return Err(BindError::Closed);
        }
        let expected = parent
            .window
            .repair_window(permit.position, permit.window.ssrc())
            .map_err(|_| BindError::UnsupportedProfile)?;
        if expected != permit.window {
            return Err(BindError::Closed);
        }
        let spec = pulsebeam_link::wan::WindowSpec {
            authority: permit.source,
            direction: pulsebeam_link::wan::Direction::Send,
            start: expected.start(),
            ssrc: expected.ssrc(),
            purpose: pulsebeam_link::wan::Purpose::Repair {
                position: permit.position,
            },
            expires: expires.into_std(),
        };
        self.stage_wan_send(
            peer,
            expected,
            permit.window,
            spec,
            Some(wan_host::SendParent::Repair(parent)),
        )
    }

    fn stage_wan_send(
        &mut self,
        peer: NetworkId,
        expected: Window,
        acknowledged: Window,
        spec: pulsebeam_link::wan::WindowSpec,
        parent: Option<wan_host::SendParent>,
    ) -> Result<(BindingCutoff, wan_host::PendingTx), BindError> {
        let commands = &self.wan.get(&peer).ok_or(BindError::Closed)?.commands;
        let cutoff = self.control.reserve_wan_send(peer)?;
        match commands.install_send(cutoff.clone(), expected, acknowledged, spec, parent) {
            Ok(pending) => Ok((cutoff, pending)),
            Err(error) => {
                self.control.unbind(cutoff.id());
                Err(error)
            }
        }
    }

    pub(in crate::control) fn retire_wan_send(&mut self, cutoff: &BindingCutoff) {
        cutoff.fence();
        self.control.unbind(cutoff.id());
    }

    fn bind_wan_locals(&mut self, addresses: &[NodeRouteAddress]) -> Result<Vec<Local>, BindError> {
        let ports = addresses
            .iter()
            .map(|address| {
                self.ports
                    .get(address.shard().index())
                    .copied()
                    .ok_or(BindError::Closed)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut bindings = Vec::with_capacity(ports.len());
        for (&address, port) in addresses.iter().zip(ports) {
            match self.control.bind_send(Destination::Local {
                port,
                tag: crate::link::delivery_tag(address),
            }) {
                Ok(binding) => bindings.push(binding),
                Err(error) => {
                    unbind(&mut self.control, bindings);
                    return Err(error);
                }
            }
        }
        Ok(bindings)
    }

    pub(in crate::control) fn stage_wan_live_receive(
        &mut self,
        now: Instant,
        spec: WanLiveReceiveSpec,
        mut stage_child: impl FnMut(
            LiveNetworkChild,
            BindingCutoff,
        ) -> oneshot::Receiver<Option<LiveNetworkChild>>,
    ) -> Result<Window, BindError> {
        self.reap();
        let live = spec.live;
        if spec.expires <= now
            || live.addresses.is_empty()
            || live.addresses.len() > crate::route::fabric::local::MAX_BRANCHES
            || !self.is_wan_peer(spec.canonical_ingress)
        {
            return Err(BindError::Closed);
        }
        if live.addresses.iter().enumerate().any(|(i, &address)| {
            !self.allows_live_scope(spec.canonical_ingress, live.room, live.publication, address)
                || live
                    .addresses
                    .iter()
                    .take(i)
                    .any(|earlier| *earlier == address)
        }) {
            return Err(BindError::Closed);
        }
        let peer = self.wan.get_mut(&live.peer).ok_or(BindError::Closed)?;
        if peer.receives.len() >= RECEIVE_LIMIT {
            return Err(BindError::Capacity);
        }
        if peer
            .receives
            .values()
            .filter_map(|receive| receive.live.as_ref())
            .any(|existing| {
                existing.ack.room == live.room
                    && existing.ack.publication == live.publication
                    && existing.ack.incarnation == live.incarnation
                    && existing.ack.window.start() == spec.start
            })
        {
            return Err(BindError::Closed);
        }
        let ssrc = peer.allocate_label()?;
        let window = Window::new(
            live.emission.profile(),
            spec.start,
            spec.forms,
            ssrc,
            live.emission.wire_profile().1,
        )
        .map_err(|_| BindError::UnsupportedProfile)?;
        let bindings = self.bind_wan_locals(&live.addresses)?;
        let built = live
            .addresses
            .iter()
            .zip(&bindings)
            .map(|(&destination, (_, tx))| {
                let child = LiveNetworkChild {
                    ingress: FabricIngress {
                        association: live.peer,
                        key: pulsebeam_link::NodeRouteKey::from_bytes(ssrc.get().to_be_bytes()),
                        purpose: pulsebeam_link::FabricPurpose::Live,
                    },
                    destination,
                    publication: live.publication,
                    incarnation: live.incarnation,
                    sequence_anchor: live.sequence_anchor,
                    major: fabric::MAJOR,
                    revision: fabric::REGISTRY_REVISION,
                    emission: live.emission,
                    window: Some(window),
                };
                let cutoff = tx.local_cutoff().ok_or(BindError::Closed)?;
                Ok(ChildInstall {
                    child,
                    tx: tx.clone(),
                    receipt: stage_child(child, cutoff),
                })
            })
            .collect::<Result<Vec<_>, BindError>>();
        let children = match built {
            Ok(children) => children,
            Err(error) => {
                unbind(&mut self.control, bindings);
                return Err(error);
            }
        };
        let live_children = children.iter().map(|install| install.child).collect();
        let (report, installed) = oneshot::channel();
        let Some(peer) = self.wan.get_mut(&live.peer) else {
            unbind(&mut self.control, bindings);
            return Err(BindError::Closed);
        };
        if let Err(error) = peer.commands.try_send(ReceiveCommand::Live {
            expires: spec.expires,
            timing: spec.timing,
            children,
            report,
        }) {
            unbind(&mut self.control, bindings);
            return Err(error);
        }
        let ack = WanLiveRouteAck {
            room: live.room,
            publication: live.publication,
            incarnation: live.incarnation,
            sequence_anchor: live.sequence_anchor,
            window,
        };
        peer.receives.insert(
            ssrc,
            Receive {
                expires: spec.expires,
                bindings,
                window,
                pending: Some(installed),
                live: Some(LiveAdvertisement {
                    ack,
                    canonical_ingress: spec.canonical_ingress,
                    children: live_children,
                    evidence: self.node.map(|local| RelayEvidence {
                        local,
                        peer: self.peer_ids.get(&live.peer).copied(),
                        link: live.peer,
                        label: pulsebeam_link::NodeRouteKey::from_bytes(ssrc.get().to_be_bytes()),
                        project: live.room.project,
                        publication: live.publication,
                        reverse: false,
                    }),
                }),
                repair: None,
            },
        );
        Ok(window)
    }

    pub(in crate::control) fn accept_wan_retirements(
        &mut self,
        peer: NetworkId,
        labels: &[[u8; 4]],
    ) -> bool {
        let Some(peer) = self.wan.get_mut(&peer) else {
            return false;
        };
        let mut changed = false;
        for label in labels {
            let Some(label) = NonZeroU32::new(u32::from_be_bytes(*label)) else {
                continue;
            };
            if peer
                .receives
                .get(&label)
                .is_some_and(|receive| receive.live.is_some())
                && let Some(receive) = peer.receives.remove(&label)
            {
                receive.record_removal();
                unbind(&mut self.control, receive.bindings);
                changed = true;
            }
        }
        changed
    }

    pub(in crate::control) fn retain_wan_live_receives(
        &mut self,
        peer: NetworkId,
        mut keep: impl FnMut(WanLiveRouteAck, NetworkId, &[LiveNetworkChild]) -> bool,
    ) {
        let Some(peer) = self.wan.get_mut(&peer) else {
            return;
        };
        peer.receives.retain(|_, receive| {
            let Some(live) = &receive.live else {
                return true;
            };
            if receive.bindings.len() == live.children.len()
                && keep(live.ack, live.canonical_ingress, &live.children)
            {
                true
            } else {
                receive.record_removal();
                unbind(&mut self.control, std::mem::take(&mut receive.bindings));
                false
            }
        });
    }

    fn wan_data_receive(
        &self,
        peer: NetworkId,
        scope: crate::control::controller::peer_ownership::fabric_data::Scope,
        now: Instant,
    ) -> Option<(&Receive, &LiveAdvertisement)> {
        let label = NonZeroU32::new(u32::from_be_bytes(scope.key.to_bytes()))?;
        let receive = self.wan.get(&peer)?.receives.get(&label)?;
        let live = receive.live.as_ref()?;
        (receive.pending.is_none()
            && now < receive.expires
            && live.ack.incarnation == scope.source
            && live.ack.window.profile() == fabric::Profile::Data
            && receive.bindings.len() == live.children.len()
            && receive.bindings.iter().all(|(_, tx)| tx.is_live())
            && live.children.iter().all(|child| {
                self.allows_live_scope(
                    live.canonical_ingress,
                    live.ack.room,
                    live.ack.publication,
                    child.destination,
                )
            }))
        .then_some((receive, live))
    }

    pub(in crate::control) fn wan_data_receive_deadline(
        &self,
        peer: NetworkId,
        scope: crate::control::controller::peer_ownership::fabric_data::Scope,
        now: Instant,
    ) -> Option<Instant> {
        self.wan_data_receive(peer, scope, now)
            .map(|(receive, _)| receive.expires)
    }

    pub(in crate::control) fn wan_data_expectations(
        &self,
        peer: NetworkId,
        frame: crate::control::controller::peer_ownership::fabric_data::Frame,
        now: Instant,
    ) -> Option<Vec<crate::route::fabric::NetworkDataExpectation>> {
        let (receive, live) = self.wan_data_receive(peer, frame.scope, now)?;
        let expires = now.checked_add(frame.lease()?)?.min(receive.expires);
        Some(
            live.children
                .iter()
                .map(|&parent| crate::route::fabric::NetworkDataExpectation {
                    parent,
                    expires,
                    unit: frame.unit,
                })
                .collect(),
        )
    }

    pub(in crate::control) fn wan_offers(&mut self, network: NetworkId) -> Box<[WanLiveRouteAck]> {
        self.reap();
        let Some(peer) = self.wan.get(&network) else {
            return Box::default();
        };
        let mut offers = peer
            .receives
            .values()
            .filter_map(|receive| {
                let live = receive.live.as_ref()?;
                (receive.pending.is_none()
                    && receive.bindings.len() == live.children.len()
                    && live.children.iter().all(|child| {
                        self.allows_live_scope(
                            live.canonical_ingress,
                            live.ack.room,
                            live.ack.publication,
                            child.destination,
                        )
                    }))
                .then_some(live.ack)
            })
            .collect::<Vec<_>>();
        offers.sort_unstable_by_key(|ack| ack.window.ssrc());
        offers.into_boxed_slice()
    }

    pub(in crate::control) fn stage_wan_repair_receive(
        &mut self,
        now: Instant,
        interest: Interest,
        sequence: u64,
        stage_child: impl FnOnce(RepairChild, BindingCutoff) -> oneshot::Receiver<Option<RepairChild>>,
    ) -> Result<RepairChild, BindError> {
        self.reap();
        let parent = interest.parent;
        let live = parent.window.ok_or(BindError::UnsupportedProfile)?;
        if !self.allows_wan_repair_interest(interest, now) {
            return Err(BindError::Closed);
        }
        if now >= interest.expires
            || now
                .checked_add(MAX_REPAIR_LIFETIME)
                .is_none_or(|maximum| interest.expires > maximum)
        {
            return Err(BindError::Closed);
        }
        let peer = self
            .wan
            .get_mut(&parent.ingress.association)
            .ok_or(BindError::Closed)?;
        if peer.receives.len() >= RECEIVE_LIMIT {
            return Err(BindError::Capacity);
        }
        if peer.receives.values().any(|receive| {
            matches!(receive.repair.as_ref(), Some(ReceiveRepair::Data(data)) if data.0 == interest)
        }) {
            return Err(BindError::Closed);
        }
        let ssrc = peer.allocate_label()?;
        let window = live
            .repair_window(sequence, ssrc)
            .map_err(|_| BindError::UnsupportedProfile)?;
        let child = RepairChild {
            parent: parent.ingress,
            route: RepairRoute {
                ingress: FabricIngress {
                    association: parent.ingress.association,
                    key: pulsebeam_link::NodeRouteKey::from_bytes(ssrc.get().to_be_bytes()),
                    purpose: pulsebeam_link::FabricPurpose::RepairOnly,
                },
                publication: parent.publication,
                source: parent.incarnation,
                branch: interest.branch,
                major: parent.major,
                revision: parent.revision,
                emission: parent.emission,
            },
            destination: parent.destination,
            receiver: interest.receiver,
            demand: Demand {
                request: interest.request,
                sequence,
                expires: interest.expires,
            },
            window: Some(window),
        };
        let bindings = self.bind_wan_locals(&[child.destination])?;
        let Some((tx, cutoff)) = bindings
            .first()
            .and_then(|(_, tx)| Some((tx, tx.local_cutoff()?)))
        else {
            unbind(&mut self.control, bindings);
            return Err(BindError::Closed);
        };
        let install = ChildInstall {
            child,
            tx: tx.clone(),
            receipt: stage_child(child, cutoff),
        };
        let (report, installed) = oneshot::channel();
        let Some(peer) = self.wan.get_mut(&parent.ingress.association) else {
            unbind(&mut self.control, bindings);
            return Err(BindError::Closed);
        };
        if let Err(error) = peer.commands.try_send(ReceiveCommand::Repair {
            parent,
            install: Box::new(install),
            report,
        }) {
            unbind(&mut self.control, bindings);
            return Err(error);
        }
        peer.receives.insert(
            ssrc,
            Receive {
                expires: interest.expires,
                bindings,
                window,
                pending: Some(installed),
                live: None,
                repair: Some(ReceiveRepair::Data(Box::new((interest, child)))),
            },
        );
        Ok(child)
    }

    fn wan_repair_parent_deadline(
        &self,
        parent: LiveNetworkChild,
        now: Instant,
    ) -> Option<Instant> {
        let window = parent.window?;
        let receive = self
            .wan
            .get(&parent.ingress.association)?
            .receives
            .get(&window.ssrc())?;
        let live = receive.live.as_ref()?;
        (now < receive.expires
            && receive.pending.is_none()
            && live.children.contains(&parent)
            && self.allows_live_scope(
                live.canonical_ingress,
                live.ack.room,
                parent.publication,
                parent.destination,
            )
            && receive.bindings.iter().any(|(_, tx)| {
                tx.is_live()
                    && tx.local_delivery_tag()
                        == Some(crate::link::delivery_tag(parent.destination))
            }))
        .then_some(receive.expires)
    }

    pub(in crate::control) fn allows_wan_repair_interest(
        &self,
        interest: Interest,
        now: Instant,
    ) -> bool {
        now < interest.expires
            && self
                .wan_repair_parent_deadline(interest.parent, now)
                .is_some_and(|deadline| interest.expires <= deadline)
    }

    pub(in crate::control) fn stage_wan_media_repair_receive(
        &mut self,
        now: Instant,
        parent: LiveNetworkChild,
        position: u64,
        expires: Instant,
        stage_child: impl FnOnce(
            MediaRepairChild,
            BindingCutoff,
        ) -> oneshot::Receiver<Option<MediaRepairChild>>,
    ) -> Result<MediaRepairChild, BindError> {
        self.reap();
        let live = parent.window.ok_or(BindError::UnsupportedProfile)?;
        if live.profile() == fabric::Profile::Data || !live.contains(position) {
            return Err(BindError::UnsupportedProfile);
        }
        let expires = self
            .wan_repair_parent_deadline(parent, now)
            .ok_or(BindError::Closed)?
            .min(
                now.checked_add(MAX_REPAIR_LIFETIME)
                    .ok_or(BindError::Closed)?,
            )
            .min(expires);
        if now >= expires {
            return Err(BindError::Closed);
        }
        let peer = self
            .wan
            .get_mut(&parent.ingress.association)
            .ok_or(BindError::Closed)?;
        if peer.receives.len() >= RECEIVE_LIMIT {
            return Err(BindError::Capacity);
        }
        if peer.receives.values().any(|receive| matches!(receive.repair.as_ref(),
            Some(ReceiveRepair::Media(child)) if child.parent == parent && child.position == position))
        {
            return Err(BindError::Closed);
        }
        let label = peer.allocate_label()?;
        let window = live
            .repair_window(position, label)
            .map_err(|_| BindError::UnsupportedProfile)?;
        let child = MediaRepairChild {
            parent,
            window,
            position,
            expires,
        };
        let bindings = self.bind_wan_locals(&[parent.destination])?;
        let Some((tx, cutoff)) = bindings
            .first()
            .and_then(|(_, tx)| Some((tx, tx.local_cutoff()?)))
        else {
            unbind(&mut self.control, bindings);
            return Err(BindError::Closed);
        };
        let install = ChildInstall {
            child,
            tx: tx.clone(),
            receipt: stage_child(child, cutoff),
        };
        let (report, installed) = oneshot::channel();
        let Some(peer) = self.wan.get_mut(&parent.ingress.association) else {
            unbind(&mut self.control, bindings);
            return Err(BindError::Closed);
        };
        if let Err(error) = peer.commands.try_send(ReceiveCommand::MediaRepair {
            install: Box::new(install),
            report,
        }) {
            unbind(&mut self.control, bindings);
            return Err(error);
        }
        peer.receives.insert(
            label,
            Receive {
                expires,
                bindings,
                window,
                pending: Some(installed),
                live: None,
                repair: Some(ReceiveRepair::Media(child)),
            },
        );
        Ok(child)
    }

    pub(in crate::control) fn wan_media_repair_receive_state(
        &self,
        child: MediaRepairChild,
        now: Instant,
    ) -> Poll<Option<Window>> {
        let Some(receive) = self
            .wan
            .get(&child.parent.ingress.association)
            .and_then(|peer| peer.receives.get(&child.window.ssrc()))
            .filter(|receive| {
                matches!(receive.repair.as_ref(), Some(ReceiveRepair::Media(expected)) if *expected == child)
                    && now < receive.expires
                    && receive.bindings.iter().any(|(_, tx)| tx.is_live())
            })
        else {
            return Poll::Ready(None);
        };
        if self
            .wan_repair_parent_deadline(child.parent, now)
            .is_none_or(|deadline| child.expires > deadline)
        {
            return Poll::Ready(None);
        }
        if receive.pending.is_some() {
            Poll::Pending
        } else {
            Poll::Ready(Some(receive.window))
        }
    }

    pub(in crate::control) fn wan_repair_receive_state(
        &self,
        interest: Interest,
        child: RepairChild,
        now: Instant,
    ) -> Poll<Option<Window>> {
        if !self.allows_wan_repair_interest(interest, now) {
            return Poll::Ready(None);
        }
        let route = child
            .window
            .and_then(|window| {
                self.wan
                    .get(&interest.parent.ingress.association)?
                    .receives
                    .get(&window.ssrc())
            })
            .filter(|receive| {
                matches!(receive.repair.as_ref(), Some(ReceiveRepair::Data(data)) if **data == (interest, child))
                    && now < receive.expires
                    && receive.bindings.iter().any(|(_, tx)| tx.is_live())
            });
        match route {
            Some(receive) if receive.pending.is_some() => Poll::Pending,
            Some(receive) => Poll::Ready(Some(receive.window)),
            None => Poll::Ready(None),
        }
    }

    pub(in crate::control) fn cancel_wan_media_repair(&mut self, child: MediaRepairChild) {
        let Some(peer) = self.wan.get_mut(&child.parent.ingress.association) else {
            return;
        };
        let label = child.window.ssrc();
        if peer.receives.get(&label).is_some_and(|receive| {
            matches!(receive.repair.as_ref(), Some(ReceiveRepair::Media(expected)) if *expected == child)
        }) && let Some(receive) = peer.receives.remove(&label) {
            unbind(&mut self.control, receive.bindings);
        }
    }

    pub(in crate::control) fn cancel_wan_repair(&mut self, interest: Interest) {
        let Some(peer) = self.wan.get_mut(&interest.parent.ingress.association) else {
            return;
        };
        if let Some(label) = peer.receives.iter().find_map(|(&label, receive)| {
            matches!(receive.repair.as_ref(), Some(ReceiveRepair::Data(data)) if data.0 == interest).then_some(label)
        }) && let Some(receive) = peer.receives.remove(&label)
        {
            unbind(&mut self.control, receive.bindings);
        }
    }
}

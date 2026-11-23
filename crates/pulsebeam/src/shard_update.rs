#![deny(clippy::arithmetic_side_effects)]
#![deny(clippy::manual_find, clippy::manual_flatten)]

use std::collections::{HashSet, VecDeque};

use crate::entity::{ConnectionId, PublicationKey};
use crate::id::ShardId;
use crate::route::fabric::{
    LiveNetworkChild, LiveRelayChange, NetworkDataExpectation, network_repair::RepairChild,
};
use crate::route::{NodeRouteAddress, NodeTransportAddress};
use pulsebeam_queue as mailbox;
use pulsebeam_runtime::sync::oneshot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompiledRoute {
    pub address: NodeRouteAddress,
    pub tx: pulsebeam_link::LinkTx<crate::shard::worker::ShardFrame>,
    pub clock_origin: pulsebeam_runtime::time::Instant,
}

impl CompiledRoute {
    pub(crate) fn targets_local_route(&self) -> bool {
        self.tx.local_delivery_tag() == Some(crate::link::delivery_tag(self.address))
    }

    pub(crate) fn offer_fabric(&self, offer: crate::route::fabric::ShardOffer) -> bool {
        offer.binding.destination == self.address
            && self.targets_local_route()
            && crate::shard::worker::send_link_frame(
                &self.tx,
                self.clock_origin,
                crate::shard::worker::ShardFrame::Fabric(offer),
            )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TrackPlan {
    pub local: Vec<ConnectionId>,
    pub remote: Vec<NodeRouteAddress>,
    pub wan_transit: bool,
    pub reverse_route: Option<NodeRouteAddress>,
    pub compiled_remote: Vec<CompiledRoute>,
    pub compiled_reverse: Option<CompiledRoute>,
    pub compiled_network_reverse: Option<CompiledRoute>,
}

impl TrackPlan {
    pub(crate) fn new(
        local: impl IntoIterator<Item = ConnectionId>,
        remote: impl IntoIterator<Item = NodeRouteAddress>,
        reverse_route: Option<NodeRouteAddress>,
    ) -> Self {
        Self {
            local: unique(local, "local"),
            remote: unique(remote, "remote"),
            wan_transit: false,
            reverse_route,
            compiled_remote: Vec::new(),
            compiled_reverse: None,
            compiled_network_reverse: None,
        }
    }

    pub(crate) fn is_valid(&self) -> bool {
        fn distinct<K: Copy + Eq + std::hash::Hash>(values: &[K]) -> bool {
            let mut seen = HashSet::with_capacity(values.len());
            values.iter().copied().all(|value| seen.insert(value))
        }

        let mut remote_shards = HashSet::with_capacity(self.remote.len());
        distinct(&self.local)
            && self
                .remote
                .iter()
                .all(|address| remote_shards.insert(address.shard()))
    }
}

fn unique<K>(values: impl IntoIterator<Item = K>, name: &str) -> Vec<K>
where
    K: Copy + Eq + std::hash::Hash + std::fmt::Debug,
{
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        if !seen.insert(value) {
            debug_assert!(false, "a track plan cannot contain duplicate {name} values");
            continue;
        }
        result.push(value);
    }
    result
}

#[derive(Debug, Clone)]
pub(crate) struct TrackPlanUpdate {
    pub publication_id: PublicationKey,
    pub plan: Option<TrackPlan>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TrackRouteAction {
    Forward { publication_id: PublicationKey },
    Reverse { publication_id: PublicationKey },
}

impl TrackRouteAction {
    pub(crate) fn publication_id(self) -> PublicationKey {
        match self {
            Self::Forward { publication_id } | Self::Reverse { publication_id } => publication_id,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TrackDescriptor {
    pub origin: ConnectionId,
    pub kind: crate::entity::TrackKind,
    pub encodings: Vec<crate::track::SourceEncoding>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TrackRuntime {
    pub descriptor: Option<TrackDescriptor>,
    pub publisher: Option<ConnectionId>,
    pub publisher_effect: Option<crate::participant::ParticipantEffect>,
}

#[allow(
    clippy::large_enum_variant,
    reason = "shard updates are control-plane messages and runtime state stays inline"
)]
#[derive(Debug, Clone)]
pub(crate) enum ShardUpdateOp {
    InstallRoute {
        address: NodeRouteAddress,
        action: TrackRouteAction,
    },
    InstallShardFabricSource {
        binding: crate::route::fabric::ShardBinding,
        encoding: Option<crate::route::fabric::RtpSourceEncoding>,
        emission: crate::route::fabric::local::Emission,
    },
    BindShardFabricDestination {
        binding: crate::route::fabric::ShardBinding,
    },
    SetShardDataReceiver {
        ready: crate::route::fabric::ShardDataReady,
        enabled: bool,
    },
    RetireRoute {
        address: NodeRouteAddress,
    },
    RetireTransport {
        address: NodeTransportAddress,
    },
    RemoveParticipant {
        connection: ConnectionId,
        address: NodeTransportAddress,
    },
    InsertTrackRuntime {
        publication_id: PublicationKey,
        runtime: TrackRuntime,
    },
    RemoveTrackRuntime {
        publication_id: PublicationKey,
    },
    Placeholder,
}

#[derive(Debug)]
pub(crate) struct InstallNetworkChild<T> {
    pub child: T,
    pub lifetime: pulsebeam_link::BindingCutoff,
    pub result: oneshot::Sender<Option<T>>,
}

#[derive(Debug)]
pub(crate) struct ObserveNetworkDataExpectation {
    pub notice: NetworkDataExpectation,
    pub result: oneshot::Sender<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PublisherAnchor {
    pub publication: PublicationKey,
    pub connection: ConnectionId,
    pub incarnation: std::num::NonZeroU64,
    pub emission: crate::route::fabric::local::Emission,
}

impl PublisherAnchor {
    pub(crate) fn data_publisher(self) -> Option<crate::route::fabric::DataPublisher> {
        (self.emission == crate::route::fabric::local::Emission::DATA).then_some(
            crate::route::fabric::DataPublisher {
                publication: self.publication,
                connection: self.connection,
                incarnation: self.incarnation,
            },
        )
    }
}

impl From<crate::route::fabric::DataPublisher> for PublisherAnchor {
    fn from(publisher: crate::route::fabric::DataPublisher) -> Self {
        Self {
            publication: publisher.publication,
            connection: publisher.connection,
            incarnation: publisher.incarnation,
            emission: crate::route::fabric::local::Emission::DATA,
        }
    }
}

#[derive(Debug)]
pub(crate) struct SamplePublisherAnchor {
    pub publisher: PublisherAnchor,
    pub result: oneshot::Sender<Option<u64>>,
}

#[derive(Debug)]
pub(crate) struct LocateRelayRepair {
    pub lookup: crate::route::fabric::relay::PeerRepairLookup,
    pub result: oneshot::Sender<Option<crate::route::fabric::relay::LocatedRepair>>,
}

#[derive(Debug)]
pub(crate) struct UpdatePublisherSend {
    pub change: crate::route::fabric::PublisherSendChange,
    pub result: oneshot::Sender<bool>,
}

#[derive(Debug)]
pub(crate) struct UpdateLiveRelay {
    pub change: LiveRelayChange,
    pub result: oneshot::Sender<bool>,
}

#[derive(Debug)]
pub(crate) struct ShardUpdate {
    pub shard: ShardId,
    pub generation: u64,
    pub participant_effects: Vec<(ConnectionId, crate::participant::ParticipantEffect)>,
    pub lifecycle: Vec<ShardUpdateOp>,
    pub plans: Vec<TrackPlanUpdate>,
    pub fabric_children: Vec<InstallNetworkChild<LiveNetworkChild>>,
    pub repair_children: Vec<InstallNetworkChild<RepairChild>>,
    pub media_repair_children:
        Vec<InstallNetworkChild<crate::route::fabric::network_repair::MediaRepairChild>>,
    pub data_expectations: Vec<ObserveNetworkDataExpectation>,
    pub relay_observations: Vec<crate::route::fabric::relay::RelayObservation>,
    pub publisher_observations: Vec<crate::route::fabric::PublisherObservation>,
    pub source_anchors: Vec<SamplePublisherAnchor>,
    pub repair_lookups: Vec<LocateRelayRepair>,
    pub repair_handoffs: Vec<crate::route::fabric::relay::PeerRepairHandoff>,
    pub repair_unavailable: Vec<crate::route::fabric::network_repair::Interest>,
    pub fabric_relays: Vec<UpdateLiveRelay>,
    pub publisher_sends: Vec<UpdatePublisherSend>,
    pub cutoff_bindings: Vec<pulsebeam_link::BindingCutoff>,
    pub cutoff_acks: Vec<pulsebeam_runtime::sync::oneshot::Sender<()>>,
}

impl ShardUpdate {
    fn new(shard: ShardId, generation: u64) -> Self {
        Self {
            shard,
            generation,
            participant_effects: Vec::new(),
            lifecycle: Vec::new(),
            plans: Vec::new(),
            fabric_children: Vec::new(),
            repair_children: Vec::new(),
            media_repair_children: Vec::new(),
            data_expectations: Vec::new(),
            relay_observations: Vec::new(),
            publisher_observations: Vec::new(),
            source_anchors: Vec::new(),
            repair_lookups: Vec::new(),
            repair_handoffs: Vec::new(),
            repair_unavailable: Vec::new(),
            fabric_relays: Vec::new(),
            publisher_sends: Vec::new(),
            cutoff_bindings: Vec::new(),
            cutoff_acks: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.participant_effects.is_empty()
            && self.lifecycle.is_empty()
            && self.plans.is_empty()
            && self.fabric_children.is_empty()
            && self.repair_children.is_empty()
            && self.media_repair_children.is_empty()
            && self.data_expectations.is_empty()
            && self.relay_observations.is_empty()
            && self.publisher_observations.is_empty()
            && self.source_anchors.is_empty()
            && self.repair_lookups.is_empty()
            && self.repair_handoffs.is_empty()
            && self.repair_unavailable.is_empty()
            && self.fabric_relays.is_empty()
            && self.publisher_sends.is_empty()
            && self.cutoff_bindings.is_empty()
            && self.cutoff_acks.is_empty()
    }

    pub(crate) fn validate_for(&self, shard: ShardId, current_revision: u64) -> bool {
        self.shard == shard
            && self.generation > current_revision
            && self.lifecycle.iter().all(|op| op.is_owned_by(shard))
            && self
                .fabric_children
                .iter()
                .all(|install| install.child.destination.shard() == shard)
            && self
                .repair_children
                .iter()
                .all(|install| install.child.destination.shard() == shard)
            && self
                .media_repair_children
                .iter()
                .all(|install| install.child.parent.destination.shard() == shard)
            && self
                .data_expectations
                .iter()
                .all(|record| record.notice.parent.destination.shard() == shard)
            && self
                .relay_observations
                .iter()
                .all(|record| record.route.input.destination.shard() == shard)
            && self
                .publisher_observations
                .iter()
                .all(|observation| observation.route.shard == shard)
            && self
                .repair_lookups
                .iter()
                .all(|record| record.lookup.shard() == shard)
            && self
                .repair_handoffs
                .iter()
                .all(|handoff| handoff.located.lookup.shard() == shard)
            && self
                .repair_unavailable
                .iter()
                .all(|interest| interest.parent.destination.shard() == shard)
            && self
                .fabric_relays
                .iter()
                .all(|update| update.change.route().input.destination.shard() == shard)
            && self
                .publisher_sends
                .iter()
                .all(|update| update.change.route().shard == shard)
    }
}

pub(crate) struct ShardUpdateWriter {
    shard: ShardId,
    tx: mailbox::Sender<Box<ShardUpdate>>,
    staged: Option<Box<ShardUpdate>>,
    backlog: VecDeque<Box<ShardUpdate>>,
    closed: bool,
}

impl ShardUpdateWriter {
    pub(crate) fn is_closed(&self) -> bool {
        self.closed
    }

    pub(crate) fn stage_live_relay(
        &mut self,
        generation: u64,
        change: LiveRelayChange,
    ) -> oneshot::Receiver<bool> {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation || change.route().input.destination.shard() != self.shard
        {
            pulsebeam_runtime::fatal!("a Fabric relay update must target its shard generation");
        }
        let (result, receipt) = oneshot::channel();
        commit
            .fabric_relays
            .push(UpdateLiveRelay { change, result });
        receipt
    }

    pub(crate) fn stage_live_network_child(
        &mut self,
        generation: u64,
        child: LiveNetworkChild,
        lifetime: pulsebeam_link::BindingCutoff,
    ) -> oneshot::Receiver<Option<LiveNetworkChild>> {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation || child.destination.shard() != self.shard {
            pulsebeam_runtime::fatal!("a Fabric child must be installed at its shard generation");
        }
        let (result, receipt) = oneshot::channel();
        commit.fabric_children.push(InstallNetworkChild {
            child,
            lifetime,
            result,
        });
        receipt
    }

    pub(crate) fn stage_repair_network_child(
        &mut self,
        generation: u64,
        child: RepairChild,
        lifetime: pulsebeam_link::BindingCutoff,
    ) -> oneshot::Receiver<Option<RepairChild>> {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation || child.destination.shard() != self.shard {
            pulsebeam_runtime::fatal!(
                "a Fabric repair child must be installed at its shard generation"
            );
        }
        let (result, receipt) = oneshot::channel();
        commit.repair_children.push(InstallNetworkChild {
            child,
            lifetime,
            result,
        });
        receipt
    }

    pub(crate) fn stage_media_repair_child(
        &mut self,
        generation: u64,
        child: crate::route::fabric::network_repair::MediaRepairChild,
        lifetime: pulsebeam_link::BindingCutoff,
    ) -> oneshot::Receiver<Option<crate::route::fabric::network_repair::MediaRepairChild>> {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation || child.parent.destination.shard() != self.shard {
            pulsebeam_runtime::fatal!("a WAN media repair child must target its shard generation");
        }
        let (result, receipt) = oneshot::channel();
        commit.media_repair_children.push(InstallNetworkChild {
            child,
            lifetime,
            result,
        });
        receipt
    }

    pub(crate) fn stage_publisher_send(
        &mut self,
        generation: u64,
        change: crate::route::fabric::PublisherSendChange,
    ) -> oneshot::Receiver<bool> {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation || change.route().shard != self.shard {
            pulsebeam_runtime::fatal!("a DATA publisher send must target its shard generation");
        }
        let (result, receipt) = oneshot::channel();
        commit
            .publisher_sends
            .push(UpdatePublisherSend { change, result });
        receipt
    }

    pub(crate) fn stage_publisher_anchor(
        &mut self,
        generation: u64,
        publisher: PublisherAnchor,
    ) -> oneshot::Receiver<Option<u64>> {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation {
            pulsebeam_runtime::fatal!("a publisher anchor must be sampled at its shard generation");
        }
        let (result, receipt) = oneshot::channel();
        commit
            .source_anchors
            .push(SamplePublisherAnchor { publisher, result });
        receipt
    }

    pub(crate) fn stage_unavailable_network_repair(
        &mut self,
        generation: u64,
        interest: crate::route::fabric::network_repair::Interest,
    ) {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation || interest.parent.destination.shard() != self.shard {
            pulsebeam_runtime::fatal!(
                "a network DATA repair outcome must target its shard generation"
            );
        }
        commit.repair_unavailable.push(interest);
    }

    pub(crate) fn stage_relay_repair_handoff(
        &mut self,
        generation: u64,
        handoff: crate::route::fabric::relay::PeerRepairHandoff,
    ) {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation || handoff.located.lookup.shard() != self.shard {
            pulsebeam_runtime::fatal!("a relay repair handoff must target its shard generation");
        }
        commit.repair_handoffs.push(handoff);
    }

    pub(crate) fn stage_relay_repair_lookup(
        &mut self,
        generation: u64,
        lookup: crate::route::fabric::relay::PeerRepairLookup,
    ) -> oneshot::Receiver<Option<crate::route::fabric::relay::LocatedRepair>> {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation || lookup.shard() != self.shard {
            pulsebeam_runtime::fatal!("a relay repair lookup must target its shard generation");
        }
        let (result, receipt) = oneshot::channel();
        commit
            .repair_lookups
            .push(LocateRelayRepair { lookup, result });
        receipt
    }

    pub(crate) fn stage_publisher_data_observation(
        &mut self,
        generation: u64,
        observation: crate::route::fabric::PublisherObservation,
    ) {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation || observation.route.shard != self.shard {
            pulsebeam_runtime::fatal!(
                "a DATA publisher observation must target its shard generation"
            );
        }
        commit.publisher_observations.push(observation);
    }

    pub(crate) fn stage_relay_data_observation(
        &mut self,
        generation: u64,
        observation: crate::route::fabric::relay::RelayObservation,
    ) {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation
            || observation.route.input.destination.shard() != self.shard
        {
            pulsebeam_runtime::fatal!("a relay DATA observation must target its shard generation");
        }
        commit.relay_observations.push(observation);
    }

    pub(crate) fn stage_network_data_expectation(
        &mut self,
        generation: u64,
        notice: NetworkDataExpectation,
    ) -> oneshot::Receiver<bool> {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation || notice.parent.destination.shard() != self.shard {
            pulsebeam_runtime::fatal!(
                "a network DATA expectation must target its shard generation"
            );
        }
        let (result, receipt) = oneshot::channel();
        commit
            .data_expectations
            .push(ObserveNetworkDataExpectation { notice, result });
        receipt
    }

    pub(crate) fn stage_cutoff(
        &mut self,
        generation: u64,
    ) -> pulsebeam_runtime::sync::oneshot::Receiver<()> {
        self.stage_link_cutoff(generation, Vec::new())
    }

    pub(crate) fn stage_link_cutoff(
        &mut self,
        generation: u64,
        link: Vec<pulsebeam_link::BindingCutoff>,
    ) -> pulsebeam_runtime::sync::oneshot::Receiver<()> {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation {
            pulsebeam_runtime::fatal!("a cutoff fence must have its own shard generation");
        }
        let (tx, rx) = pulsebeam_runtime::sync::oneshot::channel();
        commit.cutoff_bindings.extend(link);
        commit.cutoff_acks.push(tx);
        rx
    }
    pub fn stage(&mut self, generation: u64, op: ShardUpdateOp) {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation {
            pulsebeam_runtime::fatal!("a shard generation cannot mix lifecycle generations");
        }
        commit.lifecycle.push(op);
    }

    pub fn stage_participant_effect(
        &mut self,
        generation: u64,
        connection: ConnectionId,
        effect: crate::participant::ParticipantEffect,
    ) {
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation {
            pulsebeam_runtime::fatal!("a shard generation cannot mix participant effects");
        }
        commit.participant_effects.push((connection, effect));
    }

    pub fn stage_plans(&mut self, generation: u64, plans: Vec<TrackPlanUpdate>) {
        if plans.is_empty() {
            return;
        }
        let commit = self
            .staged
            .get_or_insert_with(|| Box::new(ShardUpdate::new(self.shard, generation)));
        if commit.generation != generation {
            pulsebeam_runtime::fatal!("a shard generation cannot mix plan generations");
        }
        commit.plans.extend(plans);
    }

    pub fn publish(&mut self) -> Option<u64> {
        let commit = self.staged.take()?;
        if commit.is_empty() {
            return None;
        }
        let generation = commit.generation;
        if self.closed || !self.flush_backlog() {
            self.backlog.push_back(commit);
            return None;
        }
        match self.tx.try_send(commit) {
            Ok(()) => Some(generation),
            Err(mailbox::TrySendError::Full(commit)) => {
                self.backlog.push_back(commit);
                Some(generation)
            }
            Err(mailbox::TrySendError::Closed(commit)) => {
                self.backlog.push_back(commit);
                self.closed = true;
                None
            }
        }
    }

    #[cfg(test)]
    pub fn enqueue(&mut self) -> Option<u64> {
        let commit = self.staged.take()?;
        if commit.is_empty() {
            return None;
        }
        let generation = commit.generation;
        self.backlog.push_back(commit);
        Some(generation)
    }

    pub fn flush_one(&mut self) -> bool {
        if self.closed {
            return false;
        }
        let Some(commit) = self.backlog.pop_front() else {
            return true;
        };
        match self.tx.try_send(commit) {
            Ok(()) => true,
            Err(mailbox::TrySendError::Full(commit)) => {
                self.backlog.push_front(commit);
                false
            }
            Err(mailbox::TrySendError::Closed(commit)) => {
                self.backlog.push_front(commit);
                self.closed = true;
                false
            }
        }
    }

    fn flush_backlog(&mut self) -> bool {
        if self.closed {
            return false;
        }
        while let Some(commit) = self.backlog.pop_front() {
            match self.tx.try_send(commit) {
                Ok(()) => {}
                Err(mailbox::TrySendError::Full(commit)) => {
                    self.backlog.push_front(commit);
                    return false;
                }
                Err(mailbox::TrySendError::Closed(commit)) => {
                    self.backlog.push_front(commit);
                    self.closed = true;
                    return false;
                }
            }
        }
        true
    }

    pub fn has_backlog(&self) -> bool {
        !self.backlog.is_empty()
    }
}

pub(crate) fn new_shard_update(
    shard: ShardId,
) -> (ShardUpdateWriter, mailbox::Receiver<Box<ShardUpdate>>) {
    let (tx, rx) = mailbox::bounded(
        const { core::num::NonZeroUsize::new(crate::shard::worker::SHARD_UPDATE_CAPACITY).unwrap() },
    );
    (
        ShardUpdateWriter {
            shard,
            tx,
            staged: None,
            backlog: VecDeque::new(),
            closed: false,
        },
        rx,
    )
}

impl ShardUpdateOp {
    pub(crate) fn is_owned_by(&self, shard: ShardId) -> bool {
        match self {
            Self::InstallShardFabricSource { binding, .. } => binding.destination.shard() == shard,
            Self::BindShardFabricDestination { binding } => binding.origin == shard,
            Self::SetShardDataReceiver { ready, .. } => ready.binding.origin == shard,
            Self::InstallRoute { address, .. } | Self::RetireRoute { address } => {
                address.shard() == shard
            }
            Self::RetireTransport { address } | Self::RemoveParticipant { address, .. } => {
                address.shard() == shard
            }
            Self::InsertTrackRuntime { .. }
            | Self::RemoveTrackRuntime { .. }
            | Self::Placeholder => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track_plan() -> (PublicationKey, Vec<TrackPlanUpdate>) {
        let publication_id = crate::entity::ConnectionId::new()
            .fixture_publication(crate::entity::TrackKind::Audio, "audio");
        let plans = vec![TrackPlanUpdate {
            publication_id,
            plan: Some(TrackPlan::default()),
        }];
        (publication_id, plans)
    }

    #[test]
    fn one_track_has_at_most_one_remote_route_per_destination_shard() {
        let route = |shard, slot| {
            NodeRouteAddress::new(crate::route::RouteId::new(ShardId::new(shard), slot), 1)
        };
        let mut plan = TrackPlan {
            remote: vec![route(1, 0), route(1, 1)],
            ..Default::default()
        };
        assert!(!plan.is_valid());
        plan.remote[1] = route(2, 0);
        assert!(plan.is_valid());
    }

    #[test]
    fn lifecycle_and_plan_are_one_ordered_generation_without_shared_interpretation() {
        let shard = ShardId::new(1);
        let (mut writer, mut rx) = new_shard_update(shard);
        let (_, plans) = track_plan();
        writer.stage(7, ShardUpdateOp::Placeholder);
        writer.stage_plans(7, plans);
        assert_eq!(writer.publish(), Some(7));

        let commit = rx.try_recv().unwrap();
        assert_eq!(commit.shard, shard);
        assert_eq!(commit.generation, 7);
        assert_eq!(commit.lifecycle.len(), 1);
        assert_eq!(commit.plans.len(), 1);
    }

    #[test]
    fn queued_generations_remain_in_order() {
        let shard = ShardId::new(0);
        let (mut writer, mut rx) = new_shard_update(shard);
        for generation in 1..=(crate::shard::worker::SHARD_UPDATE_CAPACITY + 1) as u64 {
            writer.stage(generation, ShardUpdateOp::Placeholder);
            assert_eq!(writer.publish(), Some(generation));
        }
        let fence_generation = (crate::shard::worker::SHARD_UPDATE_CAPACITY + 2) as u64;
        let mut cutoff = writer.stage_cutoff(fence_generation);
        assert_eq!(writer.publish(), None);
        assert!(matches!(
            cutoff.try_recv(),
            Err(pulsebeam_runtime::sync::oneshot::error::TryRecvError::Empty)
        ));
        for expected in 1..=crate::shard::worker::SHARD_UPDATE_CAPACITY as u64 {
            assert_eq!(rx.try_recv().unwrap().generation, expected);
        }
        assert!(writer.flush_one());
        assert_eq!(
            rx.try_recv().unwrap().generation,
            (crate::shard::worker::SHARD_UPDATE_CAPACITY + 1) as u64
        );
        assert!(writer.flush_one());
        let mut fence = rx.try_recv().unwrap();
        assert_eq!(fence.generation, fence_generation);
        assert!(matches!(
            cutoff.try_recv(),
            Err(pulsebeam_runtime::sync::oneshot::error::TryRecvError::Empty)
        ));
        for ack in fence.cutoff_acks.drain(..) {
            ack.send(()).unwrap();
        }
        assert_eq!(cutoff.try_recv(), Ok(()));
    }

    #[test]
    fn one_flush_attempt_never_drains_multiple_generations() {
        let shard = ShardId::new(0);
        let (mut writer, mut rx) = new_shard_update(shard);
        writer.stage(1, ShardUpdateOp::Placeholder);
        assert_eq!(writer.enqueue(), Some(1));
        writer.stage(2, ShardUpdateOp::Placeholder);
        assert_eq!(writer.enqueue(), Some(2));

        assert!(writer.flush_one());
        assert_eq!(rx.try_recv().unwrap().generation, 1);
        assert!(writer.has_backlog());
        assert!(matches!(rx.try_recv(), Err(mailbox::TryRecvError::Empty)));

        assert!(writer.flush_one());
        assert_eq!(rx.try_recv().unwrap().generation, 2);
        assert!(!writer.has_backlog());
    }

    #[test]
    fn shard_update_validation_requires_new_owned_revision() {
        let shard = ShardId::new(2);
        let valid = ShardUpdate::new(shard, 4);
        assert!(valid.validate_for(shard, 3));
        assert!(!valid.validate_for(shard, 4));
        assert!(!valid.validate_for(ShardId::new(3), 3));
    }
}

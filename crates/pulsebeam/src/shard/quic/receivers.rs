use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU64;

use pulsebeam_agent_quic::{
    data_bindings::{DataBinding, DataBindings, InstallError},
    records::{Datagram, DatagramKind},
    transport::{DatagramAdmission, listener::Association},
};
use pulsebeam_proto::{
    data_control::SubscriberBinding, data_endpoint::Delivery, state::DataV1CaptureClock,
};
use pulsebeam_runtime::time::Instant;

use super::{Fact, Installed, ShardEvent};
use crate::{
    control::native::{Preparation, ReceiveTransport, ReceiverPreparation, ResourceExecution},
    entity::ProjectRoomKey,
    participant::{
        data_fabric::project_sctp_record,
        policy::{ExecutionFailure, ExecutionFailureKind, ReceiverExecution, ResourceObservation},
    },
    route::fabric::local::DataWrite,
    shard::{core::ShardCore, router::FabricLocalBranch},
    track::Track,
};

const MAX_SUBSCRIBERS: usize = 64;

pub(super) struct Subscribers {
    bindings: DataBindings<()>,
    owned: BTreeMap<u64, Subscriber>,
}

struct Subscriber {
    owned: u64,
    binding: DataBinding,
    source_room: ProjectRoomKey,
    endpoint: SubscriberBinding,
    capture_clock: Option<DataV1CaptureClock>,
    branch: Option<FabricLocalBranch>,
    execution: ReceiverExecution,
    sequence: u64,
}

impl Default for Subscribers {
    fn default() -> Self {
        Self {
            bindings: DataBindings::new(MAX_SUBSCRIBERS),
            owned: BTreeMap::new(),
        }
    }
}

impl Installed {
    pub(super) fn receiver_sequence(&self, owned: u64) -> Option<u64> {
        self.subscribers
            .owned
            .get(&owned)
            .map(|subscriber| subscriber.sequence)
    }

    pub(super) fn prepare_receiver(
        &mut self,
        work: Preparation,
        plan: ReceiverPreparation,
        core: &mut ShardCore,
        events: &mut VecDeque<ShardEvent>,
    ) -> Result<ResourceExecution, ExecutionFailure> {
        let reference = work.reference;
        let sequence = work.scope().sequence;
        match plan {
            ReceiverPreparation::Ready(plan) => {
                let ReceiveTransport::QuicData(fabric) = plan.transport else {
                    return Err(ExecutionFailureKind::Unsupported.into());
                };
                let binding = DataBinding::from_parameters(fabric.parameters)
                    .ok_or(ExecutionFailureKind::Invalid)?;
                if self.subscribers.owned.contains_key(&reference) {
                    return Err(ExecutionFailureKind::Conflict.into());
                }
                self.subscribers
                    .bindings
                    .install(binding, ())
                    .map_err(|error| match error {
                        InstallError::Capacity => ExecutionFailureKind::Capacity,
                        InstallError::Reused => ExecutionFailureKind::Conflict,
                    })?;
                let execution = ReceiverExecution {
                    source: plan.source.id(),
                    enabled: plan.policy.enabled,
                    ready: false,
                    failed: false,
                };
                let capture_clock = match plan.source {
                    Track::Data(data) => data.capture_clock,
                    _ => None,
                };
                self.subscribers.owned.insert(
                    reference,
                    Subscriber {
                        owned: work.owned(),
                        binding,
                        source_room: plan.source_room,
                        endpoint: fabric.endpoint,
                        capture_clock,
                        branch: None,
                        execution,
                        sequence,
                    },
                );
            }
            ReceiverPreparation::Policy(policy) => {
                let subscriber = self
                    .subscribers
                    .owned
                    .get_mut(&reference)
                    .ok_or(ExecutionFailureKind::Retired)?;
                if subscriber.sequence >= sequence {
                    return Err(ExecutionFailureKind::Conflict.into());
                }
                subscriber.sequence = sequence;
                subscriber.execution.enabled = policy.enabled;
                if !policy.enabled {
                    if let Some((branch, receiver)) = subscriber
                        .branch
                        .take()
                        .zip(core.quic_owner_handle(self.transport))
                    {
                        core.retire_quic_data_branch(branch, receiver);
                    }
                    subscriber.execution.ready = false;
                }
            }
            ReceiverPreparation::Waiting(_) => return Err(ExecutionFailureKind::Unsupported.into()),
        }
        let subscriber = self
            .subscribers
            .owned
            .get(&reference)
            .ok_or(ExecutionFailureKind::Internal)?;
        events.push_back(ShardEvent::OwnedSubscriptionSet {
            connection_id: self.connection,
            request: crate::control::topology::OwnedSubscription {
                source_room: subscriber.source_room,
                subscriber: self.participant,
                reference,
                track: subscriber.execution.source,
                enabled: subscriber.execution.enabled,
            },
        });
        Ok(ResourceExecution::Receiver(subscriber.execution))
    }

    pub(super) fn retire_receiver(
        &mut self,
        scope: crate::participant::policy::ExecutionScope,
        core: &mut ShardCore,
        events: &mut VecDeque<ShardEvent>,
    ) -> Option<bool> {
        let subscriber = self.subscribers.owned.get(&scope.binding)?;
        if subscriber.sequence > scope.sequence {
            return Some(true);
        }
        let subscriber = self.subscribers.owned.remove(&scope.binding)?;
        self.subscribers.bindings.retire(subscriber.binding);
        if let Some((branch, receiver)) = subscriber
            .branch
            .zip(core.quic_owner_handle(self.transport))
        {
            core.retire_quic_data_branch(branch, receiver);
        }
        events.push_back(ShardEvent::OwnedSubscriptionRemoved {
            source_room: subscriber.source_room,
            subscriber: self.participant,
            connection_id: self.connection,
            reference: scope.binding,
        });
        Some(
            self.handoff
                .fact(Fact::Resource(ResourceObservation::Released {
                    binding: scope.binding,
                })),
        )
    }

    pub(super) fn retire_receivers(&self, core: &mut ShardCore) {
        if let Some(receiver) = core.quic_owner_handle(self.transport) {
            for branch in self
                .subscribers
                .owned
                .values()
                .filter_map(|subscriber| subscriber.branch)
            {
                core.retire_quic_data_branch(branch, receiver);
            }
        }
    }

    /// Commit the Fabric handoff only when Noq takes the whole datagram.
    pub(super) fn poll_data_output(
        &mut self,
        association: NonZeroU64,
        peer: &mut Association,
        core: &mut ShardCore,
        now: Instant,
    ) -> bool {
        if self.handoff.execution_stopped()
            || core
                .current_quic_grant(self.connection, self.transport, association)
                .is_none()
        {
            return false;
        }
        let Some(receiver) = core.quic_owner_handle(self.transport) else {
            return false;
        };
        let subscribers = &mut self.subscribers;
        let mut retry = false;
        for (&reference, subscriber) in &mut subscribers.owned {
            if !subscriber.execution.enabled {
                continue;
            }
            if subscriber.branch.is_none() {
                subscriber.branch = core.install_quic_data_branch(
                    receiver,
                    subscriber.execution.source,
                    subscriber.endpoint,
                    (subscriber.binding.format.delivery() == Delivery::Reliable)
                        .then_some((reference, subscriber.owned)),
                );
                if subscriber.branch.is_some() {
                    subscriber.execution.ready = true;
                    if !self
                        .handoff
                        .fact(Fact::Resource(ResourceObservation::Receiver {
                            binding: reference,
                            execution: subscriber.execution,
                        }))
                    {
                        self.failed = true;
                        return true;
                    }
                }
            }
            let Some(branch) = subscriber.branch else {
                continue;
            };
            let clock = subscriber.capture_clock.as_ref().map(|clock| {
                pulsebeam_proto::data_record::Clock {
                    id: &clock.id,
                    epoch: clock.epoch,
                }
            });
            if core.admit_quic_receiver_data(
                branch,
                receiver,
                now,
                subscriber.binding.format.delivery() == Delivery::Reliable,
                |packet, repair| {
                    if project_sctp_record(&packet, clock, repair).is_err() {
                        return DataWrite::Rejected;
                    }
                    match peer.scheduler.offer_bound(
                        &mut peer.connection,
                        Datagram {
                            kind: DatagramKind::Data,
                            binding: subscriber.binding.id,
                            payload: packet.payload(),
                        },
                    ) {
                        Ok(()) => DataWrite::Written,
                        Err(DatagramAdmission::ControlTurn) => {
                            retry = true;
                            DataWrite::Deferred
                        }
                        Err(DatagramAdmission::Full) => DataWrite::Deferred,
                        Err(DatagramAdmission::Unsupported | DatagramAdmission::Size) => {
                            DataWrite::Rejected
                        }
                    }
                },
            ) {
                retry = true;
            }
        }
        retry
    }
}

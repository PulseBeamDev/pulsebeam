use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU64;

use pulsebeam_agent_quic::{
    data_bindings::{DataBinding, DataBindings, InstallError},
    records::{Datagram, DatagramKind},
};
use pulsebeam_proto::data_endpoint::Delivery;
use pulsebeam_runtime::time::Instant;

use super::{ExecutionAction, ExecutionCompletion, ExecutionDeferral, Fact, Installed, ShardEvent};
use crate::{
    control::native::{
        DataPublication, Preparation, PreparedResource, Publication, ResourceExecution,
    },
    entity::{PublicationId, PublicationKey},
    participant::{
        data_fabric::QuicInstallation,
        policy::{
            ExecutionFailure, ExecutionFailureKind, PublicationExecution, ResourceObservation,
        },
    },
    shard::{core::ShardCore, router::FabricSourceHandle},
    track::{Track, TrackMeta},
};

const MAX_PUBLISHERS: usize = 64;

pub(super) struct Publishers {
    bindings: DataBindings<(FabricSourceHandle, u64)>,
    owned: BTreeMap<u64, Publisher>,
}

struct Publisher {
    source: FabricSourceHandle,
    binding: DataBinding,
    retiring: bool,
    execution: PublicationExecution,
    sequence: u64,
}

impl Publisher {
    fn reliable_live(&self) -> bool {
        !self.retiring
            && self.execution.enabled
            && self.binding.format.delivery() == Delivery::Reliable
    }
}

impl Default for Publishers {
    fn default() -> Self {
        Self {
            bindings: DataBindings::new(MAX_PUBLISHERS),
            owned: BTreeMap::new(),
        }
    }
}

impl Installed {
    pub(super) fn execute(
        &mut self,
        association: NonZeroU64,
        action: ExecutionAction,
        core: &mut ShardCore,
        events: &mut VecDeque<ShardEvent>,
    ) -> bool {
        let fact = match action {
            ExecutionAction::Prepare { work, resource } => {
                let current = core.current_quic_grant(self.connection, self.transport, association);
                if let Some((_, unix_now)) =
                    current.filter(|(grant, _)| grant.revision == work.scope().authorization)
                {
                    let result = match resource {
                        PreparedResource::Publication(Publication::Data(plan)) => {
                            self.prepare_publisher(association, work, *plan, core, events)
                        }
                        PreparedResource::Receiver(plan) => {
                            self.prepare_receiver(work, plan, core, events)
                        }
                        _ => Err(ExecutionFailureKind::Unsupported.into()),
                    };
                    Fact::Completion(ExecutionCompletion {
                        work,
                        released: result.is_err()
                            && !self.publishers.owned.contains_key(&work.reference)
                            && self.receiver_sequence(work.reference).is_none(),
                        result,
                        unix_now,
                    })
                } else {
                    Fact::Deferral(ExecutionDeferral {
                        work,
                        retained_sequence: self
                            .publishers
                            .owned
                            .get(&work.reference)
                            .map(|value| value.sequence)
                            .or_else(|| self.receiver_sequence(work.reference)),
                    })
                }
            }
            ExecutionAction::Retire(scope) => {
                if let Some(released) = self.retire_receiver(scope, core, events) {
                    return released;
                }
                if let Some(previous) = self.publishers.owned.get_mut(&scope.binding) {
                    if previous.sequence > scope.sequence || previous.retiring {
                        return true;
                    }
                    previous.retiring = true;
                    let meta = previous.execution.track.meta().clone();
                    let _ = self.publishers.bindings.retire(previous.binding);
                    events.push_back(ShardEvent::OwnedTrackUnpublished {
                        meta,
                        reference: scope.binding,
                    });
                    return true;
                }
                Fact::Resource(ResourceObservation::Released {
                    binding: scope.binding,
                })
            }
            ExecutionAction::FailReceiver(scope) => Fact::Resource(ResourceObservation::Released {
                binding: scope.binding,
            }),
        };
        self.handoff.fact(fact)
    }

    fn prepare_publisher(
        &mut self,
        association: NonZeroU64,
        work: Preparation,
        plan: DataPublication,
        core: &mut ShardCore,
        events: &mut VecDeque<ShardEvent>,
    ) -> Result<ResourceExecution, ExecutionFailure> {
        let room = self
            .scope
            .joined_room()
            .ok_or(ExecutionFailureKind::Invalid)?;
        let pulsebeam_proto::state::DataV1Binding::Quic(parameters) = plan.parameters else {
            return Err(ExecutionFailureKind::Unsupported.into());
        };
        let binding =
            DataBinding::from_parameters(parameters).ok_or(ExecutionFailureKind::Invalid)?;
        if let Some(previous) = self.publishers.owned.get_mut(&work.reference) {
            if previous.retiring
                || previous.binding != binding
                || previous.sequence >= work.scope().sequence
            {
                return Err(ExecutionFailureKind::Conflict.into());
            }
            previous.execution.enabled = plan.enabled;
            previous.sequence = work.scope().sequence;
            return Ok(ResourceExecution::Publication(Box::new(
                previous.execution.clone(),
            )));
        }
        if self.publishers.owned.len() == MAX_PUBLISHERS {
            return Err(ExecutionFailureKind::Capacity.into());
        }
        let publisher = core
            .quic_owner_handle(self.transport)
            .ok_or(ExecutionFailureKind::Retired)?;
        let id = PublicationKey::Data(PublicationId::mint());
        let installation = QuicInstallation {
            association,
            binding,
        };
        let clock = plan
            .capture_clock
            .as_ref()
            .map(|clock| pulsebeam_proto::data_record::Clock {
                id: &clock.id,
                epoch: clock.epoch,
            });
        let (source, incarnation) = core
            .install_quic_data_source(publisher, id, installation, clock)
            .ok_or(ExecutionFailureKind::Capacity)?;
        if let Err(error) = self
            .publishers
            .bindings
            .install(binding, (source, work.reference))
        {
            core.retire_quic_data_source(id);
            return Err(match error {
                InstallError::Capacity => ExecutionFailureKind::Capacity,
                InstallError::Reused => ExecutionFailureKind::Conflict,
            }
            .into());
        }
        let meta = TrackMeta {
            room,
            id,
            origin: self.participant,
            connection_id: self.connection,
            track_id: Some(plan.track_id),
        };
        let track =
            Track::native_data_publication(meta, plan.lane, Some(incarnation), plan.capture_clock);
        events.push_back(ShardEvent::TrackPublished(track.clone()));
        let execution = PublicationExecution {
            track,
            data_v1_source: Some(incarnation),
            enabled: plan.enabled,
            active: false,
        };
        self.publishers.owned.insert(
            work.reference,
            Publisher {
                source,
                binding,
                retiring: false,
                sequence: work.scope().sequence,
                execution: execution.clone(),
            },
        );
        Ok(ResourceExecution::Publication(Box::new(execution)))
    }

    pub(super) fn ingest_data(
        &self,
        association: NonZeroU64,
        packet: Datagram<'_>,
        core: &mut ShardCore,
        now: Instant,
    ) -> Option<(u64, u64)> {
        if packet.kind != DatagramKind::Data
            || self.handoff.execution_stopped()
            || core
                .current_quic_grant(self.connection, self.transport, association)
                .is_none()
        {
            return None;
        }
        let (binding, (source, reference)) = self.publishers.bindings.resolve(packet.binding)?;
        let installed = self.publishers.owned.get(reference)?;
        if !installed.execution.enabled || installed.retiring {
            return None;
        }
        let publisher = core.quic_owner_handle(self.transport)?;
        core.ingest_quic_data(
            *source,
            publisher,
            QuicInstallation {
                association,
                binding,
            },
            packet.payload,
            now,
        )
        .map(|unit_seq| (*reference, unit_seq))
    }

    pub(super) fn publisher_admission(&self, reference: u64, unit_seq: u64) -> Option<Vec<u8>> {
        use pulsebeam_proto::{data_control::PublisherBinding, state::codec};
        let publisher = self.publishers.owned.get(&reference)?;
        if publisher.retiring || !publisher.execution.enabled {
            return None;
        }
        let source = publisher.execution.data_v1_source?;
        let frame = crate::participant::data_fabric::publisher_admission_observation(
            reference,
            PublisherBinding { source },
            unit_seq,
        )
        .ok()?;
        codec::encode_data_control(&frame).ok()
    }

    pub(super) fn publisher_control(
        &self,
        source: FabricSourceHandle,
        record: pulsebeam_proto::data_control::publisher_observation::Record,
    ) -> Option<Vec<u8>> {
        use pulsebeam_proto::{data_control::PublisherBinding, state::codec};
        let (&owned, publisher) = self
            .publishers
            .owned
            .iter()
            .find(|(_, publisher)| publisher.source == source && publisher.reliable_live())?;
        let endpoint = PublisherBinding {
            source: publisher.execution.data_v1_source?,
        };
        let frame =
            crate::participant::data_fabric::publisher_observation(owned, endpoint, record).ok()?;
        codec::encode_data_control(&frame).ok()
    }

    pub(super) fn receive_publisher_control(
        &self,
        association: NonZeroU64,
        frame: pulsebeam_proto::state::OwnedDataControl,
        core: &mut ShardCore,
        now: Instant,
    ) {
        use pulsebeam_proto::data_control::PublisherBinding;
        if self.handoff.execution_stopped()
            || core
                .current_quic_grant(self.connection, self.transport, association)
                .is_none()
        {
            return;
        }
        let Some(publisher) = self
            .publishers
            .owned
            .get(&frame.owned)
            .filter(|publisher| publisher.reliable_live())
        else {
            return;
        };
        let Some(incarnation) = publisher.execution.data_v1_source else {
            return;
        };
        let Some(demand) = crate::participant::data_fabric::decode_original_unavailable(
            PublisherBinding {
                source: incarnation,
            },
            &frame.record,
        ) else {
            return;
        };
        if let Some(owner) = core.quic_owner_handle(self.transport) {
            core.settle_quic_original_unavailable(publisher.source, owner, demand, now);
        }
    }

    pub(super) fn publication_cutoff_result(&mut self, reference: u64, success: bool) {
        if !self
            .publishers
            .owned
            .get(&reference)
            .is_some_and(|publisher| publisher.retiring)
        {
            return;
        }
        if success {
            self.publishers.owned.remove(&reference);
            self.failed |= !self
                .handoff
                .fact(Fact::Resource(ResourceObservation::Released {
                    binding: reference,
                }));
        } else {
            self.failed = true;
        }
    }

    pub(super) fn retire_publishers(&self, core: &mut ShardCore) {
        for publisher in self.publishers.owned.values() {
            core.retire_quic_data_source(publisher.execution.track.id());
        }
    }
}

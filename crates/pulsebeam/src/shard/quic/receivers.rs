use std::collections::{BTreeMap, VecDeque};
use std::num::{NonZeroU16, NonZeroU64};

use pulsebeam_agent_quic::{
    data_bindings::{Bindings, DataBinding, InstallError, PayloadBinding},
    records::{Datagram, DatagramKind, PictureLossIndication},
    transport::{DatagramAdmission, listener::Association},
};
use pulsebeam_proto::{
    data_control::SubscriberBinding,
    data_endpoint::Delivery,
    state::{DataV1CaptureClock, TrackKind, validation::QuicRtpBinding},
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
    route::fabric::local::{DataWrite, PacketView},
    rtp::fabric_egress::timeline,
    shard::{core::ShardCore, router::FabricLocalBranch},
    track::Track,
};

const MAX_SUBSCRIBERS: usize = 64;

pub(super) struct Subscribers {
    bindings: Bindings<PayloadBinding, ()>,
    owned: BTreeMap<u64, Subscriber>,
}

struct Subscriber {
    owned: u64,
    binding: PayloadBinding,
    source_room: ProjectRoomKey,
    payload: SubscriberPayload,
    branch: Option<FabricLocalBranch>,
    execution: ReceiverExecution,
    sequence: u64,
}

enum SubscriberPayload {
    Data {
        endpoint: SubscriberBinding,
        capture_clock: Option<DataV1CaptureClock>,
    },
    Rtp(RtpOutput),
}

struct RtpOutput {
    profile: pulsebeam_core::fabric::Profile,
    id: NonZeroU16,
    ssrc: u32,
    payload_type: u8,
    timeline: timeline::Timeline<FabricLocalBranch>,
    admitted: Option<u64>,
    last_branch: Option<FabricLocalBranch>,
}

impl RtpOutput {
    fn offer(
        &mut self,
        branch: FabricLocalBranch,
        now: Instant,
        packet: PacketView<'_>,
        position: u64,
        peer: &mut Association,
    ) -> Result<(), DatagramAdmission> {
        let header = packet.header();
        if header.profile != self.profile {
            return Ok(());
        }
        let Ok((projected, next)) = self.timeline.prepare(
            branch,
            timeline::Input {
                sequence: position,
                timestamp: header.timestamp,
            },
            now.into_std(),
            timeline::Transmitted {
                primary: self.admitted,
                sender_report: None,
            },
            self.last_branch.is_some_and(|previous| previous != branch),
        ) else {
            return Ok(());
        };
        let mut bytes = Vec::with_capacity(packet.payload().len().saturating_add(12));
        bytes.extend_from_slice(&[0x80, self.payload_type | (u8::from(header.marker) << 7)]);
        let [.., high, low] = projected.sequence.to_be_bytes();
        bytes.extend_from_slice(&[high, low]);
        bytes.extend_from_slice(&projected.timestamp.to_be_bytes());
        bytes.extend_from_slice(&self.ssrc.to_be_bytes());
        bytes.extend_from_slice(packet.payload());
        peer.scheduler.offer_bound(
            &mut peer.connection,
            Datagram {
                kind: DatagramKind::Rtp,
                binding: self.id,
                payload: &bytes,
            },
        )?;
        self.timeline = next;
        self.last_branch = Some(branch);
        self.admitted = Some(
            self.admitted
                .map_or(projected.sequence, |high| high.max(projected.sequence)),
        );
        Ok(())
    }
}

impl Default for Subscribers {
    fn default() -> Self {
        Self {
            bindings: Bindings::new(MAX_SUBSCRIBERS),
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
                let (binding, payload) = match plan.transport {
                    ReceiveTransport::QuicData(fabric) => (
                        PayloadBinding::Data(
                            DataBinding::from_parameters(fabric.parameters)
                                .ok_or(ExecutionFailureKind::Invalid)?,
                        ),
                        SubscriberPayload::Data {
                            endpoint: fabric.endpoint,
                            capture_clock: match &plan.source {
                                Track::Data(data) => data.capture_clock.clone(),
                                _ => None,
                            },
                        },
                    ),
                    ReceiveTransport::QuicRtp(parameters) => {
                        let (profile, kind) = match &plan.source {
                            Track::Audio(_) => {
                                (pulsebeam_core::fabric::Profile::Opus, TrackKind::Audio)
                            }
                            Track::Video(_) => {
                                (pulsebeam_core::fabric::Profile::H264, TrackKind::Video)
                            }
                            _ => return Err(ExecutionFailureKind::Unsupported.into()),
                        };
                        let binding = QuicRtpBinding::compile(&parameters, kind)
                            .map_err(|_| ExecutionFailureKind::Invalid)?;
                        let [.., a, b, c, d, e, f] = *self.connection.as_bytes();
                        let timeline = timeline::Timeline::new(
                            u16::from_be_bytes([a, b]),
                            u32::from_be_bytes([c, d, e, f]),
                            binding.format().clock_rate,
                            64,
                        )
                        .ok_or(ExecutionFailureKind::Invalid)?;
                        (
                            PayloadBinding::from(binding),
                            SubscriberPayload::Rtp(RtpOutput {
                                profile,
                                id: binding.id(),
                                ssrc: binding.ssrc().get(),
                                payload_type: binding.payload_type(),
                                timeline,
                                admitted: None,
                                last_branch: None,
                            }),
                        )
                    }
                    _ => return Err(ExecutionFailureKind::Unsupported.into()),
                };
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
                self.subscribers.owned.insert(
                    reference,
                    Subscriber {
                        owned: work.owned(),
                        binding,
                        source_room: plan.source_room,
                        payload,
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

    pub(super) fn ingest_receiver_pli(
        &self,
        receiver: crate::shard::participants::ParticipantHandle,
        packet: Datagram<'_>,
        core: &mut ShardCore,
    ) {
        let Ok(pli) = PictureLossIndication::decode(packet.payload) else {
            return;
        };
        let Some((binding, _)) = self.subscribers.bindings.resolve(packet.binding) else {
            return;
        };
        let Some(branch) = self.subscribers.owned.values().find_map(|subscriber| {
            let SubscriberPayload::Rtp(output) = &subscriber.payload else {
                return None;
            };
            (subscriber.binding == binding
                && subscriber.execution.enabled
                && output.profile == pulsebeam_core::fabric::Profile::H264
                && output.ssrc == pli.media_ssrc.get())
            .then_some(subscriber.branch)
            .flatten()
        }) else {
            return;
        };
        core.request_quic_receiver_refresh(receiver, branch);
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
                subscriber.branch = match (&subscriber.payload, subscriber.binding) {
                    (SubscriberPayload::Data { endpoint, .. }, PayloadBinding::Data(binding)) => {
                        core.install_quic_data_branch(
                            receiver,
                            subscriber.execution.source,
                            *endpoint,
                            (binding.format.delivery() == Delivery::Reliable)
                                .then_some((reference, subscriber.owned)),
                        )
                    }
                    (SubscriberPayload::Rtp(output), PayloadBinding::Rtp { .. }) => core
                        .install_quic_rtp_branch(
                            receiver,
                            subscriber.execution.source,
                            output.profile,
                        ),
                    _ => None,
                };
                if let Some(branch) = subscriber.branch {
                    if matches!(&subscriber.payload, SubscriberPayload::Rtp(output) if output.profile == pulsebeam_core::fabric::Profile::H264)
                    {
                        core.request_quic_receiver_refresh(receiver, branch);
                    }
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
            let capture_clock = match &mut subscriber.payload {
                SubscriberPayload::Data { capture_clock, .. } => capture_clock,
                SubscriberPayload::Rtp(output) => {
                    if matches!(
                        core.admit_quic_rtp(branch, receiver, now, |packet, position| {
                            match output.offer(branch, now, packet, position, peer) {
                                Ok(())
                                | Err(DatagramAdmission::Unsupported | DatagramAdmission::Size) => {
                                    Ok(())
                                }
                                Err(DatagramAdmission::ControlTurn) => {
                                    retry = true;
                                    Err(())
                                }
                                Err(DatagramAdmission::Full) => Err(()),
                            }
                        }),
                        Ok(Some(()))
                    ) {
                        retry = true;
                    }
                    continue;
                }
            };
            let PayloadBinding::Data(binding) = subscriber.binding else {
                continue;
            };
            let clock = capture_clock
                .as_ref()
                .map(|clock| pulsebeam_proto::data_record::Clock {
                    id: &clock.id,
                    epoch: clock.epoch,
                });
            if core.admit_quic_receiver_data(
                branch,
                receiver,
                now,
                binding.format.delivery() == Delivery::Reliable,
                |packet, repair| {
                    if project_sctp_record(&packet, clock, repair).is_err() {
                        return DataWrite::Rejected;
                    }
                    match peer.scheduler.offer_bound(
                        &mut peer.connection,
                        Datagram {
                            kind: DatagramKind::Data,
                            binding: binding.id,
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

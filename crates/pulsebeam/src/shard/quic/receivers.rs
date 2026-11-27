use std::collections::{BTreeMap, VecDeque};
use std::num::{NonZeroU16, NonZeroU32, NonZeroU64};
use std::time::Duration;

use pulsebeam_agent_quic::{
    data_bindings::{
        BindingId, Bindings, DataBinding, FabricBinding, FabricOffer, InstallError, PayloadBinding,
        rtp_profile,
    },
    records::{Datagram, DatagramKind, PictureLossIndication, RtcpSenderReport},
    transport::{
        DatagramAdmission,
        framing::{SendProgress, Sender},
        listener::Association,
    },
};
use pulsebeam_core::{
    dd::{evidence::DecodeTarget, output},
    fabric::{
        ExtensionKind, MAJOR, Profile,
        packet::{Agreement, MAX_PACKET_BYTES},
    },
};
use pulsebeam_proto::{
    data_control::SubscriberBinding,
    data_endpoint::Delivery,
    state::{DataV1CaptureClock, QuicVideoDd, TrackKind, validation::QuicRtpBinding},
};
use pulsebeam_runtime::time::Instant;

use super::{Fact, Installed, ShardEvent};
use crate::{
    control::native::{Preparation, ReceiveTransport, ReceiverPreparation, ResourceExecution},
    entity::ProjectRoomKey,
    participant::{
        data_fabric::project_record,
        policy::{
            ExecutionFailure, ExecutionFailureKind, ReceiverBinding, ReceiverExecution,
            ResourceObservation,
        },
    },
    route::fabric::local::{DataWrite, Emission, OutputEvidence, PacketView, RepairRequest},
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
    target: Option<DecodeTarget>,
    payload: SubscriberPayload,
    emission: Emission,
    branch: Option<FabricLocalBranch>,
    execution: ReceiverExecution,
    sequence: u64,
}

enum SubscriberPayload {
    Data {
        endpoint: SubscriberBinding,
        capture_clock: Option<DataV1CaptureClock>,
        wire: Box<FramedOutput<()>>,
    },
    Rtp(Box<RtpOutput>),
}

struct RtpOutput {
    profile: Profile,
    ssrc: NonZeroU32,
    wire: FramedOutput<RtpCommit>,
    dd: output::OutputMapper<FabricLocalBranch>,
    dd_reset: Option<(FabricLocalBranch, u64)>,
    requires_source_dd: bool,
    await_whole_entry: bool,
    timeline: timeline::Timeline<FabricLocalBranch>,
    admitted: Option<u64>,
    last_branch: Option<FabricLocalBranch>,
    packets: u32,
    octets: u32,
    pending_report: Option<(Instant, RtcpSenderReport)>,
    last_report: Option<Instant>,
}

struct RtpCommit {
    projected: timeline::Projected,
    timeline: timeline::Timeline<FabricLocalBranch>,
    dd: Option<output::Prepared<FabricLocalBranch>>,
    sender_ntp: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RtpEvidenceKey {
    sequence: u64,
    mode: output::Mode,
    entry: bool,
    reset: Option<u64>,
    dd: Option<(u64, u64, u32)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct OutputKey {
    branch: FabricLocalBranch,
    position: u64,
    repair: Option<RepairRequest>,
    rtp: Option<RtpEvidenceKey>,
}

struct FramedOutput<T> {
    agreement: Agreement,
    sender: Sender,
    pending: Option<(OutputKey, T)>,
}

enum OutputError {
    Deferred { runnable: bool },
    Rejected,
}

impl<T> FramedOutput<T> {
    fn data(peer: FabricOffer, binding: DataBinding) -> Result<(Self, Emission), ExecutionFailure> {
        let wire = FabricOffer::CURRENT
            .with_data_record_dd()
            .bind_data(peer, binding.record_dd)
            .map_err(|_| ExecutionFailureKind::Unsupported)?;
        Self::with_binding(wire, binding.id)
    }

    fn with_reception(
        peer: FabricOffer,
        profile: Profile,
        binding: NonZeroU16,
        reception: Option<QuicVideoDd>,
    ) -> Result<(Self, Emission), ExecutionFailure> {
        let wire = FabricOffer::CURRENT
            .bind_rtp_receiver(peer, profile, reception)
            .map_err(|_| ExecutionFailureKind::Unsupported)?;
        Self::with_binding(wire, binding)
    }

    fn with_binding(
        wire: FabricBinding,
        binding: NonZeroU16,
    ) -> Result<(Self, Emission), ExecutionFailure> {
        let emission = Emission::from_wire_profile(
            wire.agreement.profile() as u8,
            wire.agreement.extension_mask(),
            MAJOR,
            wire.agreement.revision(),
        )
        .ok_or(ExecutionFailureKind::Invalid)?;
        Ok((
            Self {
                agreement: wire.agreement,
                sender: Sender::new(wire.framing, binding),
                pending: None,
            },
            emission,
        ))
    }

    fn discard(&mut self) {
        self.pending = None;
        self.sender.discard_pending();
    }

    fn offer(
        &mut self,
        key: OutputKey,
        now: Instant,
        peer: &mut Association,
        prepare: impl FnOnce(Agreement, &mut [u8]) -> Option<(usize, T)>,
    ) -> Result<T, OutputError> {
        if self
            .pending
            .as_ref()
            .is_some_and(|(current, _)| *current != key)
        {
            self.discard();
        }
        if self.pending.is_none() {
            let mut bytes = [0; MAX_PACKET_BYTES];
            let (len, commit) = prepare(self.agreement, &mut bytes).ok_or(OutputError::Rejected)?;
            let bytes = bytes.get(..len).ok_or(OutputError::Rejected)?;
            self.sender
                .start(now.into_std(), Box::from(bytes))
                .map_err(|_| OutputError::Rejected)?;
            self.pending = Some((key, commit));
        }
        let result = self
            .sender
            .offer(&mut peer.scheduler, &mut peer.connection, now.into_std());
        match result {
            Ok(SendProgress::Admitted) => self
                .pending
                .take()
                .map(|(_, state)| state)
                .ok_or(OutputError::Rejected),
            Ok(SendProgress::Partial) => Err(OutputError::Deferred { runnable: true }),
            Ok(SendProgress::Deferred(reason)) => Err(OutputError::Deferred {
                runnable: reason == DatagramAdmission::ControlTurn,
            }),
            Ok(SendProgress::Idle) | Err(_) => {
                self.discard();
                Err(OutputError::Rejected)
            }
        }
    }
}

impl RtpOutput {
    fn offer(
        &mut self,
        branch: FabricLocalBranch,
        now: Instant,
        packet: PacketView<'_>,
        sequence: u64,
        evidence: OutputEvidence,
        peer: &mut Association,
    ) -> Result<(), OutputError> {
        if let Some(fence) = evidence.reset_after
            && self.dd_reset != Some((branch, fence))
        {
            self.wire.discard();
            self.dd.lose_context();
            self.dd_reset = Some((branch, fence));
        }
        if self.await_whole_entry
            && evidence.mode == output::Mode::Whole
            && !evidence.dd.as_ref().is_some_and(|(_, q)| {
                let dd = &q.descriptor;
                dd.start_of_frame
                    && dd.attached_structure.is_some()
                    && dd.frame_dependencies.frame_diffs.is_empty()
            })
        {
            return Err(OutputError::Rejected);
        }
        let position = evidence.output_position;
        let key = OutputKey {
            branch,
            position,
            repair: None,
            rtp: Some(RtpEvidenceKey {
                sequence,
                mode: evidence.mode,
                entry: evidence.entry,
                reset: evidence.reset_after,
                dd: evidence
                    .dd
                    .as_ref()
                    .map(|(frame, q)| (*frame, q.structure_sequence, q.active_targets)),
            }),
        };
        let payload_bytes = u32::try_from(packet.payload().len()).unwrap_or(0);
        let RtpCommit {
            projected,
            timeline: next,
            dd,
            sender_ntp,
        } = self.wire.offer(key, now, peer, |agreement, bytes| {
            let header = packet.header();
            if header.profile != self.profile {
                return None;
            }
            let (projected, next) = self
                .timeline
                .prepare(
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
                )
                .ok()?;
            let [.., a, b, c, d] = projected.sequence.to_be_bytes();
            let wire_sequence = u32::from_be_bytes([a, b, c, d]);
            let packet = packet.with_receiver_media_position(wire_sequence, projected.timestamp);
            let kind = ExtensionKind::DependencyDescriptor;
            let (len, dd) =
                if header.profile.is_video() && agreement.extension_mask() & kind.bit() != 0 {
                    let proposal = self
                        .dd
                        .prepare(
                            now.into_std(),
                            output::Retention::SenderLifetime,
                            output::Packet {
                                source: branch,
                                output_sequence: projected.sequence,
                                mode: evidence.mode,
                                entry: evidence.entry,
                                dd: evidence.dd.as_ref().map(|(frame, qualified)| {
                                    output::Evidence {
                                        frame: *frame,
                                        qualified,
                                    }
                                }),
                            },
                        )
                        .ok()?;
                    let mapped_dd = proposal.descriptor();
                    if self.requires_source_dd && mapped_dd.is_none() {
                        return None;
                    }
                    (
                        agreement.write_mapped_dd(&packet, mapped_dd, bytes).ok()?,
                        Some(proposal),
                    )
                } else if evidence.mode == output::Mode::Whole && !self.requires_source_dd {
                    (agreement.write(&packet, bytes).ok()?, None)
                } else {
                    return None;
                };
            Some((
                len,
                RtpCommit {
                    projected,
                    timeline: next,
                    dd,
                    sender_ntp: evidence.sender_ntp,
                },
            ))
        })?;
        if let Some(dd) = dd {
            self.dd.commit(dd).map_err(|_| OutputError::Rejected)?;
        }
        self.await_whole_entry = false;
        self.timeline = next;
        if self.last_branch != Some(branch) {
            self.pending_report = None;
            self.last_report = None;
        }
        self.last_branch = Some(branch);
        self.packets = self.packets.wrapping_add(1);
        self.octets = self.octets.wrapping_add(payload_bytes);
        if let Some(ntp) = sender_ntp
            && self
                .last_report
                .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(1))
        {
            self.pending_report = Some((
                now,
                RtcpSenderReport {
                    ssrc: self.ssrc,
                    ntp,
                    rtp_timestamp: projected.timestamp,
                    packets: self.packets,
                    octets: self.octets,
                },
            ));
        }
        self.admitted = Some(
            self.admitted
                .map_or(projected.sequence, |high| high.max(projected.sequence)),
        );
        Ok(())
    }

    fn poll_report(&mut self, binding: NonZeroU16, now: Instant, peer: &mut Association) -> bool {
        let Some((sampled, report)) = self.pending_report else {
            return false;
        };
        if now.saturating_duration_since(sampled) > Duration::from_secs(1) {
            self.pending_report = None;
            return false;
        }
        match peer.scheduler.offer_bound(
            &mut peer.connection,
            Datagram {
                kind: DatagramKind::Rtcp,
                binding,
                payload: &report.encode(),
            },
        ) {
            Err(DatagramAdmission::ControlTurn) => return true,
            Err(DatagramAdmission::Full) => return false,
            Ok(()) => self.last_report = Some(now),
            Err(DatagramAdmission::Size | DatagramAdmission::Unsupported) => {}
        }
        self.pending_report = None;
        false
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
                let peer = self.peer_fabric.ok_or(ExecutionFailureKind::Retired)?;
                let (binding, payload, emission) = match plan.transport {
                    ReceiveTransport::QuicData(fabric) => {
                        let binding = DataBinding::from_parameters(fabric.parameters)
                            .ok_or(ExecutionFailureKind::Invalid)?;
                        let (wire, emission) = FramedOutput::data(peer, binding)?;
                        (
                            PayloadBinding::Data(binding),
                            SubscriberPayload::Data {
                                endpoint: fabric.endpoint,
                                capture_clock: fabric.capture_clock,
                                wire: Box::new(wire),
                            },
                            emission,
                        )
                    }
                    ReceiveTransport::QuicRtp(parameters) => {
                        let (kind, guarantees_dd) = match &plan.source {
                            Track::Audio(_) => (TrackKind::Audio, false),
                            Track::Video(video) => (TrackKind::Video, video.guarantees_video_dd),
                            _ => return Err(ExecutionFailureKind::Unsupported.into()),
                        };
                        let binding = QuicRtpBinding::compile(&parameters, kind)
                            .map_err(|_| ExecutionFailureKind::Invalid)?;
                        binding
                            .validate_video_dd_direction(false)
                            .map_err(|_| ExecutionFailureKind::Invalid)?;
                        if !guarantees_dd
                            && (binding.video_dd() == Some(QuicVideoDd::Require)
                                || plan.policy.decode_target.is_some())
                        {
                            return Err(ExecutionFailureKind::Unsupported.into());
                        }
                        let profile = rtp_profile(&binding.format().mime_type)
                            .ok_or(ExecutionFailureKind::Unsupported)?;
                        let [.., a, b, c, d, e, f] = *self.connection.as_bytes();
                        let timeline = timeline::Timeline::new(
                            u16::from_be_bytes([a, b]),
                            u32::from_be_bytes([c, d, e, f]),
                            binding.format().clock_rate,
                            64,
                        )
                        .ok_or(ExecutionFailureKind::Invalid)?;
                        let binding_id = binding.id();
                        let (wire, emission) = FramedOutput::with_reception(
                            peer,
                            profile,
                            binding_id,
                            binding.video_dd(),
                        )?;
                        if guarantees_dd && !emission.carries_video_dd() {
                            return Err(ExecutionFailureKind::Unsupported.into());
                        }
                        (
                            PayloadBinding::from(binding),
                            SubscriberPayload::Rtp(Box::new(RtpOutput {
                                profile,
                                ssrc: binding.ssrc(),
                                wire,
                                dd: output::OutputMapper::new(u16::from_be_bytes([a, b])),
                                dd_reset: None,
                                requires_source_dd: guarantees_dd,
                                await_whole_entry: false,
                                timeline,
                                admitted: None,
                                last_branch: None,
                                packets: 0,
                                octets: 0,
                                pending_report: None,
                                last_report: None,
                            })),
                            emission,
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
                        target: plan.policy.decode_target,
                        payload,
                        emission,
                        branch: None,
                        execution,
                        sequence,
                    },
                );
                if !self
                    .handoff
                    .fact(Fact::Resource(ResourceObservation::ReceiverBinding {
                        binding: reference,
                        state: Some(ReceiverBinding {
                            available: true,
                            rtp: None,
                        }),
                    }))
                {
                    self.failed = true;
                    return Err(ExecutionFailureKind::Retired.into());
                }
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
                let target_changed = subscriber.target != policy.decode_target;
                if policy.decode_target.is_some()
                    && !matches!(&subscriber.payload, SubscriberPayload::Rtp(output) if output.requires_source_dd)
                {
                    return Err(ExecutionFailureKind::Unsupported.into());
                }
                let leaving_filtered =
                    subscriber.target.is_some() && policy.decode_target.is_none();
                subscriber.target = policy.decode_target;
                subscriber.sequence = sequence;
                subscriber.execution.enabled = policy.enabled;
                if !policy.enabled || target_changed {
                    match &mut subscriber.payload {
                        SubscriberPayload::Data { wire, .. } => wire.discard(),
                        SubscriberPayload::Rtp(output) => {
                            output.wire.discard();
                            output.dd.lose_context();
                            output.await_whole_entry |= leaving_filtered;
                        }
                    }
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
                && output.ssrc == pli.media_ssrc)
                .then_some(subscriber.branch)
                .flatten()
        }) else {
            return;
        };
        core.request_quic_receiver_refresh(receiver, branch);
    }

    /// Commit the Fabric handoff only after Noq admits every frame.
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
                            subscriber.emission,
                        )
                    }
                    (SubscriberPayload::Rtp(_), PayloadBinding::Rtp { .. }) => core
                        .install_quic_rtp_branch(
                            receiver,
                            subscriber.execution.source,
                            subscriber.emission,
                            subscriber.target,
                        ),
                    _ => None,
                };
                if let Some(branch) = subscriber.branch {
                    if matches!(&subscriber.payload, SubscriberPayload::Rtp(output) if output.profile == Profile::H264 || subscriber.target.is_some() || output.await_whole_entry)
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
            let (capture_clock, wire) = match &mut subscriber.payload {
                SubscriberPayload::Data {
                    capture_clock,
                    wire,
                    ..
                } => (capture_clock, wire),
                SubscriberPayload::Rtp(output) => {
                    if output.pending_report.is_some() {
                        if !core.quic_rtp_branch_live(branch, receiver)
                            || output.last_branch != Some(branch)
                        {
                            output.pending_report = None;
                        } else {
                            retry |= output.poll_report(subscriber.binding.binding_id(), now, peer);
                        }
                    }
                    let mut rejected_filtered = false;
                    let handoff =
                        core.admit_quic_rtp(branch, receiver, now, |sequence, packet, evidence| {
                            let filtered = matches!(evidence.mode, output::Mode::Filtered(_));
                            match output.offer(branch, now, packet, sequence, evidence, peer) {
                                Ok(()) => Ok(()),
                                Err(OutputError::Rejected) => {
                                    output.wire.discard();
                                    output.dd.lose_context();
                                    rejected_filtered = filtered;
                                    Ok(())
                                }
                                Err(OutputError::Deferred { runnable }) => {
                                    retry |= runnable;
                                    Err(())
                                }
                            }
                        });
                    if let Ok(consumed) = handoff {
                        if consumed.is_some() {
                            retry = true;
                        } else if let Some((key, _)) = output.wire.pending.as_ref() {
                            rejected_filtered = key
                                .rtp
                                .is_some_and(|key| matches!(key.mode, output::Mode::Filtered(_)));
                            output.wire.discard();
                            if rejected_filtered {
                                output.dd.lose_context();
                            }
                        }
                    }
                    if rejected_filtered {
                        core.reject_filtered_quic_rtp(branch, receiver, now);
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
                |position, packet, repair| {
                    let key = OutputKey {
                        branch,
                        position,
                        repair,
                        rtp: None,
                    };
                    match wire.offer(key, now, peer, |agreement, bytes| {
                        project_record(&packet, clock, repair.map(|repair| repair.fragment))
                            .ok()?;
                        Some((agreement.write(&packet, bytes).ok()?, ()))
                    }) {
                        Ok(()) => DataWrite::Written,
                        Err(OutputError::Deferred { runnable }) => {
                            retry |= runnable;
                            DataWrite::Deferred
                        }
                        Err(OutputError::Rejected) => DataWrite::Rejected,
                    }
                },
            ) {
                retry = true;
            }
        }
        retry
    }
}

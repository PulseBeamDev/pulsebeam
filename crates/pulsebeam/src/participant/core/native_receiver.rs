use super::native_data::{execution_bytes, unix_now};
use super::*;
use crate::control::native::Preparation;
use crate::control::native::{
    ReceiveTransport as ReceivePlan, ReceiverPolicy, ReceiverPreparation, ResourceExecution,
};
use crate::participant::policy::{ExecutionFailureContext, ExecutionFailureKind};
#[cfg(test)]
use pulsebeam_proto::state as wire;
use slotmap::Key as _;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::Bound::{Excluded, Included},
};

mod rtp;
#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct NativeReceivers {
    pub(super) receivers: BTreeMap<u64, Receiver>,
    sources: BTreeMap<[u8; 16], Source>,
    source_refs: BTreeSet<([u8; 16], u64)>,
    received: BTreeSet<(u64, u64)>,
    source_work: VecDeque<[u8; 16]>,
    pub(super) channels: BTreeMap<crate::media::ChannelId, u64>,
    mids: BTreeMap<Mid, u64>,
    pub(super) deadlines: BTreeSet<(Instant, u64)>,
}

pub(super) struct Receiver {
    transport: ReceiveTransport,
    source: TrackId,
    source_id: String,
    handle: Option<TrackHandle>,
    enabled: bool,
    failed: bool,
    pub(super) opened: bool,
    pub(super) installation: Option<pulsebeam_rtc::DataChannelInstallation>,
    pub(super) retiring: bool,
    preparation: Option<(Preparation, ReceiverPolicy)>,
    storage_sequence: u64,
    deadline: Option<Instant>,
}

impl Receiver {
    fn execution(
        &self,
        enabled: bool,
        ready: bool,
    ) -> crate::participant::policy::ReceiverExecution {
        crate::participant::policy::ReceiverExecution {
            source: self.source,
            enabled,
            ready,
            failed: self.failed,
        }
    }
}

enum ReceiveTransport {
    Sctp {
        channel: crate::media::ChannelId,
        lane: DataLane,
    },
    Rtp(Box<rtp::RtpReceiver>),
}

impl ReceiveTransport {
    fn data(&self) -> Option<(crate::media::ChannelId, DataLane)> {
        match self {
            Self::Sctp { channel, lane } => Some((*channel, *lane)),
            Self::Rtp(_) => None,
        }
    }
}

struct Source {
    handle: Option<TrackHandle>,
    track: Track,
    bound: bool,
    resident: bool,
    retired: bool,
    queued: bool,
    cursor: u64,
}

impl NativeReceivers {
    pub(super) fn contains_source(&self, source: TrackId) -> bool {
        self.sources.contains_key(source.as_bytes())
    }

    pub(super) fn storage_sequence(&self, binding: u64) -> Option<u64> {
        self.receivers
            .get(&binding)
            .map(|receiver| receiver.storage_sequence)
    }

    pub(super) fn transfer_storage(&mut self, binding: u64, sequence: u64) {
        if let Some(receiver) = self.receivers.get_mut(&binding) {
            receiver.storage_sequence = sequence;
        }
    }

    fn binding(&self, reference: u64) -> Option<crate::participant::policy::ReceiverBinding> {
        self.receivers
            .get(&reference)
            .map(|receiver| crate::participant::policy::ReceiverBinding {
                available: receiver.opened && !receiver.retiring,
                rtp: match &receiver.transport {
                    ReceiveTransport::Rtp(rtp) => Some((rtp.kind, rtp.supports_playout_delay)),
                    ReceiveTransport::Sctp { .. } => None,
                },
            })
    }

    pub(super) fn channel_installation(
        &self,
        reference: u64,
    ) -> Option<pulsebeam_rtc::DataChannelInstallation> {
        self.receivers.get(&reference)?.installation
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadlines.first().map(|(deadline, _)| *deadline)
    }

    pub(super) fn can_write(&self, channel: crate::media::ChannelId) -> bool {
        self.channels
            .get(&channel)
            .and_then(|reference| self.receivers.get(reference))
            .is_some_and(|receiver| self.receiver_ready(receiver))
    }

    pub(super) fn owns_mid(&self, mid: Mid) -> bool {
        self.mids.contains_key(&mid)
    }

    pub(super) fn can_write_rtp(&self, mid: Mid, track: TrackId, ssrc: Ssrc, pt: Pt) -> bool {
        let Some(receiver) = self
            .mids
            .get(&mid)
            .and_then(|reference| self.receivers.get(reference))
        else {
            return false;
        };
        let ReceiveTransport::Rtp(rtp) = &receiver.transport else {
            return false;
        };
        receiver.source == track
            && rtp.ssrc() == ssrc
            && rtp.payload_type() == pt
            && self.receiver_ready(receiver)
    }

    pub(super) fn playout_delay(
        &self,
        mid: Mid,
    ) -> Option<(crate::rtp::MediaTime, crate::rtp::MediaTime)> {
        let receiver = self.receivers.get(self.mids.get(&mid)?)?;
        match &receiver.transport {
            ReceiveTransport::Rtp(rtp) => rtp.playout_delay(),
            ReceiveTransport::Sctp { .. } => None,
        }
    }

    fn receiver_ready(&self, receiver: &Receiver) -> bool {
        receiver.enabled
            && !receiver.failed
            && receiver.opened
            && !receiver.retiring
            && self
                .sources
                .get(receiver.source.as_bytes())
                .is_some_and(|source| {
                    !source.retired
                        && source.bound
                        && source.handle.is_some()
                        && source.handle == receiver.handle
                })
    }

    fn queue_source(&mut self, id: [u8; 16]) {
        if let Some(source) = self.sources.get_mut(&id) {
            source.cursor = 0;
            if !source.queued {
                source.queued = true;
                self.source_work.push_back(id);
            }
        }
    }

    fn queue_unused_source(&mut self, id: [u8; 16]) {
        let referenced = self
            .source_refs
            .range((Included((id, 0)), Included((id, u64::MAX))))
            .next()
            .is_some();
        if !referenced && self.sources.get(&id).is_some_and(|source| !source.resident) {
            self.queue_source(id);
        }
        debug_assert!(
            self.sources
                .get(&id)
                .is_none_or(|source| source.resident || referenced || source.queued)
        );
    }

    fn remove_receiver(&mut self, reference: u64) -> Option<Receiver> {
        let receiver = self.receivers.remove(&reference)?;
        match &receiver.transport {
            ReceiveTransport::Sctp { channel, .. } => {
                if self.channels.get(channel) == Some(&reference) {
                    self.channels.remove(channel);
                }
            }
            ReceiveTransport::Rtp(rtp) => {
                if self.mids.get(&rtp.mid()) == Some(&reference) {
                    self.mids.remove(&rtp.mid());
                }
            }
        }
        if let Some(handle) = receiver.handle {
            self.received.remove(&(handle.data().as_ffi(), reference));
        }
        self.source_refs
            .remove(&(*receiver.source.as_bytes(), reference));
        self.queue_unused_source(*receiver.source.as_bytes());
        if let Some(deadline) = receiver.deadline {
            self.deadlines.remove(&(deadline, reference));
        }
        Some(receiver)
    }
}

impl Connection {
    fn record_native_receiver_binding(&mut self, reference: u64) {
        self.record_native_resource_observation(
            crate::participant::policy::ResourceObservation::ReceiverBinding {
                binding: reference,
                state: self.native_receivers.binding(reference),
            },
        );
    }

    pub(crate) fn native_receiver_execution_bytes(
        label: &str,
        source_id: &str,
        rtp_kind: Option<MediaKind>,
    ) -> usize {
        execution_bytes(label)
            .saturating_add(source_id.len())
            .saturating_add(
                std::mem::size_of::<Receiver>()
                    .saturating_add(std::mem::size_of::<rtp::RtpReceiver>())
                    .saturating_mul(12),
            )
            .saturating_add(
                match rtp_kind {
                    Some(MediaKind::Audio) => {
                        crate::participant::downstream::DownstreamAudio::owned_slot_bytes()
                    }
                    Some(MediaKind::Video) => {
                        crate::participant::downstream::DownstreamVideo::owned_slot_bytes()
                    }
                    None => 0,
                }
                .saturating_mul(12),
            )
            .saturating_add(4096)
    }

    pub(in crate::participant::core) fn native_source_added(
        &mut self,
        handle: TrackHandle,
        track: &Track,
    ) {
        if track.meta().room_id != self.room_id || track.meta().origin == self.participant_id {
            self.disconnect(DisconnectReason::SignalingInput);
            return;
        }
        let id = *track.id().as_bytes();
        let source = self
            .native_receivers
            .sources
            .entry(id)
            .or_insert_with(|| Source {
                handle: None,
                track: track.clone(),
                bound: false,
                resident: true,
                retired: false,
                queued: false,
                cursor: 0,
            });
        let prior = source.track.meta();
        let current = track.meta();
        if source.retired
            || source.track.kind() != track.kind()
            || prior.origin != current.origin
            || prior.connection_id != current.connection_id
            || prior.label != current.label
        {
            self.disconnect(DisconnectReason::SignalingInput);
            return;
        }
        if source.handle != Some(handle) {
            source.bound = false;
        }
        source.handle = Some(handle);
        source.track = track.clone();
        source.resident = true;
        self.native_receivers.queue_source(id);
    }

    pub(in crate::participant::core) fn native_source_removed(
        &mut self,
        track: TrackId,
        reason: crate::participant::effect::TrackRemoval,
    ) {
        let id = *track.as_bytes();
        if let Some(source) = self.native_receivers.sources.get_mut(&id) {
            source.handle = None;
            source.resident = false;
            source.bound = false;
            source.retired |= reason == crate::participant::effect::TrackRemoval::SourceEnded;
            self.native_receivers.queue_source(id);
        }
    }

    pub(in crate::participant::core) fn native_source_binding(
        &mut self,
        track: TrackId,
        handle: TrackHandle,
        bound: bool,
    ) {
        let id = *track.as_bytes();
        if let Some(source) = self.native_receivers.sources.get_mut(&id) {
            if source.handle != Some(handle) {
                return;
            }
            source.bound = bound;
            if bound {
                self.downstream.activate_track_binding(handle, track);
            } else {
                self.downstream.deactivate_track_binding(handle, track);
            }
            self.native_receivers.queue_source(id);
        }
    }

    pub(in crate::participant::core) fn advance_native_source(
        &mut self,
        _events: &mut impl ParticipantSink,
    ) -> bool {
        let Some(id) = self.native_receivers.source_work.pop_front() else {
            return false;
        };
        let Some(source) = self.native_receivers.sources.get_mut(&id) else {
            return true;
        };
        let next = self
            .native_receivers
            .source_refs
            .range((Excluded((id, source.cursor)), Included((id, u64::MAX))))
            .next()
            .copied();
        let Some((_, reference)) = next else {
            source.queued = false;
            if !source.resident
                && self
                    .native_receivers
                    .source_refs
                    .range((Included((id, 0)), Included((id, u64::MAX))))
                    .next()
                    .is_none()
            {
                self.native_receivers.sources.remove(&id);
            }
            if self.native_receivers.source_work.is_empty() {
                self.native_receivers.source_work = Default::default();
            }
            return true;
        };
        source.cursor = reference;
        let (handle, retired) = (source.handle, source.retired);
        self.native_receivers.source_work.push_back(id);
        if retired {
            self.native_resource_ended(reference);
            return true;
        }
        let Some(receiver) = self.native_receivers.receivers.get_mut(&reference) else {
            return true;
        };
        if let Some(previous) = receiver.handle {
            self.native_receivers
                .received
                .remove(&(previous.data().as_ffi(), reference));
        }
        receiver.handle = handle;
        if let Some(handle) = handle {
            self.native_receivers
                .received
                .insert((handle.data().as_ffi(), reference));
        }
        let Some(receiver) = self.native_receivers.receivers.get(&reference) else {
            return true;
        };
        let ready = self.native_receivers.receiver_ready(receiver);
        let execution = receiver.execution(receiver.enabled, ready);
        if self.apply_native_receiver_gate(reference, ready).is_err() {
            self.disconnect(DisconnectReason::SignalingOutput);
            return true;
        }
        self.record_native_resource_observation(
            crate::participant::policy::ResourceObservation::Receiver {
                binding: reference,
                execution,
            },
        );
        true
    }

    pub(super) fn execute_native_receiver(
        &mut self,
        work: Preparation,
        preparation: ReceiverPreparation,
        events: &mut impl ParticipantSink,
    ) {
        let Some(work) = self.current_native_preparation(work) else {
            return;
        };
        let plan = match preparation {
            ReceiverPreparation::Policy(policy) => {
                if !self
                    .native_receivers
                    .receivers
                    .get(&work.reference)
                    .is_some_and(|receiver| receiver.opened && !receiver.retiring)
                {
                    self.complete_native_preparation(
                        work,
                        Err(ExecutionFailureKind::Conflict.into()),
                    );
                    return;
                }
                self.commit_native_receiver(work, policy, events);
                return;
            }
            ReceiverPreparation::Ready(plan) => *plan,
            ReceiverPreparation::Waiting(_) => {
                self.disconnect(DisconnectReason::SignalingOutput);
                return;
            }
        };
        let crate::control::native::Receiver {
            source,
            source_id,
            transport,
            policy,
        } = plan;
        let source_track = source;
        let source = source_track.id();
        let metadata = source_track.meta();
        if metadata.room_id != self.room_id
            || metadata.origin == self.participant_id
            || self
                .native_receivers
                .sources
                .get(source.as_bytes())
                .is_some_and(|current| {
                    let previous = current.track.meta();
                    current.retired
                        || current.track.kind() != source_track.kind()
                        || previous.origin != metadata.origin
                        || previous.connection_id != metadata.connection_id
                        || previous.label != metadata.label
                })
        {
            self.complete_native_preparation(work, Err(ExecutionFailureKind::Retired.into()));
            return;
        }
        let mut transport = match self.prepare_native_receiver_transport(transport) {
            Ok(transport) => transport,
            Err(code) => {
                self.complete_native_preparation(
                    work,
                    Err(code.at(ExecutionFailureContext::PrepareReceiver)),
                );
                return;
            }
        };
        let staged_video_mid = if let ReceiveTransport::Rtp(rtp) = &transport
            && rtp.video_options.is_some()
        {
            if !self.downstream.video.install_owned_slot(
                rtp.mid(),
                rtp.rid(),
                rtp.ssrc(),
                rtp.payload_type(),
            ) {
                self.complete_native_preparation(
                    work,
                    Err(ExecutionFailureKind::Conflict
                        .at(ExecutionFailureContext::ReceiverVideoSlot)),
                );
                return;
            }
            Some(rtp.mid())
        } else {
            None
        };
        let staged_audio_mid = if let ReceiveTransport::Rtp(rtp) = &transport
            && rtp.kind == MediaKind::Audio
        {
            if !self
                .downstream
                .audio
                .install_owned_slot(rtp.mid(), rtp.ssrc(), rtp.payload_type())
            {
                self.complete_native_preparation(
                    work,
                    Err(ExecutionFailureKind::Conflict
                        .at(ExecutionFailureContext::ReceiverAudioSlot)),
                );
                return;
            }
            Some(rtp.mid())
        } else {
            None
        };
        let opened = match &mut transport {
            ReceiveTransport::Sctp { channel, lane } => self
                .transport
                .open_native_data(*channel, *lane)
                .map(|installation| (false, Some(installation))),
            ReceiveTransport::Rtp(rtp) => rtp
                .install(&mut self.transport)
                .map(|()| (true, None))
                .map_err(ExecutionFailureKind::from),
        };
        let (opened, installation) = match opened {
            Ok(state) => state,
            Err(code) => {
                if let Some(mid) = staged_video_mid {
                    self.downstream.video.remove_owned_slot(mid);
                }
                if let Some(mid) = staged_audio_mid {
                    self.downstream.audio.remove_owned_slot(mid);
                }
                self.complete_native_preparation(
                    work,
                    Err(code.at(ExecutionFailureContext::InstallReceiver)),
                );
                return;
            }
        };
        let reference = work.reference;
        let deadline = if opened {
            None
        } else {
            let now = Instant::now();
            Some(now.checked_add(Duration::from_secs(30)).unwrap_or(now))
        };
        if let Some(deadline) = deadline {
            self.native_receivers
                .deadlines
                .insert((deadline, reference));
        }
        match &transport {
            ReceiveTransport::Sctp { channel, .. } => {
                self.native_receivers.channels.insert(*channel, reference);
            }
            ReceiveTransport::Rtp(rtp) => {
                self.native_receivers.mids.insert(rtp.mid(), reference);
            }
        }
        let existing = self.downstream.candidate_binding(source);
        self.native_receivers
            .sources
            .entry(*source.as_bytes())
            .or_insert_with(|| Source {
                handle: existing.map(|(handle, _)| handle),
                track: source_track,
                bound: existing.is_some_and(|(_, bound)| bound),
                resident: existing.is_some(),
                retired: false,
                queued: false,
                cursor: 0,
            });
        self.native_receivers
            .source_refs
            .insert((*source.as_bytes(), reference));
        self.native_receivers.receivers.insert(
            reference,
            Receiver {
                source,
                source_id,
                handle: None,
                enabled: false,
                failed: false,
                transport,
                opened,
                installation,
                retiring: false,
                preparation: Some((work, policy)),
                storage_sequence: work.scope().sequence,
                deadline,
            },
        );
        self.native_receivers.queue_source(*source.as_bytes());
        self.record_native_receiver_binding(reference);
        if opened
            && let Some((work, policy)) = self
                .native_receivers
                .receivers
                .get_mut(&reference)
                .and_then(|receiver| receiver.preparation.take())
        {
            self.commit_native_receiver(work, policy, events);
            if self.native_callback_fenced(reference) {
                self.fail_native_receiver(reference, ExecutionFailureKind::Retired);
            }
        }
    }

    fn prepare_native_receiver_transport(
        &self,
        plan: ReceivePlan,
    ) -> Result<ReceiveTransport, ExecutionFailureKind> {
        match plan {
            ReceivePlan::Sctp { channel, lane } => {
                if self.native_data.channels.contains_key(&channel)
                    || self.native_receivers.channels.contains_key(&channel)
                {
                    return Err(ExecutionFailureKind::Conflict);
                }
                Ok(ReceiveTransport::Sctp { channel, lane })
            }
            ReceivePlan::Rtp(configuration) => {
                let receiver = rtp::RtpReceiver::new(*configuration);
                if receiver.video_options.is_some() != (receiver.kind == MediaKind::Video) {
                    return Err(ExecutionFailureKind::Invalid);
                }
                if self.native_receivers.mids.contains_key(&receiver.mid()) {
                    return Err(ExecutionFailureKind::Conflict);
                }
                Ok(ReceiveTransport::Rtp(Box::new(receiver)))
            }
        }
    }

    pub(super) fn native_receiver_opened(
        &mut self,
        reference: u64,
        config: pulsebeam_rtc::DataChannelConfig,
        events: &mut impl ParticipantSink,
    ) {
        if self.native_callback_fenced(reference) {
            self.fail_native_receiver(reference, ExecutionFailureKind::Retired);
            return;
        }
        let Some(receiver) = self.native_receivers.receivers.get_mut(&reference) else {
            return;
        };
        if !receiver.installation.is_some_and(|installation| {
            matches!(
                self.transport.data_channel_installation(installation),
                Some(pulsebeam_rtc::DataChannelInstallationState::Open { .. })
            )
        }) {
            return;
        }
        let Some((channel, lane)) = receiver.transport.data() else {
            return;
        };
        if config != crate::participant::transport::native_data_config(channel, lane) {
            if receiver.preparation.is_some() {
                self.fail_native_receiver(reference, ExecutionFailureKind::Conflict);
            } else {
                self.native_data_closed(channel, events);
            }
            return;
        }
        receiver.opened = true;
        if let Some(deadline) = receiver.deadline.take() {
            self.native_receivers
                .deadlines
                .remove(&(deadline, reference));
        }
        let preparation = receiver.preparation.take();
        self.record_native_receiver_binding(reference);
        if let Some((work, policy)) = preparation {
            self.commit_native_receiver(work, policy, events);
        }
        if self.native_callback_fenced(reference) {
            self.fail_native_receiver(reference, ExecutionFailureKind::Retired);
        }
        self.evaluate_native_preparations(events);
    }

    fn apply_native_receiver_gate(
        &mut self,
        reference: u64,
        active: bool,
    ) -> Result<(), ExecutionFailureKind> {
        let Some(receiver) = self.native_receivers.receivers.get(&reference) else {
            return Ok(());
        };
        match &receiver.transport {
            ReceiveTransport::Sctp { channel, .. } => {
                if !active {
                    self.transport.discard_channel_mutations(*channel);
                }
            }
            ReceiveTransport::Rtp(rtp) => {
                if !active {
                    self.stream_writer.discard_mid(rtp.mid());
                    if !self.transport.discard_rtp(rtp.mid()) {
                        return Err(ExecutionFailureKind::Internal);
                    }
                }
                rtp.set_active(&mut self.transport, active)
                    .map_err(ExecutionFailureKind::from)?;
                if let Some(options) = &rtp.video_options {
                    self.downstream.video.configure_owned_slot(
                        rtp.mid(),
                        receiver.source,
                        options,
                        active,
                    );
                    self.downstream.dirty_allocation = true;
                }
            }
        }
        Ok(())
    }

    fn commit_native_receiver(
        &mut self,
        work: Preparation,
        policy: ReceiverPolicy,
        events: &mut impl ParticipantSink,
    ) {
        let Some(work) = self.current_native_preparation(work) else {
            return;
        };
        let reference = work.reference;
        let Some(receiver) = self.native_receivers.receivers.get(&reference) else {
            return;
        };
        if !receiver.opened || receiver.retiring {
            self.complete_native_preparation(work, Err(ExecutionFailureKind::Conflict.into()));
            return;
        }
        let video_options = policy.video;
        let discard = match &receiver.transport {
            ReceiveTransport::Rtp(rtp) if rtp.video_options != video_options => Some(rtp.mid()),
            _ => None,
        };
        let Some(source) = self
            .native_receivers
            .sources
            .get(receiver.source.as_bytes())
            .filter(|source| !source.retired)
        else {
            self.complete_native_preparation(work, Err(ExecutionFailureKind::Retired.into()));
            return;
        };
        let enabled = policy.enabled;
        let handle = source.handle;
        let ready = enabled && !receiver.failed && source.bound && handle.is_some();
        let execution = receiver.execution(enabled, ready);
        let Some(now) = self.finish_native_preparation(work) else {
            return;
        };
        if let Some(mid) = discard {
            if !self.transport.discard_rtp(mid) {
                self.record_native_preparation(
                    work,
                    Err(ExecutionFailureKind::Internal.into()),
                    now,
                );
                return;
            }
            self.stream_writer.discard_mid(mid);
        }
        if let Err(code) = self.apply_native_receiver_gate(reference, ready) {
            self.record_native_preparation(work, Err(code.into()), now);
            return;
        }
        let Some(receiver) = self.native_receivers.receivers.get_mut(&reference) else {
            self.disconnect(DisconnectReason::SignalingOutput);
            return;
        };
        receiver.enabled = enabled;
        if let ReceiveTransport::Rtp(rtp) = &mut receiver.transport {
            rtp.video_options = video_options;
            if let Some(options) = &rtp.video_options {
                self.downstream.video.configure_owned_slot(
                    rtp.mid(),
                    receiver.source,
                    options,
                    ready,
                );
                self.downstream.dirty_allocation = true;
            }
        }
        if let Some(previous) = receiver.handle {
            self.native_receivers
                .received
                .remove(&(previous.data().as_ffi(), reference));
        }
        receiver.handle = handle;
        if let Some(handle) = handle {
            self.native_receivers
                .received
                .insert((handle.data().as_ffi(), reference));
        }
        events.set_owned_subscription(reference, receiver.source, enabled && !receiver.failed);
        self.record_native_preparation(work, Ok(ResourceExecution::Receiver(execution)), now);
    }

    pub(super) fn apply_native_receiver_failure(
        &mut self,
        scope: crate::participant::policy::ExecutionScope,
        events: &mut impl ParticipantSink,
    ) {
        if !self.native_execution.advance(scope) || !self.native_scope_authorized(scope, unix_now())
        {
            return;
        }
        let reference = scope.binding;
        let Some(receiver) = self
            .native_receivers
            .receivers
            .get_mut(&reference)
            .filter(|receiver| !receiver.retiring && receiver.opened && !receiver.failed)
        else {
            return;
        };
        if !matches!(&receiver.transport, ReceiveTransport::Rtp(_)) {
            return;
        }
        receiver.failed = true;
        let Some(receiver) = self.native_receivers.receivers.get(&reference) else {
            return;
        };
        let source = receiver.source;
        let demand = receiver.enabled && !receiver.failed;
        let ready = self.native_receivers.receiver_ready(receiver);
        if self.apply_native_receiver_gate(reference, ready).is_err() {
            self.disconnect(DisconnectReason::SignalingOutput);
            return;
        }
        events.set_owned_subscription(reference, source, demand);
    }

    fn close_native_receiver_transport(&mut self, receiver: &Receiver) {
        match &receiver.transport {
            ReceiveTransport::Sctp { .. } => {
                if let Some(installation) = receiver.installation {
                    self.transport
                        .cancel_data_channel_installation(installation);
                }
            }
            ReceiveTransport::Rtp(rtp) => {
                self.stream_writer.discard_mid(rtp.mid());
                if rtp.video_options.is_some() {
                    self.downstream.video.remove_owned_slot(rtp.mid());
                    self.downstream.dirty_allocation = true;
                } else {
                    self.downstream.audio.remove_owned_slot(rtp.mid());
                }
                if !self.transport.discard_rtp(rtp.mid())
                    || rtp.remove(&mut self.transport).is_err()
                {
                    self.disconnect(DisconnectReason::SignalingOutput);
                }
            }
        }
    }

    pub(super) fn fail_native_receiver(&mut self, reference: u64, code: ExecutionFailureKind) {
        let Some(receiver) = self.native_receivers.remove_receiver(reference) else {
            return;
        };
        self.record_native_receiver_binding(reference);
        self.close_native_receiver_transport(&receiver);
        let release_in_completion = receiver
            .preparation
            .as_ref()
            .is_some_and(|(work, _)| work.scope().sequence == receiver.storage_sequence);
        if let Some((work, _)) = receiver.preparation {
            self.complete_native_preparation(work, Err(code.into()));
        }
        if !release_in_completion {
            self.record_native_resource_observation(
                crate::participant::policy::ResourceObservation::StorageReleased {
                    binding: reference,
                    sequence: receiver.storage_sequence,
                },
            );
        }
    }

    pub(super) fn fence_native_receiver(&mut self, reference: u64) {
        let Some(receiver) = self.native_receivers.receivers.get_mut(&reference) else {
            return;
        };
        if receiver.retiring {
            return;
        }
        receiver.enabled = false;
        if self.apply_native_receiver_gate(reference, false).is_err() {
            self.disconnect(DisconnectReason::SignalingOutput);
        }
    }

    pub(super) fn retire_native_receiver(
        &mut self,
        reference: u64,
        events: &mut impl ParticipantSink,
    ) {
        if self
            .native_receivers
            .receivers
            .get(&reference)
            .is_some_and(|receiver| receiver.retiring)
        {
            return;
        }
        let Some(mut receiver) = self.native_receivers.remove_receiver(reference) else {
            return;
        };
        self.close_native_receiver_transport(&receiver);
        receiver.enabled = false;
        receiver.retiring = true;
        receiver.handle = None;
        receiver.deadline = None;
        let preparation = receiver.preparation.take();
        // Keep retirement bookkeeping funded until the route receipt removes it.
        self.native_receivers.receivers.insert(reference, receiver);
        self.record_native_receiver_binding(reference);
        if let Some((work, _)) = preparation {
            self.complete_native_preparation(work, Err(ExecutionFailureKind::Retired.into()));
        }
        events.unsubscribe_owned(reference);
    }

    pub(super) fn complete_native_receiver_cutoff(&mut self, reference: u64, success: bool) {
        if !self
            .native_receivers
            .receivers
            .get(&reference)
            .is_some_and(|receiver| receiver.retiring)
        {
            return;
        }
        if !success {
            self.disconnect(DisconnectReason::SignalingOutput);
            return;
        }
        self.native_receivers.remove_receiver(reference);
        self.record_native_resource_observation(
            crate::participant::policy::ResourceObservation::Released { binding: reference },
        );
    }

    pub(super) fn native_receiver_message(
        &mut self,
        reference: u64,
        bytes: &[u8],
        events: &mut impl ParticipantSink,
    ) {
        let Some(receiver) = self.native_receivers.receivers.get(&reference) else {
            return;
        };
        if !receiver
            .transport
            .data()
            .is_some_and(|(_, lane)| lane == DataLane::Ordered)
            || !self.native_receivers.receiver_ready(receiver)
        {
            return;
        }
        let Ok(frame) = TopicFrame::decode_bounded(bytes) else {
            return;
        };
        if frame.is_subscriber_control()
            && frame.data_track_id() == Some(receiver.source_id.as_str())
            && let Some(handle) = receiver.handle
        {
            events.request_reverse(handle, ReversePacket::reliable_control(bytes.to_vec()));
        }
    }

    pub(in crate::participant::core) fn native_data_delivery(
        &mut self,
        handle: TrackHandle,
        lane: DataLane,
        bytes: &[u8],
    ) {
        if self.native_receivers.received.is_empty()
            || self.disconnect_reason.is_some()
            || self
                .authorization_expiry
                .is_none_or(|expiry| expiry.is_expired_at(unix_now()))
        {
            return;
        }
        let Ok(frame) = TopicFrame::decode_bounded(bytes) else {
            return;
        };
        if !frame.is_publisher_output() {
            return;
        }
        let required = lane == DataLane::Ordered
            && !matches!(
                frame.body,
                Some(pulsebeam_proto::topic::topic_frame::Body::Message(_))
            );
        let key = handle.data().as_ffi();
        let mut failed = false;
        for (_, reference) in self
            .native_receivers
            .received
            .range((Included((key, 0)), Included((key, u64::MAX))))
        {
            let Some(receiver) = self.native_receivers.receivers.get(reference) else {
                continue;
            };
            let Some((channel, received_lane)) = receiver.transport.data() else {
                continue;
            };
            if received_lane == lane
                && self.native_receivers.receiver_ready(receiver)
                && frame.data_track_id() == Some(receiver.source_id.as_str())
            {
                failed |= !self.transport.enqueue_mutation(TransportMutation::Data {
                    channel,
                    bytes: bytes.to_vec(),
                    required,
                }) && required;
            }
        }
        if failed {
            self.disconnect(DisconnectReason::SignalingOutput);
        }
    }

    pub(in crate::participant::core) fn native_rtp_delivery(
        &mut self,
        handle: TrackHandle,
        packet: &crate::rtp::RtpPacket,
        cache: Option<&crate::rtp::cache::TrackStreamCache>,
    ) {
        if self.native_receivers.received.is_empty()
            || self.disconnect_reason.is_some()
            || self
                .authorization_expiry
                .is_none_or(|expiry| expiry.is_expired_at(unix_now()))
        {
            return;
        }
        let key = handle.data().as_ffi();
        let receivers = &mut self.native_receivers;
        for (_, reference) in receivers
            .received
            .range((Included((key, 0)), Included((key, u64::MAX))))
        {
            let Some(receiver) = receivers.receivers.get_mut(reference) else {
                continue;
            };
            let ready = receiver.enabled
                && !receiver.failed
                && receiver.opened
                && !receiver.retiring
                && receivers
                    .sources
                    .get(receiver.source.as_bytes())
                    .is_some_and(|source| {
                        !source.retired
                            && source.bound
                            && source.handle == Some(handle)
                            && receiver.handle == Some(handle)
                    });
            if ready && let ReceiveTransport::Rtp(rtp) = &mut receiver.transport {
                if rtp.video_options.is_some() {
                    self.downstream.video.on_owned_rtp(
                        rtp.mid(),
                        receiver.source,
                        packet.arrival_ts,
                        cache,
                        &mut self.stream_writer,
                    );
                } else {
                    self.downstream.audio.on_owned_rtp(
                        rtp.mid(),
                        receiver.source,
                        packet,
                        &mut self.stream_writer,
                    );
                }
            }
        }
    }
}

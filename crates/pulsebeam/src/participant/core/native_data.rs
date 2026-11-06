use slotmap::Key as _;
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use super::*;
use crate::control::native::{
    DataPublication as DataPublicationPlan, ExecutionAction, ExecutionCommand, ExecutionCompletion,
    ExecutionDeferral, Preparation, PreparedResource, Publication, ResourceExecution,
};
use crate::participant::policy::{ExecutionFailure, ExecutionFailureKind, ExecutionScope};
#[cfg(test)]
use pulsebeam_proto::state as wire;

#[cfg(test)]
pub(super) mod tests;

#[derive(Default)]
pub(super) struct NativeExecution {
    sequence: u64,
    pending: BTreeMap<u64, NativeExecutionState>,
}

enum NativeExecutionState {
    Preparing(ExecutionScope),
    Fenced,
}

impl NativeExecution {
    pub(super) fn begin(&mut self, scope: ExecutionScope) -> bool {
        if !self.advance(scope) {
            return false;
        }
        self.pending
            .insert(scope.binding, NativeExecutionState::Preparing(scope));
        true
    }

    pub(super) fn advance(&mut self, scope: ExecutionScope) -> bool {
        if scope.sequence <= self.sequence {
            return false;
        }
        self.sequence = scope.sequence;
        !self.is_fenced(scope.binding)
    }

    pub(super) fn is_fenced(&self, binding: u64) -> bool {
        matches!(
            self.pending.get(&binding),
            Some(NativeExecutionState::Fenced)
        )
    }

    pub(super) fn contains(&self, scope: ExecutionScope) -> bool {
        matches!(self.pending.get(&scope.binding), Some(NativeExecutionState::Preparing(current)) if *current == scope)
    }

    pub(super) fn fence(&mut self, binding: u64) {
        self.pending.insert(binding, NativeExecutionState::Fenced);
    }

    pub(super) fn finish(&mut self, scope: ExecutionScope) {
        if self.contains(scope) {
            self.pending.remove(&scope.binding);
        }
    }

    pub(super) fn retire(&mut self, scope: ExecutionScope) -> bool {
        if scope.sequence <= self.sequence {
            return false;
        }
        self.sequence = scope.sequence;
        self.pending.remove(&scope.binding);
        true
    }
}

struct DataPublication {
    channel: crate::media::ChannelId,
    track: Track,
    source_id: String,
    fanout: Option<TrackHandle>,
    enabled: bool,
    opened: bool,
    installation: pulsebeam_rtc::DataChannelInstallation,
    published: bool,
    retiring: bool,
    preparation: Option<Preparation>,
    storage_sequence: u64,
    deadline: Option<Instant>,
}

#[derive(Default)]
pub(super) struct NativeData {
    publications: BTreeMap<u64, DataPublication>,
    pub(super) channels: BTreeMap<crate::media::ChannelId, u64>,
    tracks: BTreeMap<[u8; 16], u64>,
    fanouts: BTreeMap<u64, u64>,
    deadlines: BTreeSet<(Instant, u64)>,
}

impl NativeData {
    pub(super) fn owns_fanout(&self, handle: TrackHandle) -> bool {
        self.fanouts.contains_key(&handle.data().as_ffi())
    }

    pub(super) fn can_write(&self, channel: crate::media::ChannelId) -> bool {
        self.channels
            .get(&channel)
            .and_then(|reference| self.publications.get(reference))
            .is_some_and(|publication| {
                publication.enabled && publication.published && !publication.retiring
            })
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadlines.first().map(|(deadline, _)| *deadline)
    }

    pub(super) fn bind_source(&mut self, track: TrackId, handle: TrackHandle) {
        if let Some(reference) = self.tracks.get(track.as_bytes())
            && let Some(publication) = self.publications.get_mut(reference)
            && publication.published
        {
            if let Some(previous) = publication.fanout
                && self.fanouts.get(&previous.data().as_ffi()) == Some(reference)
            {
                self.fanouts.remove(&previous.data().as_ffi());
            }
            self.fanouts.insert(handle.data().as_ffi(), *reference);
            publication.fanout = Some(handle);
        }
    }

    fn remove(&mut self, reference: u64) -> Option<DataPublication> {
        let publication = self.publications.remove(&reference)?;
        if self.channels.get(&publication.channel) == Some(&reference) {
            self.channels.remove(&publication.channel);
        }
        if self.tracks.get(publication.track.id().as_bytes()) == Some(&reference) {
            self.tracks.remove(publication.track.id().as_bytes());
        }
        if let Some(handle) = publication.fanout
            && self.fanouts.get(&handle.data().as_ffi()) == Some(&reference)
        {
            self.fanouts.remove(&handle.data().as_ffi());
        }
        if let Some(deadline) = publication.deadline {
            self.deadlines.remove(&(deadline, reference));
        }
        Some(publication)
    }
}

pub(super) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn execution_bytes(label: &str) -> usize {
    // Prepay full B-tree node backing, including sparse-node slack, for every index.
    std::mem::size_of::<(u64, DataPublication)>()
        .saturating_add(std::mem::size_of::<(crate::media::ChannelId, u64)>())
        .saturating_add(std::mem::size_of::<([u8; 16], u64)>())
        .saturating_add(std::mem::size_of::<(u64, u64)>())
        .saturating_add(std::mem::size_of::<(Instant, u64)>())
        .saturating_add(128)
        .saturating_mul(12)
        .saturating_add(Connection::native_execution_metadata_bytes())
        .saturating_add(128)
        .saturating_add(label.len().saturating_mul(4))
}

impl Connection {
    pub(crate) fn native_execution_metadata_bytes() -> usize {
        std::mem::size_of::<(u64, NativeExecutionState)>().saturating_mul(12)
    }

    pub(crate) fn native_data_execution_bytes(label: &str) -> usize {
        execution_bytes(label)
    }

    pub(super) fn admit_native_preparation(&mut self, work: Preparation) -> bool {
        if !self.native_execution.begin(work.scope()) {
            if !self.native_execution.contains(work.scope()) {
                self.defer_native_preparation(work);
            }
            return false;
        }
        if !self.native_execution_current(work.scope(), unix_now()) {
            self.defer_native_preparation(work);
            return false;
        }
        true
    }

    pub(super) fn native_execution_current(&self, scope: ExecutionScope, now: u64) -> bool {
        self.native_execution.contains(scope) && self.native_scope_authorized(scope, now)
    }

    pub(super) fn native_scope_authorized(&self, scope: ExecutionScope, now: u64) -> bool {
        scope.authorization == self.authorization_revision
            && self
                .authorization_expiry
                .is_none_or(|expiry| !expiry.is_expired_at(now))
            && self.disconnect_reason.is_none()
    }

    pub(super) fn native_callback_fenced(&self, binding: u64) -> bool {
        self.disconnect_reason.is_some() || self.native_execution.is_fenced(binding)
    }

    fn native_preparation_storage(&self, work: Preparation) -> Option<u64> {
        self.native_data
            .publications
            .get(&work.reference)
            .map(|publication| publication.storage_sequence)
            .or_else(|| self.native_receivers.storage_sequence(work.reference))
            .or_else(|| {
                self.native_rtp
                    .owns_reference(work.reference)
                    .then_some(work.scope().sequence)
            })
    }

    pub(super) fn defer_native_preparation(&mut self, work: Preparation) {
        self.native_execution.finish(work.scope());
        let deferral = ExecutionDeferral {
            work,
            retained_sequence: self.native_preparation_storage(work),
        };
        if let Some(handoff) = &self.native_handoff {
            if !handoff.fact(Fact::Deferral(deferral)) {
                self.disconnect(DisconnectReason::SignalingOutput);
            }
            return;
        }
    }

    pub(super) fn current_native_preparation(&mut self, work: Preparation) -> Option<Preparation> {
        let now = unix_now();
        if !self.native_execution_current(work.scope(), now) {
            self.defer_native_preparation(work);
            return None;
        }
        Some(work)
    }

    pub(super) fn native_preparation_time(&self, work: Preparation) -> Option<u64> {
        let now = unix_now();
        self.native_execution_current(work.scope(), now)
            .then_some(now)
    }

    pub(super) fn finish_native_preparation(&mut self, work: Preparation) -> Option<u64> {
        let Some(now) = self.native_preparation_time(work) else {
            self.defer_native_preparation(work);
            return None;
        };
        self.native_execution.finish(work.scope());
        Some(now)
    }

    pub(super) fn complete_native_preparation(
        &mut self,
        work: Preparation,
        result: Result<ResourceExecution, ExecutionFailure>,
    ) {
        if let Some(now) = self.finish_native_preparation(work) {
            self.record_native_preparation(work, result, now);
        }
    }

    pub(super) fn record_native_preparation(
        &mut self,
        work: Preparation,
        result: Result<ResourceExecution, ExecutionFailure>,
        now: u64,
    ) {
        let released = result.is_err() && self.native_preparation_storage(work).is_none();
        let completion = ExecutionCompletion {
            work,
            result,
            released,
            unix_now: now,
        };
        if let Some(handoff) = &self.native_handoff {
            if !handoff.fact(Fact::Completion(completion)) {
                self.disconnect(DisconnectReason::SignalingOutput);
            }
            return;
        }
    }

    pub(super) fn record_native_resource_observation(
        &mut self,
        observation: crate::participant::policy::ResourceObservation,
    ) {
        if let Some(handoff) = &self.native_handoff
            && !handoff.fact(Fact::Resource(observation))
        {
            self.disconnect(DisconnectReason::SignalingOutput);
        }
    }

    pub(super) fn record_native_publication(
        &mut self,
        work: Preparation,
        execution: crate::participant::policy::PublicationExecution,
        now: u64,
    ) {
        self.record_native_preparation(
            work,
            Ok(ResourceExecution::Publication(Box::new(execution))),
            now,
        );
    }

    pub(super) fn execute_native_command(
        &mut self,
        command: ExecutionCommand,
        events: &mut impl ParticipantSink,
    ) {
        match command.into_action() {
            ExecutionAction::Prepare { work, resource } => {
                if !self.admit_native_preparation(work) {
                    return;
                }
                let Some(work) = self.current_native_preparation(work) else {
                    return;
                };
                if let Some(publication) = self.native_data.publications.get_mut(&work.reference) {
                    publication.storage_sequence = work.scope().sequence;
                }
                self.native_receivers
                    .transfer_storage(work.reference, work.scope().sequence);
                self.execute_native_resource(work, resource, events);
            }
            ExecutionAction::Retire(scope) => self.apply_native_retirement(scope, events),
            ExecutionAction::FailReceiver(scope) => {
                self.apply_native_receiver_failure(scope, events);
            }
        }
    }

    fn execute_native_resource(
        &mut self,
        work: Preparation,
        command: PreparedResource,
        events: &mut impl ParticipantSink,
    ) {
        let plan = match command {
            PreparedResource::Publication(Publication::Data(plan)) => plan,
            PreparedResource::Publication(Publication::Rtp(plan)) => {
                self.prepare_native_rtp(work, *plan, events);
                return;
            }
            PreparedResource::Receiver(plan) => {
                self.execute_native_receiver(work, plan, events);
                return;
            }
        };
        let DataPublicationPlan {
            label,
            lane,
            channel,
            enabled,
        } = plan;
        if let Some(existing) = self.native_data.publications.get(&work.reference) {
            if existing.channel == channel && existing.opened {
                self.commit_native_data(work, enabled, events);
            } else {
                self.complete_native_preparation(work, Err(ExecutionFailureKind::Conflict.into()));
            }
            return;
        }
        if self.native_data.channels.contains_key(&channel)
            || self.native_receivers.channels.contains_key(&channel)
        {
            self.complete_native_preparation(work, Err(ExecutionFailureKind::Conflict.into()));
            return;
        }
        let Some(incarnation) = NonZeroU64::new(work.reference) else {
            self.complete_native_preparation(work, Err(ExecutionFailureKind::Invalid.into()));
            return;
        };
        let installation = match self.transport.open_native_data(channel, lane) {
            Ok(installation) => installation,
            Err(code) => {
                self.complete_native_preparation(work, Err(code.into()));
                return;
            }
        };
        let id =
            self.connection_id
                .derive_publication_track_id(TrackKind::Data, &label, incarnation);
        let track = Track::data_publication(
            track::TrackMeta {
                room_id: self.room_id,
                shard_id: self.shard_id,
                id,
                origin: self.participant_id,
                connection_id: self.connection_id,
                label: Some(label),
            },
            lane,
        );
        let reference = work.reference;
        let now = Instant::now();
        let deadline = now.checked_add(Duration::from_secs(30)).unwrap_or(now);
        self.native_data.channels.insert(channel, reference);
        self.native_data.tracks.insert(*id.as_bytes(), reference);
        self.native_data.deadlines.insert((deadline, reference));
        self.native_data.publications.insert(
            reference,
            DataPublication {
                channel,
                track,
                source_id: id.as_str(),
                fanout: None,
                enabled,
                opened: false,
                installation,
                published: false,
                retiring: false,
                preparation: Some(work),
                storage_sequence: work.scope().sequence,
                deadline: Some(deadline),
            },
        );
    }

    pub(super) fn native_data_opened(
        &mut self,
        channel: crate::media::ChannelId,
        config: pulsebeam_rtc::DataChannelConfig,
        events: &mut impl ParticipantSink,
    ) {
        let Some(reference) = self
            .native_data
            .channels
            .get(&channel)
            .or_else(|| self.native_receivers.channels.get(&channel))
            .copied()
        else {
            self.transport.close_native_data(channel);
            return;
        };
        if self.native_receivers.receivers.contains_key(&reference) {
            self.native_receiver_opened(reference, config, events);
            return;
        }
        let Some(publication) = self.native_data.publications.get_mut(&reference) else {
            return;
        };
        if !matches!(
            self.transport
                .data_channel_installation(publication.installation),
            Some(pulsebeam_rtc::DataChannelInstallationState::Open { .. })
        ) {
            return;
        }
        let Track::Data(data) = &publication.track else {
            return;
        };
        if config != super::super::transport::native_data_config(channel, data.lane) {
            if publication.published {
                self.native_data_closed(channel, events);
            } else {
                self.fail_native_data(reference, ExecutionFailureKind::Conflict);
            }
            return;
        }
        publication.opened = true;
        if let Some(deadline) = publication.deadline.take() {
            self.native_data.deadlines.remove(&(deadline, reference));
        }
        let Some(work) = publication.preparation.take() else {
            return;
        };
        let enabled = publication.enabled;
        if self.native_callback_fenced(reference) {
            self.drop_unpublished_native_data(work);
            return;
        }
        self.commit_native_data(work, enabled, events);
        if self.native_callback_fenced(reference) {
            self.drop_unpublished_native_data(work);
        }
        self.evaluate_native_preparations(events);
    }

    fn commit_native_data(
        &mut self,
        work: Preparation,
        enabled: bool,
        events: &mut impl ParticipantSink,
    ) {
        let Some(work) = self.current_native_preparation(work) else {
            return;
        };
        let reference = work.reference;
        let Some(publication) = self.native_data.publications.get(&reference) else {
            return;
        };
        if !matches!(publication.track, Track::Data(_)) {
            return;
        }
        let execution = crate::participant::policy::PublicationExecution {
            track: publication.track.clone(),
            enabled,
            active: false,
        };
        let Some(now) = self.finish_native_preparation(work) else {
            return;
        };
        if let Some(publication) = self.native_data.publications.get_mut(&reference) {
            publication.enabled = enabled;
            if !enabled {
                self.transport
                    .discard_channel_mutations(publication.channel);
            }
            if !publication.published {
                publication.published = true;
                events.publish_track(publication.track.clone());
            }
        }
        self.record_native_publication(work, execution, now);
    }

    fn drop_unpublished_native_data(&mut self, work: Preparation) {
        let reference = work.reference;
        if self
            .native_data
            .publications
            .get(&reference)
            .is_some_and(|publication| !publication.published)
            && let Some(publication) = self.native_data.remove(reference)
        {
            self.transport
                .cancel_data_channel_installation(publication.installation);
            self.defer_native_preparation(work);
        }
    }

    fn fail_native_data(&mut self, reference: u64, code: ExecutionFailureKind) {
        if self.native_receivers.receivers.contains_key(&reference) {
            self.fail_native_receiver(reference, code);
            return;
        }
        let Some(publication) = self.native_data.remove(reference) else {
            return;
        };
        self.transport
            .cancel_data_channel_installation(publication.installation);
        let release_in_completion = publication
            .preparation
            .is_some_and(|work| work.scope().sequence == publication.storage_sequence);
        if let Some(work) = publication.preparation {
            self.complete_native_preparation(work, Err(code.into()));
        }
        if !release_in_completion {
            self.record_native_resource_observation(
                crate::participant::policy::ResourceObservation::StorageReleased {
                    binding: reference,
                    sequence: publication.storage_sequence,
                },
            );
        }
    }

    pub(super) fn retire_native_data(&mut self, reference: u64, events: &mut impl ParticipantSink) {
        if self.native_receivers.receivers.contains_key(&reference) {
            self.retire_native_receiver(reference, events);
            return;
        }
        if self
            .native_data
            .publications
            .get(&reference)
            .is_some_and(|publication| publication.retiring)
        {
            return;
        }
        let Some(mut publication) = self.native_data.remove(reference) else {
            self.record_native_resource_observation(
                crate::participant::policy::ResourceObservation::Released { binding: reference },
            );
            return;
        };
        self.transport
            .cancel_data_channel_installation(publication.installation);
        if let Some(work) = publication.preparation.take() {
            self.complete_native_preparation(work, Err(ExecutionFailureKind::Retired.into()));
        }
        if publication.published {
            publication.enabled = false;
            publication.fanout = None;
            publication.deadline = None;
            publication.retiring = true;
            let meta = publication.track.meta().clone();
            self.native_data.publications.insert(reference, publication);
            events.unpublish_owned_track(meta, reference);
        } else {
            self.record_native_resource_observation(
                crate::participant::policy::ResourceObservation::Released { binding: reference },
            );
        }
    }

    pub(super) fn complete_native_data_cutoff(&mut self, reference: u64, success: bool) {
        if self.native_receivers.receivers.contains_key(&reference) {
            self.complete_native_receiver_cutoff(reference, success);
            return;
        }
        if !self
            .native_data
            .publications
            .get(&reference)
            .is_some_and(|publication| publication.retiring)
        {
            return;
        }
        if !success {
            self.disconnect(DisconnectReason::SignalingOutput);
            return;
        }
        self.native_data.remove(reference);
        self.record_native_resource_observation(
            crate::participant::policy::ResourceObservation::Released { binding: reference },
        );
    }

    pub(super) fn fence_native_data(&mut self, reference: u64) {
        if let Some(publication) = self.native_data.publications.get_mut(&reference) {
            publication.enabled = false;
            self.transport
                .discard_channel_mutations(publication.channel);
        }
    }

    pub(super) fn native_data_closed(
        &mut self,
        channel: crate::media::ChannelId,
        _events: &mut impl ParticipantSink,
    ) {
        let Some(reference) = self
            .native_data
            .channels
            .get(&channel)
            .or_else(|| self.native_receivers.channels.get(&channel))
            .copied()
        else {
            return;
        };
        let installation = self
            .native_data
            .publications
            .get(&reference)
            .map(|publication| publication.installation)
            .or_else(|| self.native_receivers.channel_installation(reference));
        if installation.is_some_and(|installation| {
            matches!(
                self.transport.data_channel_installation(installation),
                Some(
                    pulsebeam_rtc::DataChannelInstallationState::Pending
                        | pulsebeam_rtc::DataChannelInstallationState::Open { .. }
                )
            )
        }) {
            return;
        }
        if self
            .native_data
            .publications
            .get(&reference)
            .is_some_and(|publication| !publication.opened)
            || self
                .native_receivers
                .receivers
                .get(&reference)
                .is_some_and(|receiver| !receiver.opened)
        {
            self.fail_native_data(reference, ExecutionFailureKind::Conflict);
            return;
        }
        self.native_resource_ended(reference);
    }

    pub(super) fn native_data_installation_failed(
        &mut self,
        failed: pulsebeam_rtc::DataChannelInstallation,
    ) {
        let channel = failed.channel();
        let reference = self
            .native_data
            .channels
            .get(&channel)
            .or_else(|| self.native_receivers.channels.get(&channel))
            .copied();
        let Some(reference) = reference else { return };
        let installation = self
            .native_data
            .publications
            .get(&reference)
            .filter(|publication| !publication.opened)
            .map(|publication| publication.installation)
            .or_else(|| {
                self.native_receivers
                    .receivers
                    .get(&reference)
                    .filter(|receiver| !receiver.opened)
                    .and_then(|receiver| receiver.installation)
            });
        if installation == Some(failed) {
            self.fail_native_data(reference, ExecutionFailureKind::Conflict);
        }
    }

    pub(super) fn expire_native_data(&mut self, now: Instant, events: &mut impl ParticipantSink) {
        for _ in 0..16 {
            let Some((deadline, reference)) = self
                .native_data
                .deadlines
                .first()
                .into_iter()
                .chain(self.native_receivers.deadlines.first())
                .min()
                .copied()
            else {
                break;
            };
            if deadline > now {
                break;
            }
            self.fail_native_data(reference, ExecutionFailureKind::Internal);
        }
        self.evaluate_native_preparations(events);
    }

    pub(super) fn native_data_reverse(&mut self, stream: TrackHandle, bytes: Vec<u8>) {
        if self.disconnect_reason.is_some()
            || self
                .authorization_expiry
                .is_none_or(|expiry| expiry.is_expired_at(unix_now()))
        {
            return;
        }
        let Some(reference) = self.native_data.fanouts.get(&stream.data().as_ffi()) else {
            return;
        };
        let Some(publication) = self.native_data.publications.get(reference) else {
            return;
        };
        let Track::Data(data) = &publication.track else {
            return;
        };
        if !publication.enabled || !publication.published || data.lane != DataLane::Ordered {
            return;
        }
        let Ok(frame) = TopicFrame::decode_bounded(&bytes) else {
            return;
        };
        if !frame.is_subscriber_control()
            || frame.data_track_id() != Some(publication.source_id.as_str())
        {
            return;
        }
        self.enqueue_fanout(TransportMutation::Data {
            channel: publication.channel,
            bytes,
            required: true,
        });
    }

    pub(super) fn native_data_message(
        &mut self,
        channel: crate::media::ChannelId,
        bytes: &[u8],
        events: &mut impl ParticipantSink,
    ) {
        if self.disconnect_reason.is_some()
            || self
                .authorization_expiry
                .is_none_or(|expiry| expiry.is_expired_at(unix_now()))
        {
            return;
        }
        let Some(reference) = self
            .native_data
            .channels
            .get(&channel)
            .or_else(|| self.native_receivers.channels.get(&channel))
            .copied()
        else {
            return;
        };
        if self.native_receivers.receivers.contains_key(&reference) {
            self.native_receiver_message(reference, bytes, events);
            return;
        }
        let Some(publication) = self.native_data.publications.get(&reference) else {
            return;
        };
        let Track::Data(data) = &publication.track else {
            return;
        };
        let Ok(frame) = TopicFrame::decode_bounded(bytes) else {
            return;
        };
        if frame.data_track_id() != Some(publication.source_id.as_str())
            || !frame.is_publisher_output()
            || (data.lane != DataLane::Ordered
                && !matches!(
                    frame.body,
                    Some(pulsebeam_proto::topic::topic_frame::Body::Message(_))
                ))
        {
            return;
        }
        if publication.enabled
            && publication.published
            && let Some(fanout) = publication.fanout
        {
            events.publish_track_packet(
                Some(fanout),
                TrackPacket::Data {
                    lane: data.lane,
                    bytes: bytes.to_vec(),
                },
            );
        }
    }
}

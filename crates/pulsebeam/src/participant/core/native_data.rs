use slotmap::Key as _;
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use super::*;
use crate::participant::native::{OutputError, Preparation};
use pulsebeam_proto::state::{self as wire, resource_spec};

#[cfg(test)]
pub(super) mod tests;

struct DataPublication {
    channel: crate::media::ChannelId,
    track: Track,
    source_id: String,
    fanout: Option<TrackHandle>,
    enabled: bool,
    opened: bool,
    reset_pending: bool,
    published: bool,
    retiring: bool,
    preparation: Option<Preparation>,
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

fn data_declaration(
    spec: &wire::ResourceSpec,
) -> Option<(&str, DataLane, crate::media::ChannelId)> {
    let resource_spec::Declaration::Publication(publication) = spec.declaration.as_ref()? else {
        return None;
    };
    let wire::publication_spec::Kind::Data(data) = publication.kind.as_ref()? else {
        return None;
    };
    let lane = match wire::DataDelivery::try_from(data.delivery).ok()? {
        wire::DataDelivery::Latest => DataLane::Latest,
        wire::DataDelivery::Lossy => DataLane::Lossy,
        wire::DataDelivery::Ordered => DataLane::Ordered,
        _ => return None,
    };
    let wire::track_binding::Transport::Sctp(binding) =
        data.binding.as_ref()?.transport.as_ref()?
    else {
        return None;
    };
    let channel = crate::media::ChannelId::from_value(u16::try_from(binding.stream_id?).ok()?);
    Some((publication.label.as_str(), lane, channel))
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
        .saturating_add(128)
        .saturating_add(label.len().saturating_mul(4))
}

impl Participant {
    pub(super) fn current_native_preparation(&mut self, work: Preparation) -> Option<Preparation> {
        match self.signaling.preparation_current(&work, unix_now()) {
            Ok(true) => Some(work),
            Ok(false) => {
                self.complete_native_preparation(
                    work,
                    Err(failure(wire::FailureCode::ResourceGone)),
                );
                None
            }
            Err(OutputError::AuthorizationExpired) => {
                if self.signaling.defer_preparation(work).is_err() {
                    self.disconnect(DisconnectReason::SignalingOutput);
                }
                None
            }
            Err(_) => {
                self.disconnect(DisconnectReason::SignalingOutput);
                None
            }
        }
    }

    pub(super) fn complete_native_preparation(
        &mut self,
        work: Preparation,
        result: Result<wire::LiveResource, wire::Failure>,
    ) -> bool {
        match self.signaling.complete(work, result, unix_now()) {
            Ok(accepted) => accepted,
            Err(_) => {
                self.disconnect(DisconnectReason::SignalingOutput);
                false
            }
        }
    }

    pub(super) fn prepare_native_resource(
        &mut self,
        work: Preparation,
        events: &mut impl ParticipantSink,
    ) {
        let Some((label, lane, channel)) = data_declaration(&work.spec) else {
            if matches!(
                work.spec.declaration.as_ref(),
                Some(resource_spec::Declaration::Publication(_))
            ) {
                self.prepare_native_rtp(work, events);
            } else {
                self.prepare_native_receiver(work, events);
            }
            return;
        };
        if let Some(existing) = self.native_data.publications.get(&work.reference) {
            if existing.channel == channel && existing.opened {
                self.commit_native_data(work, events);
            } else {
                self.complete_native_preparation(
                    work,
                    Err(failure(wire::FailureCode::Incompatible)),
                );
            }
            return;
        }
        if self.native_data.channels.contains_key(&channel)
            || self.native_receivers.channels.contains_key(&channel)
        {
            self.complete_native_preparation(work, Err(failure(wire::FailureCode::Incompatible)));
            return;
        }
        let Some(incarnation) = NonZeroU64::new(work.reference) else {
            self.complete_native_preparation(
                work,
                Err(failure(wire::FailureCode::InvalidConfiguration)),
            );
            return;
        };
        if self
            .signaling
            .reserve_execution(work.reference, execution_bytes(label))
            .is_err()
        {
            self.complete_native_preparation(
                work,
                Err(failure(wire::FailureCode::CapacityExceeded)),
            );
            return;
        }
        let reset_pending = match self.transport.open_native_data(channel, lane) {
            Ok(state) => state == super::super::transport::NativeDataOpen::ResetPending,
            Err(code) => {
                let _ = self.signaling.reserve_execution(work.reference, 0);
                self.complete_native_preparation(work, Err(failure(code)));
                return;
            }
        };
        let id =
            self.connection_id
                .derive_publication_track_id(TrackKind::Data, label, incarnation);
        let track = Track::data_publication(
            track::TrackMeta {
                room_id: self.room_id,
                shard_id: self.shard_id,
                id,
                origin: self.participant_id,
                connection_id: self.connection_id,
                label: Some(label.to_owned()),
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
                enabled: false,
                opened: false,
                reset_pending,
                published: false,
                retiring: false,
                preparation: Some(work),
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
        if publication.reset_pending {
            return;
        }
        let Track::Data(data) = &publication.track else {
            return;
        };
        if config != super::super::transport::native_data_config(channel, data.lane) {
            if publication.published {
                self.native_data_closed(channel, events);
            } else {
                self.fail_native_data(reference, wire::FailureCode::Incompatible);
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
        self.commit_native_data(work, events);
        if !self.signaling.owns_lifetime(reference) {
            self.drop_unpublished_native_data(reference);
        }
        self.evaluate_native_preparations(events);
    }

    fn commit_native_data(&mut self, work: Preparation, events: &mut impl ParticipantSink) {
        let Some(work) = self.current_native_preparation(work) else {
            return;
        };
        let reference = work.reference;
        let Some(publication) = self.native_data.publications.get(&reference) else {
            return;
        };
        let Track::Data(data) = &publication.track else {
            return;
        };
        let enabled = work
            .spec
            .settings
            .as_ref()
            .is_some_and(|settings| settings.enabled == Some(true));
        let delivery = match data.lane {
            DataLane::Latest => wire::DataDelivery::Latest,
            DataLane::Lossy => wire::DataDelivery::Lossy,
            DataLane::Ordered => wire::DataDelivery::Ordered,
        };
        let live = wire::LiveResource {
            spec: Some(work.spec.clone()),
            participant: Some(wire::Participant {
                participant_id: self.participant_id.as_str(),
                participant_external_id: self.participant_external_id.as_str().to_owned(),
            }),
            track: Some(wire::Track {
                track_id: publication.track.id().as_str(),
                participant_id: self.participant_id.as_str(),
                label: publication.track.meta().label.clone().unwrap_or_default(),
                kind: Some(wire::track::Kind::Data(wire::DataTrack {
                    delivery: delivery as i32,
                })),
            }),
            execution: if enabled {
                wire::ExecutionStatus::Installed
            } else {
                wire::ExecutionStatus::Disabled
            } as i32,
            execution_failure: None,
            activity: None,
        };
        if !self.complete_native_preparation(work, Ok(live)) {
            return;
        }
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
    }

    fn drop_unpublished_native_data(&mut self, reference: u64) {
        if self
            .native_data
            .publications
            .get(&reference)
            .is_some_and(|publication| !publication.published)
            && let Some(publication) = self.native_data.remove(reference)
        {
            self.transport.close_native_data(publication.channel);
            let _ = self.signaling.reserve_execution(reference, 0);
        }
    }

    fn fail_native_data(&mut self, reference: u64, code: wire::FailureCode) {
        if self.native_receivers.receivers.contains_key(&reference)
            || self.native_receivers.waiting.contains_key(&reference)
        {
            self.fail_native_receiver(reference, code);
            return;
        }
        let Some(publication) = self.native_data.remove(reference) else {
            return;
        };
        self.transport.close_native_data(publication.channel);
        let _ = self.signaling.reserve_execution(reference, 0);
        if let Some(work) = publication.preparation {
            self.complete_native_preparation(work, Err(failure(code)));
        }
    }

    pub(super) fn retire_native_data(&mut self, reference: u64, events: &mut impl ParticipantSink) {
        if self.native_receivers.waiting.contains_key(&reference) {
            self.fail_native_receiver(reference, wire::FailureCode::ResourceGone);
            if self.signaling.cutoff_complete(reference).is_err() {
                self.disconnect(DisconnectReason::SignalingOutput);
            }
            return;
        }
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
            if self.signaling.cutoff_complete(reference).is_err() {
                self.disconnect(DisconnectReason::SignalingOutput);
            }
            return;
        };
        self.transport.close_native_data(publication.channel);
        if let Some(work) = publication.preparation.take() {
            self.complete_native_preparation(work, Err(failure(wire::FailureCode::ResourceGone)));
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
            let _ = self.signaling.reserve_execution(reference, 0);
            if self.signaling.cutoff_complete(reference).is_err() {
                self.disconnect(DisconnectReason::SignalingOutput);
            }
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
        if self.signaling.reserve_execution(reference, 0).is_err()
            || self.signaling.cutoff_complete(reference).is_err()
        {
            self.disconnect(DisconnectReason::SignalingOutput);
        }
    }

    pub(super) fn native_data_closed(
        &mut self,
        channel: crate::media::ChannelId,
        events: &mut impl ParticipantSink,
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
        // Reset-pending replacements do not own the previous lifetime's notices.
        if self
            .native_data
            .publications
            .get(&reference)
            .is_some_and(|publication| publication.reset_pending)
            || self.native_receivers.reset_pending(reference)
        {
            return;
        }
        if self.signaling.source_ended(reference).is_err() {
            self.disconnect(DisconnectReason::SignalingOutput);
            return;
        }
        self.retire_native_data(reference, events);
    }

    pub(super) fn native_data_reset(
        &mut self,
        channel: crate::media::ChannelId,
        success: bool,
        events: &mut impl ParticipantSink,
    ) {
        let Some(reference) = self.native_data.channels.get(&channel).copied() else {
            self.native_receiver_reset(channel, success, events);
            return;
        };
        if !self
            .native_data
            .publications
            .get(&reference)
            .is_some_and(|publication| publication.reset_pending)
        {
            return;
        }
        if !success {
            self.fail_native_data(reference, wire::FailureCode::Incompatible);
            return;
        }
        let Some(publication) = self.native_data.publications.get_mut(&reference) else {
            return;
        };
        let Track::Data(data) = &publication.track else {
            return;
        };
        let lane = data.lane;
        let Some(work) = publication.preparation.take() else {
            return;
        };
        let Some(work) = self.current_native_preparation(work) else {
            self.fail_native_data(reference, wire::FailureCode::ResourceGone);
            return;
        };
        let Some(publication) = self.native_data.publications.get_mut(&reference) else {
            return;
        };
        publication.preparation = Some(work);
        match self.transport.open_native_data(channel, lane) {
            Ok(state) => {
                publication.reset_pending =
                    state == super::super::transport::NativeDataOpen::ResetPending;
            }
            Err(code) => self.fail_native_data(reference, code),
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
            self.fail_native_data(reference, wire::FailureCode::InternalFailure);
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

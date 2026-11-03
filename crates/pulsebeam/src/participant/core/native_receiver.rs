use super::native_data::{execution_bytes, unix_now};
use super::*;
use crate::participant::native::Preparation;
use pulsebeam_proto::{
    prelude::Message as _,
    state::{self as wire, resource_spec},
};
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
    pub(super) waiting: BTreeMap<u64, WaitingSource>,
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
    participant: wire::Participant,
    track: wire::Track,
    handle: Option<TrackHandle>,
    enabled: bool,
    opened: bool,
    pub(super) retiring: bool,
    preparation: Option<Preparation>,
    deadline: Option<Instant>,
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

pub(super) struct WaitingSource {
    work: Preparation,
    source: TrackId,
    deadline: Instant,
}

struct Source {
    handle: Option<TrackHandle>,
    track: Track,
    bound: bool,
    visible: bool,
    retired: bool,
    queued: bool,
    cursor: u64,
}

impl NativeReceivers {
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadlines.first().map(|(deadline, _)| *deadline)
    }

    pub(super) fn can_write(&self, channel: crate::media::ChannelId) -> bool {
        self.channels
            .get(&channel)
            .and_then(|reference| self.receivers.get(reference))
            .is_some_and(|receiver| self.receiver_ready(receiver))
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
            && rtp.binding.codec.payload_type == u8::from(pt)
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
        if !referenced && self.sources.get(&id).is_some_and(|source| !source.visible) {
            self.queue_source(id);
        }
        debug_assert!(
            self.sources
                .get(&id)
                .is_none_or(|source| source.visible || referenced || source.queued)
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

impl Participant {
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
                visible: true,
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
        source.visible = true;
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
            source.visible = false;
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
            if track.kind() == TrackKind::Video {
                if bound {
                    self.downstream.activate_track_binding(handle, track);
                } else {
                    self.downstream.deactivate_track_binding(handle, track);
                }
            }
            self.native_receivers.queue_source(id);
        }
    }

    pub(in crate::participant::core) fn advance_native_source(
        &mut self,
        events: &mut impl ParticipantSink,
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
            if !source.visible
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
            if self.signaling.source_ended(reference).is_err() {
                self.disconnect(DisconnectReason::SignalingOutput);
            }
            self.retire_native_data(reference, events);
            return true;
        }
        if let Some(waiting) = self.native_receivers.waiting.remove(&reference) {
            self.native_receivers.source_refs.remove(&(id, reference));
            self.native_receivers
                .deadlines
                .remove(&(waiting.deadline, reference));
            let _ = self.signaling.reserve_execution(reference, 0);
            self.prepare_native_receiver(waiting.work, events);
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
        let enabled = receiver.enabled;
        if self.apply_native_receiver_gate(reference, ready).is_err() {
            self.disconnect(DisconnectReason::SignalingOutput);
            return true;
        }
        let status = if !enabled {
            wire::ExecutionStatus::Disabled
        } else if ready {
            wire::ExecutionStatus::Installed
        } else {
            wire::ExecutionStatus::WaitingForSource
        };
        if self.signaling.observe_execution(reference, status).is_err() {
            self.disconnect(DisconnectReason::SignalingOutput);
        }
        true
    }

    pub(super) fn prepare_native_receiver(
        &mut self,
        work: Preparation,
        events: &mut impl ParticipantSink,
    ) {
        let Some(work) = self.current_native_preparation(work) else {
            return;
        };
        let Some(resource_spec::Declaration::Subscription(subscription)) =
            work.spec.declaration.as_ref()
        else {
            self.complete_native_preparation(work, Err(failure(wire::FailureCode::Unsupported)));
            return;
        };
        if !matches!(
            wire::TrackKind::try_from(subscription.kind),
            Ok(wire::TrackKind::Audio | wire::TrackKind::Video | wire::TrackKind::Data)
        ) {
            self.complete_native_preparation(work, Err(failure(wire::FailureCode::Unsupported)));
            return;
        }
        if self
            .native_receivers
            .receivers
            .contains_key(&work.reference)
        {
            self.commit_native_receiver(work, events);
            return;
        }
        let Ok(source) = subscription.track_id.parse::<TrackId>() else {
            self.complete_native_preparation(
                work,
                Err(failure(wire::FailureCode::InvalidConfiguration)),
            );
            return;
        };
        let Some((participant, track)) = self.signaling.source(&subscription.track_id) else {
            if self
                .signaling
                .reserve_execution(work.reference, 4096)
                .is_err()
            {
                self.complete_native_preparation(
                    work,
                    Err(failure(wire::FailureCode::CapacityExceeded)),
                );
                return;
            }
            let now = Instant::now();
            let deadline = now.checked_add(Duration::from_secs(30)).unwrap_or(now);
            self.native_receivers
                .deadlines
                .insert((deadline, work.reference));
            self.native_receivers
                .source_refs
                .insert((*source.as_bytes(), work.reference));
            self.native_receivers.waiting.insert(
                work.reference,
                WaitingSource {
                    work,
                    source,
                    deadline,
                },
            );
            return;
        };
        let transport = match self.prepare_native_receiver_transport(&work.spec, source, &track) {
            Ok(transport) => transport,
            Err(code) => {
                self.complete_native_preparation(
                    work,
                    Err(wire::Failure {
                        diagnostic: "native receiver transport preparation failed".to_owned(),
                        ..failure(code)
                    }),
                );
                return;
            }
        };
        let bytes = execution_bytes(&track.label)
            .saturating_add(participant.encoded_len())
            .saturating_add(track.encoded_len())
            .saturating_add(
                std::mem::size_of::<Receiver>()
                    .saturating_add(std::mem::size_of::<rtp::RtpReceiver>())
                    .saturating_mul(12),
            )
            .saturating_add(
                if matches!(&transport, ReceiveTransport::Rtp(rtp) if rtp.video_options.is_some()) {
                    self.downstream.video.owned_slot_bytes().saturating_mul(12)
                } else {
                    0
                },
            )
            .saturating_add(4096);
        if self
            .signaling
            .reserve_execution(work.reference, bytes)
            .is_err()
        {
            self.complete_native_preparation(
                work,
                Err(failure(wire::FailureCode::CapacityExceeded)),
            );
            return;
        }
        let opened = match &transport {
            ReceiveTransport::Sctp { channel, lane } => self
                .transport
                .open_native_data(*channel, *lane)
                .map(|()| false),
            ReceiveTransport::Rtp(rtp) => rtp.install(&mut self.transport, false).map(|()| true),
        };
        let opened = match opened {
            Ok(opened) => opened,
            Err(code) => {
                let _ = self.signaling.reserve_execution(work.reference, 0);
                self.complete_native_preparation(
                    work,
                    Err(wire::Failure {
                        diagnostic: "native receiver transport installation failed".to_owned(),
                        ..failure(code)
                    }),
                );
                return;
            }
        };
        if let ReceiveTransport::Rtp(rtp) = &transport
            && rtp.video_options.is_some()
            && !self.downstream.video.install_owned_slot(
                rtp.mid(),
                rtp.rid(),
                rtp.ssrc(),
                rtp.binding.codec.payload_type.into(),
            )
        {
            if rtp.remove(&mut self.transport).is_err() {
                self.disconnect(DisconnectReason::SignalingOutput);
            }
            let _ = self.signaling.reserve_execution(work.reference, 0);
            self.complete_native_preparation(
                work,
                Err(wire::Failure {
                    diagnostic: "native receiver MID already has a video output".to_owned(),
                    ..failure(wire::FailureCode::Incompatible)
                }),
            );
            return;
        }
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
        self.native_receivers
            .source_refs
            .insert((*source.as_bytes(), reference));
        self.native_receivers.receivers.insert(
            reference,
            Receiver {
                transport,
                source,
                participant,
                track,
                handle: None,
                enabled: false,
                opened,
                retiring: false,
                preparation: Some(work),
                deadline,
            },
        );
        if opened
            && let Some(work) = self
                .native_receivers
                .receivers
                .get_mut(&reference)
                .and_then(|receiver| receiver.preparation.take())
        {
            self.commit_native_receiver(work, events);
            if !self.signaling.owns_lifetime(reference) {
                self.fail_native_receiver(reference, wire::FailureCode::ResourceGone);
            }
        }
    }

    fn prepare_native_receiver_transport(
        &self,
        spec: &wire::ResourceSpec,
        source: TrackId,
        track: &wire::Track,
    ) -> Result<ReceiveTransport, wire::FailureCode> {
        let state = self
            .native_receivers
            .sources
            .get(source.as_bytes())
            .filter(|state| !state.retired)
            .ok_or(wire::FailureCode::Incompatible)?;
        let Some(resource_spec::Declaration::Subscription(subscription)) = &spec.declaration else {
            return Err(wire::FailureCode::InvalidConfiguration);
        };
        match (
            &track.kind,
            subscription
                .binding
                .as_ref()
                .and_then(|binding| binding.transport.as_ref()),
        ) {
            (
                Some(wire::track::Kind::Data(data)),
                Some(wire::track_binding::Transport::Sctp(binding)),
            ) if subscription.kind == wire::TrackKind::Data as i32 => {
                let lane = match wire::DataDelivery::try_from(data.delivery) {
                    Ok(wire::DataDelivery::Ordered) => DataLane::Ordered,
                    Ok(wire::DataDelivery::Latest) => DataLane::Latest,
                    Ok(wire::DataDelivery::Lossy) => DataLane::Lossy,
                    _ => return Err(wire::FailureCode::Incompatible),
                };
                let channel = crate::media::ChannelId::from_value(
                    u16::try_from(
                        binding
                            .stream_id
                            .ok_or(wire::FailureCode::InvalidConfiguration)?,
                    )
                    .map_err(|_| wire::FailureCode::InvalidConfiguration)?,
                );
                if self.native_data.channels.contains_key(&channel)
                    || self.native_receivers.channels.contains_key(&channel)
                {
                    return Err(wire::FailureCode::Incompatible);
                }
                Ok(ReceiveTransport::Sctp { channel, lane })
            }
            (
                Some(wire::track::Kind::Audio(_) | wire::track::Kind::Video(_)),
                Some(wire::track_binding::Transport::Rtp(_)),
            ) => {
                let capabilities = self
                    .negotiated
                    .native_rtp_capabilities
                    .as_ref()
                    .ok_or(wire::FailureCode::Unsupported)?;
                let receiver = rtp::RtpReceiver::prepare(spec, &state.track, capabilities)?;
                if self.native_receivers.mids.contains_key(&receiver.mid()) {
                    return Err(wire::FailureCode::Incompatible);
                }
                Ok(ReceiveTransport::Rtp(Box::new(receiver)))
            }
            _ => Err(wire::FailureCode::Incompatible),
        }
    }

    pub(super) fn native_receiver_opened(
        &mut self,
        reference: u64,
        config: pulsebeam_rtc::DataChannelConfig,
        events: &mut impl ParticipantSink,
    ) {
        let Some(receiver) = self.native_receivers.receivers.get_mut(&reference) else {
            return;
        };
        let Some((channel, lane)) = receiver.transport.data() else {
            return;
        };
        if config != crate::participant::transport::native_data_config(channel, lane) {
            if receiver.preparation.is_some() {
                self.fail_native_receiver(reference, wire::FailureCode::Incompatible);
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
        if let Some(work) = receiver.preparation.take() {
            self.commit_native_receiver(work, events);
        }
        if !self.signaling.owns_lifetime(reference) {
            self.fail_native_receiver(reference, wire::FailureCode::ResourceGone);
        }
        self.evaluate_native_preparations(events);
    }

    fn apply_native_receiver_gate(
        &mut self,
        reference: u64,
        active: bool,
    ) -> Result<(), wire::FailureCode> {
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
                        return Err(wire::FailureCode::InternalFailure);
                    }
                }
                rtp.install(&mut self.transport, active)?;
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

    fn commit_native_receiver(&mut self, work: Preparation, events: &mut impl ParticipantSink) {
        let Some(work) = self.current_native_preparation(work) else {
            return;
        };
        let reference = work.reference;
        let Some(receiver) = self.native_receivers.receivers.get(&reference) else {
            return;
        };
        if !receiver.opened || receiver.retiring {
            self.complete_native_preparation(work, Err(failure(wire::FailureCode::Incompatible)));
            return;
        }
        if let ReceiveTransport::Rtp(rtp) = &receiver.transport
            && let Err(code) = rtp::RtpReceiver::validate_settings(&rtp.binding, &work.spec)
        {
            self.complete_native_preparation(work, Err(failure(code)));
            return;
        }
        let video_options = match &receiver.transport {
            ReceiveTransport::Rtp(rtp) => rtp::RtpReceiver::video_options(&rtp.binding, &work.spec),
            ReceiveTransport::Sctp { .. } => None,
        };
        if let ReceiveTransport::Rtp(rtp) = &receiver.transport
            && rtp.video_options != video_options
        {
            self.stream_writer.discard_mid(rtp.mid());
            if !self.transport.discard_rtp(rtp.mid()) {
                self.complete_native_preparation(
                    work,
                    Err(failure(wire::FailureCode::InternalFailure)),
                );
                return;
            }
        }
        let Some(source) = self
            .native_receivers
            .sources
            .get(receiver.source.as_bytes())
            .filter(|source| !source.retired)
        else {
            self.complete_native_preparation(work, Err(failure(wire::FailureCode::ResourceGone)));
            return;
        };
        let enabled = work
            .spec
            .settings
            .as_ref()
            .is_some_and(|settings| settings.enabled == Some(true));
        let handle = source.handle;
        let ready = enabled && source.bound && handle.is_some();
        let previous_ready = self.native_receivers.receiver_ready(receiver);
        let live = wire::LiveResource {
            spec: Some(work.spec.clone()),
            participant: Some(receiver.participant.clone()),
            track: Some(receiver.track.clone()),
            execution: if !enabled {
                wire::ExecutionStatus::Disabled
            } else if ready {
                wire::ExecutionStatus::Installed
            } else {
                wire::ExecutionStatus::WaitingForSource
            } as i32,
            execution_failure: None,
            activity: None,
        };
        if let Err(code) = self.apply_native_receiver_gate(reference, ready) {
            self.complete_native_preparation(work, Err(failure(code)));
            return;
        }
        if !self.complete_native_preparation(work, Ok(live)) {
            if self
                .apply_native_receiver_gate(reference, previous_ready)
                .is_err()
            {
                self.disconnect(DisconnectReason::SignalingOutput);
            }
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
        events.set_owned_subscription(reference, receiver.source, enabled);
    }

    fn close_native_receiver_transport(&mut self, receiver: &Receiver) {
        match &receiver.transport {
            ReceiveTransport::Sctp { channel, .. } => self.transport.close_native_data(*channel),
            ReceiveTransport::Rtp(rtp) => {
                self.stream_writer.discard_mid(rtp.mid());
                if rtp.video_options.is_some() {
                    self.downstream.video.remove_owned_slot(rtp.mid());
                    self.downstream.dirty_allocation = true;
                }
                if !self.transport.discard_rtp(rtp.mid())
                    || rtp.remove(&mut self.transport).is_err()
                {
                    self.disconnect(DisconnectReason::SignalingOutput);
                }
            }
        }
    }

    pub(super) fn fail_native_receiver(&mut self, reference: u64, code: wire::FailureCode) {
        if let Some(waiting) = self.native_receivers.waiting.remove(&reference) {
            self.native_receivers
                .source_refs
                .remove(&(*waiting.source.as_bytes(), reference));
            self.native_receivers
                .queue_unused_source(*waiting.source.as_bytes());
            self.native_receivers
                .deadlines
                .remove(&(waiting.deadline, reference));
            let _ = self.signaling.reserve_execution(reference, 0);
            self.complete_native_preparation(waiting.work, Err(failure(code)));
            return;
        }
        let Some(receiver) = self.native_receivers.remove_receiver(reference) else {
            return;
        };
        self.close_native_receiver_transport(&receiver);
        let _ = self.signaling.reserve_execution(reference, 0);
        if let Some(work) = receiver.preparation {
            self.complete_native_preparation(work, Err(failure(code)));
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
        if let Some(work) = receiver.preparation.take() {
            self.complete_native_preparation(work, Err(failure(wire::FailureCode::ResourceGone)));
        }
        self.native_receivers.receivers.insert(reference, receiver);
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
        if self.signaling.reserve_execution(reference, 0).is_err()
            || self.signaling.cutoff_complete(reference).is_err()
        {
            self.disconnect(DisconnectReason::SignalingOutput);
        }
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
            && frame.data_track_id() == Some(receiver.track.track_id.as_str())
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
        if self.disconnect_reason.is_some()
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
                && frame.data_track_id() == Some(receiver.track.track_id.as_str())
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
        if self.disconnect_reason.is_some()
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
                    rtp.forward_audio(receiver.source, packet, &mut self.stream_writer);
                }
            }
        }
    }
}

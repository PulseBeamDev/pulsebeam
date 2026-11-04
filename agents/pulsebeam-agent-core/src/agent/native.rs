use super::*;

#[cfg(test)]
mod tests;
use crate::native_state::{Change, Resource};
use pulsebeam_proto::state;

pub(super) const STATE_MEMORY_BYTES: usize = 64 * 1024 * 1024;

pub(super) struct PayloadChannel {
    pub resource: Resource,
    pub registration: crate::TopicChannel,
    pub stream: u16,
    pub state: PayloadState,
    pub restart: bool,
    wanted: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PayloadState {
    Active,
    Closing,
    Closed { reset_complete: bool },
}

pub(super) struct MediaLease {
    resource: Resource,
    mid: String,
    identity_pending: bool,
    retired: bool,
    retirement_failed: bool,
    wire_retired: bool,
    source: Option<String>,
}

struct RetiringMedia {
    slot: MediaSlot,
    resource: Resource,
    mid: String,
}

pub(super) struct MediaRetirement {
    pub operation: OperationId,
    leases: Vec<RetiringMedia>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct MediaWant {
    resource: Resource,
    source: Option<String>,
    allocation: crate::client_sdp::Allocation,
}

pub(super) struct MediaAllocation {
    pub operation: OperationId,
    wanted: BTreeMap<MediaSlot, MediaWant>,
}

fn media_want(
    session: &Session,
    resource: Resource,
    slot: &MediaSlot,
    source: Option<&str>,
) -> Option<MediaWant> {
    let format = match source {
        Some(source) => match session.state.source(source)?.kind.as_ref()? {
            state::track::Kind::Audio(track) => Some(track.format.clone()?),
            state::track::Kind::Video(track) => Some(track.format.clone()?),
            _ => return None,
        },
        None => None,
    };
    Some(MediaWant {
        resource,
        source: source.map(str::to_owned),
        allocation: crate::client_sdp::Allocation {
            slot: slot.clone(),
            format,
        },
    })
}

pub(super) struct Renewal {
    pub token: String,
    pub operation: Option<OperationId>,
    pub expected_revision: u64,
    pub accepted: bool,
}

pub(super) fn kind(track: &state::Track) -> state::TrackKind {
    match track.kind {
        Some(state::track::Kind::Audio(_)) => state::TrackKind::Audio,
        Some(state::track::Kind::Video(_)) => state::TrackKind::Video,
        Some(state::track::Kind::Data(_)) => state::TrackKind::Data,
        None => state::TrackKind::Unspecified,
    }
}

fn selected<'a>(
    session: &'a Session,
    resource: &Resource,
    selector: &crate::TrackSelector,
    expected: state::TrackKind,
) -> Option<&'a str> {
    let matches = |track: &state::Track| {
        kind(track) == expected
            && track.label == selector.label
            && session
                .state
                .source_participant(&track.participant_id)
                .is_some_and(|owner| {
                    owner.participant_external_id == selector.participant_external_id
                })
    };
    if let Some(live) =
        session
            .state
            .confirmed(resource)
            .and_then(|confirmed| match confirmed.value.as_ref()? {
                state::confirmed_state::Value::Owned(live) => Some(live),
                _ => None,
            })
        && live.execution != state::ExecutionStatus::WaitingForSource as i32
        && let Some(track) = live.track.as_ref().filter(|track| matches(track))
    {
        return Some(&track.track_id);
    }
    if expected == state::TrackKind::Audio {
        let retained = session
            .state
            .owned()
            .filter(|(resource, live)| {
                matches!(resource, Resource::Reception(MediaSlot::RemoteAudio(_)))
                    && live.execution != state::ExecutionStatus::WaitingForSource as i32
            })
            .filter_map(|(_, live)| live.track.as_ref())
            .filter(|track| matches(track))
            .map(|track| track.track_id.as_str())
            .collect::<BTreeSet<_>>();
        if retained.len() == 1 {
            return retained.first().copied();
        }
        if !retained.is_empty() {
            return None;
        }
    }
    let mut candidates = session
        .state
        .tracks()
        .values()
        .filter(|track| matches(track));
    let candidate = candidates.next()?;
    candidates
        .next()
        .is_none()
        .then_some(candidate.track_id.as_str())
}

fn rtp(
    session: &mut Session,
    resource: &Resource,
    slot: &MediaSlot,
    source: Option<&str>,
) -> Option<state::TrackBinding> {
    if let Some(lease) = session.media_leases.get_mut(slot)
        && (lease.resource != *resource || lease.source.as_deref() != source)
    {
        if lease.resource != *resource || slot.direction() != crate::MediaDirection::ReceiveOnly {
            lease.retired = true;
        }
        return None;
    }
    let prepared = session
        .media_preparation
        .as_ref()
        .and_then(|cache| cache.as_ref().ok())
        .and_then(|cache| cache.get(slot));
    let pending = matches!(
        prepared,
        Some(Err(crate::client_sdp::Error::IdentityPending))
    );
    let Some(prepared) = prepared.and_then(|prepared| prepared.as_ref().ok()) else {
        let preparation_error = prepared.and_then(|value| value.as_ref().err()).cloned();
        if media_want(session, resource.clone(), slot, source).is_some()
            && let Some(coordinate) = session.coordinates.get(slot).and_then(Option::as_ref)
        {
            session
                .media_leases
                .entry(slot.clone())
                .or_insert_with(|| {
                    log::debug!(
                        "Native bootstrap media lease awaits preparation: slot={slot:?} resource={resource:?} error={preparation_error:?} pending={pending}"
                    );
                    MediaLease {
                        resource: resource.clone(),
                        mid: coordinate.mid.clone(),
                        source: source.map(str::to_owned),
                        identity_pending: pending,
                        retired: !pending,
                        retirement_failed: false,
                        wire_retired: false,
                    }
                });
        }
        return None;
    };
    let Some(state::track_binding::Transport::Rtp(parameters)) =
        prepared.binding.transport.as_ref()
    else {
        return None;
    };
    if let Some(lease) = session.media_leases.get_mut(slot) {
        if lease.retired || lease.resource != *resource || lease.mid != parameters.mid {
            return None;
        }
        lease.identity_pending = false;
    } else {
        session.media_leases.insert(
            slot.clone(),
            MediaLease {
                resource: resource.clone(),
                mid: parameters.mid.clone(),
                identity_pending: false,
                retired: false,
                retirement_failed: false,
                wire_retired: false,
                source: source.map(str::to_owned),
            },
        );
    }
    Some(prepared.binding.clone())
}

fn receive_rtp(
    session: &mut Session,
    resource: &Resource,
    slot: &MediaSlot,
    source: &str,
) -> Option<state::TrackBinding> {
    if session.state.target_spec(resource).and_then(|spec| spec.declaration.as_ref()).is_some_and(|declaration| matches!(declaration, state::resource_spec::Declaration::Subscription(value) if value.track_id != source)) {
        return None;
    }
    rtp(session, resource, slot, Some(source))
}

fn receiver_spec_available(
    session: &Session,
    slot: &MediaSlot,
    resource: &Resource,
    spec: &state::ResourceSpec,
) -> bool {
    !session.media_leases.get(slot).is_some_and(|lease| lease.wire_retired)
        || session.state.target_spec(resource).is_some_and(|target| {
            state::validation::declarations_equivalent(target, spec)
                && matches!((&target.settings, &spec.settings), (Some(left), Some(right)) if state::validation::settings_equivalent(left, right))
        })
}

fn settings(enabled: bool) -> state::ResourceSettings {
    state::ResourceSettings {
        enabled: Some(enabled),
        receive: None,
    }
}

fn delay(value: crate::PlayoutDelay) -> Option<state::PlayoutDelay> {
    match value {
        crate::PlayoutDelay::Adaptive => None,
        crate::PlayoutDelay::Fixed { min_ms, max_ms } => {
            Some(state::PlayoutDelay { min_ms, max_ms })
        }
    }
}

fn publication(
    label: &str,
    enabled: bool,
    kind: state::publication_spec::Kind,
) -> state::ResourceSpec {
    state::ResourceSpec {
        settings: Some(settings(enabled)),
        declaration: Some(state::resource_spec::Declaration::Publication(
            state::PublicationSpec {
                label: label.into(),
                kind: Some(kind),
            },
        )),
    }
}

fn subscription(
    track: String,
    kind: state::TrackKind,
    binding: state::TrackBinding,
    receive: Option<state::resource_settings::Receive>,
) -> state::ResourceSpec {
    state::ResourceSpec {
        settings: Some(state::ResourceSettings {
            enabled: Some(true),
            receive,
        }),
        declaration: Some(state::resource_spec::Declaration::Subscription(
            state::SubscriptionSpec {
                track_id: track,
                kind: kind as i32,
                binding: Some(binding),
            },
        )),
    }
}

fn payload(
    session: &mut Session,
    resource: &Resource,
    registration: crate::TopicChannel,
    ids: &mut IdGenerator,
    effects: &mut VecDeque<Effect>,
) -> Option<state::TrackBinding> {
    let mode = match &registration {
        crate::TopicChannel::Publisher(value) => value.mode,
        crate::TopicChannel::Subscriber(value) => value.mode,
    };
    let channel = if let Some(channel) = session.payload_resources.get(resource).copied() {
        let physical = session.payloads.get(&channel)?;
        if physical.registration != registration || physical.state != PayloadState::Active {
            return None;
        }
        channel
    } else {
        let capacity = session
            .data_preparations
            .get(&session.signaling_channel)?
            .as_ref()
            .ok()?
            .association_max_channels?;
        let stream = if session
            .reusable_streams
            .first()
            .is_some_and(|stream| u32::from(*stream) < capacity)
        {
            session.reusable_streams.pop_first()?
        } else {
            if session.next_stream >= capacity || session.next_stream >= u32::from(u16::MAX) {
                return None;
            }
            let stream = u16::try_from(session.next_stream).ok()?;
            session.next_stream = session.next_stream.checked_add(1)?;
            stream
        };
        let channel = loop {
            let candidate = ids.channel();
            if candidate != session.signaling_channel && !session.payloads.contains_key(&candidate)
            {
                break candidate;
            }
        };
        session.payloads.insert(
            channel,
            PayloadChannel {
                resource: resource.clone(),
                registration,
                stream,
                state: PayloadState::Active,
                restart: false,
                wanted: true,
            },
        );
        session.payload_resources.insert(resource.clone(), channel);
        effects.push_back(Effect::Rtc(RtcEffect::CreateDataChannel {
            generation: session.generation,
            channel,
            specification: DataChannelSpec {
                label: format!("native/payload/{}", channel.get()),
                ordered: mode == crate::TopicMode::Ordered,
                reliability: if mode == crate::TopicMode::Ordered {
                    DataChannelReliability::Reliable
                } else {
                    DataChannelReliability::MaxRetransmits(0)
                },
                negotiated_id: Some(stream),
            },
        }));
        channel
    };
    let prepared = session.data_preparations.get(&channel)?.as_ref().ok()?;
    let physical = session.payloads.get(&channel)?;
    let reliable = mode == crate::TopicMode::Ordered;
    if prepared.stream_id != physical.stream
        || !prepared.negotiated
        || prepared.ordered != reliable
        || prepared.reliability
            != if reliable {
                DataChannelReliability::Reliable
            } else {
                DataChannelReliability::MaxRetransmits(0)
            }
    {
        return None;
    }
    Some(state::TrackBinding {
        transport: Some(state::track_binding::Transport::Sctp(
            state::SctpParameters {
                stream_id: Some(u32::from(prepared.stream_id)),
            },
        )),
    })
}

pub(super) fn prepared_payload(
    session: &mut Session,
    channel: ChannelId,
    ids: &mut IdGenerator,
    effects: &mut VecDeque<Effect>,
) -> Result<(), SignalingError> {
    let Some(physical) = session
        .payloads
        .get(&channel)
        .filter(|physical| physical.wanted && physical.state == PayloadState::Active)
    else {
        return Ok(());
    };
    let resource = physical.resource.clone();
    let registration = physical.registration.clone();
    let Some(binding) = payload(session, &resource, registration.clone(), ids, effects) else {
        return Ok(());
    };
    let spec = match registration {
        crate::TopicChannel::Publisher(value) => publication(
            &value.topic,
            true,
            state::publication_spec::Kind::Data(state::DataPublication {
                delivery: value.mode.delivery() as i32,
                binding: Some(binding),
            }),
        ),
        crate::TopicChannel::Subscriber(_) => {
            let Resource::DataSubscription(id) = &resource else {
                return Ok(());
            };
            subscription(id.clone(), state::TrackKind::Data, binding, None)
        }
    };
    session.state.stage(resource, Some(spec))
}

pub(super) fn project_retired_receivers(session: &Session, snapshot: &mut Snapshot) -> bool {
    let mut changed = false;
    for (slot, lease) in &session.media_leases {
        if !lease.retired {
            continue;
        }
        let Some(coordinate) = session.coordinates.get(slot).and_then(Option::as_ref) else {
            continue;
        };
        let mappings = match slot {
            MediaSlot::RemoteAudio(_) => &mut snapshot.audio_mapping,
            MediaSlot::RemoteVideo(_) => &mut snapshot.video_mapping,
            _ => continue,
        };
        changed |= mappings.remove(&coordinate.media_index).is_some();
    }
    changed
}

pub(super) fn reconcile(
    session: &mut Session,
    desired: &DesiredState,
    topics: &crate::topic::Topics,
    ids: &mut IdGenerator,
    effects: &mut VecDeque<Effect>,
) -> Result<(), SignalingError> {
    let mut wanted = BTreeMap::new();
    let mut media = BTreeMap::new();
    for value in &desired.publications {
        let kind = value.kind;
        let slot = value.media_slot();
        let wire_kind = if kind == crate::MediaKind::Audio {
            state::TrackKind::Audio
        } else {
            state::TrackKind::Video
        };
        let resource = Resource::Publication {
            kind: wire_kind,
            label: value.label.clone(),
        };
        if let Some(want) = media_want(session, resource.clone(), &slot, None) {
            media.insert(slot.clone(), want);
        }
        if let Some(binding) = rtp(session, &resource, &slot, None) {
            let kind = if kind == crate::MediaKind::Audio {
                state::publication_spec::Kind::Audio(state::AudioPublication {
                    binding: Some(binding),
                })
            } else {
                state::publication_spec::Kind::Video(state::VideoPublication {
                    binding: Some(binding),
                })
            };
            wanted.insert(resource, publication(&value.label, value.active, kind));
        }
    }
    for video in &desired.video {
        let slot = MediaSlot::RemoteVideo(video.slot);
        let resource = Resource::Reception(slot.clone());
        let track = match &video.selector {
            Some(selector) => {
                selected(session, &resource, selector, state::TrackKind::Video).map(str::to_owned)
            }
            None => Some(video.track_id.clone()),
        };
        if let Some(track) = track.as_ref()
            && let Some(want) = media_want(session, resource.clone(), &slot, Some(track))
        {
            media.insert(slot.clone(), want);
        }
        if let Some(track) = track
            && let Some(binding) = receive_rtp(session, &resource, &slot, &track)
        {
            let spec = subscription(
                track,
                state::TrackKind::Video,
                binding,
                Some(state::resource_settings::Receive::Video(
                    state::VideoReceiveOptions {
                        max_height: (video.height != 0).then_some(video.height),
                        min_height: (video.min_height != 0).then_some(video.min_height),
                        min_fps: (video.min_fps != 0).then_some(video.min_fps),
                        priority: (video.priority != 0).then_some(video.priority),
                        playout_delay: delay(video.playout_delay),
                    },
                )),
            );
            if receiver_spec_available(session, &slot, &resource, &spec) {
                wanted.insert(resource, spec);
            }
        }
    }
    let mut audio = desired
        .audio
        .pinned
        .iter()
        .cloned()
        .map(|id| {
            let buffering = desired
                .audio
                .playout_delays
                .get(&id)
                .copied()
                .unwrap_or_default();
            (id, buffering)
        })
        .collect::<BTreeMap<_, _>>();
    for selector in &desired.audio.selected {
        let resource = Resource::Reception(MediaSlot::RemoteAudio(0));
        if let Some(id) = selected(session, &resource, selector, state::TrackKind::Audio) {
            audio.insert(
                id.into(),
                desired
                    .audio
                    .selector_delays
                    .get(selector)
                    .copied()
                    .unwrap_or_default(),
            );
        }
    }
    if desired.audio.automatic {
        for track in session
            .state
            .tracks()
            .values()
            .chain(session.state.owned().filter_map(|(resource, live)| {
                (matches!(resource, Resource::Reception(MediaSlot::RemoteAudio(_)))
                    && live.execution == state::ExecutionStatus::Installed as i32)
                    .then_some(live.track.as_ref())
                    .flatten()
            }))
            .filter(|track| {
                kind(track) == state::TrackKind::Audio
                    && track.participant_id != session.participant_id
            })
        {
            audio.entry(track.track_id.clone()).or_default();
        }
    }
    let mut occupied = BTreeSet::new();
    let mut assigned = Vec::new();
    // Keep in-flight assignments stable before native and StateBatch confirmation.
    for (slot, want) in &session.media_wanted {
        if let MediaSlot::RemoteAudio(index) = slot
            && let Some(source) = &want.source
            && let Some(buffering) = audio.remove(source)
        {
            occupied.insert(*index);
            assigned.push((*index, source.clone(), buffering));
        }
    }
    for (resource, live) in session.state.owned() {
        if let Resource::Reception(MediaSlot::RemoteAudio(index)) = resource
            && !occupied.contains(index)
            && let Some(track) = live.track.as_ref()
            && let Some(buffering) = audio.remove(&track.track_id)
        {
            occupied.insert(*index);
            assigned.push((*index, track.track_id.clone(), buffering));
        }
    }
    for (id, buffering) in audio {
        let index = (0..=u32::MAX)
            .find(|index| !occupied.contains(index))
            .ok_or(SignalingError::Invalid("audio receiver coordinate space"))?;
        occupied.insert(index);
        assigned.push((index, id, buffering));
    }
    for (index, id, buffering) in assigned {
        let slot = MediaSlot::RemoteAudio(index);
        let resource = Resource::Reception(slot.clone());
        if let Some(want) = media_want(session, resource.clone(), &slot, Some(&id)) {
            media.insert(slot.clone(), want);
        }
        if let Some(binding) = receive_rtp(session, &resource, &slot, &id) {
            let spec = subscription(
                id,
                state::TrackKind::Audio,
                binding,
                Some(state::resource_settings::Receive::Audio(
                    state::AudioReceiveOptions {
                        playout_delay: delay(buffering),
                    },
                )),
            );
            if receiver_spec_available(session, &slot, &resource, &spec) {
                wanted.insert(resource, spec);
            }
        }
    }
    let mut data = BTreeMap::new();
    let subscribers = desired
        .topics
        .subscribers
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    for publisher in &desired.topics.publishers {
        data.insert(
            Resource::Publication {
                kind: state::TrackKind::Data,
                label: publisher.topic.clone(),
            },
            crate::TopicChannel::Publisher(publisher.clone()),
        );
    }
    for (registration, source) in topics.recovery_sources() {
        if subscribers.contains(registration) {
            data.insert(
                Resource::DataSubscription(source.into()),
                crate::TopicChannel::Subscriber(registration.clone()),
            );
        }
    }
    for track in session
        .state
        .tracks()
        .values()
        .chain(session.state.owned().filter_map(|(resource, live)| {
            matches!(resource, Resource::DataSubscription(_))
                .then_some(live.track.as_ref())
                .flatten()
        }))
    {
        let Some(state::track::Kind::Data(format)) = track.kind.as_ref() else {
            continue;
        };
        if track.participant_id == session.participant_id {
            continue;
        }
        let Some(mode) = crate::TopicMode::from_delivery(format.delivery) else {
            continue;
        };
        let registration = crate::TopicSubscriber {
            topic: track.label.clone(),
            mode,
        };
        if subscribers.contains(&registration) {
            data.insert(
                Resource::DataSubscription(track.track_id.clone()),
                crate::TopicChannel::Subscriber(registration),
            );
        }
    }
    for physical in session
        .payloads
        .values()
        .filter(|physical| physical.state == PayloadState::Active)
    {
        if let crate::TopicChannel::Subscriber(registration) = &physical.registration
            && subscribers.contains(registration)
            && (session.state.retaining(&physical.resource)
                || session.state.target_spec(&physical.resource).is_some())
        {
            data.entry(physical.resource.clone())
                .or_insert_with(|| physical.registration.clone());
        }
    }
    for physical in session.payloads.values_mut() {
        physical.wanted = data.get(&physical.resource) == Some(&physical.registration);
    }
    let replaced = data
        .iter()
        .filter_map(|(resource, registration)| {
            let channel = *session.payload_resources.get(resource)?;
            let physical = session.payloads.get(&channel)?;
            ((physical.registration != *registration || physical.restart)
                && !session.state.retaining(resource))
            .then_some((resource.clone(), channel))
        })
        .collect::<Vec<_>>();
    for (resource, channel) in replaced {
        session.state.stage(resource.clone(), None)?;
        let Some(physical) = session.payloads.get_mut(&channel) else {
            continue;
        };
        match physical.state {
            PayloadState::Active => {
                physical.state = PayloadState::Closing;
                effects.push_back(Effect::Rtc(RtcEffect::CloseDataChannel {
                    generation: session.generation,
                    channel,
                }));
                continue;
            }
            PayloadState::Closing => continue,
            PayloadState::Closed { .. } => {}
        }
        release_payload(session, &resource, channel);
    }
    let data_wanted = data.keys().cloned().collect::<BTreeSet<_>>();
    let released = session
        .payload_resources
        .iter()
        .filter(|(resource, _)| !data_wanted.contains(*resource))
        .map(|(resource, channel)| (resource.clone(), *channel))
        .collect::<Vec<_>>();
    for (resource, channel) in released {
        session.state.stage(resource.clone(), None)?;
        if session.state.retaining(&resource) {
            continue;
        }
        let Some(physical) = session.payloads.get_mut(&channel) else {
            continue;
        };
        match physical.state {
            PayloadState::Active => {
                physical.state = PayloadState::Closing;
                effects.push_back(Effect::Rtc(RtcEffect::CloseDataChannel {
                    generation: session.generation,
                    channel,
                }));
                continue;
            }
            PayloadState::Closing => continue,
            PayloadState::Closed { .. } => {}
        }
        release_payload(session, &resource, channel);
    }
    for (resource, registration) in data {
        if let Some(binding) = payload(session, &resource, registration.clone(), ids, effects) {
            let spec = match &registration {
                crate::TopicChannel::Publisher(publisher) => publication(
                    &publisher.topic,
                    true,
                    state::publication_spec::Kind::Data(state::DataPublication {
                        delivery: publisher.mode.delivery() as i32,
                        binding: Some(binding),
                    }),
                ),
                crate::TopicChannel::Subscriber(_) => {
                    let Resource::DataSubscription(id) = &resource else {
                        continue;
                    };
                    subscription(id.clone(), state::TrackKind::Data, binding, None)
                }
            };
            wanted.insert(resource, spec);
        } else if session
            .payload_resources
            .get(&resource)
            .and_then(|channel| session.payloads.get(channel))
            .is_some_and(|physical| physical.state != PayloadState::Active)
            && let Some(spec) = session.state.target_spec(&resource)
        {
            wanted.insert(resource, spec.clone());
        }
    }
    let removed = session
        .state
        .resources()
        .filter(|resource| !wanted.contains_key(*resource))
        .cloned()
        .collect::<Vec<_>>();
    for resource in removed {
        if session.state.target_spec(&resource).is_some() {
            log::debug!(
                "Native resource no longer wanted: generation={:?} resource={resource:?}",
                session.generation
            );
        }
        session.state.stage(resource.clone(), None)?;
        for lease in session.media_leases.values_mut() {
            if lease.resource == resource {
                lease.retired = true;
            }
        }
    }
    for (slot, lease) in &mut session.media_leases {
        if (lease.identity_pending || slot.direction() == crate::MediaDirection::ReceiveOnly)
            && !media.contains_key(slot)
            && !wanted.contains_key(&lease.resource)
        {
            lease.retired = true;
        }
    }
    let mut failure = None;
    for (resource, spec) in wanted {
        if matches!(
            &resource,
            Resource::Publication {
                kind: state::TrackKind::Video | state::TrackKind::Audio,
                ..
            }
        ) && session.state.target_spec(&resource) != Some(&spec)
        {
            log::debug!(
                "Native media publication wanted: generation={:?} resource={resource:?} declaration={:?}",
                session.generation,
                spec.declaration
            );
        }
        if let Err(error) = session.state.stage(resource, Some(spec)) {
            failure.get_or_insert(error);
        }
    }
    session
        .media_blocked
        .retain(|slot, blocked| media.get(slot) == Some(blocked));
    session.media_wanted = media;
    retire_media(session, ids, effects);
    allocate_media(session, ids, effects);
    failure.map_or(Ok(()), Err)
}

fn release_payload(session: &mut Session, resource: &Resource, channel: ChannelId) {
    session.payload_resources.remove(resource);
    if let Some(physical) = session.payloads.remove(&channel)
        && physical.state
            == (PayloadState::Closed {
                reset_complete: true,
            })
    {
        session.reusable_streams.insert(physical.stream);
    }
    session.data_preparations.remove(&channel);
}

fn retire_media(session: &mut Session, ids: &mut IdGenerator, effects: &mut VecDeque<Effect>) {
    if session.media_retirement.is_some() || session.media_allocation.is_some() {
        return;
    }
    let leases = session
        .media_leases
        .iter()
        .filter(|(_, lease)| {
            lease.retired && !lease.retirement_failed && !session.state.retaining(&lease.resource)
        })
        .take(1)
        .map(|(slot, lease)| RetiringMedia {
            slot: slot.clone(),
            resource: lease.resource.clone(),
            mid: lease.mid.clone(),
        })
        .collect::<Vec<_>>();
    if leases.is_empty() {
        return;
    }
    let operation = ids.operation();
    effects.push_back(Effect::Rtc(RtcEffect::RetireMedia {
        generation: session.generation,
        operation,
        mids: leases.iter().map(|lease| lease.mid.clone()).collect(),
    }));
    session.media_retirement = Some(MediaRetirement { operation, leases });
}

// One physical lease per operation keeps unsupported sources independent.
fn allocate_media(session: &mut Session, ids: &mut IdGenerator, effects: &mut VecDeque<Effect>) {
    if session.media_retirement.is_some() || session.media_allocation.is_some() {
        return;
    }
    let wanted = session
        .media_wanted
        .iter()
        .filter(|(slot, want)| {
            !session.media_leases.contains_key(*slot)
                && session.media_blocked.get(*slot) != Some(*want)
        })
        .map(|(slot, want)| (slot.clone(), want.clone()))
        .take(1)
        .collect::<BTreeMap<_, _>>();
    if wanted.is_empty() {
        return;
    }
    let operation = ids.operation();
    effects.push_back(Effect::Rtc(RtcEffect::AllocateMedia {
        generation: session.generation,
        operation,
        allocations: wanted
            .values()
            .map(|want| want.allocation.clone())
            .collect(),
    }));
    session.media_allocation = Some(MediaAllocation { operation, wanted });
}

impl Agent {
    pub(super) fn media_observation_target(
        &self,
        slot: &MediaSlot,
    ) -> Option<crate::MediaObservationTarget> {
        if !self.desired.connected || self.attempt.is_some() || self.closing.is_some() {
            return None;
        }
        let active = self.active.as_ref()?;
        let lease = active.media_leases.get(slot)?;
        let wanted = active.media_wanted.get(slot)?;
        if lease.retired || lease.identity_pending || lease.source != wanted.source {
            return None;
        }
        let source = lease.source.as_ref()?;
        let confirmed = active.state.confirmed(&lease.resource)?;
        let state::confirmed_state::Value::Owned(live) = confirmed.value.as_ref()? else {
            return None;
        };
        if active
            .media_leases
            .get(slot)
            .is_none_or(|lease| lease.retired)
            || live.execution != state::ExecutionStatus::Installed as i32
            || live.track.as_ref()?.track_id != *source
        {
            return None;
        }
        let binding = active.coordinates.get(slot)?.as_ref()?;
        if binding.mid != lease.mid || binding.direction != crate::MediaDirection::ReceiveOnly {
            return None;
        }
        Some(crate::MediaObservationTarget {
            generation: active.generation,
            reference: active.state.reference(&lease.resource)?,
            source: source.clone(),
            binding: binding.clone(),
        })
    }

    pub(super) fn audio_observation_target(
        &self,
        selector: &crate::TrackSelector,
    ) -> Option<crate::MediaObservationTarget> {
        let active = self.active.as_ref()?;
        let source = selected(
            active,
            &Resource::Reception(MediaSlot::RemoteAudio(0)),
            selector,
            state::TrackKind::Audio,
        )?;
        self.audio_source_observation_target(source)
    }

    pub(super) fn audio_source_observation_target(
        &self,
        source: &str,
    ) -> Option<crate::MediaObservationTarget> {
        self.active
            .as_ref()?
            .media_leases
            .keys()
            .filter(|slot| matches!(slot, MediaSlot::RemoteAudio(_)))
            .find_map(|slot| {
                self.media_observation_target(slot)
                    .filter(|target| target.source == source)
            })
    }

    pub(super) fn audio_source_authorized(&self, owner: u64, source: &str) -> bool {
        let active = self.active.as_ref();
        let track = active.and_then(|active| {
            active.state.tracks().get(source).or_else(|| {
                active
                    .state
                    .owned()
                    .filter_map(|(_, live)| live.track.as_ref())
                    .find(|track| track.track_id == source)
            })
        });
        if track.is_some_and(|track| {
            kind(track) != state::TrackKind::Audio
                || active.is_some_and(|active| track.participant_id == active.participant_id)
        }) {
            return false;
        }
        let selector = track.and_then(|track| {
            let participant = active?.state.source_participant(&track.participant_id)?;
            Some(crate::TrackSelector {
                participant_external_id: participant.participant_external_id.clone(),
                label: track.label.clone(),
            })
        });
        self.resources
            .audio_source_authorized(crate::TrackOwner(owner), source, selector.as_ref())
    }

    pub fn source_preparation(&self, slot: &MediaSlot) -> SourcePreparation<'_> {
        let target = match slot {
            MediaSlot::LocalVideo(name) | MediaSlot::LocalAudio(name) if self.desired.connected => {
                self.desired
                    .publications
                    .iter()
                    .find(|publication| {
                        publication.slot == *name && publication.kind == slot.kind()
                    })
                    .map(|publication| publication.label.as_str())
            }
            _ => None,
        };
        let readiness = self
            .active
            .as_ref()
            .map_or(SourceReadiness::Ready, |active| {
                if active
                    .media_leases
                    .get(slot)
                    .is_some_and(|lease| lease.retirement_failed)
                    || active
                        .media_wanted
                        .get(slot)
                        .is_some_and(|want| active.media_blocked.get(slot) == Some(want))
                {
                    SourceReadiness::Failed
                } else if active
                    .media_leases
                    .get(slot)
                    .is_some_and(|lease| lease.retired)
                    || active.media_retirement.as_ref().is_some_and(|retirement| {
                        retirement.leases.iter().any(|lease| lease.slot == *slot)
                    })
                    || active
                        .media_allocation
                        .as_ref()
                        .is_some_and(|allocation| allocation.wanted.contains_key(slot))
                    || active.media_wanted.contains_key(slot)
                        && !active.media_leases.contains_key(slot)
                {
                    SourceReadiness::Preparing
                } else {
                    SourceReadiness::Ready
                }
            });
        SourcePreparation { target, readiness }
    }

    pub(super) fn media_observed(
        &mut self,
        generation: Generation,
        bindings: crate::client_sdp::Bindings,
    ) {
        if let Some(attempt) = self.attempt.as_mut()
            && attempt.generation == generation
        {
            let (Some(Ok(prepared)), Some(resources), Some(candidate)) = (
                attempt.media_preparation.as_mut(),
                attempt.resources.as_ref(),
                attempt.candidate.as_ref(),
            ) else {
                return;
            };
            if let Ok(observed) = crate::rtc_preparation::prepare_bootstrap_bindings(
                bindings,
                &resources.slots,
                &candidate.capabilities.rtp,
            ) {
                for (slot, binding) in observed {
                    if matches!(
                        prepared.get(&slot),
                        Some(Err(crate::client_sdp::Error::IdentityPending))
                    ) {
                        prepared.insert(slot, binding);
                    }
                }
            }
            return;
        }
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.generation == generation)
        else {
            return;
        };
        let slots = active
            .coordinates
            .values()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        let Ok(observed) = crate::rtc_preparation::prepare_bootstrap_bindings(
            bindings,
            &slots,
            &active.capabilities.rtp,
        ) else {
            return;
        };
        let Some(Ok(prepared)) = active.media_preparation.as_mut() else {
            return;
        };
        let mut changed = false;
        for (slot, binding) in observed {
            let receiver_changed = slot.direction() == crate::MediaDirection::ReceiveOnly
                && prepared
                    .get(&slot)
                    .and_then(|value| value.as_ref().ok())
                    .is_some_and(|expected| {
                        !binding.as_ref().is_ok_and(|actual| {
                            state::validation::bindings_equivalent(
                                Some(&expected.binding),
                                Some(&actual.binding),
                            )
                        })
                    });
            if receiver_changed
                && let Some(lease) = active
                    .media_leases
                    .get_mut(&slot)
                    .filter(|lease| !lease.retired)
            {
                if let Some(reference) = active.state.reference(&lease.resource) {
                    let _ = active.state.report_execution(
                        reference,
                        state::Failure {
                            code: state::FailureCode::InternalFailure as i32,
                            diagnostic: "configured receiver changed unexpectedly".into(),
                        },
                    );
                }
                lease.retired = true;
                prepared.insert(slot, binding);
                changed = true;
                continue;
            }
            if matches!(
                prepared.get(&slot),
                Some(Err(crate::client_sdp::Error::IdentityPending))
            ) && !matches!(binding, Err(crate::client_sdp::Error::IdentityPending))
                && active
                    .media_leases
                    .get(&slot)
                    .is_none_or(|lease| lease.identity_pending && !lease.retired)
            {
                prepared.insert(slot, binding);
                changed = true;
            }
        }
        if changed {
            self.intent_dirty = true;
            self.send_intent_if_ready();
        }
    }

    pub(super) fn media_allocated(
        &mut self,
        generation: Generation,
        operation: OperationId,
        slots: Vec<SlotBinding>,
        outcome: crate::client_sdp::ApplyOutcome,
        preparation: Result<crate::client_sdp::Bindings, crate::client_sdp::Error>,
    ) -> bool {
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.generation == generation && !active.signaling_terminal)
        else {
            return false;
        };
        if !active
            .media_allocation
            .as_ref()
            .is_some_and(|pending| pending.operation == operation)
        {
            return false;
        }
        let Some(pending) = active.media_allocation.take() else {
            return true;
        };
        if let Err(error) = &preparation {
            agent_log!(self, Warn, "RTP allocation {operation:?} failed: {error:?}");
        }
        let correlated = slots.len() == pending.wanted.len()
            && slots
                .iter()
                .map(|slot| &slot.slot)
                .collect::<BTreeSet<_>>()
                .len()
                == slots.len()
            && slots
                .iter()
                .map(|slot| &slot.mid)
                .collect::<BTreeSet<_>>()
                .len()
                == slots.len()
            && slots.iter().all(|slot| {
                pending.wanted.contains_key(&slot.slot)
                    && slot.kind == slot.slot.kind()
                    && slot.direction == slot.slot.direction()
            });
        let mut geometry = active
            .coordinates
            .iter()
            .filter_map(|(slot, binding)| {
                binding
                    .as_ref()
                    .map(|binding| (slot.clone(), binding.clone()))
            })
            .collect::<BTreeMap<_, _>>();
        if correlated {
            for slot in &slots {
                geometry.insert(slot.slot.clone(), slot.clone());
            }
        }
        let prepared = (outcome == crate::client_sdp::ApplyOutcome::Applied && correlated)
            .then(|| {
                preparation.and_then(|bindings| {
                    crate::rtc_preparation::prepare_bootstrap_bindings(
                        bindings,
                        &geometry.values().cloned().collect::<Vec<_>>(),
                        &active.capabilities.rtp,
                    )
                })
            })
            .transpose();
        if let Err(error) = &prepared {
            agent_log!(
                self,
                Warn,
                "RTP allocation {operation:?} facts rejected: {error:?}"
            );
        }
        let prepared = prepared.ok().flatten();
        for (slot, want) in pending.wanted {
            let assigned = slots
                .iter()
                .find(|value| value.slot == slot)
                .filter(|_| correlated);
            let value = prepared
                .as_ref()
                .and_then(|prepared| prepared.get(&slot))
                .and_then(|value| value.as_ref().ok());
            let identity_pending = matches!(
                prepared.as_ref().and_then(|prepared| prepared.get(&slot)),
                Some(Err(crate::client_sdp::Error::IdentityPending))
            );
            if let Some(Err(error)) = prepared.as_ref().and_then(|prepared| prepared.get(&slot))
                && !identity_pending
            {
                agent_log!(
                    self,
                    Warn,
                    "RTP allocation {operation:?} for {slot:?} facts rejected: {error:?}"
                );
            }
            let complete = value.is_some_and(|value| want.allocation.format.as_ref().is_none_or(|expected| {
                matches!(&value.binding.transport, Some(state::track_binding::Transport::Rtp(parameters))
                    if parameters.codec.as_ref().and_then(|codec| codec.format.as_ref())
                        .is_some_and(|format| state::validation::formats_equivalent(expected, format)))
            }));
            let accepted = active.media_wanted.get(&slot) == Some(&want)
                && (identity_pending && want.allocation.format.is_none() || complete);
            if let Some(assigned) = assigned {
                active
                    .coordinates
                    .insert(slot.clone(), Some(assigned.clone()));
                active
                    .media_leases
                    .entry(slot.clone())
                    .or_insert_with(|| MediaLease {
                        resource: want.resource.clone(),
                        mid: assigned.mid.clone(),
                        identity_pending,
                        retired: !accepted,
                        retirement_failed: false,
                        wire_retired: false,
                        source: want.source.clone(),
                    });
            }
            if let Some(lease) = active.media_leases.get_mut(&slot)
                && lease.resource == want.resource
                && !accepted
            {
                lease.retired = true;
            }
            if accepted
                && let Some(value) = prepared.as_ref().and_then(|prepared| prepared.get(&slot))
            {
                let cache = active
                    .media_preparation
                    .get_or_insert_with(|| Ok(BTreeMap::new()));
                if cache.is_err() {
                    *cache = Ok(BTreeMap::new());
                }
                if let Ok(cache) = cache {
                    cache.insert(slot.clone(), value.clone());
                }
            }
            if !accepted {
                active.media_blocked.insert(slot, want);
            }
        }
        self.intent_dirty = true;
        self.send_intent_if_ready();
        true
    }

    pub(super) fn media_retired(
        &mut self,
        generation: Generation,
        operation: OperationId,
        outcome: crate::client_sdp::ApplyOutcome,
        preparation: Result<crate::client_sdp::Bindings, crate::client_sdp::Error>,
    ) {
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.generation == generation && !active.signaling_terminal)
        else {
            return;
        };
        if !active
            .media_retirement
            .as_ref()
            .is_some_and(|pending| pending.operation == operation)
        {
            return;
        }
        let Some(pending) = active.media_retirement.take() else {
            return;
        };
        if let Err(error) = &preparation {
            agent_log!(self, Warn, "RTP retirement {operation:?} failed: {error:?}");
        }
        let applied = outcome == crate::client_sdp::ApplyOutcome::Applied
            && preparation.as_ref().is_ok_and(|bindings| {
                !bindings
                    .publications
                    .iter()
                    .chain(&bindings.receptions)
                    .any(|binding| pending.leases.iter().any(|lease| lease.mid == binding.mid))
            });
        for retired in pending.leases {
            if active
                .media_leases
                .get(&retired.slot)
                .is_some_and(|lease| lease.resource == retired.resource && lease.mid == retired.mid)
            {
                if applied {
                    active.media_leases.remove(&retired.slot);
                } else if let Some(lease) = active.media_leases.get_mut(&retired.slot) {
                    lease.retirement_failed = true;
                }
            }
            let coordinate_retired = applied
                && active
                    .coordinates
                    .get(&retired.slot)
                    .and_then(Option::as_ref)
                    .is_some_and(|coordinate| coordinate.mid == retired.mid);
            if coordinate_retired {
                active.coordinates.insert(retired.slot.clone(), None);
            }
            if applied && let Some(Ok(prepared)) = active.media_preparation.as_mut()
                && prepared.get(&retired.slot).is_some_and(|value| match value {
                    Ok(value) => matches!(&value.binding.transport, Some(state::track_binding::Transport::Rtp(parameters)) if parameters.mid == retired.mid),
                    Err(_) => coordinate_retired,
                }) {
                prepared.remove(&retired.slot);
            }
        }
        if !applied {
            self.notifications.push_back(Notification::ServerError(
                "local media retirement was not confirmed".into(),
            ));
        }
        self.intent_dirty = true;
        self.send_intent_if_ready();
    }

    pub(super) fn apply_native(&mut self, payload: &[u8]) -> Result<(), AgentError> {
        let changes = self
            .active
            .as_mut()
            .ok_or(SignalingError::Invalid("inactive Session"))?
            .state
            .apply(payload)?;
        let mut discovery_changed = false;
        for change in changes {
            match change {
                Change::Session => {
                    let Some(active) = self.active.as_mut() else {
                        break;
                    };
                    let Some(session) = active.state.session() else {
                        continue;
                    };
                    if let Some(grant) = &session.authorization {
                        active.authorization_expires_at = Some(grant.expires_at_unix_seconds);
                        self.snapshot.authorization_expires_at = active.authorization_expires_at;
                    }
                    if let Some(reason) = session.closed {
                        let failure = match state::CloseReason::try_from(reason).ok() {
                            Some(
                                state::CloseReason::ReconstructionRequired
                                | state::CloseReason::InternalError,
                            ) => Failure::transient(session.diagnostic.clone()),
                            Some(
                                state::CloseReason::AuthorizationExpired
                                | state::CloseReason::AuthorizationRevoked
                                | state::CloseReason::ParticipantReplaced,
                            ) => Failure {
                                class: FailureClass::Authorization,
                                message: session.diagnostic.clone(),
                            },
                            _ => Failure::protocol(session.diagnostic.clone()),
                        };
                        if failure.class == FailureClass::Transient {
                            let generation = active.generation;
                            self.fence_signaling(generation);
                            self.start_attempt(AttemptMode::Replace);
                        } else {
                            self.fail_terminal(failure);
                        }
                        return Ok(());
                    }
                    self.commit_renewal();
                }
                Change::Participant(id) => {
                    discovery_changed = true;
                    let Some(active) = self.active.as_ref() else {
                        continue;
                    };
                    if id != active.participant_id {
                        if let Some(value) = active.state.participants().get(&id) {
                            self.snapshot.participants.insert(
                                id.clone(),
                                crate::Participant {
                                    id: id.clone(),
                                    external_id: value.participant_external_id.clone(),
                                },
                            );
                        } else {
                            self.snapshot.participants.remove(&id);
                        }
                        let tracks = active
                            .state
                            .tracks()
                            .values()
                            .filter(|track| track.participant_id == id)
                            .cloned()
                            .collect::<Vec<_>>();
                        for track in tracks {
                            self.project_native_track(&track.track_id);
                        }
                    }
                    self.intent_dirty = true;
                }
                Change::Track(id) => {
                    discovery_changed = true;
                    self.project_native_track(&id);
                    log::debug!(
                        "Native track discovery changed: generation={:?} track={id} publication={:?}",
                        self.snapshot.generation,
                        self.snapshot.publications.get(&id)
                    );
                    self.intent_dirty = true;
                }
                Change::InitialComplete => {
                    self.intent_dirty = true;
                }
                Change::Owned {
                    resource,
                    retired_binding,
                    state: confirmed,
                    new_evaluation,
                    ..
                } => {
                    if let Some(rejection) = &confirmed.rejection {
                        if new_evaluation {
                            agent_log!(
                                self,
                                Warn,
                                "Native resource {resource:?} rejected: {:?}: {}",
                                rejection.code(),
                                rejection.diagnostic
                            );
                            let message = if rejection.diagnostic.is_empty() {
                                alloc::format!("resource rejected: {:?}", rejection.code())
                            } else {
                                rejection.diagnostic.clone()
                            };
                            self.notifications
                                .push_back(Notification::ServerError(message));
                        }
                    } else {
                        self.snapshot.accepted_intent_revision = self
                            .snapshot
                            .accepted_intent_revision
                            .max(confirmed.evaluated_revision);
                    }
                    let Some(active) = self.active.as_mut() else {
                        continue;
                    };
                    if matches!(
                        confirmed.value,
                        Some(state::confirmed_state::Value::Absent(_))
                    ) {
                        for (slot, lease) in active
                            .media_leases
                            .iter_mut()
                            .filter(|(_, lease)| lease.resource == resource)
                        {
                            if matches!(&confirmed.value, Some(state::confirmed_state::Value::Absent(absent))
                                if absent.reason == state::AbsenceReason::SourceEnded as i32)
                                && let Some(source) = lease.source.as_deref()
                            {
                                self.resources.end_audio_source_observations(source);
                            }
                            if matches!(resource, Resource::Reception(_)) {
                                if active
                                    .media_preparation
                                    .as_ref()
                                    .and_then(|cache| cache.as_ref().ok())
                                    .and_then(|cache| cache.get(slot))
                                    .and_then(|value| value.as_ref().ok())
                                    .is_some_and(|value| {
                                        state::validation::bindings_equivalent(
                                            Some(&value.binding),
                                            retired_binding.as_deref(),
                                        )
                                    })
                                {
                                    lease.wire_retired = true;
                                }
                            } else {
                                lease.retired = true;
                            }
                        }
                        self.intent_dirty = true;
                    } else if matches!(&resource, Resource::Reception(_))
                        && matches!(confirmed.value.as_ref(), Some(state::confirmed_state::Value::Owned(live))
                            if live.execution == state::ExecutionStatus::WaitingForSource as i32)
                    {
                        self.intent_dirty = true;
                    }
                    if let Resource::Reception(slot) = &resource
                        && let Some(coordinate) =
                            active.coordinates.get(slot).and_then(Option::as_ref)
                    {
                        let mappings = if slot.kind() == crate::MediaKind::Audio {
                            &mut self.snapshot.audio_mapping
                        } else {
                            &mut self.snapshot.video_mapping
                        };
                        if let Some(live) = active.state.eligible(&resource)
                            && let Some(track) = &live.track
                        {
                            mappings.insert(coordinate.media_index, track.track_id.clone());
                        } else {
                            mappings.remove(&coordinate.media_index);
                        }
                    }
                    if let Some(channel) = active.payload_resources.get(&resource).copied() {
                        if let Some(live) = active.state.eligible(&resource)
                            && let (Some(track), Some(participant), Some(physical)) = (
                                &live.track,
                                &live.participant,
                                active
                                    .payloads
                                    .get(&channel)
                                    .filter(|physical| physical.state == PayloadState::Active),
                            )
                        {
                            self.topics.install_source(
                                active.generation,
                                crate::topic::TopicSource {
                                    channel,
                                    registration: physical.registration.clone(),
                                    track,
                                    participant,
                                },
                                &mut self.snapshot,
                                &mut self.notifications,
                                &mut self.ids,
                                &mut self.effects,
                            );
                        } else if matches!(
                            confirmed.value,
                            Some(state::confirmed_state::Value::Absent(_))
                        ) {
                            let source_ended = matches!(
                                &confirmed.value,
                                Some(state::confirmed_state::Value::Absent(absent))
                                    if absent.reason == state::AbsenceReason::SourceEnded as i32
                            );
                            if source_ended
                                && let Resource::DataSubscription(source) = &resource
                                && let Some(physical) = active.payloads.get(&channel)
                                && let crate::TopicChannel::Subscriber(subscriber) =
                                    &physical.registration
                            {
                                self.topics.end_source(subscriber, source);
                            }
                            self.topics.detach_source(
                                active.generation,
                                channel,
                                source_ended,
                                &mut self.snapshot,
                                &mut self.notifications,
                                &mut self.effects,
                            );
                        } else {
                            self.topics.suspend_source(active.generation, channel);
                        }
                        if let Some(state::confirmed_state::Value::Absent(absent)) =
                            &confirmed.value
                        {
                            if let Some(physical) = active.payloads.get_mut(&channel) {
                                let reason = absent.reason;
                                physical.restart =
                                    matches!(
                                        state::AbsenceReason::try_from(reason),
                                        Ok(state::AbsenceReason::Removed
                                            | state::AbsenceReason::Gone)
                                    ) || matches!(resource, Resource::Publication { .. })
                                        && reason == state::AbsenceReason::SourceEnded as i32;
                                if physical.state == PayloadState::Active {
                                    physical.state = PayloadState::Closing;
                                    self.effects.push_back(Effect::Rtc(
                                        RtcEffect::CloseDataChannel {
                                            generation: active.generation,
                                            channel,
                                        },
                                    ));
                                }
                            }
                            self.intent_dirty = true;
                        }
                    }
                }
            }
        }
        self.snapshot.catalog_revision = self.snapshot.catalog_revision.saturating_add(1);
        self.bump_snapshot();
        self.notifications.push_back(Notification::SnapshotChanged);
        if discovery_changed {
            self.refresh_resources()?;
        }
        self.send_intent_if_ready();
        Ok(())
    }

    fn project_native_track(&mut self, id: &str) {
        self.snapshot.remote_tracks.remove(id);
        self.snapshot.publications.remove(id);
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let Some(track) = active
            .state
            .tracks()
            .get(id)
            .filter(|track| track.participant_id != active.participant_id)
        else {
            return;
        };
        let Some(spec) = crate::TrackSpec::from_state(track) else {
            return;
        };
        if let Some(kind) = spec.media_kind() {
            self.snapshot.publications.insert(
                id.into(),
                crate::Publication {
                    id: id.into(),
                    participant_id: track.participant_id.clone(),
                    kind,
                    label: track.label.clone(),
                },
            );
        }
        if let Some(participant) = active.state.source_participant(&track.participant_id) {
            self.snapshot.remote_tracks.insert(
                id.into(),
                crate::RemoteTrack {
                    id: id.into(),
                    participant_id: participant.participant_external_id.clone(),
                    spec,
                },
            );
        }
    }

    pub(super) fn commit_renewal(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let Some(renewal) = active.pending_renewal.as_ref() else {
            return;
        };
        if renewal.accepted
            && active
                .state
                .session()
                .and_then(|session| session.authorization.as_ref())
                .is_some_and(|grant| grant.revision >= renewal.expected_revision)
        {
            if let Some(renewal) = active.pending_renewal.take() {
                self.config.token = renewal.token;
            }
        }
    }
}

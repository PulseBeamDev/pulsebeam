use super::*;

mod source_failure;
#[cfg(test)]
mod tests;
use crate::native_state::{Change, Offer, ParticipantKey, Resource};
use pulsebeam_proto::state;

pub(super) const STATE_MEMORY_BYTES: usize = 64 * 1024 * 1024;

impl Session {
    pub(super) fn reliable_subscriber_control_deadline(&self) -> Option<Duration> {
        if !self.reliable_data_enabled || self.signaling_terminal {
            return None;
        }
        self.payloads
            .values()
            .filter(|physical| current_reliable_source(&self.state, physical).is_some())
            .filter_map(|physical| {
                let receiver = physical.data_v1_receiver.as_ref()?;
                receiver
                    .receipt_deadline()
                    .into_iter()
                    .chain(receiver.repair_deadline())
                    .min()
            })
            .min()
    }

    pub(super) fn next_reliable_subscriber_control(
        &mut self,
        now: Duration,
    ) -> Option<crate::data_v1::ReceiverReceipt> {
        if !self.reliable_data_enabled || self.signaling_terminal {
            return None;
        }
        for physical in self.payloads.values_mut() {
            if current_reliable_source(&self.state, physical).is_some()
                && let Some(receiver) = &mut physical.data_v1_receiver
            {
                if let Ok(Some(control)) = receiver.next_receipt_control(now) {
                    return Some(control);
                }
                if let Ok(Some(control)) = receiver.next_repair_control(now) {
                    return Some(crate::data_v1::ReceiverReceipt::Send(control));
                }
            }
        }
        None
    }
}

fn native_capture_epoch(active: &Session, owned: u64) -> Option<u64> {
    let resource = active.state.resource_for_reference(owned)?;
    let physical = active
        .payloads
        .get(active.payload_resources.get(resource)?)?;
    let live = active.state.eligible(resource)?;
    if !physical.current_data_v1_publisher(active.generation, live) {
        return None;
    }
    let state::track::Kind::Data(track) = live.track.as_ref()?.kind.as_ref()? else {
        return None;
    };
    let clock = track.capture_clock.as_ref()?;
    (clock.id == state::codec::NATIVE_CAPTURE_CLOCK_ID).then_some(clock.epoch)
}

fn current_reliable_source<'a>(
    state: &'a NativeState,
    physical: &PayloadChannel,
) -> Option<&'a state::Track> {
    if physical.state != PayloadState::Active
        || physical.data_v1.delivery() != state::DataV1Delivery::Reliable as i32
    {
        return None;
    }
    let (offer, current) = state.logical_offer(&physical.resource)?;
    if physical.offer != Some(offer) || !installed_payload(physical, current) {
        return None;
    }
    current.source.as_ref()
}

pub(super) struct PayloadChannel {
    pub resource: Resource,
    pub registration: crate::TopicChannel,
    pub stream: u16,
    pub state: PayloadState,
    pub restart: bool,
    pub offer: Option<Offer>,
    pub(super) data_v1: state::DataV1Binding,
    pub(super) data_v1_receiver: Option<Box<crate::data_v1::ReceiveChannel>>,
    pub(super) data_v1_sender: Option<Box<crate::data_v1::SendChannel>>,
}

impl PayloadChannel {
    fn install_data_v1_receiver(
        &mut self,
        generation: Generation,
        channel_id: ChannelId,
        logical: &state::LogicalSubscriptionState,
        prepared: Option<&crate::PreparedDataChannel>,
        control: Option<(u64, ChannelId)>,
    ) {
        use core::num::NonZeroU64;
        use pulsebeam_proto::data_endpoint::{Binding, Endpoint};

        if !installed_payload(self, logical) {
            self.data_v1_receiver = None;
            return;
        }
        if self.data_v1_receiver.is_some() {
            return;
        }
        let receiver = || {
            let parameters = self.data_v1;
            let offer = logical.data_v1.as_ref()?;
            let accepted = match logical.execution.as_ref()? {
                state::logical_subscription_state::Execution::Installed(binding) => binding,
                state::logical_subscription_state::Execution::Failure(_) => return None,
            };
            let binding = Binding {
                channel: parameters.payload_channel(generation)?,
                format: parameters.format()?,
                endpoint: Endpoint::Subscriber(offer.endpoint()?),
            };
            let channel =
                crate::data_v1::Channel::from_confirmed(channel_id, binding, prepared, accepted)
                    .ok()?;
            let clock = offer
                .capture_clock
                .as_ref()
                .map(|clock| crate::ffi::CaptureClock {
                    id: clock.id.clone(),
                    epoch: clock.epoch,
                });
            if binding.format.delivery() == pulsebeam_proto::data_endpoint::Delivery::Reliable {
                let (owned, signaling) = control?;
                crate::data_v1::ReceiveChannel::new_reliable(
                    channel,
                    clock,
                    NonZeroU64::new(owned)?,
                    signaling,
                )
                .ok()
            } else {
                crate::data_v1::ReceiveChannel::new(channel, clock).ok()
            }
        };
        self.data_v1_receiver = receiver().map(Box::new);
    }

    pub(super) fn data_v1_publisher(
        &mut self,
        generation: Generation,
        channel_id: ChannelId,
        live: &state::LiveResource,
        prepared: Option<&crate::PreparedDataChannel>,
        control: Option<(core::num::NonZeroU64, ChannelId)>,
    ) -> Option<&mut crate::data_v1::SendChannel> {
        use pulsebeam_proto::data_endpoint::Delivery;
        let (binding, channel) =
            self.prepared_publisher(generation, channel_id, live, prepared, control.is_some())?;
        if self.data_v1_sender.is_none() {
            let state::track::Kind::Data(track) = live.track.as_ref()?.kind.as_ref()? else {
                return None;
            };
            let clock = track
                .capture_clock
                .as_ref()
                .map(|clock| crate::ffi::CaptureClock {
                    id: clock.id.clone(),
                    epoch: clock.epoch,
                });
            let sender = if binding.format.delivery() == Delivery::Reliable {
                let (owned, signaling) = control?;
                crate::data_v1::SendChannel::new_reliable(channel, clock, 0, owned, signaling)
            } else {
                crate::data_v1::SendChannel::new(channel, clock, 0)
            }
            .ok()?;
            self.data_v1_sender = Some(Box::new(sender));
        }
        self.data_v1_sender
            .as_deref_mut()
            .filter(|sender| sender.matches_binding(binding))
    }

    pub(super) fn prepared_publisher(
        &self,
        generation: Generation,
        channel: ChannelId,
        live: &state::LiveResource,
        prepared: Option<&crate::PreparedDataChannel>,
        has_control: bool,
    ) -> Option<(
        pulsebeam_proto::data_endpoint::Binding<Generation>,
        crate::data_v1::Channel,
    )> {
        let (binding, accepted) = self.publisher_binding(generation, live)?;
        if (binding.format.delivery() == pulsebeam_proto::data_endpoint::Delivery::Reliable
            && !has_control)
            || self
                .data_v1_sender
                .as_ref()
                .is_some_and(|sender| !sender.matches_binding(binding))
        {
            return None;
        }
        let prepared =
            crate::data_v1::Channel::from_confirmed(channel, binding, prepared, accepted).ok()?;
        Some((binding, prepared))
    }

    pub(super) fn current_data_v1_publisher(
        &self,
        generation: Generation,
        live: &state::LiveResource,
    ) -> bool {
        self.publisher_binding(generation, live)
            .is_some_and(|(binding, _)| {
                self.data_v1_sender
                    .as_ref()
                    .is_some_and(|sender| sender.matches_binding(binding))
            })
    }

    pub(super) fn settle_data_sender(
        &mut self,
        generation: Generation,
        now: core::time::Duration,
        retiring: bool,
        notifications: &mut VecDeque<Notification>,
    ) {
        let (Some(sender), crate::TopicChannel::Publisher(publisher)) =
            (&mut self.data_v1_sender, &self.registration)
        else {
            return;
        };
        sender.expire(now);
        if let Some(source) = sender.reliable_source() {
            while let Some(unit_seq) = sender.take_unconfirmed(now, retiring) {
                notifications.push_back(Notification::ReliableData {
                    track_id: publisher.topic.clone(),
                    generation,
                    source: source.get(),
                    event: crate::ffi::ReliablePublishEvent::AdmissionUnknown { unit_seq },
                });
            }
        }
        if retiring {
            sender.discard_unit();
        }
    }

    fn publisher_binding<'a>(
        &self,
        generation: Generation,
        live: &'a state::LiveResource,
    ) -> Option<(
        pulsebeam_proto::data_endpoint::Binding<Generation>,
        &'a state::TrackBinding,
    )> {
        use core::num::NonZeroU64;
        use pulsebeam_proto::{
            data_control::PublisherBinding,
            data_endpoint::{Binding, Endpoint},
        };
        let parameters = self.data_v1;
        if self.state != PayloadState::Active {
            return None;
        }
        let state::resource_spec::Declaration::Publication(publication) =
            live.spec.as_ref()?.declaration.as_ref()?
        else {
            return None;
        };
        let state::publication_spec::Kind::Data(data) = publication.kind.as_ref()? else {
            return None;
        };
        let accepted = data.binding.as_ref()?;
        if *accepted != self.binding() {
            return None;
        }
        let source = NonZeroU64::new(live.data_v1_source?)?;
        Some((
            Binding {
                channel: parameters.payload_channel(generation)?,
                format: parameters.format()?,
                endpoint: Endpoint::Publisher(PublisherBinding { source }),
            },
            accepted,
        ))
    }

    fn binding(&self) -> state::TrackBinding {
        self.data_v1.track_binding()
    }
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
    offer: Option<Offer>,
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
    offer: Option<Offer>,
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
        offer: None,
        allocation: crate::client_sdp::Allocation {
            slot: slot.clone(),
            format,
        },
    })
}

pub(super) struct Renewal {
    pub credential: super::Credential,
    pub operation: Option<OperationId>,
    pub expected_revision: u64,
    pub accepted: bool,
    pub retry: Option<TimerId>,
    pub attempts: u8,
    pub uncertain: bool,
    pub issued: bool,
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
    selector: &crate::TrackSelector,
    expected: state::TrackKind,
) -> Option<&'a str> {
    let resource = logical_resource(session, selector, expected)?;
    session
        .state
        .logical_offer(resource)?
        .1
        .source
        .as_ref()
        .map(|track| track.publication_id.as_str())
}

fn rtp(
    session: &mut Session,
    logs: &mut crate::logging::LogBuffer,
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
        if media_want(session, resource.clone(), slot, source).is_some()
            && let Some(coordinate) = session.coordinates.get(slot).and_then(Option::as_ref)
        {
            session
                .media_leases
                .entry(slot.clone())
                .or_insert_with(|| {
                    logs.write(log::Level::Debug, module_path!(), format_args!(
                        "Native bootstrap media lease awaits preparation: kind={} pending={pending}",
                        resource.diagnostic_kind()
                    ));
                    MediaLease {
                        resource: resource.clone(),
                        mid: coordinate.mid.clone(),
                        source: source.map(str::to_owned),
                        identity_pending: pending,
                        retired: !pending,
                        retirement_failed: false,
                        wire_retired: false,
                        offer: None,
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
                offer: None,
            },
        );
    }
    Some(prepared.binding.clone())
}

fn receive_rtp(
    session: &mut Session,
    logs: &mut crate::logging::LogBuffer,
    resource: &Resource,
    slot: &MediaSlot,
    source: &str,
) -> Option<state::TrackBinding> {
    if session.state.target_spec(resource).and_then(|spec| spec.declaration.as_ref()).is_some_and(|declaration| matches!(declaration, state::resource_spec::Declaration::Subscription(value) if value.publication_id != source)) {
        return None;
    }
    rtp(session, logs, resource, slot, Some(source))
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

fn audio_options() -> Option<state::resource_settings::Receive> {
    Some(state::resource_settings::Receive::Audio(
        state::AudioReceiveOptions {},
    ))
}

fn logical_subscription(
    selector: &crate::TrackSelector,
    kind: state::TrackKind,
    receive: Option<state::resource_settings::Receive>,
    data_version: Option<u32>,
) -> state::ResourceSpec {
    state::ResourceSpec {
        settings: Some(state::ResourceSettings {
            enabled: Some(true),
            receive,
        }),
        declaration: Some(state::resource_spec::Declaration::LogicalSubscription(
            state::LogicalSubscriptionSpec {
                data_version,
                source: Some(state::LogicalSourceAddress {
                    room: selector.room.clone(),
                    participant_id: selector.participant_id.clone(),
                    kind: kind as i32,
                    track_id: selector.track_id.clone(),
                }),
            },
        )),
    }
}

fn matches_logical_spec(
    spec: &state::ResourceSpec,
    selector: &crate::TrackSelector,
    kind: state::TrackKind,
) -> bool {
    matches!(&spec.declaration, Some(state::resource_spec::Declaration::LogicalSubscription(subscription))
    if subscription.source.as_ref().is_some_and(|source| {
        source.kind == kind as i32 && source.room == selector.room
            && source.participant_id == selector.participant_id && source.track_id == selector.track_id
    }))
}

pub(super) fn logical_resource<'a>(
    session: &'a Session,
    selector: &crate::TrackSelector,
    kind: state::TrackKind,
) -> Option<&'a Resource> {
    session.state.resources().find(|resource| {
        session
            .state
            .target_spec(resource)
            .is_some_and(|spec| matches_logical_spec(spec, selector, kind))
    })
}

fn prepared_binding<'a>(session: &'a Session, slot: &MediaSlot) -> Option<&'a state::TrackBinding> {
    Some(
        &session
            .media_preparation
            .as_ref()?
            .as_ref()
            .ok()?
            .get(slot)?
            .as_ref()
            .ok()?
            .binding,
    )
}

fn reconcile_logical_media(
    session: &mut Session,
    media: &mut BTreeMap<MediaSlot, MediaWant>,
    slot: &MediaSlot,
    resource: &Resource,
    desired: &state::ResourceSpec,
) -> Result<(), SignalingError> {
    let Some((offer, logical)) = session.state.logical_offer(resource) else {
        return Ok(());
    };
    if !logical
        .spec
        .as_ref()
        .is_some_and(|spec| state::validation::declarations_equivalent(spec, desired))
    {
        return Ok(());
    }
    let Some(source) = &logical.source else {
        return Ok(());
    };
    let Some(mut want) = media_want(
        session,
        resource.clone(),
        slot,
        Some(&source.publication_id),
    ) else {
        return Ok(());
    };
    want.offer = Some(offer);
    if !session.media_leases.contains_key(slot)
        && !session
            .media_allocation
            .as_ref()
            .is_some_and(|pending| pending.wanted.contains_key(slot))
        && let Some(coordinate) = session.coordinates.get(slot).and_then(Option::as_ref)
    {
        // A never-leased bootstrap receiver can serve its first offer. Any
        // subsequent offer must wait for retirement and fresh preparation.
        let compatible = prepared_binding(session, slot).is_some_and(|binding| {
            matches!(binding.transport.as_ref(), Some(state::track_binding::Transport::Rtp(rtp))
                if rtp.mid == coordinate.mid && rtp.codec.as_ref().and_then(|codec| codec.format.as_ref())
                    .zip(want.allocation.format.as_ref()).is_some_and(|(actual, expected)| {
                        state::validation::formats_equivalent(actual, expected)
                    }))
        });
        session.media_leases.insert(
            slot.clone(),
            MediaLease {
                resource: resource.clone(),
                mid: coordinate.mid.clone(),
                identity_pending: false,
                retired: !compatible,
                retirement_failed: false,
                wire_retired: false,
                source: want.source.clone(),
                offer: Some(offer),
            },
        );
    }
    media.insert(slot.clone(), want);
    if session.media_leases.get(slot).is_some_and(|lease| {
        !lease.retired && lease.resource == *resource && lease.offer == Some(offer)
    }) && let Some(binding) = prepared_binding(session, slot).cloned()
    {
        session.state.report_logical_receiver(
            resource,
            offer,
            state::logical_receiver_report::Outcome::Prepared(binding),
        )?;
    }
    Ok(())
}

fn publication(
    track_id: &str,
    enabled: bool,
    kind: state::publication_spec::Kind,
) -> state::ResourceSpec {
    state::ResourceSpec {
        settings: Some(settings(enabled)),
        declaration: Some(state::resource_spec::Declaration::Publication(
            state::PublicationSpec {
                track_id: track_id.into(),
                kind: Some(kind),
            },
        )),
    }
}

fn subscription(
    track: String,
    source_room: String,
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
                publication_id: track,
                kind: kind as i32,
                binding: Some(binding),
                source_room,
            },
        )),
    }
}

fn source_room_for_track(session: &Session, desired: &DesiredState, track: &str) -> Option<String> {
    for selector in &desired.audio.selected {
        if selected(session, selector, state::TrackKind::Audio) == Some(track) {
            return Some(selector.room.clone());
        }
    }
    for selector in desired
        .video
        .iter()
        .filter_map(|video| video.selector.as_ref())
    {
        if selected(session, selector, state::TrackKind::Video) == Some(track) {
            return Some(selector.room.clone());
        }
    }
    None
}

fn data_v1_delivery(mode: crate::TopicMode, reliable: bool) -> Option<state::DataV1Delivery> {
    match mode {
        crate::TopicMode::Lossy => Some(state::DataV1Delivery::Unreliable),
        crate::TopicMode::Latest => Some(state::DataV1Delivery::Latest),
        crate::TopicMode::Ordered if reliable => Some(state::DataV1Delivery::Reliable),
        crate::TopicMode::Ordered => None,
    }
}

fn payload(
    session: &mut Session,
    resource: &Resource,
    registration: crate::TopicChannel,
    offer: Option<Offer>,
    data_v1: state::DataV1Delivery,
    ids: &mut IdGenerator,
    effects: &mut VecDeque<Effect>,
) -> Option<state::TrackBinding> {
    use state::validation::BindingProfile;
    let quic = session.state.binding_profile() == BindingProfile::Quic;
    let channel = if let Some(channel) = session.payload_resources.get(resource).copied() {
        let physical = session.payloads.get(&channel)?;
        if physical.registration != registration
            || physical.offer != offer
            || physical.state != PayloadState::Active
            || physical.data_v1.delivery() != data_v1 as i32
        {
            return None;
        }
        channel
    } else {
        let capacity = if quic {
            const MAX_PER_DIRECTION: usize = 64;
            let publisher = matches!(registration, crate::TopicChannel::Publisher(_));
            let count = session
                .payloads
                .values()
                .filter(|physical| {
                    matches!(physical.registration, crate::TopicChannel::Publisher(_)) == publisher
                })
                .count();
            if count >= MAX_PER_DIRECTION {
                return None;
            }
            u32::from(u16::MAX) + 1
        } else {
            session
                .data_preparations
                .get(&session.signaling_channel)?
                .as_ref()
                .ok()?
                .association_max_channels?
                .min(u32::from(u16::MAX))
        };
        let stream = if !quic
            && session
                .reusable_streams
                .first()
                .is_some_and(|stream| u32::from(*stream) < capacity)
        {
            session.reusable_streams.pop_first()?
        } else {
            if session.next_stream >= capacity {
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
        let version = u32::from(pulsebeam_proto::data_endpoint::DATA_V1);
        let parameters = if quic {
            state::DataV1Binding::Quic(state::QuicDataV1Parameters {
                binding_id: u32::from(stream),
                version,
                incarnation: channel.get(),
                delivery: data_v1 as i32,
            })
        } else {
            state::DataV1Binding::Sctp(state::SctpDataV1Parameters {
                stream_id: Some(u32::from(stream)),
                version,
                channel_incarnation: channel.get(),
                delivery: data_v1 as i32,
            })
        };
        session.payloads.insert(
            channel,
            PayloadChannel {
                resource: resource.clone(),
                registration,
                stream,
                state: PayloadState::Active,
                restart: false,
                offer,
                data_v1: parameters,
                data_v1_receiver: None,
                data_v1_sender: None,
            },
        );
        session.payload_resources.insert(resource.clone(), channel);
        if !quic {
            effects.push_back(Effect::Rtc(RtcEffect::CreateDataChannel {
                generation: session.generation,
                channel,
                specification: DataChannelSpec {
                    label: format!("native/payload/{}", channel.get()),
                    ordered: false,
                    reliability: DataChannelReliability::MaxRetransmits(0),
                    negotiated_id: Some(stream),
                },
            }));
        }
        channel
    };
    let physical = session.payloads.get(&channel)?;
    if quic {
        return Some(physical.binding());
    }
    let prepared = session.data_preparations.get(&channel)?.as_ref().ok()?;
    if prepared.stream_id != physical.stream
        || !prepared.negotiated
        || prepared.ordered
        || prepared.reliability != DataChannelReliability::MaxRetransmits(0)
    {
        return None;
    }
    Some(physical.binding())
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
    logs: &mut crate::logging::LogBuffer,
    desired: &DesiredState,
    resources: &crate::track::TrackResources,
    ids: &mut IdGenerator,
    effects: &mut VecDeque<Effect>,
) -> Result<(), SignalingError> {
    let mut wanted = BTreeMap::new();

    let mut media = BTreeMap::new();
    let joined = session.room_id.is_some();
    for value in desired.publications.iter().filter(|_| joined) {
        let kind = value.kind;
        let slot = value.media_slot();
        let wire_kind = if kind == crate::MediaKind::Audio {
            state::TrackKind::Audio
        } else {
            state::TrackKind::Video
        };
        let resource = Resource::Publication {
            kind: wire_kind,
            track_id: value.track_id.clone(),
        };
        if let Some(want) = media_want(session, resource.clone(), &slot, None) {
            media.insert(slot.clone(), want);
        }
        if let Some(binding) = rtp(session, logs, &resource, &slot, None) {
            let kind = if kind == crate::MediaKind::Audio {
                state::publication_spec::Kind::Audio(state::AudioPublication {
                    binding: Some(binding),
                })
            } else {
                state::publication_spec::Kind::Video(state::VideoPublication {
                    binding: Some(binding),
                })
            };
            wanted.insert(resource, publication(&value.track_id, value.active, kind));
        }
    }
    for video in &desired.video {
        let slot = MediaSlot::RemoteVideo(video.slot);
        let receive = Some(state::resource_settings::Receive::Video(
            state::VideoReceiveOptions {
                max_height: (video.height != 0).then_some(video.height),
                min_height: (video.min_height != 0).then_some(video.min_height),
                min_fps: (video.min_fps != 0).then_some(video.min_fps),
                priority: (video.priority != 0).then_some(video.priority),
            },
        ));
        if let Some(selector) = &video.selector {
            let spec = logical_subscription(selector, state::TrackKind::Video, receive, None);
            let resource = Resource::LogicalReception(slot.clone());
            reconcile_logical_media(session, &mut media, &slot, &resource, &spec)?;
            wanted.insert(resource, spec);
            continue;
        }
        let resource = Resource::Reception(slot.clone());
        let track = &video.publication_id;
        if let Some(want) = media_want(session, resource.clone(), &slot, Some(track)) {
            media.insert(slot.clone(), want);
        }
        if let Some(binding) = receive_rtp(session, logs, &resource, &slot, track) {
            let Some(source_room) = source_room_for_track(session, desired, track) else {
                continue;
            };
            let spec = subscription(
                track.clone(),
                source_room,
                state::TrackKind::Video,
                binding,
                receive,
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
        .collect::<BTreeSet<_>>();
    let mut occupied = BTreeSet::new();
    let mut named = BTreeMap::new();
    for resource in session.state.resources() {
        let Resource::LogicalReception(MediaSlot::RemoteAudio(index)) = resource else {
            continue;
        };
        if let Some(selector) = desired.audio.selected.iter().find(|selector| {
            session
                .state
                .target_spec(resource)
                .is_some_and(|spec| matches_logical_spec(spec, selector, state::TrackKind::Audio))
        }) {
            occupied.insert(*index);
            named.insert(selector.clone(), *index);
        }
    }
    let mut assigned = Vec::new();
    // Keep in-flight exact assignments stable before native and StateBatch confirmation.
    for (slot, want) in &session.media_wanted {
        if let MediaSlot::RemoteAudio(index) = slot
            && want.offer.is_none()
            && !occupied.contains(index)
            && let Some(source) = &want.source
            && audio.remove(source)
        {
            occupied.insert(*index);
            assigned.push((*index, source.clone()));
        }
    }
    for (resource, live) in session.state.owned() {
        if let Resource::Reception(MediaSlot::RemoteAudio(index)) = resource
            && !occupied.contains(index)
            && let Some(track) = live.track.as_ref()
            && audio.remove(&track.publication_id)
        {
            occupied.insert(*index);
            assigned.push((*index, track.publication_id.clone()));
        }
    }
    let free = |occupied: &mut BTreeSet<u32>| {
        let index = (0..=u32::MAX)
            .find(|index| !occupied.contains(index))
            .ok_or(SignalingError::Invalid("audio receiver coordinate space"))?;
        occupied.insert(index);
        Ok::<_, SignalingError>(index)
    };
    for id in audio {
        assigned.push((free(&mut occupied)?, id));
    }
    for (index, id) in assigned {
        let slot = MediaSlot::RemoteAudio(index);
        let resource = Resource::Reception(slot.clone());
        if let Some(want) = media_want(session, resource.clone(), &slot, Some(&id)) {
            media.insert(slot.clone(), want);
        }
        if let Some(binding) = receive_rtp(session, logs, &resource, &slot, &id) {
            let Some(source_room) = source_room_for_track(session, desired, &id) else {
                continue;
            };
            let spec = subscription(
                id,
                source_room,
                state::TrackKind::Audio,
                binding,
                audio_options(),
            );
            if receiver_spec_available(session, &slot, &resource, &spec) {
                wanted.insert(resource, spec);
            }
        }
    }
    for selector in &desired.audio.selected {
        let index = match named.get(selector) {
            Some(index) => *index,
            None => free(&mut occupied)?,
        };
        let slot = MediaSlot::RemoteAudio(index);
        let resource = Resource::LogicalReception(slot.clone());
        let spec = logical_subscription(selector, state::TrackKind::Audio, audio_options(), None);
        reconcile_logical_media(session, &mut media, &slot, &resource, &spec)?;
        wanted.insert(resource, spec);
    }
    let mut data = BTreeMap::new();
    let subscribers: BTreeMap<_, Vec<_>> = desired
        .topics
        .subscribers
        .iter()
        .filter(|_| session.capabilities.data_v1)
        .fold(BTreeMap::new(), |mut by_source, subscriber| {
            by_source
                .entry(crate::TrackSelector {
                    room: subscriber.room.clone(),
                    participant_id: subscriber.participant_id.clone(),
                    track_id: subscriber.topic.clone(),
                })
                .or_default()
                .push(subscriber);
            by_source
        });
    for publisher in desired
        .topics
        .publishers
        .iter()
        .filter(|_| joined && session.capabilities.data_v1)
    {
        data.insert(
            Resource::Publication {
                kind: state::TrackKind::Data,
                track_id: publisher.topic.clone(),
            },
            crate::TopicChannel::Publisher(publisher.clone()),
        );
    }
    for (selector, registrations) in subscribers {
        let resource = Resource::LogicalData(selector.clone());
        wanted.insert(
            resource.clone(),
            logical_subscription(
                &selector,
                state::TrackKind::Data,
                None,
                Some(u32::from(pulsebeam_proto::data_endpoint::DATA_V1)),
            ),
        );
        let Some((offer, logical)) = session.state.logical_offer(&resource) else {
            continue;
        };
        if logical.data_v1.is_none() {
            continue;
        }
        let Some(track) = logical.source.as_ref() else {
            continue;
        };
        let Some(state::track::Kind::Data(format)) = track.kind.as_ref() else {
            continue;
        };
        if session.room_id.as_ref() == Some(&track.room_id)
            && session.participant_id == track.participant_id
        {
            continue;
        }
        let Some(registration) = registrations.into_iter().find(|subscriber| {
            crate::TopicMode::from_delivery(format.delivery) == Some(subscriber.mode)
        }) else {
            continue;
        };
        let Some(mode) = data_v1_delivery(registration.mode, session.reliable_data_enabled) else {
            continue;
        };
        if let Some(channel) = session.payload_resources.get(&resource).copied() {
            let Some(physical) = session.payloads.get_mut(&channel) else {
                continue;
            };
            if physical.offer != Some(offer)
                && matches!(physical.data_v1, state::DataV1Binding::Quic(_))
            {
                retire_quic_payload(session, &resource, channel, effects);
            } else if physical.offer != Some(offer) {
                // The new offer fences the old server binding. Only SID reuse, not
                // installation on another SID, needs the local reset barrier.
                session.payload_resources.remove(&resource);
                match physical.state {
                    PayloadState::Active => {
                        physical.data_v1_receiver = None;
                        physical.state = PayloadState::Closing;
                        effects.push_back(Effect::Rtc(RtcEffect::CloseDataChannel {
                            generation: session.generation,
                            channel,
                        }));
                    }
                    PayloadState::Closing => {}
                    PayloadState::Closed { .. } => release_payload(session, &resource, channel),
                }
            }
        }
        if let Some(binding) = payload(
            session,
            &resource,
            crate::TopicChannel::Subscriber(registration.clone()),
            Some(offer),
            mode,
            ids,
            effects,
        ) {
            session.state.report_logical_receiver(
                &resource,
                offer,
                state::logical_receiver_report::Outcome::Prepared(binding),
            )?;
        }
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
        if retire_quic_payload(session, &resource, channel, effects) {
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
    let data_wanted = data.keys().cloned().collect::<BTreeSet<_>>();
    let released = session
        .payload_resources
        .iter()
        .filter(|(resource, _)| !data_wanted.contains(*resource) && !wanted.contains_key(*resource))
        .map(|(resource, channel)| (resource.clone(), *channel))
        .collect::<Vec<_>>();
    for (resource, channel) in released {
        session.state.stage(resource.clone(), None)?;
        if retire_quic_payload(session, &resource, channel, effects)
            || session.state.retaining(&resource)
        {
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
        let crate::TopicChannel::Publisher(publisher) = &registration else {
            continue;
        };
        let Some(mode) = data_v1_delivery(publisher.mode, session.reliable_data_enabled) else {
            continue;
        };
        if let Some(binding) = payload(
            session,
            &resource,
            registration.clone(),
            None,
            mode,
            ids,
            effects,
        ) {
            wanted.insert(
                resource,
                publication(
                    &publisher.topic,
                    true,
                    state::publication_spec::Kind::Data(state::DataPublication {
                        delivery: publisher.mode.delivery() as i32,
                        binding: Some(binding),
                        capture_clock: resources.data_capture_clock(&publisher.topic).cloned(),
                    }),
                ),
            );
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
            logs.write(
                log::Level::Debug,
                module_path!(),
                format_args!(
                    "Native resource no longer wanted: generation={:?} kind={}",
                    session.generation,
                    resource.diagnostic_kind()
                ),
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
        if let Some(offer) = lease.offer {
            if media.get(slot).is_none_or(|want| want.offer != Some(offer)) {
                lease.retired = true;
            }
            continue;
        }
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
            logs.write(
                log::Level::Debug,
                module_path!(),
                format_args!(
                    "Native media publication wanted: generation={:?}",
                    session.generation
                ),
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

fn media_needed(
    session: &Session,
    desired: &DesiredState,
    slot: &MediaSlot,
    want: &MediaWant,
) -> bool {
    if let Some(expected) = want.offer {
        let Some((current, confirmed)) = session.state.logical_offer(&want.resource) else {
            return false;
        };
        if current != expected {
            return false;
        }
        let Some(spec) = &confirmed.spec else {
            return false;
        };
        return match slot {
            MediaSlot::RemoteVideo(index) => desired.video.iter().any(|video| {
                video.slot == *index
                    && video.selector.as_ref().is_some_and(|selector| {
                        matches_logical_spec(spec, selector, state::TrackKind::Video)
                    })
            }),
            MediaSlot::RemoteAudio(_) => desired
                .audio
                .selected
                .iter()
                .any(|selector| matches_logical_spec(spec, selector, state::TrackKind::Audio)),
            _ => false,
        };
    }
    match slot {
        MediaSlot::LocalVideo(_) | MediaSlot::LocalAudio(_) => {
            desired.publications.iter().any(|value| {
                value.media_slot() == *slot
                    && want.resource
                        == (Resource::Publication {
                            kind: if value.kind == crate::MediaKind::Audio {
                                state::TrackKind::Audio
                            } else {
                                state::TrackKind::Video
                            },
                            track_id: value.track_id.clone(),
                        })
            })
        }
        MediaSlot::RemoteVideo(index) => desired.video.iter().any(|value| {
            value.slot == *index
                && want.source.as_deref()
                    == match &value.selector {
                        Some(selector) => selected(session, selector, state::TrackKind::Video),
                        None => Some(value.publication_id.as_str()),
                    }
        }),
        MediaSlot::RemoteAudio(_) => want.source.as_ref().is_some_and(|source| {
            desired.audio.pinned.contains(source)
                || desired.audio.selected.iter().any(|selector| {
                    selected(session, selector, state::TrackKind::Audio) == Some(source.as_str())
                })
        }),
    }
}

pub(super) fn fence_unwanted_media(
    session: &mut Session,
    desired: &DesiredState,
    ids: &mut IdGenerator,
    effects: &mut VecDeque<Effect>,
) -> Result<(), SignalingError> {
    let removed = session
        .media_wanted
        .iter()
        .filter(|(slot, want)| !media_needed(session, desired, slot, want))
        .map(|(slot, _)| slot.clone())
        .collect::<Vec<_>>();
    for slot in removed {
        session.media_wanted.remove(&slot);
        if let Some(lease) = session.media_leases.get_mut(&slot) {
            lease.retired = true;
            if lease.offer.is_none() {
                session.state.stage(lease.resource.clone(), None)?;
            }
        }
    }
    retire_media(session, ids, effects);
    Ok(())
}

pub(super) fn logical_payload_ready(session: &Session, resource: &Resource) -> bool {
    let Some(physical) = session
        .payload_resources
        .get(resource)
        .and_then(|channel| session.payloads.get(channel))
    else {
        return false;
    };
    session
        .state
        .logical_offer(resource)
        .is_some_and(|(offer, logical)| {
            physical.offer == Some(offer)
                && installed_payload(physical, logical)
                && physical.data_v1_receiver.is_some()
        })
}

fn installed_payload(physical: &PayloadChannel, state: &state::LogicalSubscriptionState) -> bool {
    physical.state == PayloadState::Active
        && matches!(&state.execution, Some(state::logical_subscription_state::Execution::Installed(binding))
            if *binding == physical.binding())
}

fn retire_quic_payload(
    session: &mut Session,
    resource: &Resource,
    channel: ChannelId,
    effects: &mut VecDeque<Effect>,
) -> bool {
    if !session
        .payloads
        .get(&channel)
        .is_some_and(|physical| matches!(physical.data_v1, state::DataV1Binding::Quic(_)))
    {
        return false;
    }
    effects.push_back(Effect::Quic(crate::QuicEffect::RetireData {
        generation: session.generation,
        channel,
    }));
    release_payload(session, resource, channel);
    true
}

pub(super) fn release_payload(session: &mut Session, resource: &Resource, channel: ChannelId) {
    if session.payload_resources.get(resource) == Some(&channel) {
        session.payload_resources.remove(resource);
    }
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
            lease.retired
                && !lease.retirement_failed
                && (!session.state.retaining(&lease.resource)
                    || lease.offer.is_some_and(|offer| {
                        session.state.reference(&lease.resource) == Some(offer.reference)
                    }))
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

pub(super) fn receiver_track<'a>(
    session: &'a Session,
    slot: &MediaSlot,
) -> Option<&'a state::Track> {
    let lease = session.media_leases.get(slot)?;
    let wanted = session.media_wanted.get(slot)?;
    if lease.retired
        || lease.identity_pending
        || lease.source != wanted.source
        || lease.offer != wanted.offer
    {
        return None;
    }
    let source = lease.source.as_ref()?;
    let track = if let Some(expected) = lease.offer {
        let (current, logical) = session.state.logical_offer(&lease.resource)?;
        if current != expected {
            return None;
        }
        let Some(state::logical_subscription_state::Execution::Installed(binding)) =
            &logical.execution
        else {
            return None;
        };
        if !state::validation::bindings_equivalent(Some(binding), prepared_binding(session, slot)) {
            return None;
        }
        logical.source.as_ref()?
    } else {
        let live = session.state.eligible(&lease.resource)?;
        live.track.as_ref()?
    };
    (&track.publication_id == source).then_some(track)
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
        let track = receiver_track(active, slot)?;
        let binding = active.coordinates.get(slot)?.as_ref()?;
        if binding.mid != lease.mid || binding.direction != crate::MediaDirection::ReceiveOnly {
            return None;
        }
        Some(crate::MediaObservationTarget {
            generation: active.generation,
            reference: active.state.reference(&lease.resource)?,
            source: track.publication_id.clone(),
            binding: binding.clone(),
        })
    }

    pub(super) fn audio_observation_target(
        &self,
        selector: &crate::TrackSelector,
    ) -> Option<crate::MediaObservationTarget> {
        let active = self.active.as_ref()?;
        let source = selected(active, selector, state::TrackKind::Audio)?;
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
        let track = active.and_then(|active| active.state.source(source));
        if track.is_some_and(|track| {
            kind(track) != state::TrackKind::Audio
                || active.is_some_and(|active| track.participant_id == active.participant_id)
        }) {
            return false;
        }
        let selector = active.and_then(|active| {
            self.desired.audio.selected.iter().find(|selector| {
                selected(active, selector, state::TrackKind::Audio) == Some(source)
            })
        });
        self.resources
            .audio_source_authorized(crate::TrackOwner(owner), source, selector)
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
                    .map(|publication| publication.track_id.as_str())
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
                let failure = state::Failure {
                    code: state::FailureCode::InternalFailure as i32,
                    diagnostic: "configured receiver changed unexpectedly".into(),
                };
                if let Some(offer) = lease.offer {
                    let _ = active.state.report_logical_receiver(
                        &lease.resource,
                        offer,
                        state::logical_receiver_report::Outcome::Failure(failure),
                    );
                } else if let Some(reference) = active.state.reference(&lease.resource) {
                    let _ = active.state.report_execution(reference, failure);
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
            if project_retired_receivers(active, &mut self.snapshot) {
                self.snapshot.version = self.snapshot.version.saturating_add(1);
                self.notifications.push_back(Notification::SnapshotChanged);
            }
            self.intent_dirty = true;
            self.request_reconciliation();
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
                && media_needed(active, &self.desired, &slot, &want)
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
                        offer: want.offer,
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
                if let Some(offer) = want.offer
                    && active.media_wanted.get(&slot) == Some(&want)
                    && media_needed(active, &self.desired, &slot, &want)
                {
                    let _ = active.state.report_logical_receiver(
                        &want.resource,
                        offer,
                        state::logical_receiver_report::Outcome::Failure(state::Failure {
                            code: state::FailureCode::Incompatible as i32,
                            diagnostic: "local receiver preparation failed".into(),
                        }),
                    );
                }
                active.media_blocked.insert(slot, want);
            }
        }
        self.intent_dirty = true;
        self.request_reconciliation();
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
        self.request_reconciliation();
    }

    fn apply_reliable_data_control(
        &mut self,
        control: state::NativeDataControl,
    ) -> Result<(), AgentError> {
        use crate::data_v1::{PublisherControl, ReceiverControl};
        use crate::ffi::ReliablePublishEvent;
        use state::native_data_control::Endpoint;
        let active = self
            .active
            .as_mut()
            .filter(|active| active.reliable_data_enabled && !active.signaling_terminal)
            .ok_or(SignalingError::Malformed)?;
        let (owned, publisher) = match &control.endpoint {
            Some(Endpoint::Subscriber(frame)) => (frame.owned, false),
            Some(Endpoint::Publisher(frame)) => (frame.owned, true),
            None => return Ok(()),
        };
        let Some(channel) = active
            .state
            .resource_for_reference(owned)
            .and_then(|resource| active.payload_resources.get(resource))
        else {
            return Ok(());
        };
        let Some(physical) = active.payloads.get_mut(channel) else {
            return Ok(());
        };
        if publisher {
            let generation = active.generation;
            if !active
                .state
                .eligible(&physical.resource)
                .is_some_and(|live| physical.current_data_v1_publisher(generation, live))
            {
                return Ok(());
            }
            let (Some(sender), crate::TopicChannel::Publisher(registration)) =
                (&mut physical.data_v1_sender, &physical.registration)
            else {
                return Ok(());
            };
            let Some(source) = sender.reliable_source() else {
                return Ok(());
            };
            if !self.desired.topics.publishers.contains(registration) {
                return Ok(());
            }
            let signaling = active.signaling_channel;
            let event = match sender.handle_publisher_control(
                generation,
                signaling,
                control,
                self.data_now,
            ) {
                Ok(PublisherControl::Admitted { unit_seq }) => {
                    ReliablePublishEvent::SourceAdmitted { unit_seq }
                }
                Ok(PublisherControl::Delivery(outcome)) => ReliablePublishEvent::Delivery {
                    unit_seq: outcome.unit_seq,
                    branch: outcome.branch.get(),
                    outcome: outcome.outcome.into(),
                },
                Ok(PublisherControl::Reply(response)) => {
                    if self.pending_signal.is_none() && self.signal_retry.is_none() {
                        let bytes = state::codec::encode_data_control(&response)
                            .map_err(|_| SignalingError::Encode)?;
                        self.send_native_signal(generation, signaling, bytes, false);
                    }
                    return Ok(());
                }
                _ => return Ok(()),
            };
            self.notifications.push_back(Notification::ReliableData {
                track_id: registration.topic.clone(),
                generation,
                source: source.get(),
                event,
            });
            return Ok(());
        }
        let Some(source) = current_reliable_source(&active.state, physical) else {
            return Ok(());
        };
        let (Some(receiver), crate::TopicChannel::Subscriber(subscriber)) =
            (&mut physical.data_v1_receiver, &physical.registration)
        else {
            return Ok(());
        };
        let (generation, signaling) = (active.generation, active.signaling_channel);
        if let Ok(ReceiverControl::Expected {
            response,
            assembled,
        }) = receiver.handle_control(generation, signaling, control, self.data_now)
        {
            if let Some(unit) = assembled {
                let owners =
                    self.resources
                        .receive_data_v1(subscriber, &source.publication_id, unit);
                if !owners.is_empty() {
                    self.notifications
                        .push_back(Notification::DataReady { owners });
                }
            }
            // The parent retries expectations; never queue a stale outcome behind State.
            if self.pending_signal.is_none() && self.signal_retry.is_none() {
                let bytes = state::codec::encode_data_control(&response)
                    .map_err(|_| SignalingError::Encode)?;
                self.send_native_signal(generation, signaling, bytes, false);
            }
        }
        Ok(())
    }

    /// Native host hook. Call only when the environment supplies the same
    /// monotonic clock as its internal DATA capture provider.
    #[doc(hidden)]
    pub fn native_clock_probe(&mut self, now: core::time::Duration) {
        use state::{
            NativeClockControl, NativeClockEnable, NativeClockExchange,
            codec::{ClockDirection, encode_clock_control},
            native_clock_exchange::Message,
        };
        if self.pending_signal.is_some() || self.signal_retry.is_some() {
            return;
        }
        let Some(active) = self
            .active
            .as_ref()
            .filter(|active| !active.signaling_terminal)
        else {
            return;
        };
        let previous = self
            .native_probe
            .filter(|(generation, _, _)| *generation == active.generation);
        if previous
            .is_some_and(|(_, _, at)| now.saturating_sub(at) < core::time::Duration::from_secs(1))
        {
            return;
        }
        let candidates = || {
            active.payload_resources.keys().filter_map(|resource| {
                let owned = active.state.reference(resource)?;
                let epoch = native_capture_epoch(active, owned)?;
                Some((owned, epoch))
            })
        };
        let cursor = previous.map_or(0, |(_, owned, _)| owned);
        let Some((owned, clock_epoch)) = candidates()
            .filter(|(owned, _)| *owned > cursor)
            .min()
            .or_else(|| candidates().min())
        else {
            return;
        };
        let Ok(bytes) = encode_clock_control(
            &NativeClockControl {
                exchange: Some(NativeClockExchange {
                    owned,
                    clock_epoch,
                    message: Some(Message::Enable(NativeClockEnable {})),
                }),
            },
            ClockDirection::ClientToServer,
        ) else {
            return;
        };
        let (generation, channel) = (active.generation, active.signaling_channel);
        self.native_probe = Some((generation, owned, now));
        self.send_native_signal(generation, channel, bytes, false);
    }

    /// Native host hook for a binary control message from the current RTC
    /// generation. The clock sampler is never invoked for ordinary State.
    #[doc(hidden)]
    pub fn try_native_clock_frame(
        &mut self,
        generation: Generation,
        channel: ChannelId,
        payload: &[u8],
        mut sample: impl FnMut() -> Option<i64>,
    ) -> bool {
        use state::{
            NativeClockControl, NativeClockExchange, NativeClockReply,
            codec::{ClockDirection, decode_clock_control_frame, encode_clock_control},
            native_clock_exchange::Message,
        };
        let Some(active) = self.active.as_ref().filter(|active| {
            active.generation == generation
                && active.signaling_channel == channel
                && !active.signaling_terminal
        }) else {
            return false;
        };
        let Some(result) = decode_clock_control_frame(payload, ClockDirection::ServerToClient)
        else {
            return false;
        };
        if let Ok(frame) = result
            && let Some(NativeClockExchange {
                owned,
                clock_epoch,
                message: Some(Message::Challenge(challenge)),
            }) = frame.exchange
            && self
                .native_probe
                .is_some_and(|(current, probing, _)| current == generation && probing == owned)
            && native_capture_epoch(active, owned) == Some(clock_epoch)
            && self.pending_signal.is_none()
            && self.signal_retry.is_none()
            && let Some(received_nanos) = sample()
            && let Some(sent_nanos) = sample()
            && let Ok(bytes) = encode_clock_control(
                &NativeClockControl {
                    exchange: Some(NativeClockExchange {
                        owned,
                        clock_epoch,
                        message: Some(Message::Reply(NativeClockReply {
                            nonce: challenge.nonce,
                            received_nanos,
                            sent_nanos,
                        })),
                    }),
                },
                ClockDirection::ClientToServer,
            )
        {
            self.send_native_signal(generation, channel, bytes, false);
        }
        true
    }

    pub(super) fn apply_native(&mut self, payload: &[u8]) -> Result<(), AgentError> {
        let decoded = self
            .active
            .as_ref()
            .ok_or(SignalingError::Invalid("inactive Session"))?
            .state
            .decode_frame(payload)
            .map_err(|_| SignalingError::Malformed)?;
        let batch = match decoded {
            state::codec::NativeFrame::State(batch) => batch,
            state::codec::NativeFrame::Data(control) => {
                return self.apply_reliable_data_control(control);
            }
        };
        let changes = self
            .active
            .as_mut()
            .ok_or(SignalingError::Invalid("inactive Session"))?
            .state
            .apply_batch(batch)?;
        let progress = !changes.is_empty();
        let mut discovery_changed = false;
        for change in changes {
            match change {
                Change::Catalog(record) => {
                    if let Some((owner, state)) = self.catalogs.apply(*record) {
                        if state.status == crate::ffi::WatchStatus::Failed {
                            self.notifications.push_back(Notification::CatalogWatchTerminated { owner: owner.id(), state });
                        } else if !self.notifications.iter().any(|notification| matches!(notification, Notification::CatalogWatchChanged { owner: current } if *current == owner.id())) {
                            self.notifications.push_back(Notification::CatalogWatchChanged { owner: owner.id() });
                        }
                    }
                }
                Change::Session => {
                    let Some(active) = self.active.as_mut() else {
                        break;
                    };
                    let Some(session) = active.state.session() else {
                        continue;
                    };
                    if let Some(grant) = &session.authorization {
                        active.authorization_revision =
                            active.authorization_revision.max(grant.revision);
                        active.authorization_expires_at = Some(grant.expires_at_unix_seconds);
                        self.snapshot.authorization_expires_at = active.authorization_expires_at;
                    }
                    if active.room_id.is_some()
                        && active.room_id == self.snapshot.desired_room
                        && session.closed.is_none()
                    {
                        self.joined_successor = session.successor_receipt.clone();
                    }
                    if let Some(reason) = session.closed {
                        let failure = match state::CloseReason::try_from(reason).ok() {
                            Some(
                                state::CloseReason::ReconstructionRequired
                                | state::CloseReason::InternalError,
                            ) => Failure::transient(session.diagnostic.clone()),
                            Some(
                                state::CloseReason::AuthorizationExpired
                                | state::CloseReason::AuthorizationRevoked,
                            ) => Failure {
                                class: FailureClass::Authorization,
                                message: session.diagnostic.clone(),
                            },
                            Some(state::CloseReason::ParticipantReplaced) => Failure {
                                class: FailureClass::ParticipantSuperseded,
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
                    self.request_reconciliation();
                }
                Change::Participant(id) => {
                    discovery_changed = true;
                    let Some(active) = self.active.as_ref() else {
                        continue;
                    };
                    if Some(&id.room_id) == active.room_id.as_ref()
                        && id.participant_id != active.participant_id
                    {
                        if active.state.participants().contains_key(&id) {
                            self.snapshot.participants.insert(
                                id.participant_id.clone(),
                                crate::Participant {
                                    id: id.participant_id.clone(),
                                },
                            );
                        } else {
                            self.snapshot.participants.remove(&id.participant_id);
                        }
                        let tracks = active
                            .state
                            .tracks()
                            .values()
                            .filter(|track| {
                                track.room_id == id.room_id
                                    && track.participant_id == id.participant_id
                            })
                            .cloned()
                            .collect::<Vec<_>>();
                        for track in tracks {
                            self.project_native_track(&track.publication_id);
                        }
                    }
                    self.intent_dirty = true;
                }
                Change::Track(id) => {
                    discovery_changed = true;
                    self.project_native_track(&id);
                    self.logs.write(
                        log::Level::Debug,
                        module_path!(),
                        format_args!(
                            "Native track discovery changed: generation={:?} present={}",
                            self.snapshot.generation,
                            self.snapshot.publications.contains_key(&id)
                        ),
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
                    evaluates_current,
                    ..
                } => {
                    if matches!(resource, Resource::LogicalData(_)) {
                        if evaluates_current {
                            self.apply_data_read_failure(&resource, &confirmed)?;
                        }
                        self.intent_dirty = true;
                    }
                    if let Some(rejection) = &confirmed.rejection {
                        if new_evaluation {
                            let message = if rejection.diagnostic.is_empty()
                                || matches!(resource, Resource::LogicalData(_))
                            {
                                alloc::format!("resource rejected: {:?}", rejection.code())
                            } else {
                                rejection.diagnostic.clone()
                            };
                            agent_log!(
                                self,
                                Warn,
                                "Native resource rejected: kind={} code={:?}",
                                resource.diagnostic_kind(),
                                rejection.code()
                            );
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
                    // Rotating a denied RTP binding would replace its evaluated declaration
                    // and retry the denial without a change in intent.
                    let denied_rtp_publication = evaluates_current
                        && matches!(
                            resource,
                            Resource::Publication {
                                kind: state::TrackKind::Audio | state::TrackKind::Video,
                                ..
                            }
                        )
                        && matches!(&confirmed.value, Some(state::confirmed_state::Value::Absent(absent))
                            if absent.reason() == state::AbsenceReason::CreateRejected)
                        && confirmed.rejection.as_ref().is_some_and(|failure| {
                            failure.code() == state::FailureCode::PermissionDenied
                        });
                    if !denied_rtp_publication
                        && matches!(
                            confirmed.value,
                            Some(state::confirmed_state::Value::Absent(_))
                        )
                    {
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
                    if matches!(resource, Resource::LogicalReception(_)) {
                        self.intent_dirty = true;
                    }
                    if let Resource::Reception(slot) | Resource::LogicalReception(slot) = &resource
                        && let Some(coordinate) =
                            active.coordinates.get(slot).and_then(Option::as_ref)
                    {
                        let mappings = if slot.kind() == crate::MediaKind::Audio {
                            &mut self.snapshot.audio_mapping
                        } else {
                            &mut self.snapshot.video_mapping
                        };
                        let track = if matches!(resource, Resource::LogicalReception(_)) {
                            receiver_track(active, slot)
                        } else {
                            active
                                .state
                                .eligible(&resource)
                                .and_then(|live| live.track.as_ref())
                        };
                        if let Some(track) = track {
                            mappings.insert(coordinate.media_index, track.publication_id.clone());
                        } else {
                            mappings.remove(&coordinate.media_index);
                        }
                    }
                    if let Some(channel) = active.payload_resources.get(&resource).copied() {
                        if let Some(physical) = active.payloads.get_mut(&channel)
                            && let Some(offer) = physical.offer
                        {
                            let current = active
                                .state
                                .logical_offer(&resource)
                                .filter(|(current, _)| *current == offer)
                                .map(|(_, state)| state);
                            if let Some(logical) = current {
                                let prepared = active
                                    .data_preparations
                                    .get(&channel)
                                    .and_then(|result| result.as_ref().ok());
                                physical.install_data_v1_receiver(
                                    active.generation,
                                    channel,
                                    logical,
                                    prepared,
                                    active
                                        .reliable_data_enabled
                                        .then_some((offer.reference, active.signaling_channel)),
                                );
                                continue;
                            }
                            if retire_quic_payload(active, &resource, channel, &mut self.effects) {
                                continue;
                            }
                            let Some(physical) = active.payloads.get_mut(&channel) else {
                                continue;
                            };
                            physical.data_v1_receiver = None;
                            if physical.state == PayloadState::Active {
                                physical.state = PayloadState::Closing;
                                self.effects
                                    .push_back(Effect::Rtc(RtcEffect::CloseDataChannel {
                                        generation: active.generation,
                                        channel,
                                    }));
                            }
                            continue;
                        }
                        if let Some(state::confirmed_state::Value::Absent(absent)) =
                            &confirmed.value
                        {
                            if retire_quic_payload(active, &resource, channel, &mut self.effects) {
                                self.intent_dirty = true;
                                continue;
                            }
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
        if progress {
            self.request_reconciliation();
        }
        Ok(())
    }

    fn project_native_track(&mut self, id: &str) {
        self.snapshot.remote_tracks.remove(id);
        self.snapshot.publications.remove(id);
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let Some(track) = active.state.tracks().get(id).filter(|track| {
            Some(&track.room_id) == active.room_id.as_ref()
                && track.participant_id != active.participant_id
        }) else {
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
                    track_id: track.track_id.clone(),
                },
            );
        }
        if let Some(participant) = active
            .state
            .source_participant(&ParticipantKey::from(track))
        {
            self.snapshot.remote_tracks.insert(
                id.into(),
                crate::RemoteTrack {
                    id: id.into(),
                    participant_id: participant.participant_id.clone(),
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
                let revision = renewal.credential.revision;
                self.snapshot.accepted_credential_revision = Some(revision);
                active.credential = renewal.credential;
                self.bump_snapshot();
                self.notifications.push_back(Notification::SnapshotChanged);
                self.credential_accepted(revision);
                self.request_reconciliation();
            }
        }
    }
}

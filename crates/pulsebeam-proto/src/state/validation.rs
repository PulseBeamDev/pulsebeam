//! Stateless wire validity. Authority, cross-record revision order, lifetime
//! reservations and adapter capability checks belong to their connection owner.

use alloc::collections::BTreeSet;

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationError {
    Batch,
    Key,
    Desired,
    Confirmed,
    Identity,
    Settings,
    Binding,
    Format,
    Failure,
    Capability,
}

type Result<T = ()> = core::result::Result<T, ValidationError>;

pub fn validate_client_batch(batch: &ClientStateBatch) -> Result {
    if batch.records.is_empty() || batch.records.len() > codec::MAX_RECORDS {
        return Err(ValidationError::Batch);
    }
    let mut previous = 0;
    for record in &batch.records {
        if record.owned == 0 {
            return Err(ValidationError::Key);
        }
        let revision = match (&record.desired, &record.execution) {
            (Some(desired), None) => {
                if record.owned > desired.revision
                    || !matches!(&desired.value, Some(desired_state::Value::Present(_)))
                        && !matches!(&desired.value, Some(desired_state::Value::Absent(true)) if record.owned < desired.revision)
                {
                    return Err(ValidationError::Desired);
                }
                desired.revision
            }
            (None, Some(report)) if record.owned < report.revision => {
                validate_execution_report(report)?;
                report.revision
            }
            _ => return Err(ValidationError::Desired),
        };
        if revision <= previous {
            return Err(ValidationError::Desired);
        }
        previous = revision;
        // Invalid configuration gets a scoped evaluation rejection, not a
        // malformed-envelope failure.
    }
    Ok(())
}

pub fn validate_server_batch(batch: &ServerStateBatch) -> Result {
    if batch.records.len() > codec::MAX_RECORDS
        || batch.records.is_empty() && !batch.initial_complete
    {
        return Err(ValidationError::Batch);
    }
    for record in &batch.records {
        let key = record
            .key
            .as_ref()
            .and_then(|key| key.key.as_ref())
            .ok_or(ValidationError::Key)?;
        validate_key(key)?;
        let confirmed = record
            .confirmed
            .as_ref()
            .ok_or(ValidationError::Confirmed)?;
        validate_confirmed(key, confirmed)?;
    }
    Ok(())
}

fn validate_key(key: &resource_key::Key) -> Result {
    match key {
        resource_key::Key::Owned(v) if *v != 0 => Ok(()),
        resource_key::Key::Participant(id) | resource_key::Key::Track(id) if text(id, 128) => {
            Ok(())
        }
        resource_key::Key::Session(true) => Ok(()),
        _ => Err(ValidationError::Key),
    }
}

fn validate_confirmed(key: &resource_key::Key, confirmed: &ConfirmedState) -> Result {
    use confirmed_state::Value;
    use resource_key::Key;
    let owned = matches!(key, Key::Owned(_));
    if !owned && (confirmed.evaluated_revision != 0 || confirmed.rejection.is_some()) {
        return Err(ValidationError::Confirmed);
    }
    if let Some(rejection) = &confirmed.rejection {
        validate_failure(rejection)?;
    }
    match (key, &confirmed.value) {
        (Key::Owned(reference), Some(Value::Owned(live))) => {
            if confirmed.evaluated_revision < *reference {
                return Err(ValidationError::Confirmed);
            }
            validate_live(live)
        }
        (Key::Owned(_), Some(Value::Absent(absent))) => {
            if !known::<AbsenceReason>(absent.reason)
                || absent.reason == AbsenceReason::CreateRejected as i32
                    && confirmed.rejection.is_none()
            {
                return Err(ValidationError::Confirmed);
            }
            Ok(())
        }
        (Key::Participant(id), Some(Value::Participant(participant)))
            if id == &participant.participant_id =>
        {
            validate_participant(participant)
        }
        (Key::Track(id), Some(Value::Track(track))) if id == &track.track_id => {
            validate_track(track)
        }
        (Key::Participant(_) | Key::Track(_), Some(Value::Absent(absent)))
            if absent.reason == AbsenceReason::Removed as i32
                || absent.reason == AbsenceReason::Gone as i32 =>
        {
            Ok(())
        }
        (Key::Session(true), Some(Value::Session(session))) => {
            let authorization = session
                .authorization
                .as_ref()
                .ok_or(ValidationError::Confirmed)?;
            if authorization.revision == 0
                || session.closed.is_some_and(|v| !known::<CloseReason>(v))
                || session.diagnostic.len() > 256
                || session.closed.is_none() && !session.diagnostic.is_empty()
            {
                return Err(ValidationError::Confirmed);
            }
            Ok(())
        }
        _ => Err(ValidationError::Confirmed),
    }
}

pub fn validate_spec(spec: &ResourceSpec) -> Result {
    let settings = spec.settings.as_ref().ok_or(ValidationError::Settings)?;
    if settings.enabled.is_none() {
        return Err(ValidationError::Settings);
    }
    match spec.declaration.as_ref().ok_or(ValidationError::Binding)? {
        resource_spec::Declaration::Publication(publication) => {
            if !text(&publication.label, 256) || settings.receive.is_some() {
                return Err(ValidationError::Settings);
            }
            match publication.kind.as_ref().ok_or(ValidationError::Binding)? {
                publication_spec::Kind::Audio(audio) => {
                    validate_binding(audio.binding.as_ref(), TrackKind::Audio, true)
                }
                publication_spec::Kind::Video(video) => {
                    validate_binding(video.binding.as_ref(), TrackKind::Video, true)
                }
                publication_spec::Kind::Data(data) => {
                    if !known::<DataDelivery>(data.delivery) {
                        return Err(ValidationError::Binding);
                    }
                    validate_binding(data.binding.as_ref(), TrackKind::Data, true)
                }
            }
        }
        resource_spec::Declaration::Subscription(subscription) => {
            if !text(&subscription.track_id, 128) {
                return Err(ValidationError::Identity);
            }
            let kind =
                TrackKind::try_from(subscription.kind).map_err(|_| ValidationError::Binding)?;
            match (kind, &settings.receive) {
                (TrackKind::Audio, Some(resource_settings::Receive::Audio(options))) => {
                    validate_playout(options.playout_delay.as_ref())?;
                }
                (TrackKind::Video, Some(resource_settings::Receive::Video(options))) => {
                    validate_video(options)?;
                }
                (TrackKind::Data, None) => (),
                _ => return Err(ValidationError::Settings),
            }
            match (
                kind,
                subscription.binding.as_ref(),
                subscription.rtp_offer.as_ref(),
            ) {
                (TrackKind::Data, binding, None) => validate_binding(binding, kind, false),
                (TrackKind::Audio | TrackKind::Video, None, Some(offer)) => {
                    validate_receive_offer(offer, kind)
                }
                _ => Err(ValidationError::Binding),
            }
        }
    }
}

pub fn validate_participant(participant: &Participant) -> Result {
    if !text(&participant.participant_id, 128) || !text(&participant.participant_external_id, 256) {
        return Err(ValidationError::Identity);
    }
    Ok(())
}

pub fn validate_track(track: &Track) -> Result {
    if !text(&track.track_id, 128) || !text(&track.participant_id, 128) || !text(&track.label, 256)
    {
        return Err(ValidationError::Identity);
    }
    match track.kind.as_ref().ok_or(ValidationError::Format)? {
        track::Kind::Audio(audio) => validate_discovery_format(audio.format.as_ref(), true),
        track::Kind::Video(video) => validate_discovery_format(video.format.as_ref(), false),
        track::Kind::Data(data) if known::<DataDelivery>(data.delivery) => Ok(()),
        _ => Err(ValidationError::Format),
    }
}

fn validate_discovery_format(format: Option<&CodecFormat>, audio: bool) -> Result {
    let format = format.ok_or(ValidationError::Format)?;
    if validate_format(format)? != audio {
        return Err(ValidationError::Format);
    }
    Ok(())
}

pub fn validate_live(live: &LiveResource) -> Result {
    let spec = live.spec.as_ref().ok_or(ValidationError::Confirmed)?;
    validate_spec(spec)?;
    let participant = live.participant.as_ref().ok_or(ValidationError::Identity)?;
    let track = live.track.as_ref().ok_or(ValidationError::Identity)?;
    validate_participant(participant)?;
    validate_track(track)?;
    if participant.participant_id != track.participant_id {
        return Err(ValidationError::Identity);
    }
    let media_publication = match spec
        .declaration
        .as_ref()
        .ok_or(ValidationError::Confirmed)?
    {
        resource_spec::Declaration::Publication(publication) => {
            if publication.label != track.label {
                return Err(ValidationError::Identity);
            }
            match (&publication.kind, &track.kind) {
                (Some(publication_spec::Kind::Audio(audio)), Some(track::Kind::Audio(source))) => {
                    validate_source_format(audio.binding.as_ref(), source.format.as_ref())?;
                    true
                }
                (Some(publication_spec::Kind::Video(video)), Some(track::Kind::Video(source))) => {
                    validate_source_format(video.binding.as_ref(), source.format.as_ref())?;
                    true
                }
                (Some(publication_spec::Kind::Data(data)), Some(track::Kind::Data(source)))
                    if data.delivery == source.delivery =>
                {
                    false
                }
                _ => return Err(ValidationError::Identity),
            }
        }
        resource_spec::Declaration::Subscription(subscription) => {
            let kind = match &track.kind {
                Some(track::Kind::Audio(_)) => TrackKind::Audio,
                Some(track::Kind::Video(_)) => TrackKind::Video,
                Some(track::Kind::Data(_)) => TrackKind::Data,
                None => return Err(ValidationError::Identity),
            };
            if subscription.track_id != track.track_id || subscription.kind != kind as i32 {
                return Err(ValidationError::Identity);
            }
            match (
                kind,
                subscription.rtp_offer.as_ref(),
                live.binding
                    .as_ref()
                    .and_then(|binding| binding.transport.as_ref()),
            ) {
                (TrackKind::Data, None, None) => {}
                (
                    TrackKind::Audio | TrackKind::Video,
                    Some(offer),
                    Some(track_binding::Transport::Rtp(plan)),
                ) => {
                    validate_receive_plan(offer, plan, kind)?;
                    let source = match track.kind.as_ref() {
                        Some(track::Kind::Audio(source)) => source.format.as_ref(),
                        Some(track::Kind::Video(source)) => source.format.as_ref(),
                        _ => None,
                    };
                    validate_source_format(live.binding.as_ref(), source)?;
                }
                _ => return Err(ValidationError::Binding),
            }
            false
        }
    };
    if matches!(
        spec.declaration,
        Some(resource_spec::Declaration::Publication(_))
    ) && live.binding.is_some()
    {
        return Err(ValidationError::Binding);
    }
    if media_publication != live.activity.is_some()
        || live.activity.is_some_and(|v| !known::<MediaActivity>(v))
    {
        return Err(ValidationError::Confirmed);
    }
    let enabled = spec
        .settings
        .as_ref()
        .and_then(|s| s.enabled)
        .ok_or(ValidationError::Settings)?;
    let execution =
        ExecutionStatus::try_from(live.execution).map_err(|_| ValidationError::Confirmed)?;
    match execution {
        ExecutionStatus::Disabled if !enabled && live.execution_failure.is_none() => Ok(()),
        ExecutionStatus::Installed if enabled && live.execution_failure.is_none() => Ok(()),
        ExecutionStatus::WaitingForSource
            if enabled
                && live.execution_failure.is_none()
                && matches!(
                    spec.declaration,
                    Some(resource_spec::Declaration::Subscription(_))
                ) =>
        {
            Ok(())
        }
        ExecutionStatus::Preparing
            if live.execution_failure.is_none() && live.binding.is_some() =>
        {
            Ok(())
        }
        ExecutionStatus::Failed => validate_failure(
            live.execution_failure
                .as_ref()
                .ok_or(ValidationError::Failure)?,
        ),
        _ => Err(ValidationError::Confirmed),
    }
}

fn validate_source_format(binding: Option<&TrackBinding>, format: Option<&CodecFormat>) -> Result {
    let Some(track_binding::Transport::Rtp(rtp)) = binding.and_then(|b| b.transport.as_ref())
    else {
        return Err(ValidationError::Binding);
    };
    match (rtp.codec.as_ref().and_then(|c| c.format.as_ref()), format) {
        (Some(binding), Some(source)) if formats_equivalent(binding, source) => (),
        _ => return Err(ValidationError::Format),
    }
    Ok(())
}

fn validate_video(options: &VideoReceiveOptions) -> Result {
    if !range(options.max_height, 1, 16384)
        || !range(options.min_height, 1, 16384)
        || !range(options.min_fps, 1, 240)
        || !range(options.priority, 1, 255)
        || matches!((options.min_height, options.max_height), (Some(min), Some(max)) if min > max)
    {
        return Err(ValidationError::Settings);
    }
    validate_playout(options.playout_delay.as_ref())
}

fn validate_playout(delay: Option<&PlayoutDelay>) -> Result {
    if delay.is_some_and(|d| d.min_ms > d.max_ms || d.max_ms > 4095) {
        return Err(ValidationError::Settings);
    }
    Ok(())
}

pub fn validate_execution_report(report: &ExecutionReport) -> Result {
    match report.outcome.as_ref().ok_or(ValidationError::Binding)? {
        execution_report::Outcome::Installed(binding) => {
            let Some(track_binding::Transport::Rtp(parameters)) = binding.transport.as_ref() else {
                return Err(ValidationError::Binding);
            };
            let format = parameters
                .codec
                .as_ref()
                .and_then(|codec| codec.format.as_ref())
                .ok_or(ValidationError::Format)?;
            let kind = if validate_format(format)? {
                TrackKind::Audio
            } else {
                TrackKind::Video
            };
            validate_rtp(parameters, kind, false)
        }
        execution_report::Outcome::Failure(failure) => validate_failure(failure),
    }
}

pub fn validate_receive_offer(offer: &RtpReceiveOffer, kind: TrackKind) -> Result {
    if !matches!(kind, TrackKind::Audio | TrackKind::Video)
        || !mid(&offer.mid)
        || offer.codecs.is_empty()
        || offer.codecs.len() > 32
        || offer.header_extensions.len() > 16
        || !unique_text(&offer.rtcp_modes, 16, 64)
    {
        return Err(ValidationError::Binding);
    }
    let mut payloads = BTreeSet::new();
    for codec in &offer.codecs {
        if !payload_type(codec.payload_type)
            || !payloads.insert(codec.payload_type)
            || !unique_text(&codec.feedback, 32, 128)
            || codec
                .rtx_payload_type
                .is_some_and(|repair| !payload_type(Some(repair)) || !payloads.insert(Some(repair)))
            || validate_format(codec.format.as_ref().ok_or(ValidationError::Format)?)?
                != (kind == TrackKind::Audio)
        {
            return Err(ValidationError::Binding);
        }
    }
    let mut ids = BTreeSet::new();
    let mut uris = BTreeSet::new();
    if offer.header_extensions.iter().any(|extension| {
        !(1..=255).contains(&extension.id)
            || !ids.insert(extension.id)
            || !uris.insert(&extension.uri)
            || super::sdp::extension_uri(&extension.uri).is_err()
    }) {
        return Err(ValidationError::Binding);
    }
    Ok(())
}

pub fn validate_receive_plan(
    offer: &RtpReceiveOffer,
    plan: &RtpParameters,
    kind: TrackKind,
) -> Result {
    validate_receive_offer(offer, kind)?;
    validate_rtp(plan, kind, false)?;
    let selected = plan.codec.as_ref().ok_or(ValidationError::Binding)?;
    let offered = offer.codecs.iter().find(|codec|
        codec.payload_type == selected.payload_type
            && matches!((&codec.format, &selected.format), (Some(left), Some(right)) if formats_equivalent(left, right)))
        .ok_or(ValidationError::Capability)?;
    let rtcp = plan.rtcp.as_ref().ok_or(ValidationError::Binding)?;
    let mode = if rtcp.reduced_size {
        RtcpMode::ReducedSize
    } else {
        RtcpMode::Compound
    };
    if plan.mid != offer.mid
        || selected.rtx_payload_type.is_some()
            && selected.rtx_payload_type != offered.rtx_payload_type
        || selected
            .feedback
            .iter()
            .any(|value| !offered.feedback.contains(value))
        || plan
            .header_extensions
            .iter()
            .any(|extension| !offer.header_extensions.contains(extension))
        || !offer.rtcp_modes.iter().any(|value| value == mode.as_str())
        || plan
            .encodings
            .iter()
            .any(|encoding| !encoding.rid.is_empty())
    {
        return Err(ValidationError::Capability);
    }
    Ok(())
}

fn receive_offers_equivalent(left: &RtpReceiveOffer, right: &RtpReceiveOffer) -> bool {
    left.mid == right.mid
        && same_members(&left.header_extensions, &right.header_extensions)
        && same_members(&left.rtcp_modes, &right.rtcp_modes)
        && left.codecs.len() == right.codecs.len()
        && left.codecs.iter().all(|left| {
            right
                .codecs
                .iter()
                .any(|right| codecs_equivalent(left, right))
        })
}

fn codecs_equivalent(left: &RtpCodecParameters, right: &RtpCodecParameters) -> bool {
    left.payload_type == right.payload_type
        && left.rtx_payload_type == right.rtx_payload_type
        && same_members(&left.feedback, &right.feedback)
        && matches!((&left.format, &right.format), (Some(left), Some(right)) if formats_equivalent(left, right))
}

fn validate_binding(binding: Option<&TrackBinding>, kind: TrackKind, publication: bool) -> Result {
    match binding.and_then(|b| b.transport.as_ref()) {
        Some(track_binding::Transport::Sctp(sctp))
            if kind == TrackKind::Data && matches!(sctp.stream_id, Some(1..=65534)) =>
        {
            Ok(())
        }
        Some(track_binding::Transport::Rtp(rtp))
            if matches!(kind, TrackKind::Audio | TrackKind::Video) =>
        {
            validate_rtp(rtp, kind, publication)
        }
        _ => Err(ValidationError::Binding),
    }
}

pub fn validate_rtp(rtp: &RtpParameters, kind: TrackKind, publication: bool) -> Result {
    validate_rtp_envelope(rtp, kind, publication)?;
    let repair = rtp
        .codec
        .as_ref()
        .is_some_and(|codec| codec.rtx_payload_type.is_some());
    if rtp
        .encodings
        .iter()
        .any(|encoding| encoding.ssrc.is_none() || encoding.rtx_ssrc.is_some() != repair)
        || rtp
            .rtcp
            .as_ref()
            .is_none_or(|rtcp| !transport_text(&rtcp.cname, 256))
    {
        return Err(ValidationError::Binding);
    }
    Ok(())
}

/// Checks known RTP fields without granting admission for missing SSRCs or CNAME.
/// Use `validate_rtp` before admitting a wire binding.
pub fn validate_rtp_envelope(rtp: &RtpParameters, kind: TrackKind, publication: bool) -> Result {
    if !mid(&rtp.mid)
        || rtp.encodings.is_empty()
        || rtp.encodings.len() > if publication { 4 } else { 1 }
        || rtp.header_extensions.len() > 16
    {
        return Err(ValidationError::Binding);
    }
    let codec = rtp.codec.as_ref().ok_or(ValidationError::Binding)?;
    if !payload_type(codec.payload_type)
        || codec.rtx_payload_type.is_some()
            && (!payload_type(codec.rtx_payload_type)
                || codec.rtx_payload_type == codec.payload_type)
        || !unique_text(&codec.feedback, 32, 128)
        || !codec
            .feedback
            .iter()
            .any(|feedback| feedback == RtcpFeedback::TransportCc.as_str())
    {
        return Err(ValidationError::Binding);
    }
    let audio = validate_format(codec.format.as_ref().ok_or(ValidationError::Format)?)?;
    if audio != (kind == TrackKind::Audio)
        || !matches!(kind, TrackKind::Audio | TrackKind::Video)
        || audio && video_feedback(&codec.feedback)
    {
        return Err(ValidationError::Format);
    }
    let mut ids = BTreeSet::new();
    let mut types = BTreeSet::new();
    for extension in &rtp.header_extensions {
        if !(1..=255).contains(&extension.id)
            || super::sdp::extension_uri(&extension.uri).is_err()
            || !ids.insert(extension.id)
            || !types.insert(extension.uri.as_str())
        {
            return Err(ValidationError::Binding);
        }
    }
    // MID and draft-01 transport-wide CC are mandatory in the native profile.
    if !types.contains(RtpHeaderExtensionType::RtpHeaderExtensionMid.as_str())
        || !types.contains(RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01.as_str())
    {
        return Err(ValidationError::Binding);
    }
    let mut ssrcs = BTreeSet::new();
    let mut rids = BTreeSet::new();
    for encoding in &rtp.encodings {
        if encoding.ssrc.is_some_and(|ssrc| !ssrcs.insert(ssrc))
            || encoding.rtx_ssrc.is_some() && codec.rtx_payload_type.is_none()
            || encoding.rtx_ssrc.is_some_and(|ssrc| !ssrcs.insert(ssrc))
            || encoding
                .scalability_mode
                .as_ref()
                .is_some_and(|v| audio || !text(v, 64))
            || !encoding.rid.is_empty()
                && (!rid(&encoding.rid)
                    || !rids.insert(&encoding.rid)
                    || !types.contains(RtpHeaderExtensionType::RtpHeaderExtensionRid.as_str())
                    || codec.rtx_payload_type.is_some()
                        && !types.contains(
                            RtpHeaderExtensionType::RtpHeaderExtensionRepairedRid.as_str(),
                        ))
        {
            return Err(ValidationError::Binding);
        }
    }
    let rtcp = rtp.rtcp.as_ref().ok_or(ValidationError::Binding)?;
    if !rtcp.cname.is_empty() && !transport_text(&rtcp.cname, 256) {
        return Err(ValidationError::Binding);
    }
    Ok(())
}

/// Compare immutable declarations of validated specs, independently of settings.
/// This is identity equality, never source/receiver codec compatibility.
pub fn declarations_equivalent(left: &ResourceSpec, right: &ResourceSpec) -> bool {
    use publication_spec::Kind;
    use resource_spec::Declaration;
    match (&left.declaration, &right.declaration) {
        (Some(Declaration::Publication(left)), Some(Declaration::Publication(right))) => {
            left.label == right.label
                && match (&left.kind, &right.kind) {
                    (Some(Kind::Audio(left)), Some(Kind::Audio(right))) => {
                        bindings_equivalent(left.binding.as_ref(), right.binding.as_ref())
                    }
                    (Some(Kind::Video(left)), Some(Kind::Video(right))) => {
                        bindings_equivalent(left.binding.as_ref(), right.binding.as_ref())
                    }
                    (Some(Kind::Data(left)), Some(Kind::Data(right))) => {
                        left.delivery == right.delivery
                            && bindings_equivalent(left.binding.as_ref(), right.binding.as_ref())
                    }
                    _ => false,
                }
        }
        (Some(Declaration::Subscription(left)), Some(Declaration::Subscription(right))) => {
            left.track_id == right.track_id
                && left.kind == right.kind
                && match (&left.rtp_offer, &right.rtp_offer) {
                    (Some(left), Some(right)) => receive_offers_equivalent(left, right),
                    (None, None) => {
                        bindings_equivalent(left.binding.as_ref(), right.binding.as_ref())
                    }
                    _ => false,
                }
        }
        _ => false,
    }
}

/// Full mutable equality for validated settings, including documented defaults.
/// Omitted playout bounds are not an explicit zero-delay request.
pub fn settings_equivalent(left: &ResourceSettings, right: &ResourceSettings) -> bool {
    use resource_settings::Receive;
    left.enabled == right.enabled
        && match (&left.receive, &right.receive) {
            (Some(Receive::Video(left)), Some(Receive::Video(right))) => {
                left.max_height == right.max_height
                    && left.min_height == right.min_height
                    && left.min_fps == right.min_fps
                    && left.playout_delay == right.playout_delay
                    && left.priority.unwrap_or(1) == right.priority.unwrap_or(1)
            }
            _ => left.receive == right.receive,
        }
}

/// Identity equality of validated bindings, independently of mutable settings.
pub fn bindings_equivalent(left: Option<&TrackBinding>, right: Option<&TrackBinding>) -> bool {
    use track_binding::Transport;
    match (
        left.and_then(|v| v.transport.as_ref()),
        right.and_then(|v| v.transport.as_ref()),
    ) {
        (Some(Transport::Sctp(left)), Some(Transport::Sctp(right))) => left == right,
        (Some(Transport::Rtp(left)), Some(Transport::Rtp(right))) => {
            left.mid == right.mid
                && left.rtcp == right.rtcp
                && left.encodings == right.encodings
                && same_members(&left.header_extensions, &right.header_extensions)
                && match (&left.codec, &right.codec) {
                    (Some(left), Some(right)) => codecs_equivalent(left, right),
                    _ => false,
                }
        }
        _ => false,
    }
}

fn same_members<T: PartialEq>(left: &[T], right: &[T]) -> bool {
    left.len() == right.len() && left.iter().all(|value| right.contains(value))
}

/// Default-normalized transport equality, not directional codec compatibility.
pub fn formats_equivalent(left: &CodecFormat, right: &CodecFormat) -> bool {
    left.mime_type == right.mime_type
        && left.clock_rate == right.clock_rate
        && left.channels == right.channels
        && super::format::normalized_parameters(left) == super::format::normalized_parameters(right)
}

/// Validates a description without granting codec support. True means audio.
pub fn validate_format(format: &CodecFormat) -> Result<bool> {
    let (media, name) = format
        .mime_type
        .split_once('/')
        .ok_or(ValidationError::Format)?;
    let audio = match media {
        "audio" => true,
        "video" => false,
        _ => return Err(ValidationError::Format),
    };
    if !parameter_name(name)
        || format.clock_rate == 0
        || format
            .channels
            .is_some_and(|channels| !audio || channels == 0)
        || format.parameters.len() > 32
    {
        return Err(ValidationError::Format);
    }
    let mut names = BTreeSet::new();
    for parameter in &format.parameters {
        if !parameter_name(&parameter.name)
            || !names.insert(&parameter.name)
            || parameter.value.len() > 1024
            || parameter.value.chars().any(|c| c.is_control() || c == ';')
        {
            return Err(ValidationError::Format);
        }
    }
    match format.parse_known_codec()? {
        Some(codec_format::Codec::Opus(opus))
            if !range(opus.max_average_bitrate_bps, 6000, 510000)
                || !range(opus.max_playback_rate_hz, 8000, 48000)
                || !range(opus.max_capture_rate_hz, 8000, 48000)
                || !range(opus.min_packet_time_ms, 3, 120) =>
        {
            Err(ValidationError::Format)
        }
        _ => Ok(audio),
    }
}

fn parameter_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_+.".contains(&byte)
        })
}

pub fn validate_capabilities(caps: &RtpCapabilities) -> Result {
    if caps.codecs.len() > 32
        || !unique_text(&caps.header_extensions, 64, 256)
        || caps
            .header_extensions
            .iter()
            .any(|uri| super::sdp::extension_uri(uri).is_err())
        || !unique_text(&caps.rtcp_modes, 16, 64)
    {
        return Err(ValidationError::Capability);
    }
    for (index, codec) in caps.codecs.iter().enumerate() {
        let format = codec.format.as_ref().ok_or(ValidationError::Capability)?;
        let audio = validate_format(format)?;
        if !unique_text(&codec.feedback, 32, 128)
            || !unique_text(&codec.scalability_modes, 32, 64)
            || audio && !codec.scalability_modes.is_empty()
            || audio && video_feedback(&codec.feedback)
            || caps
                .codecs
                .iter()
                .take(index)
                .filter_map(|prior| prior.format.as_ref())
                .any(|prior| formats_equivalent(prior, format))
        {
            return Err(ValidationError::Capability);
        }
    }
    Ok(())
}

fn unique_text(values: &[alloc::string::String], max: usize, bytes: usize) -> bool {
    values.len() <= max
        && values.iter().enumerate().all(|(index, value)| {
            text(value, bytes)
                && !value.chars().any(char::is_control)
                && !values.iter().take(index).any(|prior| prior == value)
        })
}

fn validate_failure(failure: &Failure) -> Result {
    if !known::<FailureCode>(failure.code) || failure.diagnostic.len() > 256 {
        return Err(ValidationError::Failure);
    }
    Ok(())
}

fn video_feedback(feedback: &[alloc::string::String]) -> bool {
    feedback.iter().any(|value| {
        matches!(
            RtcpFeedback::try_from(value.as_str()),
            Ok(RtcpFeedback::NackPli | RtcpFeedback::CcmFir)
        )
    })
}

fn known<E: TryFrom<i32>>(value: i32) -> bool {
    value != 0 && E::try_from(value).is_ok()
}
fn range(value: Option<u32>, min: u32, max: u32) -> bool {
    value.is_none_or(|v| (min..=max).contains(&v))
}
fn payload_type(value: Option<u32>) -> bool {
    value.is_some_and(|v| v <= 127 && !(64..=95).contains(&v))
}
fn text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max
}
fn transport_text(value: &str, max: usize) -> bool {
    text(value, max) && value.chars().all(|c| !c.is_whitespace() && !c.is_control())
}

fn mid(value: &str) -> bool {
    text(value, 32)
        && value.bytes().all(|b| {
            matches!(b,
        0x21 | 0x23..=0x27 | 0x2a..=0x2b | 0x2d..=0x2e
        | 0x30..=0x39 | 0x41..=0x5a | 0x5e..=0x7e)
        })
}

fn rid(value: &str) -> bool {
    text(value, 16)
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

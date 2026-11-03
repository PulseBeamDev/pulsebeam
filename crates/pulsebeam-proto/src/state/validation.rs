//! Stateless wire validity. Authority, cross-record revision order, lifetime
//! reservations and adapter capability checks belong to their connection owner.

use alloc::collections::BTreeSet;

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Client,
    Server,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationError {
    Batch,
    Direction,
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

pub fn validate_batch(batch: &StateBatch, direction: Direction) -> Result {
    if batch.records.len() > codec::MAX_RECORDS
        || batch.records.is_empty() && !batch.initial_complete
        || direction == Direction::Client && batch.initial_complete
    {
        return Err(ValidationError::Batch);
    }
    let mut previous = 0;
    for record in &batch.records {
        let key = record
            .key
            .as_ref()
            .and_then(|k| k.key.as_ref())
            .ok_or(ValidationError::Key)?;
        validate_key(key)?;
        match (direction, &record.state) {
            (Direction::Client, Some(state_record::State::Desired(desired))) => {
                let resource_key::Key::Owned(owned) = key else {
                    return Err(ValidationError::Direction);
                };
                if desired.revision <= previous
                    || *owned > desired.revision
                    || !matches!(&desired.value, Some(desired_state::Value::Present(_)))
                        && !matches!(&desired.value, Some(desired_state::Value::Absent(true)) if *owned < desired.revision)
                {
                    return Err(ValidationError::Desired);
                }
                previous = desired.revision;
                // A well-formed envelope with an invalid spec gets a scoped
                // evaluation rejection, not a codec/connection failure.
            }
            (Direction::Server, Some(state_record::State::Confirmed(confirmed))) => {
                validate_confirmed(key, confirmed)?;
            }
            _ => return Err(ValidationError::Direction),
        }
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
            validate_binding(subscription.binding.as_ref(), kind, false)
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
    // A future codec is visible but unreceivable. Missing format is invalid.
    if format.codec.is_none() {
        return Ok(());
    }
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
            false
        }
    };
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
        || !unique_known::<RtcpFeedback>(&codec.feedback, 4)
        || !codec.feedback.contains(&(RtcpFeedback::TransportCc as i32))
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
            || !known::<RtpHeaderExtensionType>(extension.r#type)
            || !ids.insert(extension.id)
            || !types.insert(extension.r#type)
        {
            return Err(ValidationError::Binding);
        }
    }
    // MID and draft-01 transport-wide CC are mandatory in the native profile.
    if !types.contains(&(RtpHeaderExtensionType::RtpHeaderExtensionMid as i32))
        || !types.contains(&(RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01 as i32))
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
                .is_some_and(|v| audio || !known::<RtpScalabilityMode>(v))
            || !encoding.rid.is_empty()
                && (!rid(&encoding.rid)
                    || !rids.insert(&encoding.rid)
                    || !types.contains(&(RtpHeaderExtensionType::RtpHeaderExtensionRid as i32))
                    || codec.rtx_payload_type.is_some()
                        && !types.contains(
                            &(RtpHeaderExtensionType::RtpHeaderExtensionRepairedRid as i32),
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
                && bindings_equivalent(left.binding.as_ref(), right.binding.as_ref())
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
                    (Some(left), Some(right)) => {
                        left.payload_type == right.payload_type
                            && left.rtx_payload_type == right.rtx_payload_type
                            && same_members(&left.feedback, &right.feedback)
                            && match (&left.format, &right.format) {
                                (Some(left), Some(right)) => formats_equivalent(left, right),
                                _ => false,
                            }
                    }
                    _ => false,
                }
        }
        _ => false,
    }
}

fn same_members<T: PartialEq>(left: &[T], right: &[T]) -> bool {
    left.len() == right.len() && left.iter().all(|value| right.contains(value))
}

/// Typed/default identity equality, not directional codec compatibility.
pub fn formats_equivalent(left: &CodecFormat, right: &CodecFormat) -> bool {
    match (&left.codec, &right.codec) {
        (Some(codec_format::Codec::Opus(left)), Some(codec_format::Codec::Opus(right))) => {
            left.stereo == right.stereo
                && left.source_stereo == right.source_stereo
                && left.in_band_fec == right.in_band_fec
                && left.dtx == right.dtx
                && left.constant_bitrate == right.constant_bitrate
                && left.max_average_bitrate_bps == right.max_average_bitrate_bps
                // RFC 7587 section 6.1 supplies these two stable rate defaults.
                && left.max_playback_rate_hz.unwrap_or(48000) == right.max_playback_rate_hz.unwrap_or(48000)
                && left.max_capture_rate_hz.unwrap_or(48000) == right.max_capture_rate_hz.unwrap_or(48000)
                && left.min_packet_time_ms == right.min_packet_time_ms
        }
        (Some(codec_format::Codec::H264(left)), Some(codec_format::Codec::H264(right))) => {
            left.profile_level_id == right.profile_level_id
                && left.level_asymmetry_allowed.unwrap_or(false)
                    == right.level_asymmetry_allowed.unwrap_or(false)
        }
        (Some(codec_format::Codec::Vp9(left)), Some(codec_format::Codec::Vp9(right))) => {
            left.profile_id.unwrap_or(0) == right.profile_id.unwrap_or(0)
        }
        _ => left == right,
    }
}

/// Returns true for audio, false for video. Compatibility between two valid
/// formats is directional and must not be replaced by protobuf equality.
pub fn validate_format(format: &CodecFormat) -> Result<bool> {
    match format.codec.as_ref().ok_or(ValidationError::Format)? {
        codec_format::Codec::Opus(opus) => {
            if !range(opus.max_average_bitrate_bps, 6000, 510000)
                || !range(opus.max_playback_rate_hz, 8000, 48000)
                || !range(opus.max_capture_rate_hz, 8000, 48000)
                || !range(opus.min_packet_time_ms, 3, 120)
            {
                return Err(ValidationError::Format);
            }
            Ok(true)
        }
        codec_format::Codec::H264(h264) if matches!(h264.profile_level_id, Some(0..=0xffffff)) => {
            Ok(false)
        }
        codec_format::Codec::Vp8(_) => Ok(false),
        codec_format::Codec::Vp9(vp9) if matches!(vp9.profile_id, None | Some(0 | 2)) => Ok(false),
        _ => Err(ValidationError::Format),
    }
}

pub fn validate_capabilities(caps: &RtpCapabilities) -> Result {
    if caps.codecs.len() > 32
        || !unique_known::<RtpHeaderExtensionType>(&caps.header_extensions, 16)
        || !unique_known::<RtcpMode>(&caps.rtcp_modes, 2)
    {
        return Err(ValidationError::Capability);
    }
    for (index, codec) in caps.codecs.iter().enumerate() {
        let format = codec.format.as_ref().ok_or(ValidationError::Capability)?;
        let audio = validate_format(format)?;
        if !unique_known::<RtcpFeedback>(&codec.feedback, 4)
            || !unique_known::<RtpScalabilityMode>(&codec.scalability_modes, 3)
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

fn validate_failure(failure: &Failure) -> Result {
    if !known::<FailureCode>(failure.code) || failure.diagnostic.len() > 256 {
        return Err(ValidationError::Failure);
    }
    Ok(())
}

fn video_feedback(feedback: &[i32]) -> bool {
    feedback.contains(&(RtcpFeedback::NackPli as i32))
        || feedback.contains(&(RtcpFeedback::CcmFir as i32))
}

fn known<E: TryFrom<i32>>(value: i32) -> bool {
    value != 0 && E::try_from(value).is_ok()
}
fn unique_known<E: TryFrom<i32>>(values: &[i32], max: usize) -> bool {
    let mut seen = BTreeSet::new();
    values.len() <= max && values.iter().all(|v| known::<E>(*v) && seen.insert(*v))
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

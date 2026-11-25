//! Stateless wire validity. Authority, cross-record revision order, lifetime
//! reservations and adapter capability checks belong to their connection owner.

use alloc::{collections::BTreeSet, string::String};
use core::num::{NonZeroU16, NonZeroU32, NonZeroU64};

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
    Catalog,
}

type Result<T = ()> = core::result::Result<T, ValidationError>;

pub const MAX_SELECTOR_DECLARATIONS: usize = 8;
pub const MAX_SELECTOR_MATCHES: usize = 16;

pub fn validate_client_batch(batch: &ClientStateBatch) -> Result {
    validate_client_batch_for(batch, BindingProfile::Rtc)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BindingProfile {
    #[default]
    Rtc,
    Quic,
}

pub(super) fn validate_client_batch_for(
    batch: &ClientStateBatch,
    profile: BindingProfile,
) -> Result {
    if batch
        .records
        .len()
        .saturating_add(batch.catalog_records.len())
        > codec::MAX_RECORDS
        || batch.records.is_empty() && batch.catalog_records.is_empty()
    {
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
                validate_execution_report_for(report, profile)?;
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
    for record in &batch.catalog_records {
        match record.command.as_ref() {
            Some(catalog_client_record::Command::Open(request)) => {
                crate::catalog::validate_request(
                    request.watch_id,
                    &request.room,
                    request.participant_limit,
                    &request.cursor,
                )
                .map_err(|_| ValidationError::Catalog)?;
            }
            Some(catalog_client_record::Command::Close(close)) if close.watch_id != 0 => {}
            _ => return Err(ValidationError::Catalog),
        }
    }
    Ok(())
}

pub fn validate_server_batch(batch: &ServerStateBatch) -> Result {
    validate_server_batch_for(batch, BindingProfile::Rtc)
}

pub(super) fn validate_server_batch_for(
    batch: &ServerStateBatch,
    profile: BindingProfile,
) -> Result {
    if batch
        .records
        .len()
        .saturating_add(batch.catalog_records.len())
        > codec::MAX_RECORDS
        || batch.records.is_empty() && batch.catalog_records.is_empty() && !batch.initial_complete
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
        validate_confirmed(key, confirmed, profile)?;
    }
    for record in &batch.catalog_records {
        match record.response.as_ref() {
            Some(catalog_server_record::Response::Page(page)) => {
                crate::catalog::validate_page_record(page).map_err(|_| ValidationError::Catalog)?;
            }
            Some(catalog_server_record::Response::Error(error))
                if error.watch_id != 0 && error.failure.is_some() =>
            {
                validate_failure(error.failure.as_ref().ok_or(ValidationError::Catalog)?)?;
            }
            _ => return Err(ValidationError::Catalog),
        }
    }
    Ok(())
}

fn validate_key(key: &resource_key::Key) -> Result {
    match key {
        resource_key::Key::Owned(v) if *v != 0 => Ok(()),
        resource_key::Key::Participant(endpoint) => {
            validate_participant(endpoint).map_err(|_| ValidationError::Key)
        }
        resource_key::Key::Publication(id) if text(id, 128) => Ok(()),
        resource_key::Key::Session(true) => Ok(()),
        _ => Err(ValidationError::Key),
    }
}

fn validate_confirmed(
    key: &resource_key::Key,
    confirmed: &ConfirmedState,
    profile: BindingProfile,
) -> Result {
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
            validate_live_for(live, profile)
        }
        (Key::Owned(reference), Some(Value::SourceLookup(lookup))) => {
            if confirmed.evaluated_revision < *reference {
                return Err(ValidationError::Confirmed);
            }
            validate_source_lookup(lookup)
        }
        (Key::Owned(reference), Some(Value::LogicalSubscription(subscription))) => {
            if confirmed.evaluated_revision < *reference {
                return Err(ValidationError::Confirmed);
            }
            validate_logical_subscription_for(subscription, profile)
        }
        (Key::Owned(reference), Some(Value::SubscriptionSelector(selector))) => {
            if confirmed.evaluated_revision < *reference {
                return Err(ValidationError::Confirmed);
            }
            validate_selector_state(selector)
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
        (Key::Participant(endpoint), Some(Value::Participant(participant)))
            if endpoint == participant =>
        {
            validate_participant(participant)
        }
        (Key::Publication(id), Some(Value::Track(track))) if id == &track.publication_id => {
            validate_track(track)
        }
        (Key::Participant(_) | Key::Publication(_), Some(Value::Absent(absent)))
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
                || session.successor_receipt.as_ref().is_some_and(|receipt| {
                    session.closed.is_some()
                        || receipt.expires_at_unix_seconds == 0
                        || receipt.proof.is_empty()
                        || receipt.proof.len() > MAX_SUCCESSOR_RECEIPT_BYTES
                })
            {
                return Err(ValidationError::Confirmed);
            }
            Ok(())
        }
        _ => Err(ValidationError::Confirmed),
    }
}

pub fn validate_spec(spec: &ResourceSpec) -> Result {
    validate_spec_for(spec, BindingProfile::Rtc)
}

pub fn validate_spec_for(spec: &ResourceSpec, profile: BindingProfile) -> Result {
    let settings = spec.settings.as_ref().ok_or(ValidationError::Settings)?;
    if settings.enabled.is_none() {
        return Err(ValidationError::Settings);
    }
    match spec.declaration.as_ref().ok_or(ValidationError::Binding)? {
        resource_spec::Declaration::Publication(publication) => {
            if !is_application_name(&publication.track_id) || settings.receive.is_some() {
                return Err(ValidationError::Settings);
            }
            match publication.kind.as_ref().ok_or(ValidationError::Binding)? {
                publication_spec::Kind::Audio(audio) => {
                    validate_binding(audio.binding.as_ref(), TrackKind::Audio, true, profile)
                }
                publication_spec::Kind::Video(video) => {
                    validate_binding(video.binding.as_ref(), TrackKind::Video, true, profile)
                }
                publication_spec::Kind::Data(data) => {
                    if let Some(clock) = &data.capture_clock
                        && (!valid_data_capture_clock(clock)
                            || !matches!(
                                data.binding
                                    .as_ref()
                                    .and_then(|binding| binding.transport.as_ref()),
                                Some(
                                    track_binding::Transport::SctpDataV1(_)
                                        | track_binding::Transport::QuicDataV1(_)
                                )
                            ))
                    {
                        return Err(ValidationError::Binding);
                    }
                    validate_data_binding(data.binding.as_ref(), data.delivery, profile)
                }
            }
        }
        resource_spec::Declaration::SourceLookup(lookup) => {
            if settings.receive.is_some() {
                return Err(ValidationError::Settings);
            }
            let source = lookup.source.as_ref().ok_or(ValidationError::Identity)?;
            crate::catalog::validate_address(source).map_err(|_| ValidationError::Identity)
        }
        resource_spec::Declaration::LogicalSubscription(subscription) => {
            let source = subscription
                .source
                .as_ref()
                .ok_or(ValidationError::Identity)?;
            crate::catalog::validate_address(source).map_err(|_| ValidationError::Identity)?;
            let kind = TrackKind::try_from(source.kind).map_err(|_| ValidationError::Binding)?;
            if let Some(version) = subscription.data_version
                && (kind != TrackKind::Data || version != u32::from(crate::data_endpoint::DATA_V1))
            {
                return Err(ValidationError::Binding);
            }
            validate_receive_settings(settings, kind)
        }
        resource_spec::Declaration::SubscriptionSelector(selector) => {
            if settings.receive.is_some() {
                return Err(ValidationError::Settings);
            }
            validate_selector(selector)
        }
        resource_spec::Declaration::Subscription(subscription) => {
            if !text(&subscription.publication_id, 128)
                || !is_application_name(&subscription.source_room)
            {
                return Err(ValidationError::Identity);
            }
            let kind =
                TrackKind::try_from(subscription.kind).map_err(|_| ValidationError::Binding)?;
            validate_receive_settings(settings, kind)?;
            validate_binding(subscription.binding.as_ref(), kind, false, profile)
        }
    }
}

fn validate_receive_settings(settings: &ResourceSettings, kind: TrackKind) -> Result {
    match (kind, &settings.receive) {
        (TrackKind::Video, Some(resource_settings::Receive::Video(options))) => {
            validate_video(options)
        }
        (TrackKind::Audio, Some(resource_settings::Receive::Audio(_)))
        | (TrackKind::Data, None) => Ok(()),
        _ => Err(ValidationError::Settings),
    }
}

pub fn is_application_name(value: &str) -> bool {
    (1..=36).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub fn validate_participant(participant: &Participant) -> Result {
    if !is_application_name(&participant.room_id)
        || !is_application_name(&participant.participant_id)
    {
        return Err(ValidationError::Identity);
    }
    Ok(())
}

pub fn validate_track(track: &Track) -> Result {
    if !text(&track.publication_id, 128)
        || !is_application_name(&track.room_id)
        || !is_application_name(&track.participant_id)
        || !is_application_name(&track.track_id)
    {
        return Err(ValidationError::Identity);
    }
    match track.kind.as_ref().ok_or(ValidationError::Format)? {
        track::Kind::Audio(audio) => validate_discovery_format(audio.format.as_ref(), true),
        track::Kind::Video(video) => {
            let format = video.format.as_ref().ok_or(ValidationError::Format)?;
            validate_discovery_format(Some(format), false)?;
            if video.av1_whole_source {
                if format.mime_type != "video/av1" {
                    return Err(ValidationError::Format);
                }
                super::VideoMode::for_format(format, "L1T1")?;
            }
            Ok(())
        }
        track::Kind::Data(data)
            if known::<DataDelivery>(data.delivery)
                && data
                    .capture_clock
                    .as_ref()
                    .is_none_or(valid_data_capture_clock) =>
        {
            Ok(())
        }
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
    validate_live_for(live, BindingProfile::Rtc)
}

fn validate_live_for(live: &LiveResource, profile: BindingProfile) -> Result {
    let spec = live.spec.as_ref().ok_or(ValidationError::Confirmed)?;
    validate_spec_for(spec, profile)?;
    if matches!(
        spec.declaration,
        Some(
            resource_spec::Declaration::SourceLookup(_)
                | resource_spec::Declaration::LogicalSubscription(_)
                | resource_spec::Declaration::SubscriptionSelector(_)
        )
    ) {
        return Err(ValidationError::Confirmed);
    }
    let participant = live.participant.as_ref().ok_or(ValidationError::Identity)?;
    let track = live.track.as_ref().ok_or(ValidationError::Identity)?;
    validate_participant(participant)?;
    validate_track(track)?;
    if participant.participant_id != track.participant_id || participant.room_id != track.room_id {
        return Err(ValidationError::Identity);
    }
    let media_publication = match spec
        .declaration
        .as_ref()
        .ok_or(ValidationError::Confirmed)?
    {
        resource_spec::Declaration::Publication(publication) => {
            if publication.track_id != track.track_id {
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
                    if data.delivery == source.delivery
                        && data.capture_clock == source.capture_clock =>
                {
                    false
                }
                _ => return Err(ValidationError::Identity),
            }
        }
        resource_spec::Declaration::SourceLookup(_)
        | resource_spec::Declaration::LogicalSubscription(_)
        | resource_spec::Declaration::SubscriptionSelector(_) => {
            return Err(ValidationError::Confirmed);
        }
        resource_spec::Declaration::Subscription(subscription) => {
            let kind = match &track.kind {
                Some(track::Kind::Audio(_)) => TrackKind::Audio,
                Some(track::Kind::Video(_)) => TrackKind::Video,
                Some(track::Kind::Data(_)) => TrackKind::Data,
                None => return Err(ValidationError::Identity),
            };
            if subscription.publication_id != track.publication_id
                || subscription.source_room != track.room_id
                || subscription.kind != kind as i32
            {
                return Err(ValidationError::Identity);
            }
            validate_receiver_format(subscription.binding.as_ref(), track)?;
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
    let data_v1_publication = matches!(
        &spec.declaration,
        Some(resource_spec::Declaration::Publication(publication))
            if matches!(
                &publication.kind,
                Some(publication_spec::Kind::Data(data))
                    if matches!(data.binding.as_ref().and_then(|binding| binding.transport.as_ref()),
                        Some(track_binding::Transport::SctpDataV1(_) | track_binding::Transport::QuicDataV1(_)))
            )
    );
    if live.data_v1_source == Some(0)
        || (live.data_v1_source.is_some() && !data_v1_publication)
        || (execution == ExecutionStatus::Installed
            && data_v1_publication
            && live.data_v1_source.is_none())
    {
        return Err(ValidationError::Binding);
    }
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

fn validate_selector(selector: &SubscriptionSelectorSpec) -> Result {
    if selector
        .project_id
        .as_deref()
        .is_some_and(|project| !project.starts_with("p_") || !is_application_name(project))
        || [&selector.room, &selector.participant_id, &selector.track_id]
            .into_iter()
            .any(|component| {
                component
                    .as_deref()
                    .is_some_and(|value| !is_application_name(value))
            })
        || selector.kind.is_some_and(|kind| !known::<TrackKind>(kind))
    {
        return Err(ValidationError::Identity);
    }
    Ok(())
}

fn validate_selector_state(state: &SubscriptionSelectorState) -> Result {
    let spec = state.spec.as_ref().ok_or(ValidationError::Confirmed)?;
    validate_spec(spec)?;
    let Some(resource_spec::Declaration::SubscriptionSelector(selector)) =
        spec.declaration.as_ref()
    else {
        return Err(ValidationError::Confirmed);
    };
    let enabled = spec.settings.as_ref().and_then(|settings| settings.enabled) == Some(true);
    match SelectorMatchStatus::try_from(state.status) {
        Ok(SelectorMatchStatus::Complete) if enabled => {}
        Ok(SelectorMatchStatus::Pending | SelectorMatchStatus::Overflow)
            if enabled && state.matches.is_empty() =>
        {
            return Ok(());
        }
        Ok(SelectorMatchStatus::Complete) if state.matches.is_empty() => return Ok(()),
        _ => return Err(ValidationError::Confirmed),
    }
    if state.matches.len() > MAX_SELECTOR_MATCHES {
        return Err(ValidationError::Confirmed);
    }
    let mut previous = None;
    for source in &state.matches {
        crate::catalog::validate_address(source).map_err(|_| ValidationError::Identity)?;
        let key = (
            source.room.as_str(),
            source.participant_id.as_str(),
            source.kind,
            source.track_id.as_str(),
        );
        if previous.is_some_and(|previous| previous >= key)
            || selector.room.as_deref().is_some_and(|room| room != key.0)
            || selector
                .participant_id
                .as_deref()
                .is_some_and(|participant| participant != key.1)
            || selector.kind.is_some_and(|kind| kind != key.2)
            || selector
                .track_id
                .as_deref()
                .is_some_and(|track| track != key.3)
        {
            return Err(ValidationError::Identity);
        }
        previous = Some(key);
    }
    Ok(())
}

pub fn validate_source_lookup(lookup: &SourceLookupState) -> Result {
    let spec = lookup.spec.as_ref().ok_or(ValidationError::Confirmed)?;
    validate_spec(spec)?;
    let source = match spec.declaration.as_ref() {
        Some(resource_spec::Declaration::SourceLookup(lookup)) => {
            lookup.source.as_ref().ok_or(ValidationError::Identity)?
        }
        _ => return Err(ValidationError::Confirmed),
    };
    match (&lookup.participant, &lookup.track) {
        (None, None) => Ok(()),
        (Some(participant), Some(track)) => {
            if spec.settings.as_ref().and_then(|settings| settings.enabled) != Some(true) {
                return Err(ValidationError::Confirmed);
            }
            validate_participant(participant)?;
            validate_track(track)?;
            if participant.participant_id != track.participant_id
                || participant.room_id != track.room_id
                || !matches_source_address(track, source)
            {
                return Err(ValidationError::Identity);
            }
            Ok(())
        }
        _ => Err(ValidationError::Confirmed),
    }
}

pub fn validate_logical_subscription(subscription: &LogicalSubscriptionState) -> Result {
    validate_logical_subscription_for(subscription, BindingProfile::Rtc)
}

fn validate_logical_subscription_for(
    subscription: &LogicalSubscriptionState,
    profile: BindingProfile,
) -> Result {
    let spec = subscription
        .spec
        .as_ref()
        .ok_or(ValidationError::Confirmed)?;
    validate_spec(spec)?;
    let Some(resource_spec::Declaration::LogicalSubscription(declaration)) = &spec.declaration
    else {
        return Err(ValidationError::Confirmed);
    };
    if let Some(track) = &subscription.source {
        if subscription.source_revision == 0
            || spec.settings.as_ref().and_then(|settings| settings.enabled) != Some(true)
        {
            return Err(ValidationError::Confirmed);
        }
        validate_track(track)?;
        let address = declaration
            .source
            .as_ref()
            .ok_or(ValidationError::Identity)?;
        if !matches_source_address(track, address) {
            return Err(ValidationError::Identity);
        }
        let data_v1_requested = declaration.data_version.is_some();
        if let Some(offer) = &subscription.data_v1
            && (!data_v1_requested
                || offer.endpoint().is_none()
                || !matches!(&track.kind, Some(track::Kind::Data(data)) if data.capture_clock == offer.capture_clock))
        {
            return Err(ValidationError::Binding);
        }
        match &subscription.execution {
            Some(logical_subscription_state::Execution::Installed(binding)) => {
                if matches!(
                    binding.transport,
                    Some(
                        track_binding::Transport::SctpDataV1(_)
                            | track_binding::Transport::QuicDataV1(_)
                    )
                ) != data_v1_requested
                    || data_v1_requested != subscription.data_v1.is_some()
                {
                    return Err(ValidationError::Binding);
                }
                if let Some(track::Kind::Data(data)) = &track.kind {
                    validate_data_binding(Some(binding), data.delivery, profile)?;
                } else {
                    let kind =
                        TrackKind::try_from(address.kind).map_err(|_| ValidationError::Binding)?;
                    validate_binding(Some(binding), kind, false, profile)?;
                    validate_receiver_format(Some(binding), track)?;
                }
            }
            Some(logical_subscription_state::Execution::Failure(failure)) => {
                validate_failure(failure)?;
            }
            None => {}
        }
    } else if subscription.execution.is_some() || subscription.data_v1.is_some() {
        return Err(ValidationError::Confirmed);
    }
    Ok(())
}

pub fn matches_source_address(track: &Track, source: &LogicalSourceAddress) -> bool {
    track.room_id == source.room
        && track.participant_id == source.participant_id
        && track.track_id == source.track_id
        && track.kind.as_ref().map(track_kind) == Some(source.kind)
}

fn track_kind(kind: &track::Kind) -> i32 {
    match kind {
        track::Kind::Audio(_) => TrackKind::Audio as i32,
        track::Kind::Video(_) => TrackKind::Video as i32,
        track::Kind::Data(_) => TrackKind::Data as i32,
    }
}

fn validate_receiver_format(binding: Option<&TrackBinding>, track: &Track) -> Result {
    match track.kind.as_ref() {
        Some(track::Kind::Audio(source)) => validate_source_format(binding, source.format.as_ref()),
        Some(track::Kind::Video(source)) => validate_source_format(binding, source.format.as_ref()),
        _ => Ok(()),
    }
}

fn validate_source_format(binding: Option<&TrackBinding>, format: Option<&CodecFormat>) -> Result {
    let codec = match binding.and_then(|b| b.transport.as_ref()) {
        Some(track_binding::Transport::Rtp(rtp)) => rtp.codec.as_ref(),
        Some(track_binding::Transport::QuicRtp(rtp)) => rtp.codec.as_ref(),
        _ => return Err(ValidationError::Binding),
    };
    match (codec.and_then(|c| c.format.as_ref()), format) {
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
        || !range(options.decode_target, 0, 31)
        || matches!((options.min_height, options.max_height), (Some(min), Some(max)) if min > max)
    {
        return Err(ValidationError::Settings);
    }
    Ok(())
}

pub fn validate_execution_report(report: &ExecutionReport) -> Result {
    validate_execution_report_for(report, BindingProfile::Rtc)
}

pub fn validate_execution_report_for(report: &ExecutionReport, profile: BindingProfile) -> Result {
    match report.outcome.as_ref() {
        Some(execution_report::Outcome::Failure(failure)) => validate_failure(failure),
        Some(execution_report::Outcome::LogicalReceiver(report)) if report.source_revision != 0 => {
            match report.outcome.as_ref() {
                Some(logical_receiver_report::Outcome::Prepared(binding))
                    if binding
                        .transport
                        .as_ref()
                        .is_some_and(|transport| binding_profile(transport) == profile) =>
                {
                    Ok(())
                }
                Some(logical_receiver_report::Outcome::Failure(failure)) => {
                    validate_failure(failure)
                }
                _ => Err(ValidationError::Binding),
            }
        }
        _ => Err(ValidationError::Failure),
    }
}

fn validate_data_binding(
    binding: Option<&TrackBinding>,
    delivery: i32,
    profile: BindingProfile,
) -> Result {
    if !known::<DataDelivery>(delivery) {
        return Err(ValidationError::Binding);
    }
    if let Some(data) = binding.and_then(DataV1Binding::from_track) {
        return (matches!(
            (profile, data),
            (BindingProfile::Rtc, DataV1Binding::Sctp(_))
                | (BindingProfile::Quic, DataV1Binding::Quic(_))
        ) && data.matches_delivery(delivery))
        .then_some(())
        .ok_or(ValidationError::Binding);
    }
    validate_binding(binding, TrackKind::Data, false, profile)
}

fn binding_profile(transport: &track_binding::Transport) -> BindingProfile {
    use track_binding::Transport::*;
    match transport {
        Rtp(_) | Sctp(_) | SctpDataV1(_) => BindingProfile::Rtc,
        QuicDataV1(_) | QuicRtp(_) => BindingProfile::Quic,
    }
}

fn validate_binding(
    binding: Option<&TrackBinding>,
    kind: TrackKind,
    publication: bool,
    profile: BindingProfile,
) -> Result {
    match binding
        .and_then(|b| b.transport.as_ref())
        .filter(|transport| binding_profile(transport) == profile)
    {
        Some(track_binding::Transport::QuicDataV1(parameters))
            if kind == TrackKind::Data && parameters.format().is_some() =>
        {
            Ok(())
        }
        Some(track_binding::Transport::QuicRtp(parameters))
            if publication || !parameters.publishes_h264_dd =>
        {
            QuicRtpBinding::compile(parameters, kind)?.validate_video_dd_direction(publication)
        }
        Some(track_binding::Transport::Sctp(sctp))
            if kind == TrackKind::Data && matches!(sctp.stream_id, Some(1..=65534)) =>
        {
            Ok(())
        }
        Some(track_binding::Transport::SctpDataV1(sctp))
            if kind == TrackKind::Data && sctp.format().is_some() =>
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

/// Wire-valid, single-encoding QUIC RTP binding. Installation still requires
/// capability negotiation and authorization by the association owner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuicRtpBinding<'a> {
    id: NonZeroU16,
    incarnation: NonZeroU64,
    ssrc: NonZeroU32,
    payload_type: u8,
    format: &'a CodecFormat,
    feedback: &'a [String],
    publishes_h264_dd: bool,
    video_dd: Option<QuicVideoDd>,
}

impl<'a> QuicRtpBinding<'a> {
    pub fn compile(rtp: &'a QuicRtpParameters, kind: TrackKind) -> Result<Self> {
        let id = u16::try_from(rtp.binding_id)
            .ok()
            .and_then(NonZeroU16::new)
            .ok_or(ValidationError::Binding)?;
        let incarnation = NonZeroU64::new(rtp.incarnation).ok_or(ValidationError::Binding)?;
        let ssrc = NonZeroU32::new(rtp.primary_ssrc).ok_or(ValidationError::Binding)?;
        let codec = rtp.codec.as_ref().ok_or(ValidationError::Binding)?;
        let pt = codec
            .payload_type
            .filter(|pt| payload_type(Some(*pt)))
            .and_then(|pt| u8::try_from(pt).ok())
            .ok_or(ValidationError::Binding)?;
        if codec.rtx_payload_type.is_some() || !unique_text(&codec.feedback, 32, 128) {
            return Err(ValidationError::Binding);
        }
        let format = codec.format.as_ref().ok_or(ValidationError::Format)?;
        validate_format(format)?;
        let video_dd = rtp
            .video_dd
            .map(|value| QuicVideoDd::try_from(value).map_err(|_| ValidationError::Binding))
            .transpose()?;
        if let Some(dd) = video_dd {
            if kind != TrackKind::Video || rtp.publishes_h264_dd || dd == QuicVideoDd::Unspecified {
                return Err(ValidationError::Binding);
            }
            VideoMode::for_format(format, RtpScalabilityMode::L1t1.as_str())?;
        } else {
            match (kind, format.known_codec()) {
                (TrackKind::Audio, Some(codec_format::Codec::Opus(_)))
                    if !rtp.publishes_h264_dd && !video_feedback(&codec.feedback) => {}
                (TrackKind::Video, Some(codec_format::Codec::H264(_))) => {}
                _ => return Err(ValidationError::Format),
            }
        }
        Ok(Self {
            id,
            incarnation,
            ssrc,
            payload_type: pt,
            format,
            feedback: &codec.feedback,
            publishes_h264_dd: rtp.publishes_h264_dd,
            video_dd,
        })
    }

    pub const fn id(self) -> NonZeroU16 {
        self.id
    }
    pub const fn incarnation(self) -> NonZeroU64 {
        self.incarnation
    }
    pub const fn ssrc(self) -> NonZeroU32 {
        self.ssrc
    }
    pub const fn payload_type(self) -> u8 {
        self.payload_type
    }
    pub const fn format(self) -> &'a CodecFormat {
        self.format
    }
    pub const fn feedback(self) -> &'a [String] {
        self.feedback
    }
    pub const fn video_dd(self) -> Option<QuicVideoDd> {
        self.video_dd
    }

    pub fn validate_video_dd_direction(self, publication: bool) -> Result {
        match (publication, self.video_dd) {
            (_, None | Some(QuicVideoDd::Absent))
            | (true, Some(QuicVideoDd::Emit))
            | (false, Some(QuicVideoDd::Accept | QuicVideoDd::Require)) => Ok(()),
            _ => Err(ValidationError::Binding),
        }
    }

    pub const fn publishes_h264_dd(self) -> bool {
        self.publishes_h264_dd
    }
}

/// Checks a complete wire binding, not backend capabilities or congestion-control policy.
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
    if !types.contains(RtpHeaderExtensionType::RtpHeaderExtensionMid.as_str()) {
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
            left.track_id == right.track_id
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
            left.publication_id == right.publication_id
                && left.kind == right.kind
                && left.source_room == right.source_room
                && bindings_equivalent(left.binding.as_ref(), right.binding.as_ref())
        }
        (Some(Declaration::SourceLookup(left)), Some(Declaration::SourceLookup(right))) => {
            left.source == right.source
        }
        (
            Some(Declaration::LogicalSubscription(left)),
            Some(Declaration::LogicalSubscription(right)),
        ) => left == right,
        (
            Some(Declaration::SubscriptionSelector(left)),
            Some(Declaration::SubscriptionSelector(right)),
        ) => left == right,
        _ => false,
    }
}

/// Full mutable equality for validated settings, including documented defaults.
pub fn settings_equivalent(left: &ResourceSettings, right: &ResourceSettings) -> bool {
    use resource_settings::Receive;
    left.enabled == right.enabled
        && match (&left.receive, &right.receive) {
            (Some(Receive::Video(left)), Some(Receive::Video(right))) => {
                left.max_height == right.max_height
                    && left.min_height == right.min_height
                    && left.min_fps == right.min_fps
                    && left.priority.unwrap_or(1) == right.priority.unwrap_or(1)
                    && left.decode_target == right.decode_target
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
        (Some(Transport::QuicDataV1(left)), Some(Transport::QuicDataV1(right))) => left == right,
        (Some(Transport::QuicRtp(left)), Some(Transport::QuicRtp(right))) => left == right,
        (Some(Transport::Sctp(left)), Some(Transport::Sctp(right))) => left == right,
        (Some(Transport::SctpDataV1(left)), Some(Transport::SctpDataV1(right))) => left == right,
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

/// Default-normalized transport equality, not directional codec compatibility.
pub fn formats_equivalent(left: &CodecFormat, right: &CodecFormat) -> bool {
    left.mime_type == right.mime_type
        && left.clock_rate == right.clock_rate
        && left.channels == right.channels
        && super::format::normalized_parameters(left) == super::format::normalized_parameters(right)
}

/// Check an explicit video request against the negotiated format and its advertised modes.
pub fn negotiated_video_mode(
    format: &CodecFormat,
    capability: &RtpCodecCapability,
    requested: &str,
) -> core::result::Result<VideoMode, ValidationError> {
    if !capability
        .format
        .as_ref()
        .is_some_and(|supported| formats_equivalent(format, supported))
        || !capability
            .scalability_modes
            .iter()
            .any(|mode| mode == requested)
    {
        return Err(ValidationError::Capability);
    }
    VideoMode::for_format(format, requested)
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
            || !unique_text(&codec.scalability_modes, 40, 64)
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
pub fn valid_data_capture_clock(clock: &DataV1CaptureClock) -> bool {
    transport_text(&clock.id, 256)
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

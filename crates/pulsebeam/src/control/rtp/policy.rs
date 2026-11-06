use super::{failure_code, publication_binding, receiving_binding, transport_capabilities};
use crate::media::{Mid, Rid};
use crate::participant::policy::{ReceiverRtpConfiguration, VideoPolicy};
use crate::rtp::MediaTime;
use crate::track::Track;
use pulsebeam_proto::state::{self as wire, resource_spec};
use pulsebeam_rtc::{CompiledRtpBinding, MediaKind, RtpBindingCompiler};

const PLAYOUT_DELAY: &str = "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay";

pub(crate) fn publication_declaration(
    spec: &wire::ResourceSpec,
) -> Option<(&str, wire::TrackKind, &wire::RtpParameters)> {
    let resource_spec::Declaration::Publication(publication) = spec.declaration.as_ref()? else {
        return None;
    };
    let (kind, binding) = match publication.kind.as_ref()? {
        wire::publication_spec::Kind::Audio(audio) => {
            (wire::TrackKind::Audio, audio.binding.as_ref()?)
        }
        wire::publication_spec::Kind::Video(video) => {
            (wire::TrackKind::Video, video.binding.as_ref()?)
        }
        _ => return None,
    };
    let wire::track_binding::Transport::Rtp(parameters) = binding.transport.as_ref()? else {
        return None;
    };
    Some((&publication.label, kind, parameters))
}

pub(crate) fn publication(
    spec: &wire::ResourceSpec,
    capabilities: &wire::RtpCapabilities,
) -> Result<(String, wire::TrackKind, CompiledRtpBinding), wire::FailureCode> {
    let (label, kind, parameters) =
        publication_declaration(spec).ok_or(wire::FailureCode::Unsupported)?;
    let binding = publication_binding(parameters, kind, capabilities)?;
    // Only the existing production processors qualify a declaration. Temporal
    // H.264 requires its declared dependency descriptor, not inferred layers.
    let dependency_descriptor = parameters.header_extensions.iter().any(|extension| {
        extension.uri
            == wire::RtpHeaderExtensionType::RtpHeaderExtensionDependencyDescriptor.as_str()
    });
    if !matches!(binding.codec.parameters.name.as_str(), "opus" | "h264")
        || kind == wire::TrackKind::Audio
            && (binding.encodings.len() != 1
                || binding
                    .encodings
                    .iter()
                    .any(|encoding| encoding.rid.is_some()))
        || parameters.encodings.iter().any(|encoding| {
            encoding.scalability_mode.as_deref().is_some_and(|mode| {
                mode != wire::RtpScalabilityMode::L1t1.as_str()
                    && (mode != wire::RtpScalabilityMode::L1t3.as_str()
                        || binding.codec.parameters.name != "h264"
                        || !dependency_descriptor)
            })
        })
    {
        return Err(wire::FailureCode::Unsupported);
    }
    let compiler =
        RtpBindingCompiler::new(transport_capabilities(capabilities).map_err(failure_code)?)
            .map_err(failure_code)?;
    Ok((
        label.to_owned(),
        kind,
        compiler.compile(binding).map_err(failure_code)?,
    ))
}

pub(crate) fn receiver(
    spec: &wire::ResourceSpec,
    source: &Track,
    capabilities: &wire::RtpCapabilities,
) -> Result<ReceiverRtpConfiguration, wire::FailureCode> {
    let Some(resource_spec::Declaration::Subscription(subscription)) = &spec.declaration else {
        return Err(wire::FailureCode::InvalidConfiguration);
    };
    let Some(wire::track_binding::Transport::Rtp(parameters)) = subscription
        .binding
        .as_ref()
        .and_then(|binding| binding.transport.as_ref())
    else {
        return Err(wire::FailureCode::Incompatible);
    };
    let kind = wire::TrackKind::try_from(subscription.kind)
        .map_err(|_| wire::FailureCode::InvalidConfiguration)?;
    let binding = receiving_binding(parameters, kind, capabilities)?;
    let [encoding] = binding.encodings.as_slice() else {
        return Err(wire::FailureCode::InvalidConfiguration);
    };
    let primary = encoding.primary.into();
    let rid = encoding
        .rid
        .as_deref()
        .map(Rid::new)
        .transpose()
        .map_err(|_| wire::FailureCode::Unsupported)?;
    if matches!(source, Track::Audio(_))
        && (binding.codec.parameters.name != "opus" || encoding.rid.is_some())
    {
        return Err(wire::FailureCode::Unsupported);
    }
    let format = parameters
        .codec
        .as_ref()
        .and_then(|codec| codec.format.as_ref())
        .ok_or(wire::FailureCode::InvalidConfiguration)?;
    let source_format = match source {
        Track::Audio(audio) if kind == wire::TrackKind::Audio => audio.format.as_ref(),
        Track::Video(video) if kind == wire::TrackKind::Video => video.format.as_ref(),
        _ => return Err(wire::FailureCode::Unsupported),
    };
    if source_format.is_none_or(|source| !wire::validation::formats_equivalent(source, format)) {
        return Err(wire::FailureCode::Incompatible);
    }
    let supports_playout_delay = binding
        .extensions
        .iter()
        .any(|extension| extension.uri == PLAYOUT_DELAY);
    let video = receiver_settings(binding.kind, supports_playout_delay, spec)?;
    let mid = Mid::new(&binding.mid).map_err(|_| wire::FailureCode::Unsupported)?;
    let payload_type = binding.codec.payload_type.into();
    let kind = binding.kind;
    let compiler =
        RtpBindingCompiler::new(transport_capabilities(capabilities).map_err(failure_code)?)
            .map_err(failure_code)?;
    Ok(ReceiverRtpConfiguration {
        binding: compiler.compile(binding).map_err(failure_code)?,
        mid,
        rid,
        primary,
        payload_type,
        kind,
        supports_playout_delay,
        video,
    })
}

pub(crate) fn receiver_settings(
    kind: MediaKind,
    supports_playout_delay: bool,
    spec: &wire::ResourceSpec,
) -> Result<Option<VideoPolicy>, wire::FailureCode> {
    let receive = spec
        .settings
        .as_ref()
        .and_then(|settings| settings.receive.as_ref());
    if let Some(receive) = receive {
        match receive {
            wire::resource_settings::Receive::Audio(audio)
                if kind == MediaKind::Audio && audio.playout_delay.is_none() => {}
            wire::resource_settings::Receive::Video(video)
                if kind == MediaKind::Video
                    && (video.playout_delay.is_none() || supports_playout_delay) => {}
            _ => return Err(wire::FailureCode::Unsupported),
        }
    }
    if kind != MediaKind::Video {
        return Ok(None);
    }
    let options = match receive {
        Some(wire::resource_settings::Receive::Video(video)) => *video,
        _ => wire::VideoReceiveOptions::default(),
    };
    let rounded = |ms: u32| MediaTime::from_hundredths(u64::from(ms.saturating_add(5) / 10));
    Ok(Some(VideoPolicy {
        height_ceiling: options.max_height,
        minimum_height: options.min_height.unwrap_or(0),
        minimum_fps: options.min_fps.unwrap_or(0),
        priority: options.priority.unwrap_or(1),
        playout_delay: options
            .playout_delay
            .map(|delay| (rounded(delay.min_ms), rounded(delay.max_ms))),
    }))
}

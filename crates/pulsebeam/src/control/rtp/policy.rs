use super::{
    fabric_profile, failure_code, is_stock_av1, publication_binding, receiving_binding,
    transport_capabilities,
};
use crate::entity::TrackId;
use crate::media::{Mid, Rid};
use crate::participant::policy::{ReceiverRtpConfiguration, VideoPolicy};
use crate::track::Track;
use pulsebeam_proto::state::{self as wire, resource_spec};
use pulsebeam_rtc::{CompiledRtpBinding, MediaKind, RtpBindingCompiler};

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
    Some((&publication.track_id, kind, parameters))
}

pub(crate) fn publication(
    spec: &wire::ResourceSpec,
    capabilities: &wire::RtpCapabilities,
) -> Result<(TrackId, wire::TrackKind, CompiledRtpBinding), wire::FailureCode> {
    let (label, kind, parameters) =
        publication_declaration(spec).ok_or(wire::FailureCode::Unsupported)?;
    let track_id = TrackId::new(label).map_err(|_| wire::FailureCode::InvalidConfiguration)?;
    let binding = publication_binding(parameters, kind, capabilities)?;
    // Neither simulcast nor temporal H.264 may infer dependencies from RTP markers.
    let dependency_descriptor = parameters.header_extensions.iter().any(|extension| {
        extension.uri
            == wire::RtpHeaderExtensionType::RtpHeaderExtensionDependencyDescriptor.as_str()
    });
    use pulsebeam_core::fabric::Profile;
    let profile =
        fabric_profile(&binding.codec.parameters).ok_or(wire::FailureCode::Unsupported)?;
    if profile == Profile::Vp9
        || (profile.is_video()
            && binding.encodings.len() != 1
            && (profile != Profile::H264 || !dependency_descriptor))
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
        track_id,
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
    fabric_profile(&binding.codec.parameters).ok_or(wire::FailureCode::Unsupported)?;
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
    if binding.codec.parameters.name == "vp9"
        && matches!(source, Track::Video(video) if video.guarantees_video_dd)
        && (encoding.repair.is_none()
            || !binding
                .extensions
                .iter()
                .any(|extension| extension.uri == pulsebeam_core::dd::URI))
    {
        return Err(wire::FailureCode::Incompatible);
    }
    let video = receiver_settings(binding.kind, spec)?;
    if binding.codec.parameters.name == "av1"
        && (!is_stock_av1(&binding.codec.parameters)
            || video.as_ref().is_some_and(|options| {
                options.height_ceiling.is_some()
                    || options.minimum_height != 0
                    || options.minimum_fps != 0
            }))
    {
        return Err(wire::FailureCode::Unsupported);
    }
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
        video,
    })
}

pub(crate) fn receiver_settings(
    kind: MediaKind,
    spec: &wire::ResourceSpec,
) -> Result<Option<VideoPolicy>, wire::FailureCode> {
    let receive = spec
        .settings
        .as_ref()
        .and_then(|settings| settings.receive.as_ref());
    if let Some(receive) = receive {
        match receive {
            wire::resource_settings::Receive::Audio(_) if kind == MediaKind::Audio => {}
            wire::resource_settings::Receive::Video(_) if kind == MediaKind::Video => {}
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
    Ok(Some(VideoPolicy {
        height_ceiling: options.max_height,
        minimum_height: options.min_height.unwrap_or(0),
        minimum_fps: options.min_fps.unwrap_or(0),
        priority: options.priority.unwrap_or(1),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire::RtpHeaderExtensionType as Extension;

    #[test]
    fn h264_requires_dd_only_when_multiple_encodings_need_selection() {
        let capabilities = super::super::capabilities();
        let publish = |encodings: usize, dd: bool| {
            let mut extensions = vec![
                Extension::RtpHeaderExtensionMid,
                Extension::RtpHeaderExtensionTransportCcDraft01,
                Extension::RtpHeaderExtensionRid,
            ];
            if dd {
                extensions.push(Extension::RtpHeaderExtensionDependencyDescriptor);
            }
            let rtp = wire::RtpParameters {
                mid: "camera".into(),
                extmap_allow_mixed: None,
                codec: Some(wire::RtpCodecParameters {
                    payload_type: Some(96),
                    rtx_payload_type: None,
                    format: capabilities.codecs[1].format.clone(),
                    feedback: vec![wire::RtcpFeedback::TransportCc.into()],
                }),
                header_extensions: extensions
                    .into_iter()
                    .zip(1..)
                    .map(|(extension, id)| wire::RtpHeaderExtensionParameters {
                        id,
                        uri: extension.into(),
                    })
                    .collect(),
                encodings: (1..=u32::try_from(encodings).unwrap())
                    .map(|ssrc| wire::RtpEncodingParameters {
                        ssrc: Some(ssrc),
                        rid: format!("v{ssrc}"),
                        ..Default::default()
                    })
                    .collect(),
                rtcp: Some(wire::RtcpParameters {
                    cname: "camera".into(),
                    reduced_size: true,
                }),
            };
            let spec = wire::ResourceSpec {
                settings: None,
                declaration: Some(resource_spec::Declaration::Publication(
                    wire::PublicationSpec {
                        track_id: "camera".into(),
                        kind: Some(wire::publication_spec::Kind::Video(
                            wire::VideoPublication {
                                binding: Some(wire::TrackBinding {
                                    transport: Some(wire::track_binding::Transport::Rtp(rtp)),
                                }),
                            },
                        )),
                    },
                )),
            };
            publication(&spec, &capabilities).map(|(_, _, binding)| binding)
        };
        assert_eq!(publish(1, false).unwrap().description().encodings.len(), 1);
        assert_eq!(
            publish(2, false).unwrap_err(),
            wire::FailureCode::Unsupported
        );
        assert_eq!(publish(2, true).unwrap().description().encodings.len(), 2);
    }
}

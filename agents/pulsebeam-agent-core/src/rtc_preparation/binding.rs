use alloc::vec::Vec;
use pulsebeam_proto::state::{self as wire, sdp, validation};

use crate::client_sdp::{Binding, Error, Kind};

pub(super) fn prepared_rtp_binding(
    binding: &Binding,
    capabilities: &wire::RtpCapabilities,
    publication: bool,
) -> Result<wire::TrackBinding, Error> {
    let pending = publication && binding.identities_pending();
    if !binding.complete() && !pending {
        return Err(Error::Observation);
    }
    // Context selects exact SDP parameters from the HTTP envelope. Recover that
    // same typed format rather than inventing a second codec/FMTP policy.
    let capability = capabilities
        .codecs
        .iter()
        .find(|capability| {
            capability.format.as_ref().is_some_and(|format| {
                sdp::codec_parameters(format).is_ok_and(|parameters| {
                    parameters
                        .name
                        .eq_ignore_ascii_case(&binding.codec.parameters.name)
                        && parameters.clock_rate == binding.codec.parameters.clock_rate
                        && parameters.channels == binding.codec.parameters.channels
                        && parameters.fmtp == binding.codec.parameters.fmtp
                })
            })
        })
        .ok_or_else(|| Error::Envelope("codec format".into()))?;
    if let Some(repair) = &binding.codec.repair
        && (!capability.rtx_supported || repair.clock_rate != binding.codec.parameters.clock_rate)
    {
        return Err(Error::Envelope("RTX".into()));
    }
    let mode = if binding.rtcp.reduced_size {
        wire::RtcpMode::ReducedSize
    } else {
        wire::RtcpMode::Compound
    };
    if !capabilities.rtcp_modes.contains(&(mode as i32)) {
        return Err(Error::Envelope("RTCP mode".into()));
    }
    let feedback = binding
        .codec
        .parameters
        .feedback
        .iter()
        .map(|name| {
            capability
                .feedback
                .iter()
                .copied()
                .find(|value| sdp::feedback_name(*value) == Ok(name.as_str()))
                .ok_or_else(|| Error::Envelope("RTCP feedback".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let header_extensions = binding
        .extensions
        .iter()
        .map(|extension| {
            let kind = capabilities
                .header_extensions
                .iter()
                .copied()
                .find(|value| sdp::extension_uri(*value) == Ok(extension.uri.as_str()))
                .ok_or_else(|| Error::Envelope("RTP extension".into()))?;
            Ok(wire::RtpHeaderExtensionParameters {
                r#type: kind,
                id: u32::from(extension.id),
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let encodings = binding
        .encodings
        .iter()
        .map(|encoding| {
            let scalability_mode = encoding
                .runtime
                .scalability_mode
                .as_deref()
                .map(|name| {
                    let mode = match name {
                        "L1T1" => wire::RtpScalabilityMode::L1t1,
                        "L1T2" => wire::RtpScalabilityMode::L1t2,
                        "L1T3" => wire::RtpScalabilityMode::L1t3,
                        _ => return Err(Error::Envelope("scalability mode".into())),
                    } as i32;
                    if !capability.scalability_modes.contains(&mode) {
                        return Err(Error::Envelope("scalability mode".into()));
                    }
                    Ok(mode)
                })
                .transpose()?;
            Ok(wire::RtpEncodingParameters {
                ssrc: encoding.primary,
                rtx_ssrc: encoding.repair,
                rid: encoding.rid.clone().unwrap_or_default(),
                scalability_mode,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let rtp = wire::RtpParameters {
        mid: binding.mid.clone(),
        codec: Some(wire::RtpCodecParameters {
            payload_type: Some(u32::from(binding.codec.payload_type)),
            rtx_payload_type: binding
                .codec
                .repair
                .as_ref()
                .map(|repair| u32::from(repair.payload_type)),
            feedback,
            format: capability.format,
        }),
        header_extensions,
        encodings,
        rtcp: Some(wire::RtcpParameters {
            cname: binding.rtcp.cname.clone().unwrap_or_default(),
            reduced_size: binding.rtcp.reduced_size,
        }),
    };
    let kind = match binding.kind {
        Kind::Audio => wire::TrackKind::Audio,
        Kind::Video => wire::TrackKind::Video,
        Kind::Application => return Err(Error::Envelope("RTP kind".into())),
    };
    if pending {
        validation::validate_rtp_envelope(&rtp, kind, publication)
            .map_err(|_| Error::Envelope("RTP binding".into()))?;
        return Err(Error::IdentityPending);
    }
    validation::validate_rtp(&rtp, kind, publication)
        .map_err(|_| Error::Envelope("RTP binding".into()))?;
    Ok(wire::TrackBinding {
        transport: Some(wire::track_binding::Transport::Rtp(rtp)),
    })
}

#[cfg(test)]
pub(super) mod tests;

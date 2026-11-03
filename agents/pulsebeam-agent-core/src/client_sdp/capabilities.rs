use pulsebeam_proto::state::{self as wire, sdp, validation};

use super::{Capabilities, CodecCapability, CodecParameters, Error, RtcpMode};

impl Capabilities {
    pub fn from_wire(capabilities: &wire::RtpCapabilities) -> Result<Self, Error> {
        validation::validate_capabilities(capabilities)
            .map_err(|_| Error::Envelope("capabilities".into()))?;
        let codecs = capabilities
            .codecs
            .iter()
            .map(|capability| {
                let format = capability.format.as_ref().ok_or(Error::Identifier)?;
                let parameters = sdp::codec_parameters(format).map_err(|_| Error::Identifier)?;
                let feedback = capability
                    .feedback
                    .iter()
                    .map(|feedback| {
                        sdp::feedback_name(*feedback)
                            .map(str::to_owned)
                            .map_err(|_| Error::Identifier)
                    })
                    .collect::<Result<_, _>>()?;
                Ok(CodecCapability {
                    parameters: CodecParameters {
                        name: parameters.name.into(),
                        clock_rate: parameters.clock_rate,
                        channels: parameters.channels,
                        fmtp: parameters.fmtp,
                        feedback,
                    },
                    repair: capability.rtx_supported,
                })
            })
            .collect::<Result<_, Error>>()?;
        let extensions = capabilities
            .header_extensions
            .iter()
            .map(|extension| {
                sdp::extension_uri(*extension)
                    .map(str::to_owned)
                    .map_err(|_| Error::Identifier)
            })
            .collect::<Result<_, _>>()?;
        let rtcp_modes = capabilities
            .rtcp_modes
            .iter()
            .map(|mode| match wire::RtcpMode::try_from(*mode) {
                Ok(wire::RtcpMode::Compound) => Ok(RtcpMode::Compound),
                Ok(wire::RtcpMode::ReducedSize) => Ok(RtcpMode::ReducedSize),
                _ => Err(Error::Identifier),
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            codecs,
            extensions,
            rtcp_modes,
        })
    }
}

use pulsebeam_proto::state::{self as wire, sdp, validation};

use super::{Capabilities, CodecCapability, CodecParameters, Error, RtcpMode};

impl Capabilities {
    pub fn from_wire(capabilities: &wire::RtpCapabilities) -> Result<Self, Error> {
        validation::validate_capabilities(capabilities)
            .map_err(|_| Error::Envelope("capabilities".into()))?;
        let codecs = capabilities
            .codecs
            .iter()
            .filter_map(|capability| {
                let format = capability.format.as_ref()?;
                // A valid future description may exceed this adapter's channel representation.
                let parameters = sdp::codec_parameters(format).ok()?;
                Some(CodecCapability {
                    parameters: CodecParameters {
                        name: parameters.name,
                        clock_rate: parameters.clock_rate,
                        channels: parameters.channels,
                        fmtp: parameters.fmtp,
                        feedback: capability.feedback.iter().cloned().collect(),
                    },
                    repair: capability.rtx_supported,
                })
            })
            .collect();
        let extensions = capabilities.header_extensions.iter().cloned().collect();
        let rtcp_modes = capabilities
            .rtcp_modes
            .iter()
            .filter_map(|mode| match wire::RtcpMode::try_from(mode.as_str()) {
                Ok(wire::RtcpMode::Compound) => Some(RtcpMode::Compound),
                Ok(wire::RtcpMode::ReducedSize) => Some(RtcpMode::ReducedSize),
                _ => None,
            })
            .collect();
        Ok(Self {
            codecs,
            extensions,
            rtcp_modes,
        })
    }
}

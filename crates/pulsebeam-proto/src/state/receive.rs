use super::{RtcpMode, RtpParameters, RtpReceiveOffer};

impl From<&RtpParameters> for RtpReceiveOffer {
    fn from(binding: &RtpParameters) -> Self {
        Self {
            mid: binding.mid.clone(),
            codecs: binding.codec.iter().cloned().collect(),
            header_extensions: binding.header_extensions.clone(),
            rtcp_modes: binding
                .rtcp
                .iter()
                .map(|rtcp| {
                    if rtcp.reduced_size {
                        RtcpMode::ReducedSize
                    } else {
                        RtcpMode::Compound
                    }
                    .into()
                })
                .collect(),
        }
    }
}

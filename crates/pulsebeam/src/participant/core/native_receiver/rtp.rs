use super::*;
use crate::participant::policy::{ReceiverRtpConfiguration, VideoPolicy};
use pulsebeam_rtc::{
    CompiledRtpBinding, CompiledRtpConfigurationPatch, MediaKind, RtpConfigurationError,
};

pub(super) struct RtpReceiver {
    pub(super) video_options: Option<VideoPolicy>,
    pub(super) kind: MediaKind,
    pub(super) fabric_profile: Option<pulsebeam_core::fabric::Profile>,
    pub(super) supports_playout_delay: bool,
    pending: Option<CompiledRtpBinding>,
    mid: Mid,
    rid: Option<crate::media::Rid>,
    ssrc: Ssrc,
    fid: Option<[u32; 2]>,
    payload_type: Pt,
}

impl RtpReceiver {
    pub(super) fn new(configuration: ReceiverRtpConfiguration) -> Self {
        use pulsebeam_core::fabric::Profile;
        let codec = &configuration.binding.description().codec.parameters;
        let fabric_profile = match (configuration.kind, codec.name.as_str()) {
            (MediaKind::Audio, "opus")
                if codec.clock_rate == 48_000 && codec.channels == Some(2) =>
            {
                Some(Profile::Opus)
            }
            (MediaKind::Video, "h264")
                if codec.clock_rate == 90_000
                    && codec.channels.is_none()
                    && codec
                        .fmtp
                        .get("packetization-mode")
                        .is_some_and(|mode| mode == "1") =>
            {
                Some(Profile::H264)
            }
            (MediaKind::Video, "av1") if crate::control::rtp::is_stock_av1(codec) => {
                Some(Profile::Av1)
            }
            _ => None,
        };
        let fid = match configuration.binding.description().encodings.as_slice() {
            [encoding] if encoding.rid.is_none() => {
                encoding.repair.map(|repair| [encoding.primary, repair])
            }
            _ => None,
        };
        Self {
            fabric_profile,
            video_options: configuration.video,
            kind: configuration.kind,
            supports_playout_delay: configuration.supports_playout_delay,
            pending: Some(configuration.binding),
            mid: configuration.mid,
            rid: configuration.rid,
            ssrc: configuration.primary,
            fid,
            payload_type: configuration.payload_type,
        }
    }

    pub(super) fn playout_delay(&self) -> Option<(crate::rtp::MediaTime, crate::rtp::MediaTime)> {
        self.video_options.as_ref()?.playout_delay
    }

    pub(super) fn mid(&self) -> Mid {
        self.mid
    }

    pub(super) fn rid(&self) -> Option<crate::media::Rid> {
        self.rid
    }

    pub(super) fn ssrc(&self) -> Ssrc {
        self.ssrc
    }

    pub(super) fn fid(&self) -> Option<[u32; 2]> {
        self.fid
    }

    pub(super) fn payload_type(&self) -> Pt {
        self.payload_type
    }

    pub(super) fn install(
        &mut self,
        transport: &mut RtcDriver,
    ) -> Result<(), RtpConfigurationError> {
        let binding = self
            .pending
            .take()
            .ok_or(RtpConfigurationError::ConnectionState)?;
        if self.fabric_profile == Some(pulsebeam_core::fabric::Profile::Av1) {
            let [encoding] = binding.description().encodings.as_slice() else {
                return Err(RtpConfigurationError::Unsupported);
            };
            let repair = encoding.repair.ok_or(RtpConfigurationError::Unsupported)?;
            if transport.fabric_rtp_answer_fid(self.mid, encoding.primary)
                != Some([encoding.primary, repair])
            {
                return Err(RtpConfigurationError::Unsupported);
            }
        }
        transport.update_compiled_rtp(CompiledRtpConfigurationPatch {
            outbound: vec![binding],
            ..Default::default()
        })
    }

    pub(super) fn set_active(
        &self,
        transport: &mut RtcDriver,
        active: bool,
    ) -> Result<(), RtpConfigurationError> {
        transport.set_rtp_encoding_active(u32::from(self.ssrc), active)
    }

    pub(super) fn remove(&self, transport: &mut RtcDriver) -> Result<(), RtpConfigurationError> {
        transport.update_compiled_rtp(CompiledRtpConfigurationPatch {
            remove_encodings: [u32::from(self.ssrc)].into_iter().collect(),
            ..Default::default()
        })
    }
}

use super::*;
use crate::participant::policy::{ReceiverRtpConfiguration, VideoPolicy};
use pulsebeam_rtc::{
    CompiledRtpBinding, CompiledRtpConfigurationPatch, MediaKind, RtpConfigurationError,
};

pub(super) struct RtpReceiver {
    pub(super) video_options: Option<VideoPolicy>,
    pub(super) kind: MediaKind,
    pub(super) supports_playout_delay: bool,
    pending: Option<CompiledRtpBinding>,
    mid: Mid,
    rid: Option<crate::media::Rid>,
    ssrc: Ssrc,
    payload_type: Pt,
}

impl RtpReceiver {
    pub(super) fn new(configuration: ReceiverRtpConfiguration) -> Self {
        Self {
            video_options: configuration.video,
            kind: configuration.kind,
            supports_playout_delay: configuration.supports_playout_delay,
            pending: Some(configuration.binding),
            mid: configuration.mid,
            rid: configuration.rid,
            ssrc: configuration.primary,
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

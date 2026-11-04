use super::*;
use crate::participant::rtp::{failure_code, receiving_binding};
use crate::rtp::{AUDIO_FREQUENCY, RtpPacket, timeline::Timeline};
use pulsebeam_rtc::{RtpBinding, RtpConfigurationPatch};

pub(super) struct RtpReceiver {
    pub(super) binding: RtpBinding,
    pub(super) video_options: Option<wire::VideoReceiveOptions>,
    mid: Mid,
    rid: Option<crate::media::Rid>,
    ssrc: Ssrc,
    timeline: Timeline,
    started: bool,
}

impl RtpReceiver {
    pub(super) fn prepare(
        spec: &wire::ResourceSpec,
        source: &Track,
        capabilities: &wire::RtpCapabilities,
    ) -> Result<Self, wire::FailureCode> {
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
        let ssrc = encoding.primary.into();
        let rid = encoding
            .rid
            .as_deref()
            .map(crate::media::Rid::new)
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
        if source_format.is_none_or(|source| {
            !pulsebeam_proto::state::validation::formats_equivalent(source, format)
        }) {
            return Err(wire::FailureCode::Incompatible);
        }
        Self::validate_settings(&binding, spec)?;
        let video_options = Self::video_options(&binding, spec);
        let mid = Mid::new(&binding.mid).map_err(|_| wire::FailureCode::Unsupported)?;
        Ok(Self {
            binding,
            video_options,
            mid,
            rid,
            ssrc,
            timeline: Timeline::new(AUDIO_FREQUENCY),
            started: false,
        })
    }

    pub(super) fn video_options(
        binding: &RtpBinding,
        spec: &wire::ResourceSpec,
    ) -> Option<wire::VideoReceiveOptions> {
        (binding.kind == pulsebeam_rtc::MediaKind::Video).then(|| {
            match spec
                .settings
                .as_ref()
                .and_then(|settings| settings.receive.as_ref())
            {
                Some(wire::resource_settings::Receive::Video(video)) => *video,
                _ => wire::VideoReceiveOptions::default(),
            }
        })
    }

    pub(super) fn validate_settings(
        binding: &RtpBinding,
        spec: &wire::ResourceSpec,
    ) -> Result<(), wire::FailureCode> {
        if let Some(receive) = spec
            .settings
            .as_ref()
            .and_then(|settings| settings.receive.as_ref())
        {
            match receive {
                wire::resource_settings::Receive::Audio(audio)
                    if binding.kind == pulsebeam_rtc::MediaKind::Audio
                        && audio.playout_delay.is_none() => {}
                wire::resource_settings::Receive::Video(video)
                    if binding.kind == pulsebeam_rtc::MediaKind::Video
                        && (video.playout_delay.is_none()
                            || binding.extensions.iter().any(|extension| {
                                extension.uri
                                    == "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay"
                            })) => {}
                _ => return Err(wire::FailureCode::Unsupported),
            }
        }
        Ok(())
    }

    pub(super) fn playout_delay(&self) -> Option<(crate::rtp::MediaTime, crate::rtp::MediaTime)> {
        let delay = self.video_options.as_ref()?.playout_delay?;
        let rounded =
            |ms: u32| crate::rtp::MediaTime::from_hundredths(u64::from(ms.saturating_add(5) / 10));
        Some((rounded(delay.min_ms), rounded(delay.max_ms)))
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

    pub(super) fn install(
        &self,
        transport: &mut Transport,
        active: bool,
    ) -> Result<(), wire::FailureCode> {
        let mut binding = self.binding.clone();
        for encoding in &mut binding.encodings {
            encoding.active = active;
        }
        transport
            .update_rtp(RtpConfigurationPatch {
                outbound: vec![binding],
                ..Default::default()
            })
            .map_err(failure_code)
    }

    pub(super) fn remove(&self, transport: &mut Transport) -> Result<(), wire::FailureCode> {
        transport
            .update_rtp(RtpConfigurationPatch {
                remove_encodings: self
                    .binding
                    .encodings
                    .iter()
                    .map(|encoding| encoding.primary)
                    .collect(),
                ..Default::default()
            })
            .map_err(failure_code)
    }

    pub(super) fn forward_audio(
        &mut self,
        source: TrackId,
        packet: &RtpPacket,
        writer: &mut StreamWriter,
    ) {
        if !self.started {
            self.timeline.rebase_audio(packet);
            self.started = true;
        }
        let mut packet = packet.clone();
        if self.timeline.rewrite_audio(&mut packet) {
            writer.write_audio_owned(
                packet,
                source,
                self.mid(),
                self.ssrc,
                self.binding.codec.payload_type.into(),
            );
        }
    }
}

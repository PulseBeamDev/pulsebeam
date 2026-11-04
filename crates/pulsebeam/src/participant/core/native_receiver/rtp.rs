use super::*;
use crate::participant::rtp::{failure_code, receiving_binding};
use crate::rtp::{AUDIO_FREQUENCY, RtpPacket, timeline::Timeline};
use pulsebeam_rtc::{RtpBinding, RtpConfigurationPatch};

pub(super) struct RtpReceiver {
    pub(super) binding: RtpBinding,
    pub(super) plan: wire::TrackBinding,
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
        mut allocate: impl FnMut() -> Result<u32, wire::FailureCode>,
    ) -> Result<Self, wire::FailureCode> {
        use wire::FailureCode as Failure;
        let Some(resource_spec::Declaration::Subscription(subscription)) = &spec.declaration else {
            return Err(Failure::InvalidConfiguration);
        };
        let offer = subscription
            .rtp_offer
            .as_ref()
            .ok_or(Failure::InvalidConfiguration)?;
        let kind = wire::TrackKind::try_from(subscription.kind)
            .map_err(|_| Failure::InvalidConfiguration)?;
        wire::validation::validate_receive_offer(offer, kind)
            .map_err(|_| Failure::InvalidConfiguration)?;
        let source_format = match source {
            Track::Audio(audio) if kind == wire::TrackKind::Audio => audio.format.as_ref(),
            Track::Video(video) if kind == wire::TrackKind::Video => video.format.as_ref(),
            _ => return Err(Failure::Unsupported),
        }
        .ok_or(Failure::Unsupported)?;
        let capability = capabilities
            .codecs
            .iter()
            .find(|capability| {
                capability.format.as_ref().is_some_and(|format| {
                    wire::validation::formats_equivalent(format, source_format)
                })
            })
            .ok_or(Failure::Unsupported)?;
        let offered = offer
            .codecs
            .iter()
            .find(|codec| {
                codec.format.as_ref().is_some_and(|format| {
                    wire::validation::formats_equivalent(format, source_format)
                })
            })
            .ok_or(Failure::Incompatible)?;
        let feedback: Vec<_> = offered
            .feedback
            .iter()
            .filter(|value| capability.feedback.contains(value))
            .cloned()
            .collect();
        let extensions: Vec<_> = offer
            .header_extensions
            .iter()
            .filter(|extension| capabilities.header_extensions.contains(&extension.uri))
            .cloned()
            .collect();
        if !feedback
            .iter()
            .any(|value| value == wire::RtcpFeedback::TransportCc.as_str())
            || ![
                wire::RtpHeaderExtensionType::RtpHeaderExtensionMid,
                wire::RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01,
            ]
            .iter()
            .all(|kind| {
                extensions
                    .iter()
                    .any(|extension| extension.uri == kind.as_str())
            })
        {
            return Err(Failure::Unsupported);
        }
        let mode = [wire::RtcpMode::Compound, wire::RtcpMode::ReducedSize]
            .into_iter()
            .find(|mode| {
                offer.rtcp_modes.iter().any(|value| value == mode.as_str())
                    && capabilities
                        .rtcp_modes
                        .iter()
                        .any(|value| value == mode.as_str())
            })
            .ok_or(Failure::Unsupported)?;
        let repair = offered
            .rtx_payload_type
            .filter(|_| capability.rtx_supported);
        let primary = allocate()?;
        let parameters = wire::RtpParameters {
            mid: offer.mid.clone(),
            codec: Some(wire::RtpCodecParameters {
                payload_type: offered.payload_type,
                rtx_payload_type: repair,
                format: Some(source_format.clone()),
                feedback,
            }),
            header_extensions: extensions,
            encodings: vec![wire::RtpEncodingParameters {
                ssrc: Some(primary),
                rtx_ssrc: if repair.is_some() {
                    Some(allocate()?)
                } else {
                    None
                },
                ..Default::default()
            }],
            rtcp: Some(wire::RtcpParameters {
                cname: format!("pulsebeam-{primary:08x}"),
                reduced_size: mode == wire::RtcpMode::ReducedSize,
            }),
        };
        wire::validation::validate_receive_plan(offer, &parameters, kind)
            .map_err(|_| Failure::Unsupported)?;
        let binding = receiving_binding(&parameters, kind, capabilities)?;
        Self::validate_settings(&binding, spec)?;
        let video_options = Self::video_options(&binding, spec);
        let mid = Mid::new(&binding.mid).map_err(|_| Failure::Unsupported)?;
        Ok(Self {
            binding,
            plan: wire::TrackBinding {
                transport: Some(wire::track_binding::Transport::Rtp(parameters)),
            },
            video_options,
            mid,
            rid: None,
            ssrc: primary.into(),
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

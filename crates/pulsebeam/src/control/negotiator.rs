use std::time::Duration;

use crate::participant::downstream::INITIAL_BANDWIDTH;
use pulsebeam_proto::rtp_extensions;
use str0m::{
    Candidate, IceCreds, Rtc, RtcConfig, RtcError,
    change::{SdpAnswer, SdpOffer},
    format::{Codec, CodecConfig, FormatParams},
    media::{Direction, Frequency, MediaKind, Mid, Pt},
    rtp::Extension,
};
use tokio::time::Instant;

pub const MAX_RECV_VIDEO_SLOTS: usize = 2;
pub const MAX_RECV_AUDIO_SLOTS: usize = 2;
// https://github.com/PulseBeamDev/pulsebeam/issues/133
pub const MAX_SEND_VIDEO_SLOTS: usize = 7;
pub const MAX_SEND_AUDIO_SLOTS: usize = 3;
pub const MAX_DATA_CHANNELS: usize = 1;

#[derive(Debug)]
pub enum MediaType {
    Video,
    Audio,
    Application,
    Unknown,
}

impl MediaType {
    fn as_str(&self) -> &str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Application => "application",
            Self::Unknown => "unknown",
        }
    }
}

impl From<&str> for MediaType {
    fn from(value: &str) -> Self {
        match value {
            "video" => MediaType::Video,
            "audio" => MediaType::Audio,
            "application" => MediaType::Application,
            _ => MediaType::Unknown,
        }
    }
}

impl TryFrom<MediaType> for MediaKind {
    type Error = MediaType;

    fn try_from(value: MediaType) -> Result<Self, Self::Error> {
        match value {
            MediaType::Video => Ok(MediaKind::Video),
            MediaType::Audio => Ok(MediaKind::Audio),
            other => Err(other),
        }
    }
}

impl std::fmt::Display for MediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(thiserror::Error, Debug)]
pub enum NegotiatorError {
    #[error("RTC engine error: {0}")]
    Rtc(#[from] RtcError),
    #[error("{0} {1} slots limit exceeded (max {2})")]
    SlotsLimit(MediaType, Direction, usize),
    #[error("SendRecv direction is not supported for {0}")]
    DirectionNotSupported(MediaType),
    #[error("negotiated media resources are inconsistent")]
    InvalidResources,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NegotiatedMedia {
    pub media_index: u32,
    pub mid: Mid,
    pub kind: MediaKind,
    pub direction: Direction,
}

#[derive(Debug)]
pub struct NegotiatedResources {
    media: Vec<NegotiatedMedia>,
}

impl NegotiatedResources {
    pub fn media(&self, mid: Mid) -> Option<NegotiatedMedia> {
        self.media.iter().find(|media| media.mid == mid).copied()
    }

    #[cfg(test)]
    fn as_slice(&self) -> &[NegotiatedMedia] {
        &self.media
    }
}

pub struct Negotiator {
    candidates: Vec<Candidate>,
}

impl Negotiator {
    pub fn new(candidates: Vec<Candidate>) -> Self {
        Self { candidates }
    }

    pub fn create_answer(
        &mut self,
        offer: SdpOffer,
        creds: IceCreds,
    ) -> Result<(Rtc, SdpAnswer, NegotiatedResources), NegotiatorError> {
        tracing::debug!("{offer}");
        let mut rtc_config = RtcConfig::new()
            .clear_codecs()
            .set_rtp_mode(true)
            .set_extension(
                rtp_extensions::ABS_CAPTURE_TIME,
                Extension::AbsoluteCaptureTime,
            )
            // Sender-declared per-layer target bitrates. Browsers emit this for
            // simulcast/SVC; we allocate on the declared targets instead of
            // measured (VBR) throughput. Falls back to measurement when absent.
            .set_extension(
                rtp_extensions::VIDEO_LAYERS_ALLOCATION,
                Extension::with_serializer(str0m::rtp::vla::URI, str0m::rtp::vla::Serializer),
            )
            // Lets the SFU bound each subscriber's jitter buffer (latency cap)
            // by stamping min/max playout delay on egress RTP.
            .set_extension(rtp_extensions::PLAYOUT_DELAY, Extension::PlayoutDelay)
            // Per-frame dependency structure for SVC codecs. Captured verbatim
            // here; parsing needs per-stream template state (see rtp::dd).
            .set_extension(
                rtp_extensions::DEPENDENCY_DESCRIPTOR,
                Extension::with_serializer(pulsebeam_core::dd::URI, pulsebeam_core::dd::Serializer),
            )
            // .set_stats_interval(Some(Duration::from_millis(200)))
            // TODO: enable bwe
            .enable_bwe(Some(INITIAL_BANDWIDTH))
            // Uncomment this to see statistics
            // .set_stats_interval(Some(Duration::from_secs(1)))
            .set_ice_lite(true)
            .set_snap_enabled(true)
            .set_dtls_version(str0m::config::DtlsVersion::Auto);
        rtc_config.set_initial_stun_rto(Duration::from_millis(200));
        rtc_config.set_max_stun_rto(Duration::from_millis(1500));
        rtc_config.set_max_stun_retransmits(5);
        configure_room_codecs(rtc_config.codec_config());

        // Stamp the ICE credentials with routing metadata before building the Rtc
        // so they are used by str0m's ICE agent from the start.  Using RtcConfig
        // is the correct path — direct_api().set_local_ice_credentials() conflicts
        // with the SDP negotiation path.
        let rtc_config = rtc_config.set_local_ice_credentials(creds);

        let mut rtc = rtc_config.build(Instant::now().into());
        for c in &self.candidates {
            rtc.add_local_candidate(c.clone());
        }

        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(NegotiatorError::Rtc)?;
        Self::enforce_media_lines(&answer)?;
        let resources = Self::negotiated_resources(&answer)?;

        tracing::debug!("{answer}");
        Ok((rtc, answer, resources))
    }

    fn negotiated_resources(answer: &SdpAnswer) -> Result<NegotiatedResources, NegotiatorError> {
        let mut media = Vec::new();
        for (index, section) in answer.media_lines.iter().enumerate() {
            if section.disabled {
                continue;
            }
            let kind = match section.typ.to_string().as_str() {
                "video" => MediaKind::Video,
                "audio" => MediaKind::Audio,
                _ => continue,
            };
            let direction = section.direction();
            if !matches!(direction, Direction::SendOnly | Direction::RecvOnly) {
                return Err(NegotiatorError::InvalidResources);
            }
            let media_index =
                u32::try_from(index).map_err(|_| NegotiatorError::InvalidResources)?;
            let mid = section.mid();
            if media
                .iter()
                .any(|resource: &NegotiatedMedia| resource.mid == mid)
            {
                return Err(NegotiatorError::InvalidResources);
            }
            media.push(NegotiatedMedia {
                media_index,
                mid,
                kind,
                direction,
            });
        }
        Ok(NegotiatedResources { media })
    }

    fn enforce_media_lines(answer: &SdpAnswer) -> Result<(), NegotiatorError> {
        let mut video_recv_count = 0usize;
        let mut video_send_count = 0usize;
        let mut audio_recv_count = 0usize;
        let mut audio_send_count = 0usize;
        let mut data_channel_count = 0usize;

        for m in &answer.media_lines {
            if m.disabled {
                continue;
            }
            let kind = m.typ.to_string();
            let dir = m.direction();
            let media_type = kind.as_str().into();

            if (dir == Direction::SendRecv || dir == Direction::Inactive) && kind != "application" {
                return Err(NegotiatorError::DirectionNotSupported(media_type));
            }

            match (media_type, dir) {
                (MediaType::Video, Direction::RecvOnly) => {
                    video_recv_count = video_recv_count.saturating_add(1);
                    if video_recv_count > MAX_RECV_VIDEO_SLOTS {
                        return Err(NegotiatorError::SlotsLimit(
                            MediaType::Video,
                            Direction::SendOnly,
                            MAX_RECV_VIDEO_SLOTS,
                        ));
                    }
                }
                (MediaType::Video, Direction::SendOnly) => {
                    video_send_count = video_send_count.saturating_add(1);
                    if video_send_count > MAX_SEND_VIDEO_SLOTS {
                        return Err(NegotiatorError::SlotsLimit(
                            MediaType::Video,
                            Direction::RecvOnly,
                            MAX_SEND_VIDEO_SLOTS,
                        ));
                    }
                }
                (MediaType::Audio, Direction::RecvOnly) => {
                    audio_recv_count = audio_recv_count.saturating_add(1);
                    if audio_recv_count > MAX_RECV_AUDIO_SLOTS {
                        return Err(NegotiatorError::SlotsLimit(
                            MediaType::Audio,
                            Direction::SendOnly,
                            MAX_RECV_AUDIO_SLOTS,
                        ));
                    }
                }
                (MediaType::Audio, Direction::SendOnly) => {
                    audio_send_count = audio_send_count.saturating_add(1);
                    if audio_send_count > MAX_SEND_AUDIO_SLOTS {
                        return Err(NegotiatorError::SlotsLimit(
                            MediaType::Audio,
                            Direction::RecvOnly,
                            MAX_SEND_AUDIO_SLOTS,
                        ));
                    }
                }
                (MediaType::Application, dir) => {
                    data_channel_count = data_channel_count.saturating_add(1);
                    if data_channel_count > MAX_DATA_CHANNELS {
                        return Err(NegotiatorError::SlotsLimit(
                            MediaType::Application,
                            dir,
                            MAX_DATA_CHANNELS,
                        ));
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }
}

/// The codec set every room negotiates. Static for now: the same fixed codecs
/// apply to all rooms — per-room selection is not yet plumbed through.
///
/// H.264 only for now. It is the lowest common denominator for small clients
/// (embedded devices, smartphones, OBS), so it maximizes compatibility. VP9/AV1
/// (needed for codec-agnostic E2EE) are intentionally left off until per-room
/// codec configuration lands.
fn configure_room_codecs(codec_config: &mut CodecConfig) {
    const PT_OPUS: Pt = Pt::new_with_value(111);

    codec_config.add_config(
        PT_OPUS,
        None,
        Codec::Opus,
        Frequency::FORTY_EIGHT_KHZ,
        Some(2),
        FormatParams {
            min_p_time: Some(10),
            use_inband_fec: Some(true),
            use_dtx: Some(true),
            stereo: Some(true),
            sprop_stereo: Some(true),
            ..Default::default()
        },
    );
    codec_config.enable_h264(true);
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;

    fn chrome_like_offer(direction: Direction) -> SdpOffer {
        let mut config = RtcConfig::new()
            .clear_codecs()
            // Chrome's usual ids: dependency descriptor collides with str0m's
            // default video-orientation slot.
            .set_extension(
                13,
                Extension::with_serializer(pulsebeam_core::dd::URI, pulsebeam_core::dd::Serializer),
            )
            .set_extension(14, Extension::VideoOrientation);
        config.codec_config().enable_h264(true);
        let mut rtc = config.build(std::time::Instant::now());

        let mut change = rtc.sdp_api();
        change.add_media(MediaKind::Video, direction, None, None, None);
        change.apply().unwrap().0
    }

    fn answer_sdp() -> String {
        let mut negotiator = Negotiator::new(Vec::new());
        let (_, answer, _) = negotiator
            .create_answer(chrome_like_offer(Direction::SendOnly), IceCreds::new())
            .unwrap();
        answer.to_sdp_string()
    }

    #[test]
    fn answers_invert_strict_unidirectional_offers() {
        for (offer_direction, answer_direction) in [
            (Direction::SendOnly, "a=recvonly"),
            (Direction::RecvOnly, "a=sendonly"),
        ] {
            let mut negotiator = Negotiator::new(Vec::new());
            let (_, answer, _) = negotiator
                .create_answer(chrome_like_offer(offer_direction), IceCreds::new())
                .unwrap();
            assert!(answer.to_sdp_string().contains(answer_direction));
        }
    }

    #[test]
    fn resources_preserve_media_positions_across_data_sections() {
        let mut rtc = RtcConfig::new().build(std::time::Instant::now());
        let mut change = rtc.sdp_api();
        change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
        change.add_channel("signal".to_owned());
        change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let offer = change.apply().unwrap().0;
        let mut negotiator = Negotiator::new(Vec::new());
        let (_, _, resources) = negotiator.create_answer(offer, IceCreds::new()).unwrap();

        assert_eq!(
            resources
                .as_slice()
                .iter()
                .map(|resource| (resource.media_index, resource.kind, resource.direction))
                .collect::<Vec<_>>(),
            vec![
                (0, MediaKind::Audio, Direction::RecvOnly),
                (2, MediaKind::Video, Direction::SendOnly),
            ]
        );
    }

    #[test]
    fn port_zero_rtp_sections_do_not_fail_direction_validation() {
        let mut rtc = RtcConfig::new().build(std::time::Instant::now());
        let mut change = rtc.sdp_api();
        change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let offer = change
            .apply()
            .unwrap()
            .0
            .to_sdp_string()
            .replacen("m=video 9", "m=video 0", 1);
        let offer = SdpOffer::from_sdp_string(&offer).unwrap();
        let mut negotiator = Negotiator::new(Vec::new());
        assert!(negotiator.create_answer(offer, IceCreds::new()).is_ok());
    }

    #[test]
    fn answer_negotiates_the_dependency_descriptor() {
        assert!(
            answer_sdp().contains(pulsebeam_core::dd::URI),
            "answer did not carry the dependency descriptor extmap"
        );
    }

    #[test]
    fn dependency_descriptor_does_not_evict_video_orientation() {
        let sdp = answer_sdp();
        let dd_line = sdp
            .lines()
            .find(|l| l.contains(pulsebeam_core::dd::URI))
            .expect("dependency descriptor extmap");
        let cvo_line = sdp
            .lines()
            .find(|l| l.contains("urn:3gpp:video-orientation"))
            .expect("video orientation extmap");

        assert_ne!(dd_line, cvo_line);
        for line in [dd_line, cvo_line] {
            let id: u8 = line
                .trim_start_matches("a=extmap:")
                .split([' ', '/'])
                .next()
                .unwrap()
                .parse()
                .unwrap();
            assert!(id <= 14, "{line} exceeds the one-byte extension form");
        }
    }
}

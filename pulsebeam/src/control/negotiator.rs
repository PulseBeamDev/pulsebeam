use std::time::Duration;

use pulsebeam_proto::rtp_extensions;
use str0m::{
    Candidate, IceCreds, Rtc, RtcConfig, RtcError,
    change::{SdpAnswer, SdpOffer},
    format::{Codec, FormatParams},
    media::{Direction, Frequency, MediaKind, Pt},
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

impl From<MediaType> for MediaKind {
    fn from(value: MediaType) -> Self {
        match value {
            MediaType::Video => MediaKind::Video,
            MediaType::Audio => MediaKind::Audio,
            typ => panic!("unexpected media type: {}", typ),
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
    ) -> Result<(Rtc, SdpAnswer), NegotiatorError> {
        const PT_OPUS: Pt = Pt::new_with_value(111);

        tracing::debug!("{offer}");
        let mut rtc_config = RtcConfig::new()
            .clear_codecs()
            .set_rtp_mode(true)
            .set_extension(
                rtp_extensions::ABS_CAPTURE_TIME,
                Extension::AbsoluteCaptureTime,
            )
            // .set_stats_interval(Some(Duration::from_millis(200)))
            // TODO: enable bwe
            .enable_bwe(Some(str0m::bwe::Bitrate::kbps(500)))
            // Uncomment this to see statistics
            // .set_stats_interval(Some(Duration::from_secs(1)))
            .set_ice_lite(true);
        rtc_config.set_initial_stun_rto(Duration::from_millis(200));
        rtc_config.set_max_stun_rto(Duration::from_millis(1500));
        rtc_config.set_max_stun_retransmits(5);
        let codec_config = rtc_config.codec_config();
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
        // codec_config.enable_vp8(true);
        // h264 as the lowest common denominator due to small clients like
        // embedded devices, smartphones, OBS only supports H264.
        // Baseline profile to ensure compatibility with all platforms.

        // Level 3.1 to 4.1. This is mainly to support clients that don't handle
        // level-asymmetry-allowed=true properly.
        // let baseline_levels = [0x1f, 0x20, 0x28, 0x29];
        // let baseline_levels = [0x34]; // 5.2 level matching OpenH264
        // let mut pt = 96; // start around 96–127 range for dynamic types
        //
        // for level in &baseline_levels {
        //     // // Baseline
        //     // codec_config.add_h264(
        //     //     pt.into(),
        //     //     Some((pt + 1).into()), // RTX PT
        //     //     true,
        //     //     0x420000 | level,
        //     // );
        //     // pt += 2;
        //     //
        //     // Constrained Baseline
        //     codec_config.add_h264(
        //         pt.into(),
        //         Some((pt + 1).into()), // RTX PT
        //         true,
        //         0x42e000 | level,
        //     );
        //     pt += 2;
        // }
        // codec_config.enable_h264(true);

        // TODO: OBS only supports Baseline level 3.1
        // // ESP32-P4 supports up to 1080p@30fps
        // // https://components.espressif.com/components/espressif/esp_h264/versions/1.1.3/readme
        // // Baseline Level 4.0, (pt=127, rtx=121)
        // codec_config.add_h264(127.into(), Some(121.into()), true, 0x420028);
        // // Constrained Baseline Level 4.0, (pt=108, rtx=109)
        // codec_config.add_h264(108.into(), Some(109.into()), true, 0x42e028);

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

        tracing::debug!("{answer}");
        Ok((rtc, answer))
    }

    fn enforce_media_lines(answer: &SdpAnswer) -> Result<(), NegotiatorError> {
        let mut video_recv_count = 0;
        let mut video_send_count = 0;
        let mut audio_recv_count = 0;
        let mut audio_send_count = 0;
        let mut data_channel_count = 0;

        for m in &answer.media_lines {
            let kind = m.typ.to_string();
            let dir = m.direction();
            let media_type = kind.as_str().into();

            if (dir == Direction::SendRecv || dir == Direction::Inactive) && kind != "application" {
                return Err(NegotiatorError::DirectionNotSupported(
                    MediaType::Application,
                ));
            }

            match (media_type, dir) {
                (MediaType::Video, Direction::RecvOnly) => {
                    video_recv_count += 1;
                    if video_recv_count > MAX_RECV_VIDEO_SLOTS {
                        return Err(NegotiatorError::SlotsLimit(
                            MediaType::Video,
                            Direction::SendOnly,
                            MAX_RECV_VIDEO_SLOTS,
                        ));
                    }
                }
                (MediaType::Video, Direction::SendOnly) => {
                    video_send_count += 1;
                    if video_send_count > MAX_SEND_VIDEO_SLOTS {
                        return Err(NegotiatorError::SlotsLimit(
                            MediaType::Video,
                            Direction::RecvOnly,
                            MAX_SEND_VIDEO_SLOTS,
                        ));
                    }
                }
                (MediaType::Audio, Direction::RecvOnly) => {
                    audio_recv_count += 1;
                    if audio_recv_count > MAX_RECV_AUDIO_SLOTS {
                        return Err(NegotiatorError::SlotsLimit(
                            MediaType::Audio,
                            Direction::SendOnly,
                            MAX_RECV_AUDIO_SLOTS,
                        ));
                    }
                }
                (MediaType::Audio, Direction::SendOnly) => {
                    audio_send_count += 1;
                    if audio_send_count > MAX_SEND_AUDIO_SLOTS {
                        return Err(NegotiatorError::SlotsLimit(
                            MediaType::Audio,
                            Direction::RecvOnly,
                            MAX_SEND_AUDIO_SLOTS,
                        ));
                    }
                }
                (MediaType::Application, dir) => {
                    data_channel_count += 1;
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

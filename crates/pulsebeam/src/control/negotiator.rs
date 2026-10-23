use crate::control::ufrag::IceCredentials;
use crate::media::{Direction, MediaKind, Mid};
use pulsebeam_rtc::{
    AcceptError, AcceptedConnection, Connection, ConnectionConfig, ConnectionEntropy,
    LocalCandidate, LocalIceCredentials, MediaSectionKind, SessionInfo,
};
use pulsebeam_runtime::net::ReceiveTime;
use pulsebeam_runtime::rand::RngCore;

pub const MAX_RECV_VIDEO_SLOTS: usize = 32;
pub const MAX_RECV_AUDIO_SLOTS: usize = 32;
pub const MAX_SEND_VIDEO_SLOTS: usize = 32;
pub const MAX_SEND_AUDIO_SLOTS: usize = 32;
pub const MAX_DATA_CHANNELS: usize = 1;

#[derive(Debug)]
pub enum MediaType {
    Video,
    Audio,
    Application,
}

impl std::fmt::Display for MediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Application => "application",
        })
    }
}

#[derive(thiserror::Error, Debug)]
pub enum NegotiatorError {
    #[error("invalid SDP offer: {0}")]
    InvalidOffer(String),
    #[error("RTC negotiation failed: {0}")]
    Rtc(AcceptError),
    #[error("RTC initialization failed: {0}")]
    Initialization(AcceptError),
    #[error("{0} {1} slots limit exceeded (max {2})")]
    SlotsLimit(MediaType, Direction, usize),
    #[error("SendRecv or inactive direction is not supported for {0}")]
    DirectionNotSupported(MediaType),
    #[error("negotiated media resources are inconsistent")]
    InvalidResources,
}

impl NegotiatorError {
    fn from_accept(error: AcceptError) -> Self {
        match error {
            AcceptError::InvalidOffer => Self::InvalidOffer(error.to_string()),
            AcceptError::UnsupportedSessionProfile
            | AcceptError::MissingPacketFeedback
            | AcceptError::CapabilityConflict
            | AcceptError::SessionLimitExceeded => Self::Rtc(error),
            _ => Self::Initialization(error),
        }
    }
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
    pub(crate) local_ufrag: String,
}

impl NegotiatedResources {
    #[cfg(test)]
    pub(crate) fn empty_for_test() -> Self {
        Self {
            media: Vec::new(),
            local_ufrag: "test".to_owned(),
        }
    }
    pub fn media(&self, mid: Mid) -> Option<NegotiatedMedia> {
        self.media.iter().find(|media| media.mid == mid).copied()
    }
    pub(crate) fn media_sections(&self) -> &[NegotiatedMedia] {
        &self.media
    }
    #[cfg(test)]
    fn as_slice(&self) -> &[NegotiatedMedia] {
        &self.media
    }
}

pub struct Negotiator {
    candidates: Vec<LocalCandidate>,
}

impl Negotiator {
    pub fn new(candidates: Vec<LocalCandidate>) -> Self {
        Self { candidates }
    }

    pub fn create_answer(
        &mut self,
        offer: String,
        creds: IceCredentials,
        at: ReceiveTime,
    ) -> Result<(AcceptedConnection, String, NegotiatedResources), NegotiatorError> {
        let local_ufrag = creds.ufrag.clone();
        let credentials = LocalIceCredentials::new(creds.ufrag, creds.pass)
            .map_err(NegotiatorError::Initialization)?;
        let mut seed = [0; 32];
        pulsebeam_runtime::rand::os_rng().fill_bytes(&mut seed);
        let accepted = Connection::accept(
            ConnectionConfig {
                local_candidates: self.candidates.clone(),
                local_ice_credentials: Some(credentials),
                limits: pulsebeam_rtc::ConnectionLimits {
                    max_unsignaled_encodings: 128,
                    ..Default::default()
                },
                ..Default::default()
            },
            pulsebeam_rtc::SdpOffer::new(offer),
            at.monotonic.into(),
            at.wall,
            ConnectionEntropy::new(seed),
        )
        .map_err(NegotiatorError::from_accept)?;
        Self::enforce_media_lines(&accepted.session)?;
        let resources = Self::negotiated_resources(&accepted.session, local_ufrag)?;
        let answer = accepted.answer.as_str().to_owned();
        Ok((accepted, answer, resources))
    }

    fn negotiated_resources(
        session: &SessionInfo,
        local_ufrag: String,
    ) -> Result<NegotiatedResources, NegotiatorError> {
        let mut media = Vec::new();
        for section in session
            .offered_media
            .iter()
            .filter(|section| !section.disabled)
        {
            let kind = match section.kind {
                MediaSectionKind::Audio => MediaKind::Audio,
                MediaSectionKind::Video => MediaKind::Video,
                MediaSectionKind::Application => continue,
            };
            let direction = Direction::from(section.direction);
            if !matches!(direction, Direction::SendOnly | Direction::RecvOnly) {
                return Err(NegotiatorError::InvalidResources);
            }
            let mid = Mid::new(&section.mid).map_err(|_| NegotiatorError::InvalidResources)?;
            if session
                .inbound_media
                .iter()
                .filter(|info| info.mid == section.mid)
                .any(|info| {
                    info.rids
                        .iter()
                        .any(|rid| crate::media::Rid::new(rid).is_err())
                })
            {
                return Err(NegotiatorError::InvalidResources);
            }
            if media
                .iter()
                .any(|resource: &NegotiatedMedia| resource.mid == mid)
            {
                return Err(NegotiatorError::InvalidResources);
            }
            let accepted = match direction {
                Direction::RecvOnly => session.inbound_media.iter().any(|value| {
                    value.sender_index == section.media_index
                        && value.mid == section.mid
                        && value.kind == kind
                }),
                Direction::SendOnly => session.senders.iter().any(|value| {
                    value.receiver_index == section.media_index
                        && value.mid == section.mid
                        && value.kind == kind
                }),
                _ => false,
            };
            if !accepted {
                return Err(NegotiatorError::InvalidResources);
            }
            media.push(NegotiatedMedia {
                media_index: section.media_index,
                mid,
                kind,
                direction,
            });
        }
        Ok(NegotiatedResources { media, local_ufrag })
    }

    fn enforce_media_lines(session: &SessionInfo) -> Result<(), NegotiatorError> {
        let (mut video_recv, mut video_send, mut audio_recv, mut audio_send, mut application) =
            (0usize, 0usize, 0usize, 0usize, 0usize);
        for section in session
            .offered_media
            .iter()
            .filter(|section| !section.disabled)
        {
            let dir = Direction::from(section.direction);
            if matches!(dir, Direction::SendRecv | Direction::Inactive)
                && section.kind != MediaSectionKind::Application
            {
                return Err(NegotiatorError::DirectionNotSupported(match section.kind {
                    MediaSectionKind::Audio => MediaType::Audio,
                    _ => MediaType::Video,
                }));
            }
            let (count, limit, kind, offered_direction) = match (section.kind, dir) {
                (MediaSectionKind::Video, Direction::RecvOnly) => (
                    &mut video_recv,
                    MAX_RECV_VIDEO_SLOTS,
                    MediaType::Video,
                    Direction::SendOnly,
                ),
                (MediaSectionKind::Video, Direction::SendOnly) => (
                    &mut video_send,
                    MAX_SEND_VIDEO_SLOTS,
                    MediaType::Video,
                    Direction::RecvOnly,
                ),
                (MediaSectionKind::Audio, Direction::RecvOnly) => (
                    &mut audio_recv,
                    MAX_RECV_AUDIO_SLOTS,
                    MediaType::Audio,
                    Direction::SendOnly,
                ),
                (MediaSectionKind::Audio, Direction::SendOnly) => (
                    &mut audio_send,
                    MAX_SEND_AUDIO_SLOTS,
                    MediaType::Audio,
                    Direction::RecvOnly,
                ),
                (MediaSectionKind::Application, _) => (
                    &mut application,
                    MAX_DATA_CHANNELS,
                    MediaType::Application,
                    dir,
                ),
                _ => return Err(NegotiatorError::InvalidResources),
            };
            *count = count.saturating_add(1);
            if *count > limit {
                return Err(NegotiatorError::SlotsLimit(kind, offered_direction, limit));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn application_only_test_connection() -> AcceptedConnection {
    application_only_test_pair().0
}

#[cfg(test)]
pub(crate) fn application_only_test_connection_and_binding()
-> (AcceptedConnection, NegotiatedResources, Vec<u8>) {
    let (accepted, resources, mut peer) = application_only_test_pair();
    let binding = (0..64)
        .find_map(|_| match peer.poll_output().unwrap() {
            str0m::Output::Transmit(tx) => {
                assert_eq!(tx.source, "127.0.0.1:5000".parse().unwrap());
                assert_eq!(tx.destination, "127.0.0.1:6000".parse().unwrap());
                Some(tx.contents.to_vec())
            }
            str0m::Output::Timeout(at) => {
                peer.handle_input(str0m::Input::Timeout(at)).unwrap();
                None
            }
            _ => None,
        })
        .expect("native peer must emit its ICE binding request");
    (accepted, resources, binding)
}

#[cfg(test)]
fn application_only_test_pair() -> (AcceptedConnection, NegotiatedResources, str0m::Rtc) {
    let mut peer = str0m::RtcConfig::new().build(std::time::Instant::now());
    peer.add_local_candidate(
        str0m::Candidate::builder()
            .udp()
            .host("127.0.0.1:5000".parse().unwrap())
            .build()
            .unwrap(),
    );
    let mut changes = peer.sdp_api();
    changes.add_channel("test".to_owned());
    let (offer, pending) = changes.apply().unwrap();
    let local = "127.0.0.1:6000".parse().unwrap();
    let (accepted, answer, resources) = Negotiator::new(vec![
        LocalCandidate::Udp(local),
        LocalCandidate::TcpPassive(local),
    ])
    .create_answer(
        offer.to_sdp_string(),
        crate::control::ufrag::test_credentials(),
        ReceiveTime::now(),
    )
    .unwrap();
    peer.sdp_api()
        .accept_answer(
            pending,
            str0m::change::SdpAnswer::from_sdp_string(&answer).unwrap(),
        )
        .unwrap();
    (accepted, resources, peer)
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;
    use crate::control::ufrag::test_credentials;
    use str0m::{
        RtcConfig,
        change::SdpOffer,
        media::{Direction, MediaKind},
        rtp::Extension,
    };

    fn with_candidate(mut rtc: str0m::Rtc) -> str0m::Rtc {
        rtc.add_local_candidate(
            str0m::Candidate::builder()
                .udp()
                .host("127.0.0.1:5000".parse().unwrap())
                .build()
                .unwrap(),
        );
        rtc
    }

    fn negotiator() -> Negotiator {
        Negotiator::new(vec![LocalCandidate::Udp("127.0.0.1:6000".parse().unwrap())])
    }

    fn chrome_like_offer(direction: Direction) -> String {
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
        let mut rtc = with_candidate(config.build(std::time::Instant::now()));

        let mut change = rtc.sdp_api();
        change.add_media(MediaKind::Video, direction, None, None, None);
        change.apply().unwrap().0.to_sdp_string()
    }

    fn answer_sdp() -> String {
        let mut negotiator = negotiator();
        let (_, answer, _) = negotiator
            .create_answer(
                chrome_like_offer(Direction::SendOnly),
                test_credentials(),
                ReceiveTime::now(),
            )
            .unwrap();
        answer
    }

    #[test]
    fn answers_invert_strict_unidirectional_offers() {
        for (offer_direction, answer_direction) in [
            (Direction::SendOnly, "a=recvonly"),
            (Direction::RecvOnly, "a=sendonly"),
        ] {
            let mut negotiator = negotiator();
            let (_, answer, _) = negotiator
                .create_answer(
                    chrome_like_offer(offer_direction),
                    test_credentials(),
                    ReceiveTime::now(),
                )
                .unwrap();
            assert!(answer.contains(answer_direction));
        }
    }

    #[test]
    fn resources_preserve_media_positions_across_data_sections() {
        let mut rtc = with_candidate(RtcConfig::new().build(std::time::Instant::now()));
        let mut change = rtc.sdp_api();
        change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
        change.add_channel("signal".to_owned());
        change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let offer = change.apply().unwrap().0.to_sdp_string();
        let mut negotiator = negotiator();
        let (_, _, resources) = negotiator
            .create_answer(offer, test_credentials(), ReceiveTime::now())
            .unwrap();

        assert_eq!(
            resources
                .as_slice()
                .iter()
                .map(|resource| (resource.media_index, resource.kind, resource.direction))
                .collect::<Vec<_>>(),
            vec![
                (
                    0,
                    crate::media::MediaKind::Audio,
                    crate::media::Direction::RecvOnly
                ),
                (
                    2,
                    crate::media::MediaKind::Video,
                    crate::media::Direction::SendOnly
                ),
            ]
        );
    }

    fn with_unique_media_identities(offer: SdpOffer) -> String {
        // Pending MID collisions also reuse the first pending media's SSRCs in str0m.
        fn remap_ssrc(
            value: &str,
            section: &mut std::collections::HashMap<String, u32>,
            next: &mut u32,
        ) -> u32 {
            *section.entry(value.to_owned()).or_insert_with(|| {
                let assigned = *next;
                *next = next.saturating_add(1);
                assigned
            })
        }
        let sdp = offer.to_sdp_string();
        let bundle = sdp
            .lines()
            .filter(|line| line.starts_with("a=mid:"))
            .enumerate()
            .map(|(index, _)| format!("m{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut index = 0;
        let mut ssrcs = std::collections::HashMap::new();
        let mut next_ssrc = 1;
        let lines = sdp
            .lines()
            .map(|line| {
                if line.starts_with("m=") {
                    ssrcs.clear();
                }
                if line.starts_with("a=group:BUNDLE ") {
                    format!("a=group:BUNDLE {bundle}")
                } else if line.starts_with("a=mid:") {
                    let mid = format!("a=mid:m{index}");
                    index += 1;
                    mid
                } else if let Some(group) = line.strip_prefix("a=ssrc-group:") {
                    let (semantics, sources) = group.split_once(' ').expect("native SSRC group");
                    let sources = sources
                        .split_whitespace()
                        .map(|value| remap_ssrc(value, &mut ssrcs, &mut next_ssrc).to_string())
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!("a=ssrc-group:{semantics} {sources}")
                } else if let Some(attribute) = line.strip_prefix("a=ssrc:") {
                    let (value, attribute) =
                        attribute.split_once(' ').expect("native SSRC attribute");
                    let ssrc = remap_ssrc(value, &mut ssrcs, &mut next_ssrc);
                    format!("a=ssrc:{ssrc} {attribute}")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>();
        lines.join("\r\n") + "\r\n"
    }

    #[test]
    fn negotiated_capacity_is_independent_per_kind_and_direction() {
        let mut rtc = with_candidate(RtcConfig::new().build(std::time::Instant::now()));
        let mut change = rtc.sdp_api();
        for (kind, direction) in [
            (MediaKind::Audio, Direction::SendOnly),
            (MediaKind::Video, Direction::SendOnly),
            (MediaKind::Audio, Direction::RecvOnly),
            (MediaKind::Video, Direction::RecvOnly),
        ] {
            for _ in 0..32 {
                change.add_media(kind, direction, None, None, None);
            }
        }
        change.add_channel("signal".to_owned());
        let offer = with_unique_media_identities(change.apply().unwrap().0);
        let mut negotiator = negotiator();
        negotiator
            .create_answer(offer, test_credentials(), ReceiveTime::now())
            .unwrap();
    }

    #[test]
    fn rejects_thirty_third_video_receiver() {
        let mut rtc = with_candidate(RtcConfig::new().build(std::time::Instant::now()));
        let mut change = rtc.sdp_api();
        for _ in 0..33 {
            change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        }
        let offer = with_unique_media_identities(change.apply().unwrap().0);
        let mut negotiator = negotiator();
        assert!(matches!(
            negotiator.create_answer(offer, test_credentials(), ReceiveTime::now()),
            Err(NegotiatorError::SlotsLimit(
                MediaType::Video,
                crate::media::Direction::RecvOnly,
                32
            ))
        ));
    }

    #[test]
    fn port_zero_rtp_sections_do_not_fail_direction_validation() {
        let mut rtc = with_candidate(RtcConfig::new().build(std::time::Instant::now()));
        let mut change = rtc.sdp_api();
        change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let offer = change
            .apply()
            .unwrap()
            .0
            .to_sdp_string()
            .replacen("m=video 9", "m=video 0", 1);
        let mut negotiator = negotiator();
        assert!(
            negotiator
                .create_answer(offer, test_credentials(), ReceiveTime::now())
                .is_ok()
        );
    }

    #[test]
    fn only_subscribers_require_packet_feedback_including_audio_only() {
        for direction in [Direction::SendOnly, Direction::RecvOnly] {
            let mut peer = with_candidate(RtcConfig::new().build(std::time::Instant::now()));
            let mut change = peer.sdp_api();
            change.add_media(MediaKind::Audio, direction, None, None, None);
            let offer = change
                .apply()
                .unwrap()
                .0
                .to_sdp_string()
                .lines()
                .filter(|line| {
                    !line.starts_with("a=rtcp-fb:") && !line.contains("transport-wide-cc")
                })
                .collect::<Vec<_>>()
                .join("\r\n")
                + "\r\n";
            let result = negotiator().create_answer(offer, test_credentials(), ReceiveTime::now());
            match direction {
                Direction::SendOnly => assert!(result.is_ok()),
                Direction::RecvOnly => assert!(matches!(
                    result,
                    Err(NegotiatorError::Rtc(AcceptError::MissingPacketFeedback))
                )),
                _ => unreachable!(),
            }
        }
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

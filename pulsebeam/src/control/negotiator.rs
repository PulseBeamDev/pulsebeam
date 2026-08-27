pub const MAX_RECV_VIDEO_SLOTS: usize = 2;
pub const MAX_RECV_AUDIO_SLOTS: usize = 2;
pub const MAX_SEND_VIDEO_SLOTS: usize = 7;
pub const MAX_SEND_AUDIO_SLOTS: usize = 3;
pub const MAX_DATA_CHANNELS: usize = 1;

pub struct DirectNegotiation {
    pub answer: String,
    pub session: pulsebeam_rtc::NegotiatedSession,
    pub local: pulsebeam_rtc::LocalTransport,
}

#[derive(Debug)]
pub enum MediaType {
    Video,
    Audio,
    Application,
    Unknown,
}

impl std::fmt::Display for MediaType {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Application => "application",
            Self::Unknown => "unknown",
        })
    }
}

#[derive(thiserror::Error, Debug)]
pub enum NegotiatorError {
    #[error("{0} {1:?} slots limit exceeded (max {2})")]
    SlotsLimit(MediaType, pulsebeam_rtc::MediaDirection, usize),
    #[error("bidirectional media is not supported for {0}")]
    DirectionNotSupported(MediaType),
    #[error("SDP negotiation error: {0}")]
    Negotiation(#[from] pulsebeam_rtc::NegotiationError),
    #[error("local transport error: {0}")]
    LocalTransport(#[from] pulsebeam_rtc::LiveConnectionError),
}

pub struct Negotiator {
    candidates: Box<[pulsebeam_rtc::IceCandidate]>,
}

impl Negotiator {
    pub fn new(candidates: Box<[pulsebeam_rtc::IceCandidate]>) -> Self {
        Self { candidates }
    }

    pub fn create_answer(
        &self,
        offer: &str,
        connection_id: pulsebeam_rtc::ConnectionId,
        ice: pulsebeam_rtc::IceCredentials,
    ) -> Result<DirectNegotiation, NegotiatorError> {
        let local = pulsebeam_rtc::LocalTransport::generate(ice.clone())?;
        let server = pulsebeam_rtc::ServerTransport::new(
            connection_id.get(),
            ice,
            local.fingerprint().clone(),
            self.candidates.clone(),
        );
        let result = pulsebeam_rtc::negotiate(offer, &server)?;
        enforce_media_limits(result.session())?;
        Ok(DirectNegotiation {
            answer: result.answer().as_str().to_owned(),
            session: result.session().clone(),
            local,
        })
    }
}

fn enforce_media_limits(session: &pulsebeam_rtc::NegotiatedSession) -> Result<(), NegotiatorError> {
    let mut video_recv = 0usize;
    let mut video_send = 0usize;
    let mut audio_recv = 0usize;
    let mut audio_send = 0usize;
    let mut applications = 0usize;
    for section in session.media_sections() {
        match (section.kind(), section.direction()) {
            (pulsebeam_rtc::MediaKind::Video, pulsebeam_rtc::MediaDirection::ReceiveOnly) => {
                video_recv = video_recv.saturating_add(1);
                if video_recv > MAX_RECV_VIDEO_SLOTS {
                    return Err(NegotiatorError::SlotsLimit(
                        MediaType::Video,
                        section.direction(),
                        MAX_RECV_VIDEO_SLOTS,
                    ));
                }
            }
            (pulsebeam_rtc::MediaKind::Video, pulsebeam_rtc::MediaDirection::SendOnly) => {
                video_send = video_send.saturating_add(1);
                if video_send > MAX_SEND_VIDEO_SLOTS {
                    return Err(NegotiatorError::SlotsLimit(
                        MediaType::Video,
                        section.direction(),
                        MAX_SEND_VIDEO_SLOTS,
                    ));
                }
            }
            (pulsebeam_rtc::MediaKind::Audio, pulsebeam_rtc::MediaDirection::ReceiveOnly) => {
                audio_recv = audio_recv.saturating_add(1);
                if audio_recv > MAX_RECV_AUDIO_SLOTS {
                    return Err(NegotiatorError::SlotsLimit(
                        MediaType::Audio,
                        section.direction(),
                        MAX_RECV_AUDIO_SLOTS,
                    ));
                }
            }
            (pulsebeam_rtc::MediaKind::Audio, pulsebeam_rtc::MediaDirection::SendOnly) => {
                audio_send = audio_send.saturating_add(1);
                if audio_send > MAX_SEND_AUDIO_SLOTS {
                    return Err(NegotiatorError::SlotsLimit(
                        MediaType::Audio,
                        section.direction(),
                        MAX_SEND_AUDIO_SLOTS,
                    ));
                }
            }
            (pulsebeam_rtc::MediaKind::Application, _) => {
                applications = applications.saturating_add(1);
                if applications > MAX_DATA_CHANNELS {
                    return Err(NegotiatorError::SlotsLimit(
                        MediaType::Application,
                        section.direction(),
                        MAX_DATA_CHANNELS,
                    ));
                }
            }
            (_, pulsebeam_rtc::MediaDirection::Bidirectional) => {
                return Err(NegotiatorError::DirectionNotSupported(MediaType::Unknown));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_preserves_supported_extensions() {
        let offer = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\na=ice-ufrag:remote\r\na=ice-pwd:remote-password\r\na=fingerprint:sha-256 01:02:03:04\r\na=setup:actpass\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=sendonly\r\na=rtcp-mux\r\na=rtpmap:96 H264/90000\r\na=extmap:13 urn:ietf:params:rtp-hdrext:dependency-descriptor\r\na=extmap:14 urn:3gpp:video-orientation\r\n";
        let result = Negotiator::new(Box::new([]))
            .create_answer(
                offer,
                pulsebeam_rtc::ConnectionId::new(1),
                pulsebeam_rtc::IceCredentials::new("local".to_owned(), "local-password".to_owned())
                    .expect("credentials"),
            )
            .expect("negotiation");
        assert!(result.answer.contains("dependency-descriptor"));
        assert!(result.answer.contains("video-orientation"));
    }
}

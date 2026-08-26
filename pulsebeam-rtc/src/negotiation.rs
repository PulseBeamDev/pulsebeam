use std::{collections::HashSet, num::NonZeroU32};

use str0m::{
    Candidate,
    crypto::Fingerprint,
    rtp_::Direction,
    sdp::{MediaAttribute, MediaType, Sdp, SessionAttribute, Setup},
};

use crate::{
    Codec, DataChannelParameters, DtlsFingerprint, HeaderExtension, IceCandidate, IceCredentials,
    MediaDirection, MediaKind, MediaSectionId, NegotiatedMediaSection, NegotiatedSession,
    SdpAnswer,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerTransport {
    session_id: u64,
    ice: IceCredentials,
    fingerprint: DtlsFingerprint,
    candidates: Box<[IceCandidate]>,
}

impl ServerTransport {
    pub fn new(
        session_id: u64,
        ice: IceCredentials,
        fingerprint: DtlsFingerprint,
        candidates: Box<[IceCandidate]>,
    ) -> Self {
        Self {
            session_id,
            ice,
            fingerprint,
            candidates,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NegotiationError {
    #[error("invalid SDP: {0}")]
    Sdp(String),
    #[error("missing remote ICE credentials")]
    MissingIceCredentials,
    #[error("missing remote DTLS fingerprint")]
    MissingFingerprint,
    #[error("missing remote DTLS setup role")]
    MissingSetup,
    #[error("invalid local candidate: {0}")]
    Candidate(String),
    #[error("duplicate media section identifier: {0}")]
    DuplicateMid(String),
    #[error("unsupported media direction for {0}")]
    UnsupportedDirection(String),
    #[error("invalid header extension identifier {id} on {mid}")]
    InvalidExtensionId { mid: String, id: u8 },
    #[error("duplicate header extension identifier {id} on {mid}")]
    DuplicateExtensionId { mid: String, id: u8 },
    #[error("media section count exceeds u16")]
    TooManyMediaSections,
    #[error("application section {0} is missing an SCTP port")]
    MissingSctpPort(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegotiationResult {
    answer: SdpAnswer,
    session: NegotiatedSession,
}

impl NegotiationResult {
    pub fn answer(&self) -> &SdpAnswer {
        &self.answer
    }

    pub fn session(&self) -> &NegotiatedSession {
        &self.session
    }
}

pub fn negotiate(
    offer: &str,
    server: &ServerTransport,
) -> Result<NegotiationResult, NegotiationError> {
    let mut answer = Sdp::parse(offer).map_err(|error| NegotiationError::Sdp(error.to_string()))?;
    answer
        .assert_consistency()
        .map_err(|error| NegotiationError::Sdp(error.to_string()))?;

    let remote_ice = answer
        .ice_creds()
        .map(|credentials| IceCredentials::new(credentials.ufrag, credentials.pass))
        .flatten()
        .ok_or(NegotiationError::MissingIceCredentials)?;
    let remote_fingerprint = answer
        .fingerprint()
        .and_then(|fingerprint| {
            DtlsFingerprint::new(fingerprint.hash_func, fingerprint.bytes.into_boxed_slice())
        })
        .ok_or(NegotiationError::MissingFingerprint)?;
    let remote_setup = answer.setup().ok_or(NegotiationError::MissingSetup)?;
    let setup = Setup::Passive
        .compare_to_remote(remote_setup)
        .ok_or_else(|| NegotiationError::Sdp("incompatible DTLS setup roles".to_owned()))?;

    let local_candidates = parse_candidates(&server.candidates)?;
    let mut mids = HashSet::with_capacity(answer.media_lines.len());
    let mut sections = Vec::with_capacity(answer.media_lines.len());

    for (index, line) in answer.media_lines.iter_mut().enumerate() {
        if let Some(error) = line.check_consistent() {
            return Err(NegotiationError::Sdp(error));
        }

        let mid = line.mid().to_string();
        if !mids.insert(mid.clone()) {
            return Err(NegotiationError::DuplicateMid(mid));
        }

        let id = u16::try_from(index).map_err(|_| NegotiationError::TooManyMediaSections)?;
        let kind = media_kind(&line.typ)?;
        let direction = negotiated_direction(kind, line.direction(), &mid)?;
        let codecs = codecs(line);
        let header_extensions = header_extensions(line, &mid)?;
        let data_channel = data_channel_parameters(line, kind, &mid)?;

        filter_answer_attributes(line, direction);
        sections.push(NegotiatedMediaSection::new(
            MediaSectionId::new(id),
            mid,
            kind,
            direction,
            codecs.into_boxed_slice(),
            header_extensions.into_boxed_slice(),
            data_channel,
        ));
    }

    let bundle = answer
        .session
        .attrs
        .iter()
        .find(
            |attribute| matches!(attribute, SessionAttribute::Group { typ, .. } if typ == "BUNDLE"),
        )
        .cloned();
    answer.session.id = server.session_id.into();
    answer.session.attrs = answer_attributes(server, setup, local_candidates, bundle);

    let session = NegotiatedSession::new(
        server.ice.clone(),
        server.fingerprint.clone(),
        server.candidates.clone(),
        remote_ice,
        remote_fingerprint,
        sections.into_boxed_slice(),
    );

    Ok(NegotiationResult {
        answer: SdpAnswer::new(answer.to_string()),
        session,
    })
}

fn parse_candidates(candidates: &[IceCandidate]) -> Result<Vec<Candidate>, NegotiationError> {
    candidates
        .iter()
        .map(|candidate| {
            Candidate::from_sdp_string(candidate.as_sdp())
                .map_err(|error| NegotiationError::Candidate(error.to_string()))
        })
        .collect()
}

fn media_kind(media_type: &MediaType) -> Result<MediaKind, NegotiationError> {
    match media_type {
        MediaType::Audio => Ok(MediaKind::Audio),
        MediaType::Video => Ok(MediaKind::Video),
        MediaType::Application => Ok(MediaKind::Application),
        MediaType::Unknown(value) => Err(NegotiationError::Sdp(format!(
            "unsupported media type {value}"
        ))),
    }
}

fn negotiated_direction(
    kind: MediaKind,
    remote: Direction,
    mid: &str,
) -> Result<MediaDirection, NegotiationError> {
    let direction = match (kind, remote) {
        (_, Direction::Inactive) => MediaDirection::Inactive,
        (MediaKind::Application, _) => MediaDirection::Bidirectional,
        (_, Direction::SendOnly) => MediaDirection::ReceiveOnly,
        (_, Direction::RecvOnly) => MediaDirection::SendOnly,
        (_, Direction::SendRecv) => {
            return Err(NegotiationError::UnsupportedDirection(mid.to_owned()));
        }
    };

    Ok(direction)
}

fn codecs(line: &str0m::sdp::MediaLine) -> Vec<Codec> {
    line.rtp_params()
        .into_iter()
        .map(|params| {
            let clock_rate = u32::from(NonZeroU32::from(params.spec.clock_rate));
            Codec::new(
                *params.pt,
                params.spec.codec.to_string(),
                clock_rate,
                params.spec.channels,
                params.resend.map(|payload_type| *payload_type),
                params.fb_transport_cc,
                params.fb_nack,
                params.fb_pli,
                params.fb_fir,
            )
        })
        .collect()
}

fn header_extensions(
    line: &str0m::sdp::MediaLine,
    mid: &str,
) -> Result<Vec<HeaderExtension>, NegotiationError> {
    let mut ids = HashSet::new();
    let mut extensions = Vec::new();

    for attribute in &line.attrs {
        if let MediaAttribute::ExtMap { id, ext } = attribute {
            if !matches!(id, 1..=14) {
                return Err(NegotiationError::InvalidExtensionId {
                    mid: mid.to_owned(),
                    id: *id,
                });
            }
            if !ids.insert(*id) {
                return Err(NegotiationError::DuplicateExtensionId {
                    mid: mid.to_owned(),
                    id: *id,
                });
            }
            extensions.push(HeaderExtension::new(*id, ext.as_uri().to_owned()));
        }
    }

    Ok(extensions)
}

fn data_channel_parameters(
    line: &str0m::sdp::MediaLine,
    kind: MediaKind,
    mid: &str,
) -> Result<Option<DataChannelParameters>, NegotiationError> {
    if kind != MediaKind::Application {
        return Ok(None);
    }

    let sctp_port = line.attrs.iter().find_map(|attribute| {
        if let MediaAttribute::SctpPort(port) = attribute {
            Some(*port)
        } else {
            None
        }
    });
    let max_message_size = line.max_message_size();

    sctp_port
        .map(|port| Some(DataChannelParameters::new(port, max_message_size)))
        .ok_or_else(|| NegotiationError::MissingSctpPort(mid.to_owned()))
}

fn filter_answer_attributes(line: &mut str0m::sdp::MediaLine, direction: MediaDirection) {
    let rtcp_mux = line.attrs.iter().any(|attribute| {
        matches!(
            attribute,
            MediaAttribute::RtcpMux | MediaAttribute::RtcpMuxOnly
        )
    });
    line.attrs.retain(|attribute| {
        matches!(
            attribute,
            MediaAttribute::Mid(_)
                | MediaAttribute::SctpPort(_)
                | MediaAttribute::MaxMessageSize(_)
                | MediaAttribute::SctpInit(_)
                | MediaAttribute::ExtMap { .. }
                | MediaAttribute::RtcpMux
                | MediaAttribute::RtcpRsize
                | MediaAttribute::RtpMap { .. }
                | MediaAttribute::RtcpFb { .. }
                | MediaAttribute::Fmtp { .. }
        )
    });
    if line
        .attrs
        .iter()
        .any(|attribute| matches!(attribute, MediaAttribute::RtcpMux))
    {
        debug_assert!(
            line.attrs
                .iter()
                .all(|attribute| !matches!(attribute, MediaAttribute::RtcpMuxOnly))
        );
    }
    if rtcp_mux
        && !line
            .attrs
            .iter()
            .any(|attribute| matches!(attribute, MediaAttribute::RtcpMux))
    {
        line.attrs.push(MediaAttribute::RtcpMux);
    }
    line.attrs.push(match direction {
        MediaDirection::SendOnly => MediaAttribute::SendOnly,
        MediaDirection::ReceiveOnly => MediaAttribute::RecvOnly,
        MediaDirection::Inactive => MediaAttribute::Inactive,
        MediaDirection::Bidirectional => MediaAttribute::SendRecv,
    });
}

fn answer_attributes(
    server: &ServerTransport,
    setup: Setup,
    candidates: Vec<Candidate>,
    bundle: Option<SessionAttribute>,
) -> Vec<SessionAttribute> {
    let mut attributes = Vec::with_capacity(candidates.len().saturating_add(7));
    if let Some(group) = bundle {
        attributes.push(group);
    }
    attributes.push(SessionAttribute::IceLite);
    attributes.push(SessionAttribute::IceUfrag(server.ice.ufrag().to_owned()));
    attributes.push(SessionAttribute::IcePwd(server.ice.password().to_owned()));
    attributes.push(SessionAttribute::Fingerprint(Fingerprint {
        hash_func: server.fingerprint.algorithm().to_owned(),
        bytes: server.fingerprint.value().to_vec(),
    }));
    attributes.push(SessionAttribute::Setup(setup));
    attributes.extend(candidates.into_iter().map(SessionAttribute::Candidate));
    attributes.push(SessionAttribute::EndOfCandidates);
    attributes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> ServerTransport {
        let ice = IceCredentials::new("localufrag".to_owned(), "localpassword".to_owned())
            .expect("valid local ICE credentials");
        let fingerprint = DtlsFingerprint::new("sha-256".to_owned(), Box::new([9; 32]))
            .expect("valid local fingerprint");
        let candidate =
            IceCandidate::new("candidate:1 1 UDP 2130706431 127.0.0.1 9000 typ host".to_owned())
                .expect("valid candidate");
        ServerTransport::new(7, ice, fingerprint, Box::new([candidate]))
    }

    fn offer(direction: &str) -> String {
        format!(
            "v=0\r\n\
             o=- 1 2 IN IP4 127.0.0.1\r\n\
             s=-\r\n\
             t=0 0\r\n\
             a=group:BUNDLE 0\r\n\
             a=ice-ufrag:remoteufrag\r\n\
             a=ice-pwd:remotepassword\r\n\
             a=fingerprint:sha-256 01:02:03:04\r\n\
             a=setup:actpass\r\n\
             m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=mid:0\r\n\
             a={direction}\r\n\
             a=rtcp-mux\r\n\
             a=rtpmap:111 opus/48000/2\r\n\
             a=rtcp-fb:111 transport-cc\r\n\
             a=extmap:3 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n"
        )
    }

    #[test]
    fn negotiated_session_contains_pulsebeam_owned_facts() {
        let result = negotiate(&offer("sendonly"), &server()).expect("accepted offer");
        let session = result.session();
        let media = session.media_sections();

        assert_eq!(session.local_ice().ufrag(), "localufrag");
        assert_eq!(session.remote_ice().ufrag(), "remoteufrag");
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].kind(), MediaKind::Audio);
        assert_eq!(media[0].direction(), MediaDirection::ReceiveOnly);
        assert_eq!(media[0].codecs()[0].name(), "opus");
        assert_eq!(media[0].header_extensions()[0].id(), 3);
        assert!(result.answer().as_str().contains("a=ice-ufrag:localufrag"));
        assert!(result.answer().as_str().contains("a=recvonly"));
        assert!(result.answer().as_str().contains("a=group:BUNDLE 0"));
        assert!(result.answer().as_str().contains("a=rtcp-mux"));
    }

    #[test]
    fn negotiated_session_rejects_bidirectional_media() {
        let error = negotiate(&offer("sendrecv"), &server()).expect_err("bidirectional media");

        assert_eq!(
            error,
            NegotiationError::UnsupportedDirection("0".to_owned())
        );
    }
}

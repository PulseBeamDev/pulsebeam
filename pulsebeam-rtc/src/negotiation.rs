use std::{collections::HashSet, num::NonZeroU32};

use str0m::{
    Candidate,
    rtp_::Direction,
    sdp::{MediaAttribute, MediaLine, MediaType, Sdp, SessionAttribute, Setup},
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
    #[error("invalid remote ICE candidate: {0}")]
    RemoteCandidate(String),
    #[error("the offer does not contain a BUNDLE group")]
    MissingBundle,
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
    let parsed = Sdp::parse(offer).map_err(|error| NegotiationError::Sdp(error.to_string()))?;
    parsed
        .assert_consistency()
        .map_err(|error| NegotiationError::Sdp(error.to_string()))?;

    let remote_ice = parsed
        .ice_creds()
        .and_then(|credentials| IceCredentials::new(credentials.ufrag, credentials.pass))
        .ok_or(NegotiationError::MissingIceCredentials)?;
    let remote_fingerprint = parsed
        .fingerprint()
        .and_then(|fingerprint| {
            DtlsFingerprint::new(fingerprint.hash_func, fingerprint.bytes.into_boxed_slice())
        })
        .ok_or(NegotiationError::MissingFingerprint)?;
    let remote_candidates = remote_candidates(offer)?;
    let remote_setup = parsed.setup().ok_or(NegotiationError::MissingSetup)?;
    let setup = Setup::Passive
        .compare_to_remote(remote_setup)
        .ok_or_else(|| NegotiationError::Sdp("incompatible DTLS setup roles".to_owned()))?;

    let local_candidates = parse_local_candidates(&server.candidates)?;
    let _ = bundle(&parsed)?;
    let bundle_tag = answer_bundle_tag(&parsed)?;
    let mut mids = HashSet::with_capacity(parsed.media_lines.len());
    let mut sections = Vec::with_capacity(parsed.media_lines.len());
    let mut answer_media = Vec::with_capacity(parsed.media_lines.len());

    for (index, line) in parsed.media_lines.iter().enumerate() {
        if let Some(error) = line.check_consistent() {
            return Err(NegotiationError::Sdp(error));
        }

        let mid = line.mid().to_string();
        if !mids.insert(mid.clone()) {
            return Err(NegotiationError::DuplicateMid(mid));
        }

        let id = u16::try_from(index).map_err(|_| NegotiationError::TooManyMediaSections)?;
        let kind = media_kind(&line.typ)?;
        let codecs = codecs(kind, line);
        let media_supported = kind == MediaKind::Application || !codecs.is_empty();
        let direction = if media_supported {
            negotiated_direction(kind, line.direction(), &mid)?
        } else {
            MediaDirection::Inactive
        };
        let header_extensions = header_extensions(line, &mid)?;
        let receive_rids = receive_rids(line, kind, direction);
        let data_channel = data_channel_parameters(line, kind, &mid)?;
        let accepted_payload_types = accepted_payload_types(&codecs);
        let mut answer_line = line.clone();
        answer_line.disabled = !media_supported;
        if media_supported {
            answer_line
                .pts
                .retain(|payload_type| accepted_payload_types.contains(&**payload_type));
        }

        answer_media.push(AnswerMedia {
            line: answer_line,
            attributes: answer_attributes_for_media(
                line,
                direction,
                server,
                setup,
                &local_candidates,
                bundle_tag.as_deref() == Some(mid.as_str()),
                &accepted_payload_types,
            ),
        });
        sections.push(NegotiatedMediaSection::new(
            MediaSectionId::new(id),
            mid,
            kind,
            direction,
            codecs.into_boxed_slice(),
            header_extensions.into_boxed_slice(),
            receive_rids.into_boxed_slice(),
            data_channel,
        ));
    }

    let session = NegotiatedSession::new(
        server.ice.clone(),
        server.fingerprint.clone(),
        server.candidates.clone(),
        remote_ice,
        remote_fingerprint,
        remote_candidates,
        sections.into_boxed_slice(),
    );

    Ok(NegotiationResult {
        answer: SdpAnswer::new(format_answer(server, answer_media)),
        session,
    })
}

fn parse_local_candidates(candidates: &[IceCandidate]) -> Result<Vec<Candidate>, NegotiationError> {
    candidates
        .iter()
        .map(|candidate| {
            Candidate::from_sdp_string(candidate.as_sdp())
                .map_err(|error| NegotiationError::Candidate(error.to_string()))
        })
        .collect()
}

fn remote_candidates(offer: &str) -> Result<Box<[IceCandidate]>, NegotiationError> {
    offer
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("a=candidate:"))
        .map(|value| {
            let candidate = IceCandidate::new(format!("candidate:{value}"))
                .ok_or_else(|| NegotiationError::RemoteCandidate(value.to_owned()))?;
            if Candidate::from_sdp_string(candidate.as_sdp()).is_err() && !candidate.is_mdns() {
                return Err(NegotiationError::RemoteCandidate(value.to_owned()));
            }
            Ok(candidate)
        })
        .collect()
}

fn bundle(sdp: &Sdp) -> Result<SessionAttribute, NegotiationError> {
    sdp.session
        .attrs
        .iter()
        .find(
            |attribute| matches!(attribute, SessionAttribute::Group { typ, .. } if typ == "BUNDLE"),
        )
        .cloned()
        .ok_or(NegotiationError::MissingBundle)
}

fn answer_bundle_tag(sdp: &Sdp) -> Result<Option<String>, NegotiationError> {
    for line in &sdp.media_lines {
        let kind = media_kind(&line.typ)?;
        if kind == MediaKind::Application || !codecs(kind, line).is_empty() {
            return Ok(Some(line.mid().to_string()));
        }
    }
    Ok(None)
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

fn codecs(kind: MediaKind, line: &str0m::sdp::MediaLine) -> Vec<Codec> {
    line.rtp_params()
        .into_iter()
        .filter(|params| supports_codec(kind, &params.spec.codec.to_string()))
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

fn supports_codec(kind: MediaKind, name: &str) -> bool {
    match kind {
        MediaKind::Audio => name.eq_ignore_ascii_case("opus"),
        MediaKind::Video => name.eq_ignore_ascii_case("h264"),
        MediaKind::Application => false,
    }
}

fn accepted_payload_types(codecs: &[Codec]) -> HashSet<u8> {
    codecs
        .iter()
        .flat_map(|codec| {
            std::iter::once(codec.payload_type()).chain(codec.retransmission_payload_type())
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

fn receive_rids(
    line: &str0m::sdp::MediaLine,
    kind: MediaKind,
    direction: MediaDirection,
) -> Vec<String> {
    if kind != MediaKind::Video || direction != MediaDirection::ReceiveOnly {
        return Vec::new();
    }
    line.simulcast()
        .map(|simulcast| {
            simulcast
                .send
                .iter()
                .map(|layer| layer.restriction_id.0.clone())
                .filter(|rid| !rid.is_empty())
                .collect()
        })
        .unwrap_or_default()
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

struct AnswerMedia {
    line: MediaLine,
    attributes: Vec<MediaAttribute>,
}

fn answer_attributes_for_media(
    line: &MediaLine,
    direction: MediaDirection,
    server: &ServerTransport,
    setup: Setup,
    local_candidates: &[Candidate],
    bundle_tag: bool,
    accepted_payload_types: &HashSet<u8>,
) -> Vec<MediaAttribute> {
    let rtcp_mux = line.attrs.iter().any(|attribute| {
        matches!(
            attribute,
            MediaAttribute::RtcpMux | MediaAttribute::RtcpMuxOnly
        )
    });
    let mut attributes: Vec<_> = line
        .attrs
        .iter()
        .filter(|attribute| {
            matches!(
                attribute,
                MediaAttribute::Mid(_)
                    | MediaAttribute::SctpPort(_)
                    | MediaAttribute::MaxMessageSize(_)
                    | MediaAttribute::SctpInit(_)
                    | MediaAttribute::ExtMap { .. }
                    | MediaAttribute::RtcpMux
                    | MediaAttribute::RtcpRsize
            ) || matches!(
                attribute,
                MediaAttribute::RtpMap { pt, .. }
                        | MediaAttribute::RtcpFb { pt, .. }
                        | MediaAttribute::Fmtp { pt, .. }
                        if accepted_payload_types.is_empty() || accepted_payload_types.contains(&**pt)
            )
        })
        .cloned()
        .collect();
    if attributes
        .iter()
        .any(|attribute| matches!(attribute, MediaAttribute::RtcpMux))
    {
        debug_assert!(
            attributes
                .iter()
                .all(|attribute| !matches!(attribute, MediaAttribute::RtcpMuxOnly))
        );
    }
    if rtcp_mux
        && !attributes
            .iter()
            .any(|attribute| matches!(attribute, MediaAttribute::RtcpMux))
    {
        attributes.push(MediaAttribute::RtcpMux);
    }
    attributes.extend(answer_simulcast_attributes(
        line,
        direction,
        accepted_payload_types,
    ));
    attributes.push(MediaAttribute::IceUfrag(server.ice.ufrag().to_owned()));
    attributes.push(MediaAttribute::IcePwd(server.ice.password().to_owned()));
    attributes.push(MediaAttribute::Fingerprint(str0m::crypto::Fingerprint {
        hash_func: server.fingerprint.algorithm().to_owned(),
        bytes: server.fingerprint.value().to_vec(),
    }));
    attributes.push(MediaAttribute::Setup(setup));
    attributes.push(match direction {
        MediaDirection::SendOnly => MediaAttribute::SendOnly,
        MediaDirection::ReceiveOnly => MediaAttribute::RecvOnly,
        MediaDirection::Inactive => MediaAttribute::Inactive,
        MediaDirection::Bidirectional => MediaAttribute::SendRecv,
    });
    if bundle_tag {
        attributes.extend(
            local_candidates
                .iter()
                .cloned()
                .map(MediaAttribute::Candidate),
        );
        attributes.push(MediaAttribute::EndOfCandidates);
    }
    attributes
}

fn answer_simulcast_attributes(
    line: &MediaLine,
    direction: MediaDirection,
    accepted_payload_types: &HashSet<u8>,
) -> Vec<MediaAttribute> {
    if direction != MediaDirection::ReceiveOnly {
        return Vec::new();
    }

    let mut accepted_rids = HashSet::new();
    let mut attributes = Vec::new();
    for attribute in &line.attrs {
        let MediaAttribute::Rid {
            id,
            direction: rid_direction,
            pt,
            restriction,
        } = attribute
        else {
            continue;
        };
        if *rid_direction != "send" {
            continue;
        }
        let accepted_pts: Vec<_> = pt
            .iter()
            .copied()
            .filter(|payload_type| accepted_payload_types.contains(&**payload_type))
            .collect();
        if !pt.is_empty() && accepted_pts.is_empty() {
            continue;
        }
        if accepted_rids.insert(id.0.clone()) {
            attributes.push(MediaAttribute::Rid {
                id: id.clone(),
                direction: "recv",
                pt: accepted_pts,
                restriction: restriction.clone(),
            });
        }
    }
    for attribute in &line.attrs {
        let MediaAttribute::Simulcast(simulcast) = attribute else {
            continue;
        };
        let receive = str0m::sdp::SimulcastGroups(
            simulcast
                .send
                .iter()
                .filter(|layer| accepted_rids.contains(&layer.restriction_id.0))
                .map(|layer| str0m::sdp::SimulcastLayer {
                    restriction_id: str0m::sdp::RestrictionId::new(
                        layer.restriction_id.0.clone(),
                        true,
                    ),
                    attributes: layer.attributes.clone(),
                })
                .collect(),
        );
        if receive.0.is_empty() {
            continue;
        }
        attributes.push(MediaAttribute::Simulcast(str0m::sdp::Simulcast {
            send: str0m::sdp::SimulcastGroups(Vec::new()),
            recv: receive,
            is_munged: false,
        }));
    }

    attributes
}

fn format_answer(server: &ServerTransport, answer_media: Vec<AnswerMedia>) -> String {
    let mut answer = String::with_capacity(1024);
    answer.push_str("v=0\r\no=- ");
    answer.push_str(&server.session_id.to_string());
    answer.push_str(" 2 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n");
    let bundle_mids: Vec<_> = answer_media
        .iter()
        .filter(|media| !media.line.disabled)
        .map(|media| media.line.mid().to_owned())
        .collect();
    if !bundle_mids.is_empty() {
        answer.push_str("a=group:BUNDLE");
        for mid in bundle_mids {
            answer.push(' ');
            answer.push_str(&mid);
        }
        answer.push_str("\r\n");
    }
    answer.push_str("a=ice-lite\r\n");
    for AnswerMedia {
        mut line,
        attributes,
    } in answer_media
    {
        line.attrs = attributes;
        answer.push_str(&line.to_string());
    }
    answer
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
        let answer = Sdp::parse(result.answer().as_str()).expect("valid answer SDP");
        assert_eq!(answer.session.ice_candidates().count(), 0);
        assert_eq!(answer.media_lines[0].ice_candidates().count(), 1);
        assert!(answer.media_lines[0].end_of_candidates());
    }

    #[test]
    fn negotiated_session_rejects_bidirectional_media() {
        let error = negotiate(&offer("sendrecv"), &server()).expect_err("bidirectional media");

        assert_eq!(
            error,
            NegotiationError::UnsupportedDirection("0".to_owned())
        );
    }

    #[test]
    fn negotiated_session_keeps_remote_send_simulcast_rids() {
        let offer = format!(
            "{}a=rid:q send\r\n\
             a=rid:h send\r\n\
             a=rid:f send\r\n\
             a=simulcast:send ~q;~h;~f\r\n",
            offer("sendonly")
                .replace(
                    "m=audio 9 UDP/TLS/RTP/SAVPF 111",
                    "m=video 9 UDP/TLS/RTP/SAVPF 96"
                )
                .replace("a=rtpmap:111 opus/48000/2", "a=rtpmap:96 H264/90000")
        );

        let result = negotiate(&offer, &server()).expect("accepted simulcast offer");

        assert_eq!(
            result.session().media_sections()[0].receive_rids(),
            ["q", "h", "f"]
        );
        let answer = result.answer().as_str();
        assert!(answer.contains("a=rid:q recv\r\n"), "{answer}");
        assert!(answer.contains("a=rid:h recv\r\n"), "{answer}");
        assert!(answer.contains("a=rid:f recv\r\n"), "{answer}");
        assert!(answer.contains("a=simulcast:recv q;h;f\r\n"), "{answer}");
    }

    #[test]
    fn answer_places_transport_facts_on_each_media_section_and_candidates_on_bundle_tag() {
        let offer = format!(
            "{}m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=mid:1\r\n\
             a=sendonly\r\n\
             a=rtcp-mux\r\n\
             a=rtpmap:96 VP8/90000\r\n",
            offer("sendonly").replace("a=group:BUNDLE 0", "a=group:BUNDLE 0 1")
        );

        let result = negotiate(&offer, &server()).expect("accepted bundle offer");
        let answer = result.answer().as_str();
        let media: Vec<_> = answer.split("m=").skip(1).collect();

        assert_eq!(media.len(), 2);
        assert!(answer.contains("a=group:BUNDLE 0\r\n"));
        assert!(!answer.contains("a=group:BUNDLE 0 1\r\n"));
        assert!(
            media
                .iter()
                .all(|section| section.contains("a=ice-ufrag:localufrag"))
        );
        assert!(
            media
                .iter()
                .all(|section| section.contains("a=ice-pwd:localpassword"))
        );
        assert!(
            media
                .iter()
                .all(|section| section.contains("a=fingerprint:sha-256"))
        );
        assert!(
            media
                .iter()
                .all(|section| section.contains("a=setup:passive"))
        );
        assert!(media[0].contains("a=candidate:1 1 udp 2130706431 127.0.0.1 9000 typ host"));
        assert!(media[0].contains("a=end-of-candidates"));
        assert!(!media[1].contains("a=candidate:"));
        assert!(!media[1].contains("a=end-of-candidates"));
    }

    #[test]
    fn negotiated_session_preserves_mdns_candidates_for_peer_reflexive_ice() {
        let offer = format!(
            "{}a=candidate:1 1 UDP 2122260223 4db4c1e2-3c04-4ad0-b76b.local 52345 typ host\r\n",
            offer("sendonly")
        );

        let result = negotiate(&offer, &server()).expect("accepted mDNS offer");

        assert_eq!(result.session().remote_candidates().len(), 1);
        assert!(result.session().remote_candidates()[0].is_mdns());
    }

    #[test]
    fn negotiation_rejects_unparseable_non_mdns_candidate() {
        let offer = format!("{}a=candidate:not-a-candidate\r\n", offer("sendonly"));

        assert!(matches!(
            negotiate(&offer, &server()),
            Err(NegotiationError::RemoteCandidate(_))
        ));
    }
}

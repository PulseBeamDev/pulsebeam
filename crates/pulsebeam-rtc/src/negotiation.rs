#![allow(
    dead_code,
    reason = "immutable protocol facts are consumed by subsequent plans"
)]
#![allow(
    clippy::disallowed_types,
    reason = "the public immutable session contract uses Arc-backed values"
)]

use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
    time::Instant,
};

use p256::ecdsa::{DerSignature, SigningKey, signature::Signer};
use sha2::{Digest, Sha256};
use str0m::sdp::{MediaAttribute, MediaType, Proto, Sdp, SessionAttribute, Setup};

use crate::id::SenderId;
use crate::{
    AcceptError, ConnectionConfig, ConnectionLimits, InboundMediaInfo, LocalCandidate,
    PacketFeedbackKind, SdpAnswer, SdpOffer, SenderInfo, SessionInfo, connection::EntropyConsumer,
};
use crate::{MediaDirection as Direction, MediaSectionKind as SectionKind, OfferedMediaSection};

const MAX_SDP_BYTES: usize = 1024 * 1024;
const MAX_MEDIA_SECTIONS: usize = 256;
const MAX_SECTION_ATTRIBUTES: usize = 1024;
const MAX_PAYLOAD_TYPES: usize = 256;
const MAX_EXTENSIONS: usize = 256;
const MAX_RIDS: usize = 256;
const MAX_RID_TOKEN_BYTES: usize = 32;
const MAX_SSRC_ATTRIBUTES: usize = 1024;
const MAX_SSRC_GROUPS: usize = 256;
const MAX_CANDIDATES: usize = 512;
const MAX_SESSION_ATTRIBUTES: usize = 2048;

pub(crate) const TWCC_URIS: [&str; 2] = [
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
    "http://www.webrtc.org/experiments/rtp-hdrext/transport-wide-cc-01",
];
pub(crate) const DEPENDENCY_URIS: [&str; 2] = [
    pulsebeam_core::dd::URI,
    "http://www.webrtc.org/experiments/rtp-hdrext/video-dependency-descriptor",
];
const PLAYOUT_DELAY_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay";
const MID_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";

pub(crate) struct NegotiationResult {
    pub(crate) answer: SdpAnswer,
    pub(crate) session: SessionInfo,
    pub(crate) facts: NegotiatedSessionFacts,
}

#[allow(
    dead_code,
    reason = "immutable protocol facts are consumed by subsequent plans"
)]
pub(crate) struct NegotiatedSessionFacts {
    pub(crate) local_ice: IceCredentials,
    pub(crate) local_candidates: Box<[is::Candidate]>,
    pub(crate) remote_ice: IceCredentials,
    pub(crate) remote_candidates: Box<[String]>,
    pub(crate) local_fingerprint: Fingerprint,
    pub(crate) remote_fingerprint: Fingerprint,
    pub(crate) local_dtls_role: DtlsRole,
    pub(crate) dtls_identity: str0m::crypto::dtls::DtlsCert,
    protocol_randomness: [u8; 32],
    media: Box<[NegotiatedMediaSection]>,
    feedback: Option<PacketFeedbackKind>,
    inbound_feedback: Option<PacketFeedbackKind>,
    limits: ConnectionLimits,
    accepted_at: Instant,
}

pub(crate) struct IngressMediaFacts {
    pub(crate) sender_index: u32,
    pub(crate) kind: crate::MediaKind,
    pub(crate) mid: Box<str>,
    pub(crate) payloads: Box<[(u8, u32)]>,
    pub(crate) feedback_payloads: Box<[u8]>,
    pub(crate) repairs: Box<[(u8, u8)]>,
    pub(crate) nack_payloads: Box<[u8]>,
    pub(crate) ssrc_repairs: Box<[(u32, u32)]>,
    pub(crate) ssrcs: Box<[u32]>,
    pub(crate) rids: Box<[Box<str>]>,
    pub(crate) extensions: Box<[(u8, Box<str>)]>,
}

pub(crate) struct EgressSenderFacts {
    pub(crate) id: SenderId,
    pub(crate) kind: crate::MediaKind,
    pub(crate) mid: Box<str>,
    pub(crate) payload_type: u8,
    pub(crate) retransmission_payload_type: Option<u8>,
    pub(crate) clock_rate: u32,
    pub(crate) mid_extension_id: Option<u8>,
    pub(crate) twcc_extension_id: Option<u8>,
    pub(crate) playout_extension_id: Option<u8>,
    pub(crate) dependency_extension_id: Option<u8>,
    pub(crate) forwarded_extensions: Box<[(u8, Box<str>)]>,
}

#[derive(Clone, Copy)]
pub(crate) struct SctpSessionFacts {
    pub(crate) port: u16,
    pub(crate) max_message_size: Option<usize>,
    pub(crate) unlimited_message_size: bool,
    pub(crate) local_dtls_role: DtlsRole,
}

impl NegotiatedSessionFacts {
    pub(crate) const fn feedback(&self) -> Option<PacketFeedbackKind> {
        self.feedback
    }

    pub(crate) const fn inbound_feedback(&self) -> Option<PacketFeedbackKind> {
        self.inbound_feedback
    }

    pub(crate) fn sctp(&self) -> Option<SctpSessionFacts> {
        self.media.iter().find_map(|section| {
            section.sctp.as_ref().map(|sctp| SctpSessionFacts {
                port: sctp.port,
                max_message_size: sctp.max_message_size,
                unlimited_message_size: sctp.unlimited_message_size,
                local_dtls_role: self.local_dtls_role,
            })
        })
    }

    pub(crate) fn outbound_twcc_extension_id(&self) -> Option<u8> {
        self.media.iter().find_map(|section| {
            section
                .extensions
                .iter()
                .find(|extension| {
                    extension.direction.allows_send() && TWCC_URIS.contains(&extension.uri.as_str())
                })
                .map(|extension| extension.id)
        })
    }

    pub(crate) fn ingress_media(&self) -> Box<[IngressMediaFacts]> {
        self.media
            .iter()
            .filter(|section| section.direction.allows_receive())
            .filter_map(|section| {
                let kind = match section.kind {
                    SectionKind::Audio => crate::MediaKind::Audio,
                    SectionKind::Video => crate::MediaKind::Video,
                    SectionKind::Application => return None,
                };
                Some(IngressMediaFacts {
                    sender_index: section.media_index,
                    kind,
                    mid: section.mid.clone().into_boxed_str(),
                    payloads: section
                        .codecs
                        .iter()
                        .map(|codec| (codec.payload_type, codec.clock_rate))
                        .collect(),
                    feedback_payloads: section
                        .codecs
                        .iter()
                        .filter(|codec| match self.inbound_feedback {
                            Some(PacketFeedbackKind::TransportWide) => codec.twcc,
                            Some(PacketFeedbackKind::Rfc8888) => codec.rfc8888,
                            None => false,
                        })
                        .map(|codec| codec.payload_type)
                        .collect(),
                    repairs: section
                        .codecs
                        .iter()
                        .filter_map(|codec| {
                            codec
                                .retransmission_payload_type
                                .map(|repair| (repair, codec.payload_type))
                        })
                        .collect(),
                    nack_payloads: section
                        .codecs
                        .iter()
                        .filter(|codec| codec.nack)
                        .map(|codec| codec.payload_type)
                        .collect(),
                    ssrc_repairs: section
                        .ssrc_groups
                        .iter()
                        .filter(|group| group.semantics.eq_ignore_ascii_case("FID"))
                        .filter_map(|group| Some((*group.members.get(1)?, *group.members.first()?)))
                        .collect(),
                    ssrcs: section.ssrcs.clone(),
                    rids: section
                        .rids
                        .iter()
                        .cloned()
                        .map(String::into_boxed_str)
                        .collect(),
                    extensions: section
                        .extensions
                        .iter()
                        .filter(|extension| extension.direction.allows_receive())
                        .map(|extension| (extension.id, extension.uri.clone().into_boxed_str()))
                        .collect(),
                })
            })
            .collect()
    }

    pub(crate) fn egress_senders(&self) -> Box<[EgressSenderFacts]> {
        let mut next_id = 1_u16;
        self.media
            .iter()
            .filter(|section| {
                section.kind != SectionKind::Application && section.direction.allows_send()
            })
            .filter_map(|section| {
                let codec = section.codecs.first()?;
                let id = SenderId::new(next_id)?;
                next_id = next_id.checked_add(1).unwrap_or(next_id);
                let extension_id = |uri: &str| {
                    section
                        .extensions
                        .iter()
                        .find(|extension| extension.direction.allows_send() && extension.uri == uri)
                        .map(|extension| extension.id)
                };
                Some(EgressSenderFacts {
                    id,
                    kind: match section.kind {
                        SectionKind::Audio => crate::MediaKind::Audio,
                        SectionKind::Video => crate::MediaKind::Video,
                        SectionKind::Application => return None,
                    },
                    mid: section.mid.clone().into_boxed_str(),
                    payload_type: codec.payload_type,
                    retransmission_payload_type: codec.retransmission_payload_type,
                    clock_rate: codec.clock_rate,
                    mid_extension_id: extension_id(MID_URI),
                    twcc_extension_id: TWCC_URIS.iter().find_map(|uri| extension_id(uri)),
                    playout_extension_id: extension_id(PLAYOUT_DELAY_URI),
                    dependency_extension_id: DEPENDENCY_URIS
                        .iter()
                        .find_map(|uri| extension_id(uri)),
                    forwarded_extensions: section
                        .extensions
                        .iter()
                        .filter(|extension| extension.direction.allows_send() && extension.forward)
                        .map(|extension| (extension.id, extension.uri.clone().into_boxed_str()))
                        .collect(),
                })
            })
            .collect()
    }

    pub(crate) fn inbound_payload_types(&self) -> Box<[u8]> {
        self.media
            .iter()
            .filter(|section| section.direction.allows_receive())
            .flat_map(|section| &section.codecs)
            .flat_map(|codec| {
                std::iter::once(codec.payload_type).chain(codec.retransmission_payload_type)
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(crate) fn outbound_payload_types(&self) -> Box<[u8]> {
        self.egress_senders()
            .iter()
            .flat_map(|sender| {
                std::iter::once(sender.payload_type).chain(sender.retransmission_payload_type)
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(crate) fn outbound_twcc_payload_map(&self) -> Box<[(u8, u8)]> {
        self.egress_senders()
            .iter()
            .filter_map(|sender| {
                sender
                    .twcc_extension_id
                    .map(|extension_id| (sender.payload_type, extension_id))
            })
            .collect()
    }

    pub(crate) const fn protocol_randomness(&self) -> &[u8; 32] {
        &self.protocol_randomness
    }
}

pub(crate) struct IceCredentials {
    pub(crate) ufrag: String,
    pub(crate) password: String,
}

pub(crate) struct Fingerprint {
    pub(crate) algorithm: &'static str,
    pub(crate) value: Box<[u8]>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum DtlsRole {
    Active,
    Passive,
}

impl Direction {
    const fn allows_send(self) -> bool {
        matches!(self, Self::SendOnly | Self::Bidirectional)
    }

    const fn allows_receive(self) -> bool {
        matches!(self, Self::ReceiveOnly | Self::Bidirectional)
    }

    const fn is_subset_of(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Inactive, _)
                | (Self::SendOnly, Self::SendOnly | Self::Bidirectional)
                | (Self::ReceiveOnly, Self::ReceiveOnly | Self::Bidirectional)
                | (Self::Bidirectional, Self::Bidirectional)
        )
    }
}

const fn inherited_extension_direction(media: Direction) -> Direction {
    if matches!(media, Direction::Inactive) {
        Direction::Bidirectional
    } else {
        media
    }
}

struct HeaderExtensionFacts {
    id: u8,
    uri: String,
    direction: Direction,
    attributes: Option<String>,
    forward: bool,
}

struct SsrcGroupFacts {
    semantics: String,
    members: Box<[u32]>,
}

struct SctpFacts {
    port: u16,
    max_message_size: Option<usize>,
    unlimited_message_size: bool,
}

struct NegotiatedMediaSection {
    media_index: u32,
    mid: String,
    kind: SectionKind,
    direction: Direction,
    codecs: Box<[crate::CodecInfo]>,
    extensions: Box<[HeaderExtensionFacts]>,
    rids: Box<[String]>,
    inbound_rids: Box<[String]>,
    ssrcs: Box<[u32]>,
    ssrc_groups: Box<[SsrcGroupFacts]>,
    sctp: Option<SctpFacts>,
}

struct SectionBuild {
    facts: NegotiatedMediaSection,
    accepted_payloads: Vec<u8>,
    twcc: bool,
    rfc8888: bool,
}

#[derive(Debug)]
struct NegotiationError(AcceptError);

impl NegotiationError {
    const fn invalid() -> Self {
        Self(AcceptError::InvalidOffer)
    }
    const fn unsupported() -> Self {
        Self(AcceptError::UnsupportedSessionProfile)
    }
    const fn conflict() -> Self {
        Self(AcceptError::CapabilityConflict)
    }
    const fn limit() -> Self {
        Self(AcceptError::SessionLimitExceeded)
    }
}

impl From<NegotiationError> for AcceptError {
    fn from(value: NegotiationError) -> Self {
        value.0
    }
}

pub(crate) fn negotiate(
    config: &ConnectionConfig,
    offer: &SdpOffer,
    at: Instant,
    entropy: &mut EntropyConsumer,
) -> Result<NegotiationResult, AcceptError> {
    negotiate_inner(config, offer, at, entropy).map_err(Into::into)
}

fn negotiate_inner(
    config: &ConnectionConfig,
    offer: &SdpOffer,
    at: Instant,
    entropy: &mut EntropyConsumer,
) -> Result<NegotiationResult, NegotiationError> {
    if offer.as_str().len() > MAX_SDP_BYTES {
        return Err(NegotiationError::limit());
    }
    let parsed = Sdp::parse(offer.as_str()).map_err(|_| NegotiationError::invalid())?;
    let raw = RawSdp::parse(offer.as_str())?;
    if raw.sections.is_empty() || raw.sections.len() != parsed.media_lines.len() {
        return Err(NegotiationError::invalid());
    }
    if raw.sections.len() > MAX_MEDIA_SECTIONS
        || parsed.session.attrs.len() > MAX_SESSION_ATTRIBUTES
    {
        return Err(NegotiationError::limit());
    }
    if parsed
        .media_lines
        .iter()
        .any(|line| line.attrs.len() > MAX_SECTION_ATTRIBUTES || line.pts.len() > MAX_PAYLOAD_TYPES)
    {
        return Err(NegotiationError::limit());
    }

    let mids = validate_bundle(&parsed)?;
    if parsed.session.ice_lite() || raw.session.iter().any(|line| line == "a=ice-lite") {
        return Err(NegotiationError::unsupported());
    }
    validate_transport(&parsed)?;
    let remote_ice = remote_ice(&parsed)?;
    let remote_fingerprint = remote_fingerprint(&parsed)?;
    let setup = parsed.setup().ok_or_else(NegotiationError::invalid)?;
    let (answer_setup, local_dtls_role) = match setup {
        Setup::Passive => (Setup::Active, DtlsRole::Active),
        Setup::ActPass | Setup::Active => (Setup::Passive, DtlsRole::Passive),
    };
    let allow_mixed = parsed.session.attrs.iter().any(|attribute| {
        matches!(attribute, SessionAttribute::AllowMixedExts)
            || matches!(attribute, SessionAttribute::Unused(value) if value == "extmap-allow-mixed")
    });

    let mut builds = Vec::with_capacity(raw.sections.len());
    for (index, ((line, section), mid)) in parsed
        .media_lines
        .iter()
        .zip(&raw.sections)
        .zip(&mids)
        .enumerate()
    {
        let media_index = u32::try_from(index).map_err(|_| NegotiationError::limit())?;
        builds.push(parse_section(line, section, mid, media_index, allow_mixed)?);
    }
    for section in &mut builds {
        section.facts.extensions = std::mem::take(&mut section.facts.extensions)
            .into_vec()
            .into_iter()
            .filter_map(|mut extension| {
                let known = supported_extension(&extension.uri);
                if !known && config.unknown_extension_policy == crate::UnknownExtensionPolicy::Drop
                {
                    return None;
                }
                extension.forward = forward_extension(&extension.uri);
                Some(extension)
            })
            .collect();
    }
    validate_bundle_namespaces(&builds)?;

    let active = builds
        .iter()
        .filter(|section| {
            section.facts.kind != SectionKind::Application
                && section.facts.direction != Direction::Inactive
        })
        .collect::<Vec<_>>();
    if active.is_empty()
        && builds
            .iter()
            .any(|section| section.facts.kind != SectionKind::Application)
    {
        return Err(NegotiationError(AcceptError::MissingPacketFeedback));
    }
    let outbound = active
        .iter()
        .copied()
        .filter(|section| {
            matches!(
                section.facts.direction,
                Direction::SendOnly | Direction::Bidirectional
            )
        })
        .collect::<Vec<_>>();
    let inbound = active
        .iter()
        .copied()
        .filter(|section| section.facts.direction.allows_receive())
        .collect::<Vec<_>>();
    let inbound_feedback = select_feedback(&inbound, false);
    let feedback = if outbound.is_empty() {
        None
    } else {
        Some(
            select_feedback(&outbound, true)
                .ok_or(NegotiationError(AcceptError::MissingPacketFeedback))?,
        )
    };
    for section in &mut builds {
        section.facts.extensions = std::mem::take(&mut section.facts.extensions)
            .into_vec()
            .into_iter()
            .filter_map(|mut extension| {
                if TWCC_URIS.contains(&extension.uri.as_str()) {
                    let send = feedback == Some(PacketFeedbackKind::TransportWide)
                        && section.facts.direction.allows_send()
                        && extension.direction.allows_send();
                    let receive = inbound_feedback == Some(PacketFeedbackKind::TransportWide)
                        && section.facts.direction.allows_receive()
                        && extension.direction.allows_receive();
                    extension.direction = match (send, receive) {
                        (true, true) => Direction::Bidirectional,
                        (true, false) => Direction::SendOnly,
                        (false, true) => Direction::ReceiveOnly,
                        (false, false) => return None,
                    };
                }
                Some(extension)
            })
            .collect();
    }

    let (dtls_identity, local_fingerprint) = dtls_identity(entropy)?;
    let local_ice = match &config.local_ice_credentials {
        Some(credentials) => IceCredentials {
            ufrag: credentials.ufrag().to_owned(),
            password: credentials.password().to_owned(),
        },
        None => IceCredentials {
            ufrag: token(&entropy.take::<12>(b"ice ufrag")),
            password: token(&entropy.take::<24>(b"ice password")),
        },
    };
    let protocol_randomness = entropy.take::<32>(b"protocol randomness");
    let local_candidates = local_candidates(config)?;
    let remote_candidates = remote_candidates(offer.as_str())?;

    let inbound_media = builds
        .iter()
        .filter(|section| {
            section.facts.kind != SectionKind::Application
                && section.facts.direction.allows_receive()
        })
        .filter_map(|section| {
            let kind = match section.facts.kind {
                SectionKind::Audio => crate::MediaKind::Audio,
                SectionKind::Video => crate::MediaKind::Video,
                SectionKind::Application => return None,
            };
            Some(InboundMediaInfo {
                sender_index: section.facts.media_index,
                kind,
                mid: Arc::from(section.facts.mid.as_str()),
                codecs: Arc::from(section.facts.codecs.as_ref()),
                primary_ssrcs: section
                    .facts
                    .ssrcs
                    .iter()
                    .copied()
                    .filter(|ssrc| {
                        !section.facts.ssrc_groups.iter().any(|group| {
                            group.semantics.eq_ignore_ascii_case("FID")
                                && group.members.get(1) == Some(ssrc)
                        })
                    })
                    .collect(),
                rids: section
                    .facts
                    .inbound_rids
                    .iter()
                    .map(|rid| Arc::from(rid.as_str()))
                    .collect(),
            })
        })
        .collect::<Vec<_>>();
    let mut senders = Vec::new();
    for section in &builds {
        if section.facts.kind == SectionKind::Application
            || !matches!(
                section.facts.direction,
                Direction::SendOnly | Direction::Bidirectional
            )
        {
            continue;
        }
        let codec = section
            .facts
            .codecs
            .first()
            .ok_or_else(NegotiationError::conflict)?;
        let ordinal = senders
            .len()
            .checked_add(1)
            .ok_or_else(NegotiationError::limit)?;
        let id = u16::try_from(ordinal).map_err(|_| NegotiationError::limit())?;
        let kind = match section.facts.kind {
            SectionKind::Audio => crate::MediaKind::Audio,
            SectionKind::Video => crate::MediaKind::Video,
            SectionKind::Application => continue,
        };
        senders.push(SenderInfo {
            id: SenderId::new(id).ok_or_else(NegotiationError::limit)?,
            receiver_index: section.facts.media_index,
            kind,
            mid: Arc::from(section.facts.mid.as_str()),
            codec: *codec,
            extensions: section
                .facts
                .extensions
                .iter()
                .filter(|extension| extension.direction.allows_send())
                .map(|extension| crate::RtpExtensionInfo {
                    id: extension.id,
                    uri: Arc::from(extension.uri.as_str()),
                })
                .collect(),
            signals_playout_delay: section.facts.extensions.iter().any(|extension| {
                extension.direction.allows_send() && extension.uri == PLAYOUT_DELAY_URI
            }),
        });
    }
    let session = SessionInfo {
        offered_media: builds
            .iter()
            .zip(&parsed.media_lines)
            .zip(&raw.sections)
            .map(|((section, line), raw)| OfferedMediaSection {
                media_index: section.facts.media_index,
                mid: Arc::from(section.facts.mid.as_str()),
                kind: section.facts.kind,
                direction: section.facts.direction,
                disabled: line.disabled && !raw.iter().any(|value| value == "a=bundle-only"),
            })
            .collect(),
        feedback,
        inbound_media: Arc::from(inbound_media),
        senders: Arc::from(senders),
    };
    let cname = token(
        &Sha256::new()
            .chain_update(b"pulsebeam rtc cname v1")
            .chain_update(protocol_randomness)
            .finalize(),
    );
    let answer_sections = builds
        .iter()
        .zip(&raw.sections)
        .enumerate()
        .map(|(index, (section, raw))| {
            let mut answer = format_answer_section(
                raw,
                section,
                &local_ice,
                &local_fingerprint,
                &local_candidates,
                answer_setup,
                index == 0,
                [feedback, inbound_feedback],
            );
            if let Some(sender) = session
                .senders
                .iter()
                .find(|sender| sender.receiver_index == section.facts.media_index)
            {
                let (ssrc, rtx_ssrc, _) =
                    crate::egress::sender_rtp_identity(&protocol_randomness, sender.id);
                // Declare the primary before any RTX-only probe: an RTX SSRC
                // cannot identify its primary through MID and apt alone.
                answer.push_str(&format!("a=ssrc:{ssrc} cname:{cname}\r\n"));
                if sender.codec.retransmission_payload_type.is_some() {
                    answer.push_str(&format!(
                        "a=ssrc-group:FID {ssrc} {rtx_ssrc}\r\na=ssrc:{rtx_ssrc} cname:{cname}\r\n"
                    ));
                }
            }
            answer
        })
        .collect::<Vec<_>>();
    let answer_text = format_answer(&mids, &answer_sections, allow_mixed);
    let answer = Sdp::parse(&answer_text)
        .map_err(|_| NegotiationError(AcceptError::CryptographicFailure))?;
    answer
        .assert_consistency()
        .map_err(|_| NegotiationError(AcceptError::CryptographicFailure))?;

    Ok(NegotiationResult {
        answer: SdpAnswer::new(answer_text),
        session,
        facts: NegotiatedSessionFacts {
            local_ice,
            local_candidates: local_candidates.into_boxed_slice(),
            remote_ice,
            remote_candidates: remote_candidates.into_boxed_slice(),
            local_fingerprint,
            remote_fingerprint,
            local_dtls_role,
            dtls_identity,
            protocol_randomness,
            media: builds
                .into_iter()
                .map(|section| section.facts)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            feedback,
            inbound_feedback,
            limits: config.limits,
            accepted_at: at,
        },
    })
}

struct RawSdp {
    session: Vec<String>,
    sections: Vec<Vec<String>>,
}

impl RawSdp {
    fn parse(value: &str) -> Result<Self, NegotiationError> {
        let mut session = Vec::new();
        let mut sections = Vec::new();
        let mut current: Option<Vec<String>> = None;
        for raw in value.lines() {
            let line = raw.trim_end_matches('\r').trim().to_owned();
            if line.starts_with("m=")
                && let Some(section) = current.replace(Vec::new())
            {
                sections.push(section);
            }
            if let Some(section) = &mut current {
                section.push(line);
            } else {
                session.push(line);
            }
        }
        if let Some(section) = current {
            sections.push(section);
        }
        if sections
            .iter()
            .any(|section| section.len() > MAX_SECTION_ATTRIBUTES + 1)
        {
            return Err(NegotiationError::limit());
        }
        Ok(Self { session, sections })
    }
}

fn validate_bundle(sdp: &Sdp) -> Result<Vec<String>, NegotiationError> {
    let groups = sdp
        .session
        .attrs
        .iter()
        .filter_map(|attribute| match attribute {
            SessionAttribute::Group { typ, mids } if typ == "BUNDLE" => {
                Some(mids.iter().map(ToString::to_string).collect::<Vec<_>>())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let [mids] = groups.as_slice() else {
        return Err(NegotiationError::invalid());
    };
    let offered = sdp
        .media_lines
        .iter()
        .map(|line| line.mid().to_string())
        .collect::<Vec<_>>();
    let unique = mids.iter().collect::<HashSet<_>>();
    if mids.is_empty()
        || unique.len() != mids.len()
        || mids != &offered
        || sdp.media_lines.first().is_some_and(|line| line.disabled)
    {
        return Err(NegotiationError::invalid());
    }
    Ok(mids.clone())
}

fn validate_transport(sdp: &Sdp) -> Result<(), NegotiationError> {
    let ice = sdp.ice_creds().ok_or_else(NegotiationError::invalid)?;
    let fingerprint = sdp.fingerprint().ok_or_else(NegotiationError::invalid)?;
    let setup = sdp.setup().ok_or_else(NegotiationError::invalid)?;
    for line in &sdp.media_lines {
        if let Some(section) = line.ice_creds()
            && (section.ufrag != ice.ufrag || section.pass != ice.pass)
        {
            return Err(NegotiationError::conflict());
        }
        if let Some(section) = line.fingerprint()
            && (!section
                .hash_func
                .eq_ignore_ascii_case(&fingerprint.hash_func)
                || section.bytes != fingerprint.bytes)
        {
            return Err(NegotiationError::conflict());
        }
        if line.setup().is_some_and(|section| section != setup) {
            return Err(NegotiationError::conflict());
        }
    }
    Ok(())
}

fn remote_ice(sdp: &Sdp) -> Result<IceCredentials, NegotiationError> {
    let value = sdp.ice_creds().ok_or_else(NegotiationError::invalid)?;
    if !(4..=256).contains(&value.ufrag.len())
        || !(22..=256).contains(&value.pass.len())
        || !value.ufrag.bytes().all(ice_char)
        || !value.pass.bytes().all(ice_char)
    {
        return Err(NegotiationError::invalid());
    }
    Ok(IceCredentials {
        ufrag: value.ufrag,
        password: value.pass,
    })
}

fn ice_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/')
}

fn remote_fingerprint(sdp: &Sdp) -> Result<Fingerprint, NegotiationError> {
    let value = sdp.fingerprint().ok_or_else(NegotiationError::invalid)?;
    let (algorithm, expected) = match value.hash_func.to_ascii_lowercase().as_str() {
        "sha-256" => ("sha-256", 32),
        "sha-384" => ("sha-384", 48),
        "sha-512" => ("sha-512", 64),
        _ => return Err(NegotiationError::unsupported()),
    };
    if value.bytes.len() != expected {
        return Err(NegotiationError::invalid());
    }
    Ok(Fingerprint {
        algorithm,
        value: value.bytes.into_boxed_slice(),
    })
}

fn parse_section(
    line: &str0m::sdp::MediaLine,
    raw: &[String],
    mid: &str,
    media_index: u32,
    allow_mixed: bool,
) -> Result<SectionBuild, NegotiationError> {
    let disabled = line.disabled && !raw.iter().any(|value| value == "a=bundle-only");
    let kind = match line.typ {
        MediaType::Audio => SectionKind::Audio,
        MediaType::Video => SectionKind::Video,
        MediaType::Application => SectionKind::Application,
        MediaType::Unknown(_) => return Err(NegotiationError::unsupported()),
    };
    let direction = if disabled {
        Direction::Inactive
    } else if kind == SectionKind::Application {
        Direction::Bidirectional
    } else {
        let values = raw
            .iter()
            .filter_map(|value| match value.as_str() {
                "a=sendonly" => Some(Direction::ReceiveOnly),
                "a=recvonly" => Some(Direction::SendOnly),
                "a=sendrecv" => Some(Direction::Bidirectional),
                "a=inactive" => Some(Direction::Inactive),
                _ => None,
            })
            .collect::<Vec<_>>();
        let [direction] = values.as_slice() else {
            return Err(NegotiationError::invalid());
        };
        *direction
    };
    if kind != SectionKind::Application && !disabled && line.proto != Proto::Srtp {
        return Err(NegotiationError::unsupported());
    }
    if kind != SectionKind::Application
        && !disabled
        && !line.attrs.iter().any(|attribute| {
            matches!(
                attribute,
                MediaAttribute::RtcpMux | MediaAttribute::RtcpMuxOnly
            )
        })
    {
        return Err(NegotiationError::invalid());
    }
    let (codecs, accepted_payloads) = if disabled || kind == SectionKind::Application {
        (Vec::new(), Vec::new())
    } else {
        parse_codecs(line, raw, kind)?
    };
    let extensions = if disabled || kind == SectionKind::Application {
        Vec::new()
    } else {
        parse_extensions(raw, allow_mixed, direction)?
    };
    if kind != SectionKind::Application
        && !disabled
        && !extensions
            .iter()
            .any(|extension| extension.uri == "urn:ietf:params:rtp-hdrext:sdes:mid")
    {
        return Err(NegotiationError::conflict());
    }
    let rids = parse_rids(raw, kind)?;
    validate_simulcast(raw, kind)?;
    let (ssrcs, groups) = parse_ssrcs(raw, kind)?;
    let sctp = parse_sctp(line, raw, kind, disabled)?;
    let twcc = codecs.iter().all(|codec| codec.twcc)
        && extensions
            .iter()
            .any(|extension| TWCC_URIS.contains(&extension.uri.as_str()));
    let rfc8888 = codecs.iter().all(|codec| codec.rfc8888);
    Ok(SectionBuild {
        facts: NegotiatedMediaSection {
            media_index,
            mid: mid.to_owned(),
            kind,
            direction,
            codecs: codecs.into_boxed_slice(),
            extensions: extensions.into_boxed_slice(),
            rids: rids.into_boxed_slice(),
            inbound_rids: inbound_rids(raw)?.into_boxed_slice(),
            ssrcs: ssrcs.into_boxed_slice(),
            ssrc_groups: groups.into_boxed_slice(),
            sctp,
        },
        accepted_payloads,
        twcc,
        rfc8888,
    })
}

fn parse_codecs(
    line: &str0m::sdp::MediaLine,
    raw: &[String],
    kind: SectionKind,
) -> Result<(Vec<crate::CodecInfo>, Vec<u8>), NegotiationError> {
    let params = line
        .rtp_params()
        .into_iter()
        .filter(|parameter| line.pts.iter().any(|offered| **offered == *parameter.pt))
        .collect::<Vec<_>>();
    if params.is_empty() || params.len() > MAX_PAYLOAD_TYPES {
        return Err(NegotiationError::invalid());
    }
    let mut codecs = Vec::new();
    let mut payloads = Vec::new();
    for parameter in &params {
        let clock_rate = u32::from(std::num::NonZeroU32::from(parameter.spec.clock_rate));
        let supported = match kind {
            SectionKind::Audio => {
                parameter.spec.codec == str0m::format::Codec::Opus
                    && clock_rate == 48_000
                    && parameter.spec.channels == Some(2)
            }
            SectionKind::Video => {
                parameter.spec.codec == str0m::format::Codec::H264
                    && clock_rate == 90_000
                    && parameter.spec.channels.is_none()
            }
            SectionKind::Application => false,
        };
        if supported {
            let profile = parse_codec_profile(raw, *parameter.pt, kind)?;
            payloads.push(*parameter.pt);
            codecs.push(crate::CodecInfo {
                payload_type: *parameter.pt,
                profile,
                clock_rate,
                channels: parameter.spec.channels,
                retransmission_payload_type: None,
                nack: codec_feedback(raw, *parameter.pt, "nack"),
                twcc: codec_feedback(raw, *parameter.pt, "transport-cc"),
                rfc8888: codec_feedback(raw, *parameter.pt, "ccfb"),
                pli: codec_feedback(raw, *parameter.pt, "nack pli"),
                fir: codec_feedback(raw, *parameter.pt, "ccm fir"),
            });
        }
    }
    if codecs.is_empty() {
        return Err(NegotiationError::conflict());
    }
    // rtp_params folds RTX into primary payloads; inspect offered maps to retain
    // duplicate apt validation instead of accepting only its last resend mapping.
    for mapping in raw.iter().filter_map(|line| line.strip_prefix("a=rtpmap:")) {
        let mut fields = mapping.split_whitespace();
        let Some(payload) = fields.next().and_then(|value| value.parse::<u8>().ok()) else {
            continue;
        };
        if !line.pts.iter().any(|offered| **offered == payload)
            || !fields
                .next()
                .is_some_and(|codec| codec.eq_ignore_ascii_case("rtx/90000"))
        {
            continue;
        }
        let Some(apt) =
            fmtp_parameter(raw, payload, "apt")?.and_then(|value| value.parse::<u8>().ok())
        else {
            continue;
        };
        if let Some(primary) = codecs.iter_mut().find(|codec| codec.payload_type == apt) {
            if primary.retransmission_payload_type.is_some() {
                return Err(NegotiationError::conflict());
            }
            primary.retransmission_payload_type = Some(payload);
            payloads.push(payload);
        }
    }
    Ok((codecs, payloads))
}

fn codec_feedback(raw: &[String], payload: u8, feedback: &str) -> bool {
    raw.iter()
        .filter_map(|line| line.strip_prefix("a=rtcp-fb:"))
        .filter_map(|value| value.split_once(char::is_whitespace))
        .any(|(pt, name)| (pt == "*" || pt.parse::<u8>() == Ok(payload)) && name.trim() == feedback)
}

fn select_feedback(sections: &[&SectionBuild], outbound: bool) -> Option<PacketFeedbackKind> {
    if sections.is_empty() {
        return None;
    }
    if sections.iter().all(|section| {
        section.twcc
            && section.facts.extensions.iter().any(|extension| {
                TWCC_URIS.contains(&extension.uri.as_str())
                    && if outbound {
                        extension.direction.allows_send()
                    } else {
                        extension.direction.allows_receive()
                    }
            })
    }) {
        Some(PacketFeedbackKind::TransportWide)
    } else if sections.iter().all(|section| section.rfc8888) {
        Some(PacketFeedbackKind::Rfc8888)
    } else {
        None
    }
}

fn parse_codec_profile(
    raw: &[String],
    payload: u8,
    kind: SectionKind,
) -> Result<crate::CodecProfile, NegotiationError> {
    if kind == SectionKind::Audio {
        return Ok(crate::CodecProfile::Opus);
    }
    let packetization_mode = match fmtp_parameter(raw, payload, "packetization-mode")? {
        None | Some("0") => 0,
        Some("1") => 1,
        Some(_) => return Err(NegotiationError::unsupported()),
    };
    let profile_level_id = fmtp_parameter(raw, payload, "profile-level-id")?
        .map(|value| {
            if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(NegotiationError::invalid());
            }
            u32::from_str_radix(value, 16).map_err(|_| NegotiationError::invalid())
        })
        .transpose()?;
    let level_asymmetry_allowed = match fmtp_parameter(raw, payload, "level-asymmetry-allowed")? {
        None | Some("0") => false,
        Some("1") => true,
        Some(_) => return Err(NegotiationError::invalid()),
    };
    Ok(crate::CodecProfile::H264 {
        profile_level_id,
        packetization_mode,
        level_asymmetry_allowed,
    })
}

fn fmtp_parameter<'a>(
    raw: &'a [String],
    payload: u8,
    key: &str,
) -> Result<Option<&'a str>, NegotiationError> {
    let mut found = None;
    for value in raw.iter().filter_map(|line| line.strip_prefix("a=fmtp:")) {
        let Some((pt, parameters)) = value.split_once(char::is_whitespace) else {
            continue;
        };
        if pt.parse::<u8>().ok() != Some(payload) {
            continue;
        }
        for parameter in parameters.split(';') {
            let Some((candidate, value)) = parameter.trim().split_once('=') else {
                return Err(NegotiationError::invalid());
            };
            if candidate.eq_ignore_ascii_case(key)
                && found.replace(value).is_some_and(|old| old != value)
            {
                return Err(NegotiationError::conflict());
            }
        }
    }
    Ok(found)
}

fn parse_extensions(
    raw: &[String],
    allow_mixed: bool,
    media_direction: Direction,
) -> Result<Vec<HeaderExtensionFacts>, NegotiationError> {
    let mut ids = HashSet::new();
    let mut uris = HashSet::new();
    let mut result = Vec::new();
    for (index, value) in raw
        .iter()
        .filter_map(|line| line.strip_prefix("a=extmap:"))
        .enumerate()
    {
        if index >= MAX_EXTENSIONS {
            return Err(NegotiationError::limit());
        }
        let Some((id, value)) = value.split_once(char::is_whitespace) else {
            return Err(NegotiationError::invalid());
        };
        let (uri, attributes) = match value.split_once(char::is_whitespace) {
            Some((uri, attributes)) => (uri, Some(attributes.trim().to_owned())),
            None => (value, None),
        };
        let mut parts = id.split('/');
        let id = parts
            .next()
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|id| *id != 0 && (*id <= 14 || allow_mixed))
            .ok_or_else(NegotiationError::invalid)?;
        let direction = match parts.next() {
            None => inherited_extension_direction(media_direction),
            Some("sendrecv") => Direction::Bidirectional,
            Some("sendonly") => Direction::ReceiveOnly,
            Some("recvonly") => Direction::SendOnly,
            Some("inactive") => Direction::Inactive,
            Some(_) => return Err(NegotiationError::invalid()),
        };
        if parts.next().is_some() || !ids.insert(id) || !uris.insert(uri) {
            return Err(NegotiationError::conflict());
        }
        if media_direction != Direction::Inactive && !direction.is_subset_of(media_direction) {
            if uri == MID_URI || TWCC_URIS.contains(&uri) {
                return Err(NegotiationError::invalid());
            }
            // Firefox advertises recvonly playout-delay on sendonly media. An
            // unusable optional mapping is omitted, never answered as usable.
            continue;
        }
        result.push(HeaderExtensionFacts {
            id,
            uri: uri.to_owned(),
            direction,
            attributes,
            forward: false,
        });
    }
    result.sort_by_key(|extension| extension.id);
    Ok(result)
}

fn supported_extension(uri: &str) -> bool {
    uri == "urn:ietf:params:rtp-hdrext:sdes:mid"
        || uri == "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id"
        || uri == "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id"
        || uri == "urn:ietf:params:rtp-hdrext:ssrc-audio-level"
        || uri == "urn:3gpp:video-orientation"
        || uri == "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time"
        || TWCC_URIS.contains(&uri)
        || DEPENDENCY_URIS.contains(&uri)
        || uri.contains("video-layers-allocation")
        || uri == PLAYOUT_DELAY_URI
}

fn forward_extension(uri: &str) -> bool {
    !matches!(
        uri,
        "urn:ietf:params:rtp-hdrext:sdes:mid"
            | "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id"
            | "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id"
            | "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time"
    ) && !TWCC_URIS.contains(&uri)
}

fn parse_rids(raw: &[String], kind: SectionKind) -> Result<Vec<String>, NegotiationError> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for value in raw.iter().filter_map(|line| line.strip_prefix("a=rid:")) {
        if kind != SectionKind::Video || result.len() >= MAX_RIDS {
            return Err(NegotiationError::limit());
        }
        let mut fields = value.split_whitespace();
        let id = fields.next().unwrap_or_default();
        let direction = fields.next().unwrap_or_default();
        if id.is_empty()
            || id.len() > MAX_RID_TOKEN_BYTES
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || !seen.insert(id)
            || !matches!(direction, "send" | "recv")
        {
            return Err(NegotiationError::invalid());
        }
        result.push(id.to_owned());
    }
    Ok(result)
}

fn inbound_rids(raw: &[String]) -> Result<Vec<String>, NegotiationError> {
    let declared = raw
        .iter()
        .filter_map(|line| line.strip_prefix("a=rid:"))
        .filter_map(|value| {
            let mut fields = value.split_whitespace();
            let rid = fields.next()?;
            (fields.next() == Some("send")).then_some(rid)
        })
        .collect::<Vec<_>>();
    if declared.len() > MAX_RIDS {
        return Err(NegotiationError::limit());
    }
    let mut rids = Vec::new();
    for value in raw
        .iter()
        .filter_map(|line| line.strip_prefix("a=simulcast:"))
    {
        let fields = value.split_whitespace().collect::<Vec<_>>();
        for pair in fields.as_chunks::<2>().0 {
            let ["send", rid_list] = pair else {
                continue;
            };
            for rid in rid_list
                .split([';', ','])
                .map(|rid| rid.strip_prefix('~').unwrap_or(rid))
            {
                if !declared.contains(&rid) {
                    return Err(NegotiationError::invalid());
                }
                if !rids.iter().any(|existing| existing == rid) {
                    rids.push(rid.to_owned());
                }
            }
        }
    }
    if rids.is_empty() {
        rids.extend(declared.into_iter().map(str::to_owned));
    }
    Ok(rids)
}

fn validate_simulcast(raw: &[String], kind: SectionKind) -> Result<(), NegotiationError> {
    for value in raw
        .iter()
        .filter_map(|line| line.strip_prefix("a=simulcast:"))
    {
        if kind != SectionKind::Video {
            return Err(NegotiationError::invalid());
        }
        let fields = value.split_whitespace().collect::<Vec<_>>();
        if fields.is_empty()
            || fields.len() % 2 != 0
            || fields.as_chunks::<2>().0.iter().any(|pair| {
                let [direction, alternatives] = pair;
                !matches!(*direction, "send" | "recv") || alternatives.is_empty()
            })
        {
            return Err(NegotiationError::invalid());
        }
    }
    Ok(())
}

fn parse_ssrcs(
    raw: &[String],
    kind: SectionKind,
) -> Result<(Vec<u32>, Vec<SsrcGroupFacts>), NegotiationError> {
    if raw
        .iter()
        .filter(|line| line.starts_with("a=ssrc:"))
        .count()
        > MAX_SSRC_ATTRIBUTES
    {
        return Err(NegotiationError::limit());
    }
    let mut ssrcs = Vec::new();
    let mut seen = HashSet::new();
    for value in raw.iter().filter_map(|line| line.strip_prefix("a=ssrc:")) {
        let id = value
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(NegotiationError::invalid)?;
        if id != 0 && seen.insert(id) {
            ssrcs.push(id);
        }
    }
    let mut groups = Vec::new();
    for value in raw
        .iter()
        .filter_map(|line| line.strip_prefix("a=ssrc-group:"))
    {
        if groups.len() >= MAX_SSRC_GROUPS {
            return Err(NegotiationError::limit());
        }
        let mut fields = value.split_whitespace();
        let semantics = fields.next().ok_or_else(NegotiationError::invalid)?;
        let members = fields
            .map(|value| {
                value
                    .parse::<u32>()
                    .map_err(|_| NegotiationError::invalid())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let valid = (semantics.eq_ignore_ascii_case("FID") && members.len() == 2)
            || (semantics.eq_ignore_ascii_case("SIM")
                && kind == SectionKind::Video
                && members.len() >= 2);
        if !valid || members.iter().any(|member| !ssrcs.contains(member)) {
            return Err(NegotiationError::invalid());
        }
        groups.push(SsrcGroupFacts {
            semantics: semantics.to_ascii_uppercase(),
            members: members.into_boxed_slice(),
        });
    }
    Ok((ssrcs, groups))
}

fn parse_sctp(
    line: &str0m::sdp::MediaLine,
    raw: &[String],
    kind: SectionKind,
    disabled: bool,
) -> Result<Option<SctpFacts>, NegotiationError> {
    if kind != SectionKind::Application || disabled {
        return Ok(None);
    }
    let mline = raw.first().map(String::as_str).unwrap_or_default();
    if line.proto != Proto::Sctp
        || !mline.contains("UDP/DTLS/SCTP")
        || !mline.ends_with("webrtc-datachannel")
    {
        return Err(NegotiationError::unsupported());
    }
    let ports = raw
        .iter()
        .filter_map(|line| line.strip_prefix("a=sctp-port:"))
        .collect::<Vec<_>>();
    let [port] = ports.as_slice() else {
        return Err(NegotiationError::invalid());
    };
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(NegotiationError::invalid)?;
    let maxes = raw
        .iter()
        .filter_map(|line| line.strip_prefix("a=max-message-size:"))
        .collect::<Vec<_>>();
    if maxes.len() > 1 {
        return Err(NegotiationError::conflict());
    }
    let max = maxes
        .first()
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| NegotiationError::invalid())
        })
        .transpose()?;
    Ok(Some(SctpFacts {
        port,
        max_message_size: max.filter(|value| *value != 0),
        unlimited_message_size: max == Some(0),
    }))
}

fn validate_bundle_namespaces(sections: &[SectionBuild]) -> Result<(), NegotiationError> {
    let mut payloads = std::collections::HashMap::<u8, (crate::MediaKind, u32, Option<u8>)>::new();
    let mut extensions = std::collections::HashMap::<u8, &str>::new();
    let mut mid_extension = None;
    let mut global_ssrcs = HashSet::new();
    for section in sections {
        for codec in &section.facts.codecs {
            let signature = (codec.profile.kind(), codec.clock_rate, codec.channels);
            if payloads
                .insert(codec.payload_type, signature)
                .is_some_and(|old| old != signature)
            {
                return Err(NegotiationError::conflict());
            }
        }
        let mut section_mid = None;
        for extension in &section.facts.extensions {
            if extensions
                .insert(extension.id, &extension.uri)
                .is_some_and(|old| old != extension.uri)
            {
                return Err(NegotiationError::conflict());
            }
            if extension.uri == "urn:ietf:params:rtp-hdrext:sdes:mid" {
                section_mid = Some(extension.id);
            }
        }
        if section.facts.kind != SectionKind::Application
            && section.facts.direction != Direction::Inactive
        {
            let id = section_mid.ok_or_else(NegotiationError::conflict)?;
            if mid_extension.replace(id).is_some_and(|old| old != id) {
                return Err(NegotiationError::conflict());
            }
        }
        for ssrc in &section.facts.ssrcs {
            if !global_ssrcs.insert(*ssrc) {
                return Err(NegotiationError::conflict());
            }
        }
    }
    Ok(())
}

fn local_candidates(config: &ConnectionConfig) -> Result<Vec<is::Candidate>, NegotiationError> {
    config
        .local_candidates
        .iter()
        .map(|candidate| match candidate {
            LocalCandidate::Udp(std::net::SocketAddr::V4(address))
                if address.ip().is_link_local() =>
            {
                Ok(ipv4_link_local_candidate(
                    std::net::SocketAddr::V4(*address),
                    *address.ip(),
                    false,
                ))
            }
            LocalCandidate::Udp(address) => is::Candidate::builder().udp().host(*address).build(),
            LocalCandidate::TcpPassive(std::net::SocketAddr::V4(address))
                if address.ip().is_link_local() =>
            {
                Ok(ipv4_link_local_candidate(
                    std::net::SocketAddr::V4(*address),
                    *address.ip(),
                    true,
                ))
            }
            LocalCandidate::TcpPassive(address) => is::Candidate::builder()
                .tcp()
                .host(*address)
                .tcptype(str0m::net::TcpType::Passive)
                .build(),
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| NegotiationError(AcceptError::InvalidConfiguration))
}

fn ipv4_link_local_candidate(
    address: std::net::SocketAddr,
    ip: std::net::Ipv4Addr,
    tcp: bool,
) -> is::Candidate {
    let (protocol, tcp_type, type_preference, transport) = if tcp {
        (
            str0m::net::Protocol::Tcp,
            Some(str0m::net::TcpType::Passive),
            90_u32,
            "tcp",
        )
    } else {
        (str0m::net::Protocol::Udp, None, 126_u32, "udp")
    };
    let priority = type_preference << 24 | 65_534 << 8 | 255;
    is::Candidate::from_parts(
        format!("pb-{transport}-{:08x}", u32::from(ip)),
        1,
        protocol,
        priority,
        address,
        is::CandidateKind::Host,
        None,
        tcp_type,
        None,
    )
}

fn remote_candidates(offer: &str) -> Result<Vec<String>, NegotiationError> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for value in offer.lines().filter_map(|line| {
        line.trim_end_matches('\r')
            .trim()
            .strip_prefix("a=candidate:")
    }) {
        let candidate = format!("candidate:{value}");
        let parsed = str0m::Candidate::from_sdp_string(&candidate)
            .map_err(|_| NegotiationError::invalid())?;
        let canonical = parsed.to_sdp_string();
        if seen.insert(canonical.clone()) {
            if result.len() >= MAX_CANDIDATES {
                return Err(NegotiationError::limit());
            }
            result.push(canonical);
        }
    }
    Ok(result)
}

fn token(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    bytes
        .iter()
        .map(|byte| {
            char::from(
                ALPHABET
                    .get(usize::from(*byte & 63))
                    .copied()
                    .unwrap_or(b'A'),
            )
        })
        .collect()
}

struct DeterministicP256Key {
    signing_key: SigningKey,
    public_key: Box<[u8]>,
}

impl DeterministicP256Key {
    fn new(seed: &[u8; 32]) -> Result<Self, NegotiationError> {
        let signing_key = SigningKey::from_slice(seed)
            .map_err(|_| NegotiationError(AcceptError::CryptographicFailure))?;
        let public_key = signing_key
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()
            .into();
        Ok(Self {
            signing_key,
            public_key,
        })
    }
}

impl rcgen::PublicKeyData for DeterministicP256Key {
    fn der_bytes(&self) -> &[u8] {
        &self.public_key
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

impl rcgen::SigningKey for DeterministicP256Key {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        let signature: DerSignature = self.signing_key.sign(message);
        Ok(signature.as_bytes().to_vec())
    }
}

fn dtls_identity(
    entropy: &mut EntropyConsumer,
) -> Result<(str0m::crypto::dtls::DtlsCert, Fingerprint), NegotiationError> {
    const P256_PKCS8_PREFIX: [u8; 35] = [
        0x30, 0x41, 0x02, 0x01, 0x00, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02,
        0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x04, 0x27, 0x30, 0x25,
        0x02, 0x01, 0x01, 0x04, 0x20,
    ];
    let mut seed = entropy.take::<32>(b"dtls identity");
    if let Some(first) = seed.first_mut() {
        *first &= 0x7f;
    }
    if seed.iter().all(|byte| *byte == 0)
        && let Some(last) = seed.last_mut()
    {
        *last = 1;
    }
    let mut private_key = Vec::with_capacity(P256_PKCS8_PREFIX.len().saturating_add(seed.len()));
    private_key.extend_from_slice(&P256_PKCS8_PREFIX);
    private_key.extend_from_slice(&seed);
    let key_pair = rcgen::KeyPair::try_from(private_key)
        .map_err(|_| NegotiationError(AcceptError::CryptographicFailure))?;
    let signing_key = DeterministicP256Key::new(&seed)?;
    let certificate = rcgen::CertificateParams::default()
        .self_signed(&signing_key)
        .map_err(|_| NegotiationError(AcceptError::CryptographicFailure))?;
    let certificate = certificate.der().to_vec();
    let fingerprint = Fingerprint {
        algorithm: "sha-256",
        value: Sha256::digest(&certificate).to_vec().into_boxed_slice(),
    };
    Ok((
        str0m::crypto::dtls::DtlsCert {
            certificate,
            private_key: key_pair.serialize_der(),
        },
        fingerprint,
    ))
}

#[allow(
    clippy::too_many_arguments,
    reason = "answer formatting consumes one immutable negotiated tuple"
)]
fn format_answer_section(
    raw: &[String],
    section: &SectionBuild,
    ice: &IceCredentials,
    fingerprint: &Fingerprint,
    local_candidates: &[is::Candidate],
    setup: Setup,
    bundle_tag: bool,
    feedback: [Option<PacketFeedbackKind>; 2],
) -> String {
    let disabled = raw.first().and_then(|line| line.split_whitespace().nth(1)) == Some("0")
        && !raw.iter().any(|line| line == "a=bundle-only");
    let fields = raw
        .first()
        .and_then(|line| line.strip_prefix("m="))
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>();
    // Rejected sections still need mappings for their offered dynamic payloads.
    let payloads = if disabled {
        fields
            .iter()
            .skip(3)
            .filter_map(|value| value.parse::<u8>().ok())
            .collect::<Vec<_>>()
    } else {
        section.accepted_payloads.clone()
    };
    let mut output = String::new();
    if section.facts.kind == SectionKind::Application {
        output.push_str(&format!(
            "m=application {} UDP/DTLS/SCTP webrtc-datachannel\r\n",
            if disabled { 0 } else { 9 }
        ));
    } else {
        output.push_str(&format!(
            "m={} {} {}",
            fields.first().copied().unwrap_or("audio"),
            if disabled { 0 } else { 9 },
            fields.get(2).copied().unwrap_or("UDP/TLS/RTP/SAVPF")
        ));
        for payload in &payloads {
            output.push_str(&format!(" {payload}"));
        }
        output.push_str("\r\n");
    }
    output.push_str("c=IN IP4 0.0.0.0\r\n");
    for line in raw.iter().skip(1) {
        let keep_payload = line
            .strip_prefix("a=rtpmap:")
            .or_else(|| line.strip_prefix("a=fmtp:"))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u8>().ok())
            .is_some_and(|payload| payloads.contains(&payload));
        let keep_feedback = line.strip_prefix("a=rtcp-fb:").is_some_and(|value| {
            let mut fields = value.split_whitespace();
            let payload = fields.next().unwrap_or_default();
            let applies = payload == "*"
                || payload
                    .parse::<u8>()
                    .is_ok_and(|payload| section.accepted_payloads.contains(&payload));
            let name = fields.next().unwrap_or_default();
            applies
                && match name {
                    "transport-cc" => {
                        (section.facts.direction.allows_send()
                            && feedback[0] == Some(PacketFeedbackKind::TransportWide))
                            || (section.facts.direction.allows_receive()
                                && feedback[1] == Some(PacketFeedbackKind::TransportWide))
                    }
                    "ccfb" => {
                        (section.facts.direction.allows_send()
                            && feedback[0] == Some(PacketFeedbackKind::Rfc8888))
                            || (section.facts.direction.allows_receive()
                                && feedback[1] == Some(PacketFeedbackKind::Rfc8888))
                    }
                    _ => true,
                }
        });
        let keep_extension = line
            .strip_prefix("a=extmap:")
            .and_then(|value| value.split_whitespace().nth(1))
            .is_some_and(|uri| {
                section
                    .facts
                    .extensions
                    .iter()
                    .any(|extension| extension.uri == uri)
            });
        let keep = line.starts_with("a=mid:")
            || line == "a=rtcp-mux"
            || line == "a=rtcp-mux-only"
            || line == "a=rtcp-rsize"
            || line.starts_with("a=rid:")
            || line.starts_with("a=simulcast:")
            || line.starts_with("a=sctp-port:")
            || line.starts_with("a=max-message-size:")
            || keep_payload
            || keep_feedback
            || keep_extension;
        if keep {
            let copied = if line == "a=rtcp-mux-only" {
                "a=rtcp-mux".to_owned()
            } else if line.starts_with("a=extmap:") {
                let uri = line.split_whitespace().nth(1).unwrap_or_default();
                section
                    .facts
                    .extensions
                    .iter()
                    .find(|extension| extension.uri == uri)
                    .map(|extension| format_extension(extension, section.facts.direction))
                    .unwrap_or_else(|| line.clone())
            } else if line.starts_with("a=rid:") || line.starts_with("a=simulcast:") {
                invert_direction(line)
            } else {
                line.clone()
            };
            output.push_str(&copied);
            output.push_str("\r\n");
        }
    }
    output.push_str(&format!(
        "a=ice-ufrag:{}\r\na=ice-pwd:{}\r\na=fingerprint:{} {}\r\na=setup:{}\r\n",
        ice.ufrag,
        ice.password,
        fingerprint.algorithm,
        fingerprint
            .value
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":"),
        setup
    ));
    output.push_str(match section.facts.direction {
        Direction::SendOnly => "a=sendonly\r\n",
        Direction::ReceiveOnly => "a=recvonly\r\n",
        Direction::Bidirectional => "a=sendrecv\r\n",
        Direction::Inactive => "a=inactive\r\n",
    });
    if bundle_tag {
        for candidate in local_candidates {
            output.push_str("a=");
            output.push_str(&candidate.to_sdp_string());
            output.push_str("\r\n");
        }
        output.push_str("a=end-of-candidates\r\n");
    }
    output
}

fn invert_direction(line: &str) -> String {
    if let Some(value) = line.strip_prefix("a=rid:") {
        let mut fields = value.split_whitespace();
        let id = fields.next().unwrap_or_default();
        let direction = invert_direction_token(fields.next().unwrap_or_default());
        let restrictions = fields.collect::<Vec<_>>().join(" ");
        return if restrictions.is_empty() {
            format!("a=rid:{id} {direction}")
        } else {
            format!("a=rid:{id} {direction} {restrictions}")
        };
    }
    let value = line.strip_prefix("a=simulcast:").unwrap_or_default();
    let fields = value.split_whitespace().collect::<Vec<_>>();
    let mut output = String::from("a=simulcast:");
    for (index, pair) in fields.as_chunks::<2>().0.iter().enumerate() {
        let [direction, alternatives] = pair;
        if index != 0 {
            output.push(' ');
        }
        output.push_str(invert_direction_token(direction));
        output.push(' ');
        output.push_str(alternatives);
    }
    output
}

fn invert_direction_token(direction: &str) -> &str {
    match direction {
        "send" => "recv",
        "recv" => "send",
        _ => direction,
    }
}

fn format_extension(extension: &HeaderExtensionFacts, media_direction: Direction) -> String {
    let direction = if extension.direction == inherited_extension_direction(media_direction) {
        ""
    } else {
        match extension.direction {
            Direction::SendOnly => "/sendonly",
            Direction::ReceiveOnly => "/recvonly",
            Direction::Bidirectional => "/sendrecv",
            Direction::Inactive => "/inactive",
        }
    };
    let attributes = extension
        .attributes
        .as_deref()
        .map(|value| format!(" {value}"))
        .unwrap_or_default();
    format!(
        "a=extmap:{}{} {}{}",
        extension.id, direction, extension.uri, attributes
    )
}

fn format_answer(mids: &[String], sections: &[String], allow_mixed: bool) -> String {
    let mut output = format!(
        "v=0\r\no=- 0 2 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE {}\r\na=ice-lite\r\n",
        mids.join(" ")
    );
    if allow_mixed {
        output.push_str("a=extmap-allow-mixed\r\n");
    }
    for section in sections {
        output.push_str(section);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Connection, ConnectionEntropy};
    use std::{net::SocketAddr, time::Instant};

    fn at() -> Instant {
        Instant::now()
    }

    fn config() -> ConnectionConfig {
        ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp(SocketAddr::from((
                [192, 0, 2, 1],
                5000,
            )))],
            ..ConnectionConfig::default()
        }
    }
    fn fixture(name: &str) -> SdpOffer {
        SdpOffer::new(if name == "chrome" {
            include_str!("../tests/fixtures/chrome-representative.sdp")
        } else {
            include_str!("../tests/fixtures/firefox-representative.sdp")
        })
    }

    #[test]
    fn ice_lite_accepts_candidate_free_offers() {
        for name in ["chrome", "firefox"] {
            let value = fixture(name)
                .as_str()
                .lines()
                .filter(|line| !line.starts_with("a=candidate:"))
                .collect::<Vec<_>>()
                .join("\r\n");
            assert!(!value.contains("a=candidate:"));
            let result = Connection::accept(
                config(),
                SdpOffer::new(value),
                at(),
                std::time::UNIX_EPOCH,
                ConnectionEntropy::new([8; 32]),
            )
            .expect("ICE-lite accepts candidate-free browser offers");
            assert_eq!(result.session.offered_media.len(), 3);
        }
    }

    #[test]
    fn offered_rtx_mapping_survives_primary_codec_normalization() {
        let offer = SdpOffer::new(
            fixture("chrome")
                .as_str()
                .replace("a=sendonly", "a=sendrecv"),
        );
        let mut entropy = EntropyConsumer::new(ConnectionEntropy::new([8; 32]));
        let result = negotiate_inner(&config(), &offer, at(), &mut entropy).expect("negotiate RTX");
        assert!(
            result
                .answer
                .as_str()
                .contains("m=video 9 UDP/TLS/RTP/SAVPF 102 121")
        );
        assert!(result.answer.as_str().contains("a=rtpmap:121 rtx/90000"));
        assert!(result.answer.as_str().contains("a=fmtp:121 apt=102"));
    }

    #[test]
    fn extension_policy_keeps_media_values_and_regenerates_transport_values() {
        let offer = SdpOffer::new(
            fixture("chrome")
                .as_str()
                .replace("a=sendonly", "a=sendrecv")
                .replace(
                    "a=mid:audio",
                    concat!(
                        "a=mid:audio\n",
                        "a=extmap:10 urn:example:opaque\n",
                        "a=extmap:11 urn:3gpp:video-orientation\n",
                        "a=extmap:12 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\n",
                        "a=extmap:13/sendonly urn:example:receive-only",
                    ),
                ),
        );
        for policy in [
            crate::UnknownExtensionPolicy::Drop,
            crate::UnknownExtensionPolicy::Opaque,
        ] {
            let mut configured = config();
            configured.unknown_extension_policy = policy;
            let mut entropy = EntropyConsumer::new(ConnectionEntropy::new([8; 32]));
            let result = negotiate_inner(&configured, &offer, at(), &mut entropy)
                .expect("negotiate extensions");
            let opaque = policy == crate::UnknownExtensionPolicy::Opaque;
            assert_eq!(
                result.answer.as_str().contains("urn:example:opaque"),
                opaque
            );
            assert!(
                result
                    .answer
                    .as_str()
                    .contains("urn:3gpp:video-orientation")
            );
            let senders = result.facts.egress_senders();
            let audio = senders
                .iter()
                .find(|sender| sender.kind == crate::MediaKind::Audio)
                .expect("audio sender");
            let forwarded = |uri: &str| {
                audio
                    .forwarded_extensions
                    .iter()
                    .any(|(_, candidate)| &**candidate == uri)
            };
            assert_eq!(forwarded("urn:example:opaque"), opaque);
            assert!(forwarded("urn:ietf:params:rtp-hdrext:ssrc-audio-level"));
            assert!(forwarded("urn:3gpp:video-orientation"));
            assert!(forwarded(
                "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time"
            ));
            assert!(!forwarded("urn:example:receive-only"));
            assert!(!forwarded(
                "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time"
            ));
            assert!(!forwarded(MID_URI));
            assert!(!forwarded(TWCC_URIS[0]));
            assert!(senders.iter().all(|sender| {
                sender
                    .forwarded_extensions
                    .iter()
                    .all(|(_, uri)| &**uri != MID_URI && !TWCC_URIS.contains(&uri.as_ref()))
            }));
            let video = senders
                .iter()
                .find(|sender| sender.kind == crate::MediaKind::Video)
                .expect("video sender");
            for uri in [
                DEPENDENCY_URIS[1],
                "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00",
                PLAYOUT_DELAY_URI,
            ] {
                assert!(
                    video
                        .forwarded_extensions
                        .iter()
                        .any(|(_, value)| value.as_ref() == uri)
                );
            }
            let inbound = result.facts.ingress_media();
            assert_eq!(
                inbound
                    .iter()
                    .flat_map(|media| media.extensions.iter())
                    .any(|(_, uri)| &**uri == "urn:example:receive-only"),
                opaque
            );
        }
    }

    #[test]
    fn application_only_offer_has_no_rtp_feedback_or_senders() {
        let source = fixture("chrome");
        let (header, _) = source
            .as_str()
            .split_once("m=audio")
            .expect("session header");
        let (_, application) = source
            .as_str()
            .split_once("m=application")
            .expect("SCTP section");
        let offer = SdpOffer::new(format!(
            "{}m=application{}a=candidate:1 1 UDP 2130706431 192.0.2.1 50000 typ host\n",
            header.replace("a=group:BUNDLE audio video data", "a=group:BUNDLE data"),
            application,
        ));
        let accepted = Connection::accept(
            config(),
            offer,
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([7; 32]),
        )
        .expect("application-only admission");
        assert_eq!(accepted.session.feedback, None);
        assert!(accepted.session.inbound_media.is_empty());
        assert!(accepted.session.senders.is_empty());
        assert_eq!(accepted.connection.stats().connection.feedback, None);
        assert!(accepted.answer.as_str().contains("a=sctp-port:"));
        assert!(!accepted.answer.as_str().contains("m=audio"));
        assert!(!accepted.answer.as_str().contains("m=video"));
        assert!(!accepted.answer.as_str().contains("transport-cc"));
    }

    #[test]
    fn representative_browser_offers_are_accepted() {
        for (name, expected_feedback) in [
            ("chrome", None),
            ("firefox", Some(PacketFeedbackKind::TransportWide)),
        ] {
            let result = Connection::accept(
                config(),
                fixture(name),
                at(),
                std::time::UNIX_EPOCH,
                ConnectionEntropy::new([7; 32]),
            );
            let accepted = match result {
                Ok(accepted) => accepted,
                Err(error) => panic!("representative offer failed: {error}"),
            };
            assert_eq!(accepted.session.feedback, expected_feedback);
            assert!(accepted.answer.as_str().contains("a=ice-lite"));
            assert!(accepted.answer.as_str().contains("a=setup:passive"));
            assert!(accepted.answer.as_str().contains("a=end-of-candidates"));
            for sender in accepted.connection.stats().senders {
                assert!(
                    accepted
                        .answer
                        .as_str()
                        .contains(&format!("a=ssrc:{} cname:", sender.ssrc))
                );
                let negotiated = accepted
                    .session
                    .senders
                    .iter()
                    .find(|info| info.mid == sender.mid)
                    .unwrap();
                if negotiated.codec.retransmission_payload_type.is_some() {
                    assert!(
                        accepted
                            .answer
                            .as_str()
                            .contains(&format!("a=ssrc-group:FID {} ", sender.ssrc))
                    );
                }
            }
        }
    }

    #[test]
    fn h264_profile_facts_preserve_omission_and_reject_invalid_wire_parameters() {
        assert_eq!(
            parse_codec_profile(&[], 102, SectionKind::Video).unwrap(),
            crate::CodecProfile::H264 {
                profile_level_id: None,
                packetization_mode: 0,
                level_asymmetry_allowed: false,
            }
        );
        for parameter in [
            "profile-level-id=+12345",
            "packetization-mode=2",
            "level-asymmetry-allowed=2",
        ] {
            let raw = vec![format!("a=fmtp:102 {parameter}")];
            assert!(parse_codec_profile(&raw, 102, SectionKind::Video).is_err());
        }
    }

    #[test]
    fn session_resources_use_global_media_section_coordinates() {
        let offer = SdpOffer::new(
            fixture("firefox")
                .as_str()
                .replace(
                    "a=group:BUNDLE 0 1 2",
                    "a=group:BUNDLE audioIn videoOut data",
                )
                .replace("a=mid:0", "a=mid:audioIn")
                .replace("a=mid:1", "a=mid:videoOut")
                .replace("a=mid:2", "a=mid:data"),
        );
        let accepted = Connection::accept(
            config(),
            offer,
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([8; 32]),
        )
        .unwrap();

        assert_eq!(accepted.session.inbound_media.len(), 1);
        let media = accepted.session.inbound_for_sender_index(0).unwrap();
        assert_eq!(media.sender_index, 0);
        assert_eq!(media.kind, crate::MediaKind::Audio);
        assert_eq!(media.mid.as_ref(), "audioIn");
        assert!(media.rids.is_empty());
        let sender = accepted.session.senders.first().unwrap();
        assert_eq!(sender.mid.as_ref(), "videoOut");
        assert_eq!(sender.receiver_index, 1);
        assert_eq!(Some(sender.receiver_index), Some(1));
        assert_eq!(accepted.answer.as_str().matches("\nm=").count(), 3);
    }

    #[test]
    fn acceptance_requires_candidates_before_offer_parsing() {
        assert_eq!(
            Connection::accept(
                ConnectionConfig::default(),
                SdpOffer::new("not an SDP offer"),
                at(),
                std::time::UNIX_EPOCH,
                ConnectionEntropy::new([7; 32]),
            )
            .err(),
            Some(AcceptError::InvalidConfiguration)
        );
    }

    #[test]
    fn local_candidates_are_converted_once_retained_and_advertised_in_order() {
        let configured = vec![
            LocalCandidate::Udp(SocketAddr::from(([192, 0, 2, 10], 5000))),
            LocalCandidate::TcpPassive(SocketAddr::from((
                "2001:db8::10".parse::<std::net::Ipv6Addr>().unwrap(),
                5001,
            ))),
            LocalCandidate::Udp(SocketAddr::from((
                "2001:db8::11".parse::<std::net::Ipv6Addr>().unwrap(),
                5002,
            ))),
            LocalCandidate::TcpPassive(SocketAddr::from(([169, 254, 1, 1], 5003))),
        ];
        let mixed_config = ConnectionConfig {
            local_candidates: configured.clone(),
            ..ConnectionConfig::default()
        };
        let mut entropy = EntropyConsumer::new(ConnectionEntropy::new([8; 32]));
        let result =
            negotiate_inner(&mixed_config, &fixture("firefox"), at(), &mut entropy).unwrap();
        assert_eq!(result.facts.local_candidates.len(), configured.len());

        let advertised = result
            .answer
            .as_str()
            .lines()
            .filter_map(|line| line.strip_prefix("a=candidate:"))
            .map(|line| format!("candidate:{line}"))
            .collect::<Vec<_>>();
        let retained = result
            .facts
            .local_candidates
            .iter()
            .map(is::Candidate::to_sdp_string)
            .collect::<Vec<_>>();
        assert_eq!(advertised, retained);
        let (before_end, after_end) = result
            .answer
            .as_str()
            .split_once("a=end-of-candidates")
            .unwrap();
        assert!(before_end.contains("a=candidate:"));
        assert!(!after_end.contains("a=candidate:"));

        for (candidate, configured) in result
            .facts
            .local_candidates
            .iter()
            .zip(configured.iter().copied())
        {
            assert_eq!(candidate.kind(), is::CandidateKind::Host);
            assert_eq!(
                candidate.to_sdp_string().split_whitespace().nth(1),
                Some("1")
            );
            assert_eq!(candidate.addr(), configured.address());
            match configured {
                LocalCandidate::Udp(_) => {
                    assert_eq!(candidate.proto(), str0m::net::Protocol::Udp);
                    assert_eq!(candidate.tcptype(), None);
                }
                LocalCandidate::TcpPassive(_) => {
                    assert_eq!(candidate.proto(), str0m::net::Protocol::Tcp);
                    assert_eq!(candidate.tcptype(), Some(str0m::net::TcpType::Passive));
                }
            }
        }
        let ufrag = format!("a=ice-ufrag:{}", result.facts.local_ice.ufrag);
        let password = format!("a=ice-pwd:{}", result.facts.local_ice.password);
        assert_eq!(result.answer.as_str().matches(ufrag.as_str()).count(), 3);
        assert_eq!(result.answer.as_str().matches(password.as_str()).count(), 3);

        let tcp_only = ConnectionConfig {
            local_candidates: vec![configured[1]],
            ..ConnectionConfig::default()
        };
        let accepted = Connection::accept(
            tcp_only,
            fixture("chrome"),
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([8; 32]),
        )
        .unwrap();
        assert!(accepted.answer.as_str().contains(" tcptype passive"));

        let sixteen = ConnectionConfig {
            local_candidates: (0..16)
                .map(|offset| {
                    LocalCandidate::Udp(SocketAddr::from(([192, 0, 2, 20], 6000 + offset)))
                })
                .collect(),
            ..ConnectionConfig::default()
        };
        let accepted = Connection::accept(
            sixteen,
            fixture("firefox"),
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([8; 32]),
        )
        .unwrap();
        assert_eq!(
            accepted
                .answer
                .as_str()
                .lines()
                .filter(|line| line.starts_with("a=candidate:"))
                .count(),
            16
        );

        let relay_offer = SdpOffer::new(fixture("chrome").as_str().replace(
            "a=candidate:1 1 UDP 2130706431 192.0.2.1 50000 typ host",
            "a=candidate:2 1 udp 16777215 203.0.113.10 3478 typ relay raddr 192.0.2.1 rport 50000",
        ));
        let mut entropy = EntropyConsumer::new(ConnectionEntropy::new([8; 32]));
        let result = negotiate_inner(&config(), &relay_offer, at(), &mut entropy).unwrap();
        assert!(
            result.facts.remote_candidates[0]
                .split_whitespace()
                .any(|field| field == "relay")
        );
    }

    #[test]
    fn incompatible_optional_extensions_are_omitted_without_weakening_structure() {
        let optional = format!("a=extmap:6/recvonly {PLAYOUT_DELAY_URI}");
        let raw = vec![optional.clone(), format!("a=extmap:1 {MID_URI}")];
        let extensions = parse_extensions(&raw, false, Direction::ReceiveOnly).unwrap();
        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions[0].uri, MID_URI);
        for invalid in [
            format!("a=extmap:6 {MID_URI}"),
            format!("a=extmap:7 {PLAYOUT_DELAY_URI}"),
            format!("a=extmap:7/invalid {PLAYOUT_DELAY_URI}"),
            format!("a=extmap:7/recvonly/extra {PLAYOUT_DELAY_URI}"),
            format!("a=extmap:0/recvonly {PLAYOUT_DELAY_URI}"),
        ] {
            assert!(
                parse_extensions(
                    &[optional.clone(), invalid.clone()],
                    false,
                    Direction::ReceiveOnly
                )
                .is_err(),
                "{invalid}"
            );
        }
        for uri in [MID_URI, TWCC_URIS[0], TWCC_URIS[1]] {
            assert!(
                parse_extensions(
                    &[format!("a=extmap:6/recvonly {uri}")],
                    false,
                    Direction::ReceiveOnly,
                )
                .is_err(),
                "{uri}"
            );
        }
    }

    #[test]
    fn firefox_unusable_playout_mapping_is_absent_from_the_answer() {
        let offer = SdpOffer::new(fixture("firefox").as_str().replace(
            "a=sendonly",
            &format!("a=sendonly\na=extmap:6/recvonly {PLAYOUT_DELAY_URI}"),
        ));
        let accepted = Connection::accept(
            config(),
            offer,
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([10; 32]),
        )
        .unwrap();
        assert!(
            !accepted
                .answer
                .as_str()
                .split("m=video")
                .next()
                .unwrap()
                .contains(PLAYOUT_DELAY_URI)
        );
    }

    #[test]
    fn extension_directions_control_twcc_and_answer_serialization() {
        let mut entropy = EntropyConsumer::new(ConnectionEntropy::new([9; 32]));
        let inherited =
            negotiate_inner(&config(), &fixture("firefox"), at(), &mut entropy).unwrap();
        assert!(
            inherited.facts.media[0]
                .extensions
                .iter()
                .all(|extension| extension.direction == Direction::ReceiveOnly)
        );
        assert!(
            inherited.facts.media[1]
                .extensions
                .iter()
                .all(|extension| extension.direction == Direction::SendOnly)
        );

        let base = fixture("firefox").as_str().replace(
            "a=recvonly\na=rtcp-mux\na=rtpmap:120",
            "a=sendrecv\na=rtcp-mux\na=rtpmap:120",
        );
        let direction_base = base
            .replace(
                "a=rtcp-fb:109 transport-cc",
                "a=rtcp-fb:109 transport-cc\na=rtcp-fb:109 ccfb",
            )
            .replace(
                "a=rtcp-fb:120 transport-cc",
                "a=rtcp-fb:120 transport-cc\na=rtcp-fb:120 ccfb",
            );
        for (offered, answered, expected) in [
            ("", "", Direction::Bidirectional),
            ("/sendrecv", "", Direction::Bidirectional),
            ("/sendonly", "/recvonly", Direction::ReceiveOnly),
            ("/recvonly", "/sendonly", Direction::SendOnly),
            ("/inactive", "/inactive", Direction::Inactive),
        ] {
            let offer = SdpOffer::new(direction_base.replace(
                "a=extmap:5 http://www.webrtc.org/experiments/rtp-hdrext/video-dependency-descriptor",
                &format!("a=extmap:5{offered} http://www.webrtc.org/experiments/rtp-hdrext/video-dependency-descriptor"),
            ));
            let mut entropy = EntropyConsumer::new(ConnectionEntropy::new([9; 32]));
            let result = negotiate_inner(&config(), &offer, at(), &mut entropy).unwrap();
            let extension = result.facts.media[1]
                .extensions
                .iter()
                .find(|extension| {
                    extension.uri
                        == "http://www.webrtc.org/experiments/rtp-hdrext/video-dependency-descriptor"
                })
                .unwrap();
            assert_eq!(extension.direction, expected);
            let sender = result
                .session
                .senders
                .iter()
                .find(|sender| sender.receiver_index == 1)
                .unwrap();
            let mapping = sender
                .extensions
                .iter()
                .find(|mapping| mapping.uri.as_ref() == extension.uri);
            assert_eq!(
                mapping.map(|mapping| mapping.id),
                expected.allows_send().then_some(5)
            );
            assert!(
                result.answer.as_str().contains(&format!(
                    "a=extmap:5{answered} http://www.webrtc.org/experiments/rtp-hdrext/video-dependency-descriptor"
                )),
                "offered {offered}, answered {answered}: {}",
                result.answer.as_str()
            );
        }

        for direction in ["sendonly", "inactive"] {
            let without_fallback = SdpOffer::new(base.replace(
                "a=extmap:3 http://www.webrtc.org/experiments/rtp-hdrext/transport-wide-cc-01",
                &format!("a=extmap:3/{direction} http://www.webrtc.org/experiments/rtp-hdrext/transport-wide-cc-01"),
            ));
            assert_eq!(
                Connection::accept(
                    config(),
                    without_fallback,
                    at(),
                    std::time::UNIX_EPOCH,
                    ConnectionEntropy::new([10; 32])
                )
                .err(),
                Some(AcceptError::MissingPacketFeedback)
            );

            let with_fallback = SdpOffer::new(
                base.replace(
                    "a=rtcp-fb:109 transport-cc",
                    "a=rtcp-fb:109 transport-cc\na=rtcp-fb:109 ccfb",
                )
                .replace(
                    "a=rtcp-fb:120 transport-cc",
                    "a=rtcp-fb:120 transport-cc\na=rtcp-fb:120 ccfb",
                )
                .replace(
                    "a=extmap:3 http://www.webrtc.org/experiments/rtp-hdrext/transport-wide-cc-01",
                    &format!("a=extmap:3/{direction} http://www.webrtc.org/experiments/rtp-hdrext/transport-wide-cc-01"),
                ),
            );
            let accepted = Connection::accept(
                config(),
                with_fallback,
                at(),
                std::time::UNIX_EPOCH,
                ConnectionEntropy::new([10; 32]),
            )
            .unwrap();
            assert_eq!(accepted.session.feedback, Some(PacketFeedbackKind::Rfc8888));
        }

        let incompatible = SdpOffer::new(fixture("firefox").as_str().replace(
            "a=extmap:3 http://www.webrtc.org/experiments/rtp-hdrext/transport-wide-cc-01",
            "a=extmap:3/sendonly http://www.webrtc.org/experiments/rtp-hdrext/transport-wide-cc-01",
        ));
        assert_eq!(
            Connection::accept(
                config(),
                incompatible,
                at(),
                std::time::UNIX_EPOCH,
                ConnectionEntropy::new([10; 32])
            )
            .err(),
            Some(AcceptError::InvalidOffer)
        );

        let one_ineligible = SdpOffer::new(
            base.replace("a=sendonly\na=rtcp-mux", "a=recvonly\na=rtcp-mux")
                .replace(
                    "a=rtcp-fb:109 transport-cc",
                    "a=rtcp-fb:109 transport-cc\na=rtcp-fb:109 ccfb",
                )
                .replace(
                    "a=rtcp-fb:120 transport-cc",
                    "a=rtcp-fb:120 transport-cc\na=rtcp-fb:120 ccfb",
                )
                .replacen(
                    "a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time",
                    "a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time\na=extmap:3/inactive http://www.webrtc.org/experiments/rtp-hdrext/transport-wide-cc-01",
                    1,
                ),
        );
        let accepted = Connection::accept(
            config(),
            one_ineligible,
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([10; 32]),
        )
        .unwrap();
        assert_eq!(accepted.session.feedback, Some(PacketFeedbackKind::Rfc8888));

        let no_active_rtp = SdpOffer::new(
            fixture("chrome")
                .as_str()
                .replace("a=sendonly", "a=inactive"),
        );
        assert_eq!(
            Connection::accept(
                config(),
                no_active_rtp,
                at(),
                std::time::UNIX_EPOCH,
                ConnectionEntropy::new([10; 32])
            )
            .err(),
            Some(AcceptError::MissingPacketFeedback)
        );

        let inactive_inheritance = SdpOffer::new(fixture("firefox").as_str().replacen(
            "a=sendonly",
            "a=inactive",
            1,
        ));
        let mut entropy = EntropyConsumer::new(ConnectionEntropy::new([10; 32]));
        let result = negotiate_inner(&config(), &inactive_inheritance, at(), &mut entropy).unwrap();
        assert_eq!(
            result.facts.media[0].extensions[0].direction,
            Direction::Bidirectional
        );
    }

    #[test]
    fn inbound_rids_preserve_send_topology_order_and_paused_alternatives() {
        let raw = [
            "a=rid:f send",
            "a=rid:q send",
            "a=rid:h send",
            "a=rid:remote recv",
            "a=simulcast:recv remote send q;~h,f",
        ]
        .map(str::to_owned);
        assert_eq!(inbound_rids(&raw).unwrap(), ["q", "h", "f"]);
        assert_eq!(inbound_rids(&raw[..4]).unwrap(), ["f", "q", "h"]);
        assert!(inbound_rids(&raw[3..4]).unwrap().is_empty());
    }

    #[test]
    fn inbound_simulcast_rejects_undeclared_and_wrong_direction_rids() {
        for topology in ["q;phantom", "q;remote", "q;", "~~q"] {
            let raw = vec![
                "a=rid:q send".to_owned(),
                "a=rid:remote recv".to_owned(),
                format!("a=simulcast:send {topology}"),
            ];
            assert!(inbound_rids(&raw).is_err(), "{topology}");
        }
        let raw = (0..=MAX_RIDS)
            .map(|index| format!("a=rid:r{index} send"))
            .collect::<Vec<_>>();
        assert!(inbound_rids(&raw).is_err());
    }

    #[test]
    fn rid_and_every_simulcast_direction_group_are_inverted_by_syntax() {
        let chrome = Connection::accept(
            config(),
            fixture("chrome"),
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([11; 32]),
        )
        .unwrap();
        assert!(chrome.answer.as_str().contains("a=rid:q recv"));
        assert!(chrome.answer.as_str().contains("a=simulcast:recv q;h;f"));
        let video = chrome
            .session
            .inbound_media
            .iter()
            .find(|media| media.kind == crate::MediaKind::Video)
            .unwrap();
        assert_eq!(
            video.rids.iter().map(AsRef::as_ref).collect::<Vec<&str>>(),
            ["q", "h", "f"]
        );
        assert_eq!(video.primary_ssrcs.as_ref(), [11_111_111]);

        let firefox = fixture("firefox");
        let two_directions = SdpOffer::new(
            firefox
                .as_str()
                .replace("a=rid:low recv", "a=rid:sender_recv recv max-width=1280")
                .replace("a=rid:high recv", "a=rid:receiver_send send max-width=640")
                .replace(
                    "a=simulcast:recv low;high",
                    "a=simulcast:recv sender_recv send ~receiver_send",
                ),
        );
        let accepted = Connection::accept(
            config(),
            two_directions,
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([12; 32]),
        )
        .unwrap();
        assert!(
            accepted
                .answer
                .as_str()
                .contains("a=rid:sender_recv send max-width=1280")
        );
        assert!(
            accepted
                .answer
                .as_str()
                .contains("a=rid:receiver_send recv max-width=640")
        );
        assert!(
            accepted
                .answer
                .as_str()
                .contains("a=simulcast:send sender_recv recv ~receiver_send")
        );
    }

    #[test]
    fn deterministic_entropy_produces_deterministic_secret_safe_answers() {
        let first_result = Connection::accept(
            config(),
            fixture("chrome"),
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([11; 32]),
        );
        let first = match first_result {
            Ok(accepted) => accepted,
            Err(error) => panic!("first acceptance failed: {error}"),
        };
        let second_result = Connection::accept(
            config(),
            fixture("chrome"),
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([11; 32]),
        );
        let second = match second_result {
            Ok(accepted) => accepted,
            Err(error) => panic!("second acceptance failed: {error}"),
        };
        let different_result = Connection::accept(
            config(),
            fixture("chrome"),
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([12; 32]),
        );
        let different = match different_result {
            Ok(accepted) => accepted,
            Err(error) => panic!("different-entropy acceptance failed: {error}"),
        };
        assert_eq!(first.answer, second.answer);
        assert_ne!(first.answer, different.answer);
        assert!(!format!("{:?}", first.answer).contains("ice-pwd"));
        assert!(!format!("{:?}", fixture("chrome")).contains("chromeufrag"));
    }

    #[test]
    fn feedback_modes_and_twcc_extension_directions_are_independent() {
        for (offered, outbound, inbound, extension_direction) in [
            (
                "sendonly",
                PacketFeedbackKind::Rfc8888,
                PacketFeedbackKind::TransportWide,
                Direction::ReceiveOnly,
            ),
            (
                "recvonly",
                PacketFeedbackKind::TransportWide,
                PacketFeedbackKind::Rfc8888,
                Direction::SendOnly,
            ),
        ] {
            let offer = SdpOffer::new(
                fixture("chrome")
                    .as_str()
                    .replace("a=sendonly", "a=sendrecv")
                    .replace(
                        "a=rtcp-fb:111 transport-cc",
                        "a=rtcp-fb:111 transport-cc\na=rtcp-fb:111 ccfb",
                    )
                    .replace(
                        "a=rtcp-fb:102 transport-cc",
                        "a=rtcp-fb:102 transport-cc\na=rtcp-fb:102 ccfb",
                    )
                    .replace("a=extmap:3 ", &format!("a=extmap:3/{offered} "))
                    .replace("a=extmap:4 ", &format!("a=extmap:4/{offered} ")),
            );
            let mut entropy = EntropyConsumer::new(ConnectionEntropy::new([9; 32]));
            let negotiated = negotiate_inner(&config(), &offer, at(), &mut entropy).unwrap();
            assert_eq!(negotiated.facts.feedback(), Some(outbound));
            assert_eq!(negotiated.facts.inbound_feedback(), Some(inbound));
            let extensions = negotiated
                .facts
                .media
                .iter()
                .flat_map(|media| media.extensions.iter())
                .filter(|extension| TWCC_URIS.contains(&extension.uri.as_str()))
                .collect::<Vec<_>>();
            assert_eq!(extensions.len(), 2);
            assert!(
                extensions
                    .iter()
                    .all(|extension| extension.direction == extension_direction)
            );
            assert!(
                negotiated
                    .answer
                    .as_str()
                    .contains("a=rtcp-fb:102 transport-cc")
            );
            assert!(negotiated.answer.as_str().contains("a=rtcp-fb:102 ccfb"));
            assert!(negotiated.answer.as_str().contains(&format!(
                "a=extmap:4/{} ",
                if offered == "sendonly" {
                    "recvonly"
                } else {
                    "sendonly"
                }
            )));
        }
    }

    #[test]
    fn feedback_selection_prefers_twcc_then_rfc8888() {
        let source = fixture("firefox");
        let both = SdpOffer::new(source.as_str().replace(
            "a=rtcp-fb:120 transport-cc",
            "a=rtcp-fb:120 transport-cc\r\na=rtcp-fb:120 ccfb",
        ));
        let both_result = Connection::accept(
            config(),
            both,
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([1; 32]),
        );
        let accepted = match both_result {
            Ok(accepted) => accepted,
            Err(error) => panic!("both modes failed: {error}"),
        };
        assert_eq!(
            accepted.session.feedback,
            Some(PacketFeedbackKind::TransportWide)
        );
        let rfc = SdpOffer::new(
            source
                .as_str()
                .replace("a=rtcp-fb:120 transport-cc", "a=rtcp-fb:120 ccfb"),
        );
        let rfc_result = Connection::accept(
            config(),
            rfc,
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([2; 32]),
        );
        let accepted = match rfc_result {
            Ok(accepted) => accepted,
            Err(error) => panic!("RFC 8888 failed: {error}"),
        };
        assert_eq!(accepted.session.feedback, Some(PacketFeedbackKind::Rfc8888));
    }

    #[test]
    fn rejected_video_preserves_dynamic_payload_mappings() {
        let source = fixture("chrome");
        assert!(source.as_str().contains("m=video 9 "));
        let offer = SdpOffer::new(source.as_str().replace("m=video 9 ", "m=video 0 "));
        let accepted = Connection::accept(
            config(),
            offer,
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([14; 32]),
        )
        .expect("a rejected media section is a valid initial offer");
        let answer = accepted.answer.as_str();
        assert!(answer.contains("m=video 0 UDP/TLS/RTP/SAVPF 102 121"));
        assert!(answer.contains("a=rtpmap:102"));
        assert!(answer.contains("a=rtpmap:121"));
        Sdp::parse(answer).expect("the answer retains mappings for rejected dynamic payloads");
    }

    #[test]
    fn accepts_zero_port_bundle_only_sections() {
        let offer = SdpOffer::new(fixture("firefox").as_str().replace(
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\nc=IN IP4 0.0.0.0",
            "m=application 0 UDP/DTLS/SCTP webrtc-datachannel\nc=IN IP4 0.0.0.0\na=bundle-only",
        ));
        let accepted = Connection::accept(
            config(),
            offer,
            at(),
            std::time::UNIX_EPOCH,
            ConnectionEntropy::new([13; 32]),
        )
        .expect("BUNDLE-only sections use the shared transport despite port zero");
        assert!(
            accepted
                .answer
                .as_str()
                .contains("m=application 9 UDP/DTLS/SCTP webrtc-datachannel")
        );
    }

    #[test]
    fn rejects_missing_feedback_and_transport_requirements() {
        let source = fixture("firefox");
        let chrome = fixture("chrome");
        let no_feedback = SdpOffer::new(
            chrome
                .as_str()
                .lines()
                .filter(|line| !line.starts_with("a=rtcp-fb:"))
                .map(|line| {
                    if line == "a=sendonly" {
                        "a=recvonly"
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\r\n")
                + "\r\n",
        );
        assert_eq!(
            Connection::accept(
                config(),
                no_feedback,
                at(),
                std::time::UNIX_EPOCH,
                ConnectionEntropy::new([3; 32])
            )
            .err(),
            Some(AcceptError::MissingPacketFeedback)
        );
        let no_bundle = SdpOffer::new(source.as_str().replace("a=group:BUNDLE 0 1 2", ""));
        assert!(matches!(
            Connection::accept(
                config(),
                no_bundle,
                at(),
                std::time::UNIX_EPOCH,
                ConnectionEntropy::new([3; 32])
            ),
            Err(AcceptError::InvalidOffer)
        ));
        let remote_lite = SdpOffer::new(
            source
                .as_str()
                .replace("a=setup:actpass", "a=setup:actpass\r\na=ice-lite"),
        );
        assert!(matches!(
            Connection::accept(
                config(),
                remote_lite,
                at(),
                std::time::UNIX_EPOCH,
                ConnectionEntropy::new([3; 32])
            ),
            Err(AcceptError::UnsupportedSessionProfile)
        ));
    }

    #[test]
    fn input_bounds_are_enforced() {
        let oversized = SdpOffer::new("x".repeat(MAX_SDP_BYTES + 1));
        assert!(matches!(
            Connection::accept(
                config(),
                oversized,
                at(),
                std::time::UNIX_EPOCH,
                ConnectionEntropy::new([0; 32])
            ),
            Err(AcceptError::SessionLimitExceeded)
        ));
    }

    #[test]
    fn rejects_missing_rtcp_mux_and_dtls_fingerprint() {
        let source = fixture("chrome");
        let no_mux = SdpOffer::new(source.as_str().replacen("a=rtcp-mux", "", 1));
        assert_eq!(
            Connection::accept(
                config(),
                no_mux,
                at(),
                std::time::UNIX_EPOCH,
                ConnectionEntropy::new([4; 32])
            )
            .err(),
            Some(AcceptError::InvalidOffer)
        );
        let no_fingerprint = SdpOffer::new(
            source
                .as_str()
                .replace("a=fingerprint:sha-256 01:02:03:04:05:06:07:08:09:0a:0b:0c:0d:0e:0f:10:11:12:13:14:15:16:17:18:19:1a:1b:1c:1d:1e:1f:20", ""),
        );
        assert_eq!(
            Connection::accept(
                config(),
                no_fingerprint,
                at(),
                std::time::UNIX_EPOCH,
                ConnectionEntropy::new([4; 32])
            )
            .err(),
            Some(AcceptError::InvalidOffer)
        );
    }

    #[test]
    fn immutable_facts_retain_sctp_and_selected_limits() {
        let config = config();
        let mut entropy = EntropyConsumer::new(ConnectionEntropy::new([5; 32]));
        let result = match negotiate_inner(&config, &fixture("firefox"), at(), &mut entropy) {
            Ok(result) => result,
            Err(error) => panic!("firefox negotiation failed: {error:?}"),
        };
        let Some(sctp) = result
            .facts
            .media
            .iter()
            .find_map(|section| section.sctp.as_ref())
        else {
            panic!("SCTP facts missing");
        };
        assert_eq!(sctp.port, 5000);
        assert_eq!(sctp.max_message_size, Some(65_536));
        assert!(!sctp.unlimited_message_size);
        assert_eq!(result.facts.limits, config.limits);
        assert_eq!(
            result.facts.feedback,
            Some(PacketFeedbackKind::TransportWide)
        );
        assert!(!result.facts.dtls_identity.certificate.is_empty());
        assert!(!result.facts.dtls_identity.private_key.is_empty());
        let expected = Sha256::digest(&result.facts.dtls_identity.certificate);
        let expected: &[u8] = expected.as_ref();
        assert_eq!(result.facts.local_fingerprint.value.as_ref(), expected);
    }
}

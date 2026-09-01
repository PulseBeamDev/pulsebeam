use std::{
    collections::{HashMap, HashSet},
    fmt,
    num::NonZeroU32,
};

use str0m::{
    rtp_::Direction,
    sdp::{MediaAttribute, MediaType, Proto, Sdp, SessionAttribute, Setup},
};

use crate::{
    Codec, DataChannelParameters, DtlsFingerprint, DtlsRole, EgressSlot, H264Parameters,
    HeaderExtension, IceCandidate, IceCredentials, IngressStream, MaxMessageSize, MediaDirection,
    MediaKind, NegotiatedCodec, NegotiatedMedia, NegotiatedMediaSection, NegotiatedSession,
    NegotiationParameters, RtcConfiguration, RtcNegotiation, SdpAnswer, SsrcGroup,
};

const MAX_SDP_BYTES: usize = 256 * 1024;
const MAX_MEDIA_SECTIONS: usize = 1025;
const MAX_SECTION_ATTRIBUTES: usize = 1024;
const MAX_PAYLOAD_TYPES: usize = 256;
const MAX_EXTENSIONS: usize = 256;
const MAX_RIDS: usize = 256;
const MAX_RID_TOKEN_BYTES: usize = 32;
const MAX_SIMULCAST_ITEMS: usize = 512;
const MAX_SSRC_ATTRIBUTES: usize = 1024;
const MAX_SSRC_GROUPS: usize = 256;
const MAX_CANDIDATES: usize = 512;
const MAX_SESSION_ATTRIBUTES: usize = 2048;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegotiationError(String);

impl NegotiationError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for NegotiationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NegotiationError {}

pub fn negotiate(
    offer: &str,
    configuration: &RtcConfiguration,
    parameters: &NegotiationParameters,
) -> Result<RtcNegotiation, NegotiationError> {
    debug_assert!(configuration.max_ingress_streams() > 0);
    debug_assert!(configuration.max_egress_slots() > 0);
    if offer.len() > MAX_SDP_BYTES {
        return Err(NegotiationError::new("SDP exceeds maximum size"));
    }
    let parsed = Sdp::parse(offer).map_err(|e| {
        let message = e.to_string();
        if message.to_ascii_lowercase().contains("rtp_map") {
            NegotiationError::new(format!("payload: {message}"))
        } else {
            NegotiationError::new(format!("invalid SDP: {message}"))
        }
    })?;
    parsed
        .assert_consistency()
        .map_err(|e| consistency_error(e.to_string()))?;
    let raw_sections = raw_sections(offer)?;
    if raw_sections.len() != parsed.media_lines.len() || parsed.media_lines.is_empty() {
        return Err(NegotiationError::new(
            "invalid SDP: media sections are missing",
        ));
    }
    if parsed.media_lines.len() > MAX_MEDIA_SECTIONS {
        return Err(NegotiationError::new("SDP has too many media sections"));
    }
    if parsed.session.attrs.len() > MAX_SESSION_ATTRIBUTES {
        return Err(NegotiationError::new("SDP session has too many attributes"));
    }
    for line in &parsed.media_lines {
        if line.attrs.len() > MAX_SECTION_ATTRIBUTES || line.pts.len() > MAX_PAYLOAD_TYPES {
            return Err(NegotiationError::new("SDP media section is too large"));
        }
    }

    let mids = validate_bundle(&parsed)?;
    validate_bundle_rtp_namespaces(&parsed, &raw_sections, &mids)?;
    let allow_mixed_extmaps = parsed.session.attrs.iter().any(|attribute| {
        matches!(attribute, SessionAttribute::Unused(value) if value == "extmap-allow-mixed")
            || matches!(attribute, SessionAttribute::AllowMixedExts)
    });
    let remote_ice = parsed
        .ice_creds()
        .and_then(|v| IceCredentials::new(v.ufrag, v.pass))
        .ok_or_else(|| NegotiationError::new("missing ICE credentials"))?;
    let remote_fingerprint = parsed
        .fingerprint()
        .and_then(|v| valid_fingerprint(v.hash_func, v.bytes))
        .ok_or_else(|| NegotiationError::new("invalid DTLS fingerprint"))?;
    let remote_setup = parsed
        .setup()
        .ok_or_else(|| NegotiationError::new("missing DTLS setup"))?;
    validate_session_transport(&parsed)?;
    if parsed.session.ice_lite() {
        return Err(NegotiationError::new("remote ICE-lite is not supported"));
    }
    validate_transport_coherence(&parsed, &remote_ice, &remote_fingerprint, remote_setup)?;
    let answer_setup = match remote_setup {
        Setup::ActPass | Setup::Passive => Setup::Active,
        Setup::Active => Setup::Passive,
    };
    let local_dtls_role = match answer_setup {
        Setup::Active => DtlsRole::Active,
        Setup::Passive => DtlsRole::Passive,
        Setup::ActPass => {
            return Err(NegotiationError::new("invalid local DTLS role"));
        }
    };
    if parameters.local_candidates().is_empty() {
        return Err(NegotiationError::new(
            "at least one local ICE candidate is required",
        ));
    }
    let local_candidates = validate_candidates(parameters.local_candidates())?;
    let remote_candidates = remote_candidates(offer)?;

    let mut ingress_count = 0_u32;
    let mut egress_count = 0_u32;
    for line in &parsed.media_lines {
        if line.disabled {
            continue;
        }
        let kind = media_kind(&line.typ)?;
        if kind == MediaKind::Application {
            continue;
        }
        match line.direction() {
            Direction::SendOnly => {
                ingress_count = ingress_count
                    .checked_add(1)
                    .ok_or_else(|| NegotiationError::new("ingress capacity overflow"))?;
            }
            Direction::RecvOnly => {
                egress_count = egress_count
                    .checked_add(1)
                    .ok_or_else(|| NegotiationError::new("egress capacity overflow"))?;
            }
            Direction::SendRecv => {
                return Err(NegotiationError::new(format!(
                    "unsupported direction for {}",
                    line.mid()
                )));
            }
            Direction::Inactive => {}
        }
    }
    if ingress_count > configuration.max_ingress_streams()
        || egress_count > configuration.max_egress_slots()
    {
        return Err(NegotiationError::new(
            "media section count exceeds configured slot capacity",
        ));
    }

    let mut sections = Vec::with_capacity(parsed.media_lines.len());
    let mut media = Vec::with_capacity(parsed.media_lines.len());
    let mut validated_media = Vec::with_capacity(parsed.media_lines.len());
    let mut application_count = 0_u32;
    let mut global_ssrcs = HashSet::new();
    let mut answer_sections = Vec::with_capacity(parsed.media_lines.len());

    for (index, (line, raw)) in parsed
        .media_lines
        .iter()
        .zip(raw_sections.iter())
        .enumerate()
    {
        let mid = line.mid().to_string();
        debug_assert_eq!(mids.get(index), Some(&mid));
        let kind = media_kind(&line.typ)?;
        if kind == MediaKind::Application && !modern_application_m_line(raw) {
            return Err(NegotiationError::new(format!(
                "sctp: invalid application protocol on {mid}; expected UDP/DTLS/SCTP webrtc-datachannel"
            )));
        }
        let direction_attributes = line
            .attrs
            .iter()
            .filter(|attribute| {
                matches!(
                    attribute,
                    MediaAttribute::SendRecv
                        | MediaAttribute::SendOnly
                        | MediaAttribute::RecvOnly
                        | MediaAttribute::Inactive
                )
            })
            .count();
        if !line.disabled
            && kind != MediaKind::Application
            && !line.attrs.iter().any(|attribute| {
                matches!(
                    attribute,
                    MediaAttribute::SendRecv
                        | MediaAttribute::SendOnly
                        | MediaAttribute::RecvOnly
                        | MediaAttribute::Inactive
                )
            })
        {
            return Err(NegotiationError::new(format!(
                "missing media direction on {mid}"
            )));
        }
        if !line.disabled && kind == MediaKind::Application && direction_attributes > 1 {
            return Err(NegotiationError::new(format!(
                "sctp: duplicate application direction on {mid}"
            )));
        }
        if kind == MediaKind::Application && !line.disabled {
            application_count = application_count
                .checked_add(1)
                .ok_or_else(|| NegotiationError::new("SCTP association count overflow"))?;
            if application_count > 1 {
                return Err(NegotiationError::new(
                    "only one SCTP application association is supported",
                ));
            }
        }
        let remote_direction = if kind == MediaKind::Application && !line.disabled {
            match direction_attributes {
                0 => Direction::SendRecv,
                1 if line.direction() == Direction::SendRecv => Direction::SendRecv,
                _ => {
                    return Err(NegotiationError::new(format!(
                        "sctp: unsupported application direction on {mid}"
                    )));
                }
            }
        } else {
            line.direction()
        };
        let direction = if line.disabled {
            MediaDirection::Inactive
        } else {
            negotiated_direction(kind, remote_direction, &mid)?
        };
        let (
            codecs,
            payload_types,
            extensions,
            rids,
            receive_rids,
            ssrcs,
            has_ssrc_zero_probe,
            ssrc_groups,
            data_channel,
        ) = if line.disabled {
            (
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                false,
                Vec::new(),
                None,
            )
        } else {
            let (codecs, payload_types) = codecs(line, raw, &mid)?;
            let extensions = extensions(raw, &mid, allow_mixed_extmaps)?;
            let rids = rid_ids(raw, &mid, kind, remote_direction)?;
            validate_simulcast(raw, &mid, kind, remote_direction, &rids)?;
            let receive_rids =
                if kind == MediaKind::Video && direction == MediaDirection::ReceiveOnly {
                    rids.clone()
                } else {
                    Vec::new()
                };
            let (ssrcs, has_ssrc_zero_probe) = ssrc_ids(raw, &mid)?;
            let ssrc_groups = ssrc_groups(raw, &mid, &ssrcs, kind)?;
            for ssrc in &ssrcs {
                if *ssrc != 0 && !global_ssrcs.insert(*ssrc) {
                    return Err(NegotiationError::new(format!(
                        "duplicate SSRC ownership on {mid}"
                    )));
                }
            }
            if ssrc_groups
                .iter()
                .any(|group| group.semantics().eq_ignore_ascii_case("FID"))
                && !codecs
                    .iter()
                    .any(|codec| codec.retransmission_payload_type().is_some())
            {
                return Err(NegotiationError::new(format!(
                    "FID group has no RTX codec on {mid}"
                )));
            }
            let data_channel = data_channel(line, raw, kind, &mid, remote_direction)?;
            (
                codecs,
                payload_types,
                extensions,
                rids,
                receive_rids,
                ssrcs,
                has_ssrc_zero_probe,
                ssrc_groups,
                data_channel,
            )
        };
        let mut section_codecs = Vec::with_capacity(codecs.len());
        for codec in &codecs {
            let section_codec = Codec::new(
                codec.payload_type(),
                codec.name().to_owned(),
                codec.clock_rate(),
                codec.channels(),
            )
            .ok_or_else(|| NegotiationError::new("invalid negotiated codec"))?;
            section_codecs.push(section_codec);
        }

        sections.push(NegotiatedMediaSection::new(
            mid.clone(),
            kind,
            direction,
            section_codecs.into_boxed_slice(),
            extensions
                .iter()
                .map(|(id, uri, direction)| {
                    HeaderExtension::with_direction(*id, uri.clone(), *direction)
                        .ok_or_else(|| NegotiationError::new("invalid negotiated extension"))
                })
                .collect::<Result<Vec<_>, _>>()?
                .into_boxed_slice(),
            receive_rids
                .into_iter()
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            ssrcs.into_boxed_slice(),
            has_ssrc_zero_probe,
            ssrc_groups.into_boxed_slice(),
            data_channel,
        ));
        validated_media.push((mid.clone(), kind, direction, rids, codecs));
        answer_sections.push(format_answer_section(
            raw,
            kind,
            direction,
            &payload_types,
            parameters.local_ice(),
            parameters.local_fingerprint(),
            answer_setup,
            index == 0,
            &local_candidates,
            &extensions,
            line.disabled,
        ));
    }

    let mut ingress_id = 0_u32;
    let mut egress_id = 0_u32;
    for (mid, kind, direction, rids, codecs) in validated_media {
        let ingress = if kind != MediaKind::Application && direction == MediaDirection::ReceiveOnly
        {
            ingress_id = ingress_id
                .checked_add(1)
                .ok_or_else(|| NegotiationError::new("ingress identifier overflow"))?;
            IngressStream::new(ingress_id)
        } else {
            None
        };
        let egress = if kind != MediaKind::Application && direction == MediaDirection::SendOnly {
            egress_id = egress_id
                .checked_add(1)
                .ok_or_else(|| NegotiationError::new("egress identifier overflow"))?;
            EgressSlot::new(egress_id)
        } else {
            None
        };
        media.push(NegotiatedMedia::new(
            ingress,
            egress,
            mid,
            rids.into_boxed_slice(),
            kind,
            direction,
            codecs.into_boxed_slice(),
        ));
    }

    let session = NegotiatedSession::new(
        parameters.local_ice().clone(),
        remote_ice,
        local_candidates.into_boxed_slice(),
        remote_candidates.into_boxed_slice(),
        parameters.local_fingerprint().clone(),
        local_dtls_role,
        remote_fingerprint,
        sections.into_boxed_slice(),
    );
    let answer = format_answer(&mids, &answer_sections, allow_mixed_extmaps);
    let parsed_answer = Sdp::parse(&answer)
        .map_err(|error| NegotiationError::new(format!("invalid generated answer: {error}")))?;
    parsed_answer
        .assert_consistency()
        .map_err(|error| NegotiationError::new(format!("invalid generated answer: {error}")))?;
    Ok(RtcNegotiation::new(
        SdpAnswer::new(answer),
        media.into_boxed_slice(),
        session,
    ))
}

fn validate_transport_coherence(
    sdp: &Sdp,
    ice: &IceCredentials,
    fingerprint: &DtlsFingerprint,
    setup: Setup,
) -> Result<(), NegotiationError> {
    for line in &sdp.media_lines {
        let mut ufrag_count = 0_u8;
        let mut password_count = 0_u8;
        let mut fingerprint_count = 0_u8;
        let mut setup_count = 0_u8;
        for attribute in &line.attrs {
            match attribute {
                MediaAttribute::IceUfrag(value) => {
                    ufrag_count = ufrag_count.saturating_add(1);
                    if value != ice.ufrag() {
                        return Err(NegotiationError::new("conflicting BUNDLE ICE credentials"));
                    }
                }
                MediaAttribute::IcePwd(value) => {
                    password_count = password_count.saturating_add(1);
                    if value != ice.password() {
                        return Err(NegotiationError::new("conflicting BUNDLE ICE credentials"));
                    }
                }
                MediaAttribute::Fingerprint(_) => {
                    fingerprint_count = fingerprint_count.saturating_add(1);
                }
                MediaAttribute::Setup(_) => setup_count = setup_count.saturating_add(1),
                _ => {}
            }
        }
        if ufrag_count != password_count || ufrag_count > 1 {
            return Err(NegotiationError::new(
                "partial or duplicate BUNDLE ICE credentials",
            ));
        }
        if fingerprint_count > 1 || setup_count > 1 {
            return Err(NegotiationError::new(
                "duplicate BUNDLE DTLS transport attributes",
            ));
        }
        if let Some(section_ice) = line.ice_creds()
            && (section_ice.ufrag != ice.ufrag() || section_ice.pass != ice.password())
        {
            return Err(NegotiationError::new("conflicting BUNDLE ICE credentials"));
        }
        if let Some(section_fingerprint) = line.fingerprint()
            && (section_fingerprint.hash_func.to_ascii_lowercase() != fingerprint.algorithm()
                || section_fingerprint.bytes != fingerprint.value())
        {
            return Err(NegotiationError::new("conflicting BUNDLE DTLS fingerprint"));
        }
        if let Some(section_setup) = line.setup()
            && section_setup != setup
        {
            return Err(NegotiationError::new("conflicting BUNDLE DTLS setup"));
        }
    }
    Ok(())
}

fn validate_session_transport(sdp: &Sdp) -> Result<(), NegotiationError> {
    let mut ufrag = 0_u8;
    let mut password = 0_u8;
    let mut fingerprint = 0_u8;
    let mut setup = 0_u8;
    for attribute in &sdp.session.attrs {
        match attribute {
            SessionAttribute::IceUfrag(_) => ufrag = ufrag.saturating_add(1),
            SessionAttribute::IcePwd(_) => password = password.saturating_add(1),
            SessionAttribute::Fingerprint(_) => fingerprint = fingerprint.saturating_add(1),
            SessionAttribute::Setup(_) => setup = setup.saturating_add(1),
            _ => {}
        }
    }
    if ufrag != password || ufrag > 1 {
        return Err(NegotiationError::new("conflicting BUNDLE ICE credentials"));
    }
    if fingerprint > 1 {
        return Err(NegotiationError::new("conflicting BUNDLE DTLS fingerprint"));
    }
    if setup > 1 {
        return Err(NegotiationError::new("conflicting BUNDLE DTLS setup"));
    }
    Ok(())
}

fn consistency_error(message: String) -> NegotiationError {
    let lower = message.to_ascii_lowercase();
    if lower.contains("group") || lower.contains("mid") {
        NegotiationError::new(format!("bundle: {message}"))
    } else if lower.contains("rtp_map") {
        NegotiationError::new(format!("payload: {message}"))
    } else {
        NegotiationError::new(format!("invalid SDP: {message}"))
    }
}

fn valid_fingerprint(algorithm: String, bytes: Vec<u8>) -> Option<DtlsFingerprint> {
    let expected = match algorithm.to_ascii_lowercase().as_str() {
        "sha-256" => 32,
        "sha-384" => 48,
        "sha-512" => 64,
        _ => return None,
    };
    if bytes.len() != expected {
        return None;
    }
    DtlsFingerprint::new(algorithm, bytes.into_boxed_slice())
}

fn validate_bundle(sdp: &Sdp) -> Result<Vec<String>, NegotiationError> {
    let groups = sdp
        .session
        .attrs
        .iter()
        .filter_map(|attr| {
            if let str0m::sdp::SessionAttribute::Group { typ, mids } = attr {
                (typ == "BUNDLE").then(|| mids.iter().map(ToString::to_string).collect::<Vec<_>>())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    if groups.len() != 1 {
        return Err(NegotiationError::new(
            "bundle: exactly one BUNDLE group is required",
        ));
    }
    let Some(mids) = groups.into_iter().next() else {
        return Err(NegotiationError::new(
            "bundle: exactly one BUNDLE group is required",
        ));
    };
    let mut group_unique = HashSet::with_capacity(mids.len());
    if mids.iter().any(|mid| !group_unique.insert(mid)) {
        return Err(NegotiationError::new("bundle: BUNDLE mids must be unique"));
    }
    let offered = sdp
        .media_lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            line.attrs
                .iter()
                .find_map(|attr| {
                    if let MediaAttribute::Mid(mid) = attr {
                        Some(mid.to_string())
                    } else {
                        None
                    }
                })
                .ok_or_else(|| {
                    NegotiationError::new(format!("bundle: media section {index} is missing a mid"))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut unique = HashSet::with_capacity(mids.len());
    if offered.iter().any(|mid| !unique.insert(mid)) {
        return Err(NegotiationError::new("duplicate media section identifier"));
    }
    if mids.len() != offered.len() || mids != offered {
        return Err(NegotiationError::new(
            "bundle: BUNDLE mids must uniquely cover media sections in order",
        ));
    }
    if sdp.media_lines.first().is_some_and(|line| line.disabled) {
        return Err(NegotiationError::new(
            "bundle: first BUNDLE tag must be an enabled media section",
        ));
    }
    Ok(mids)
}

fn validate_bundle_rtp_namespaces(
    sdp: &Sdp,
    raw_sections: &[Vec<String>],
    mids: &[String],
) -> Result<(), NegotiationError> {
    let mut extensions_by_id = HashMap::<u8, String>::new();
    let mut ids_by_extension = HashMap::<String, u8>::new();
    let mut mid_id = None;
    let mut payloads = HashMap::<u8, String>::new();
    for (index, (line, raw)) in sdp.media_lines.iter().zip(raw_sections).enumerate() {
        if line.disabled || line.typ == MediaType::Application {
            continue;
        }
        let mid = mids
            .get(index)
            .ok_or_else(|| NegotiationError::new("bundle: missing RTP mid"))?;
        let mut section_mid_id = None;
        for value in raw
            .iter()
            .skip(1)
            .filter_map(|raw| raw.strip_prefix("a=extmap:"))
        {
            let (id, rest) = value.split_once(char::is_whitespace).ok_or_else(|| {
                NegotiationError::new(format!("extension: malformed extmap on {mid}"))
            })?;
            let id = id
                .split('/')
                .next()
                .and_then(|id| id.parse::<u8>().ok())
                .ok_or_else(|| {
                    NegotiationError::new(format!("extension: invalid extmap on {mid}"))
                })?;
            let mut fields = rest.split_whitespace();
            let uri = fields.next().ok_or_else(|| {
                NegotiationError::new(format!("extension: malformed extmap on {mid}"))
            })?;
            let attributes = fields.collect::<Vec<_>>().join(" ");
            let key = format!("{uri}\u{1f}{attributes}");
            if extensions_by_id
                .insert(id, key.clone())
                .is_some_and(|old| old != key)
            {
                return Err(NegotiationError::new(format!(
                    "extension: conflicting bundled extmap id {id}"
                )));
            }
            if ids_by_extension
                .insert(key, id)
                .is_some_and(|old| old != id)
            {
                return Err(NegotiationError::new(
                    "extension: bundled URI has conflicting IDs",
                ));
            }
            if uri == "urn:ietf:params:rtp-hdrext:sdes:mid"
                && section_mid_id.replace(id).is_some_and(|old| old != id)
            {
                return Err(NegotiationError::new(format!(
                    "extension: multiple MID IDs on {mid}"
                )));
            }
        }
        let section_mid_id = section_mid_id.ok_or_else(|| {
            NegotiationError::new(format!("extension: enabled RTP section {mid} lacks MID"))
        })?;
        if mid_id
            .replace(section_mid_id)
            .is_some_and(|old| old != section_mid_id)
        {
            return Err(NegotiationError::new(
                "extension: bundled RTP sections use different MID IDs",
            ));
        }
        for pt in &line.pts {
            let pt = **pt;
            let signature = payload_signature(line, raw, pt);
            if payloads
                .insert(pt, signature.clone())
                .is_some_and(|old| old != signature)
            {
                return Err(NegotiationError::new(format!(
                    "payload: conflicting bundled payload type {pt}"
                )));
            }
        }
    }
    Ok(())
}

fn payload_signature(line: &str0m::sdp::MediaLine, raw: &[String], pt: u8) -> String {
    let mut parts = raw
        .iter()
        .filter(|value| {
            ["a=rtpmap:", "a=fmtp:"].iter().any(|prefix| {
                value
                    .strip_prefix(prefix)
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse::<u8>().ok())
                    == Some(pt)
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    if parts.is_empty()
        && let Some(param) = line.rtp_params().into_iter().find(|param| *param.pt == pt)
    {
        parts.push(format!(
            "{}:{}/{}/{}",
            param.pt,
            param.spec.codec,
            param.spec.clock_rate,
            param.spec.channels.map_or(0, u8::from)
        ));
    }
    parts.join("\u{1f}")
}

fn media_kind(typ: &MediaType) -> Result<MediaKind, NegotiationError> {
    match typ {
        MediaType::Audio => Ok(MediaKind::Audio),
        MediaType::Video => Ok(MediaKind::Video),
        MediaType::Application => Ok(MediaKind::Application),
        MediaType::Unknown(value) => Err(NegotiationError::new(format!(
            "unsupported media codec/type {value}"
        ))),
    }
}

fn modern_application_m_line(raw: &[String]) -> bool {
    let Some(line) = raw.first().and_then(|line| line.strip_prefix("m=")) else {
        return false;
    };
    let fields = line.split_whitespace().collect::<Vec<_>>();
    matches!(
        fields.as_slice(),
        ["application", _, "UDP/DTLS/SCTP", "webrtc-datachannel"]
    )
}

fn negotiated_direction(
    kind: MediaKind,
    remote: Direction,
    mid: &str,
) -> Result<MediaDirection, NegotiationError> {
    match (kind, remote) {
        (_, Direction::Inactive) => Ok(MediaDirection::Inactive),
        (MediaKind::Application, _) => Ok(MediaDirection::Bidirectional),
        (_, Direction::SendOnly) => Ok(MediaDirection::ReceiveOnly),
        (_, Direction::RecvOnly) => Ok(MediaDirection::SendOnly),
        (_, Direction::SendRecv) => Err(NegotiationError::new(format!(
            "unsupported direction for {mid}"
        ))),
    }
}

fn codecs(
    line: &str0m::sdp::MediaLine,
    raw: &[String],
    mid: &str,
) -> Result<(Vec<NegotiatedCodec>, Vec<u8>), NegotiationError> {
    if line.typ == MediaType::Application {
        return Ok((Vec::new(), Vec::new()));
    }
    if line.proto != Proto::Srtp {
        return Err(NegotiationError::new(format!("media {mid} must use SRTP")));
    }
    if !line
        .attrs
        .iter()
        .any(|attr| matches!(attr, MediaAttribute::RtcpMux | MediaAttribute::RtcpMuxOnly))
    {
        return Err(NegotiationError::new(format!(
            "media {mid} must use rtcp-mux"
        )));
    }
    let params = line
        .rtp_params()
        .into_iter()
        .filter(|param| line.pts.iter().any(|offered| **offered == *param.pt))
        .collect::<Vec<_>>();
    if params.is_empty() {
        return Err(NegotiationError::new(format!(
            "codec: media {mid} has no codecs"
        )));
    }
    let mut result = Vec::with_capacity(params.len());
    let mut payload_types = Vec::with_capacity(params.len());
    let mut primary_pts = HashSet::new();
    let mut declared_primary_pts = HashSet::new();
    let mut accepted_rtx = HashSet::new();
    let mut names = HashMap::with_capacity(params.len());
    if params.len() > MAX_PAYLOAD_TYPES {
        return Err(NegotiationError::new(format!(
            "codec: too many payload types on {mid}"
        )));
    }
    for param in &params {
        let pt = *param.pt;
        let name = param.spec.codec.to_string();
        if names.insert(pt, name.clone()).is_some() {
            return Err(NegotiationError::new(format!(
                "payload: conflicting payload type {pt} on {mid}"
            )));
        }
        let supported = match &line.typ {
            MediaType::Audio => name.eq_ignore_ascii_case("opus"),
            MediaType::Video => name.eq_ignore_ascii_case("h264"),
            _ => false,
        };
        if !name.eq_ignore_ascii_case("rtx") {
            declared_primary_pts.insert(pt);
        }
        if !supported {
            continue;
        }
        let clock_rate = u32::from(NonZeroU32::from(param.spec.clock_rate));
        match line.typ {
            MediaType::Audio
                if !name.eq_ignore_ascii_case("opus")
                    || clock_rate != 48_000
                    || param.spec.channels != Some(2) =>
            {
                if name.eq_ignore_ascii_case("opus") {
                    return Err(NegotiationError::new(format!(
                        "codec: Opus must be 48000/2 on {mid}"
                    )));
                }
            }
            MediaType::Video
                if name.eq_ignore_ascii_case("h264")
                    && (clock_rate != 90_000 || param.spec.channels.is_some()) =>
            {
                return Err(NegotiationError::new(format!(
                    "codec: H264 must be 90000 on {mid}"
                )));
            }
            _ => {}
        }
        let h264 = name
            .eq_ignore_ascii_case("h264")
            .then(|| h264_parameters(raw, pt, mid))
            .transpose()?;
        result.push(NegotiatedCodec::new(
            name,
            pt,
            clock_rate,
            param.spec.channels,
            None,
            param.fb_transport_cc,
            param.fb_nack,
            param.fb_pli,
            param.fb_fir,
            h264,
        ));
        payload_types.push(pt);
        primary_pts.insert(pt);
    }
    if primary_pts.is_empty() {
        return Err(NegotiationError::new(format!(
            "codec: no supported codec remains on {mid}"
        )));
    }
    let mut rtx_by_primary = HashMap::new();
    for offered_pt in &line.pts {
        let pt = **offered_pt;
        if payload_types.contains(&pt) {
            continue;
        }
        let Some((name, rtx_clock_rate, rtx_channels)) = line.attrs.iter().find_map(|attribute| {
            if let MediaAttribute::RtpMap { pt: mapped, value } = attribute {
                (**mapped == pt).then(|| {
                    (
                        value.codec.to_string(),
                        u32::from(NonZeroU32::from(value.clock_rate)),
                        value.channels,
                    )
                })
            } else {
                None
            }
        }) else {
            continue;
        };
        if !name.eq_ignore_ascii_case("rtx") {
            continue;
        }
        let apt = rtx_apt(raw, pt, mid)?;
        let Some(apt) = apt else {
            return Err(NegotiationError::new(format!(
                "codec: RTX is missing apt on {mid}"
            )));
        };
        if apt == pt || !declared_primary_pts.contains(&apt) {
            return Err(NegotiationError::new(format!(
                "codec: RTX apt does not map to one declared primary on {mid}"
            )));
        }
        if primary_pts.contains(&apt) {
            if !accepted_rtx.insert(apt) {
                return Err(NegotiationError::new(format!(
                    "codec: multiple RTX payloads map to primary on {mid}"
                )));
            }
            let primary = params
                .iter()
                .find(|primary| *primary.pt == apt)
                .ok_or_else(|| NegotiationError::new(format!("codec: unknown RTX apt on {mid}")))?;
            let primary_clock_rate = u32::from(NonZeroU32::from(primary.spec.clock_rate));
            if rtx_clock_rate != primary_clock_rate || rtx_channels != primary.spec.channels {
                return Err(NegotiationError::new(format!(
                    "codec: RTX parameters do not match primary on {mid}"
                )));
            }
            payload_types.push(pt);
            rtx_by_primary.insert(apt, pt);
        }
    }
    let result = result
        .into_iter()
        .map(|codec| {
            let primary_pt = codec.payload_type();
            codec.with_retransmission_payload_type(rtx_by_primary.get(&primary_pt).copied())
        })
        .collect();
    Ok((result, payload_types))
}

fn rtx_apt(raw: &[String], payload_type: u8, mid: &str) -> Result<Option<u8>, NegotiationError> {
    let mut apt = None;
    for line in raw.iter().skip(1) {
        let Some(value) = line.strip_prefix("a=fmtp:") else {
            continue;
        };
        let Some((pt, parameters)) = value.split_once(char::is_whitespace) else {
            continue;
        };
        if pt.parse::<u8>().ok() != Some(payload_type) {
            continue;
        }
        for parameter in parameters.split(';') {
            let parameter = parameter.trim();
            let Some((key, value)) = parameter.split_once('=') else {
                return Err(NegotiationError::new(format!(
                    "codec: malformed RTX fmtp on {mid}"
                )));
            };
            if key.eq_ignore_ascii_case("apt") {
                let parsed = value.parse::<u8>().map_err(|_| {
                    NegotiationError::new(format!("codec: malformed RTX apt on {mid}"))
                })?;
                if apt.replace(parsed).is_some_and(|old| old != parsed) {
                    return Err(NegotiationError::new(format!(
                        "codec: conflicting RTX apt on {mid}"
                    )));
                }
            }
        }
    }
    apt.ok_or_else(|| NegotiationError::new(format!("codec: RTX is missing apt on {mid}")))
        .map(Some)
}

fn h264_parameters(
    raw: &[String],
    payload_type: u8,
    mid: &str,
) -> Result<H264Parameters, NegotiationError> {
    let mut packetization_mode = None;
    let mut profile_level_id = None;
    let mut level_asymmetry_allowed = None;
    for line in raw.iter().skip(1) {
        let Some(value) = line.strip_prefix("a=fmtp:") else {
            continue;
        };
        let Some((pt, parameters)) = value.split_once(char::is_whitespace) else {
            continue;
        };
        if pt.parse::<u8>().ok() != Some(payload_type) {
            continue;
        }
        for parameter in parameters.split(';') {
            let parameter = parameter.trim();
            let Some((key, value)) = parameter.split_once('=') else {
                return Err(NegotiationError::new(format!(
                    "codec: malformed H264 fmtp on {mid}"
                )));
            };
            if value.is_empty() {
                return Err(NegotiationError::new(format!(
                    "codec: malformed H264 fmtp on {mid}"
                )));
            }
            match key.to_ascii_lowercase().as_str() {
                "packetization-mode" => {
                    let value = value.parse::<u8>().map_err(|_| {
                        NegotiationError::new(format!("codec: malformed H264 fmtp on {mid}"))
                    })?;
                    if value > 1 {
                        return Err(NegotiationError::new(format!(
                            "codec: unsupported H264 packetization mode on {mid}"
                        )));
                    }
                    if packetization_mode
                        .replace(value)
                        .is_some_and(|old| old != value)
                    {
                        return Err(NegotiationError::new(format!(
                            "codec: conflicting H264 packetization mode on {mid}"
                        )));
                    }
                }
                "profile-level-id" => {
                    if value.len() != 6 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Err(NegotiationError::new(format!(
                            "codec: malformed H264 profile-level-id on {mid}"
                        )));
                    }
                    let value = value.to_ascii_lowercase();
                    if profile_level_id
                        .replace(value.clone())
                        .is_some_and(|old| old != value)
                    {
                        return Err(NegotiationError::new(format!(
                            "codec: conflicting H264 profile-level-id on {mid}"
                        )));
                    }
                }
                "level-asymmetry-allowed" => {
                    let value = match value {
                        "0" => false,
                        "1" => true,
                        _ => {
                            return Err(NegotiationError::new(format!(
                                "codec: malformed H264 fmtp on {mid}"
                            )));
                        }
                    };
                    if level_asymmetry_allowed
                        .replace(value)
                        .is_some_and(|old| old != value)
                    {
                        return Err(NegotiationError::new(format!(
                            "codec: conflicting H264 level asymmetry on {mid}"
                        )));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(H264Parameters::new(
        packetization_mode,
        profile_level_id,
        level_asymmetry_allowed,
    ))
}

fn extensions(
    raw: &[String],
    mid: &str,
    allow_mixed: bool,
) -> Result<Vec<(u8, String, MediaDirection)>, NegotiationError> {
    let mut found = HashMap::new();
    let mut ids = HashSet::new();
    let mut uris = HashSet::new();
    for line in raw.iter().skip(1) {
        let Some(value) = line.strip_prefix("a=extmap:") else {
            continue;
        };
        let Some((id, uri)) = value.split_once(char::is_whitespace) else {
            return Err(NegotiationError::new(format!(
                "extension: malformed extmap on {mid}"
            )));
        };
        if ids.len() >= MAX_EXTENSIONS {
            return Err(NegotiationError::new(format!(
                "extensions exceed limits on {mid}"
            )));
        }
        let mut id_parts = id.split('/');
        let id = id_parts
            .next()
            .and_then(|v| v.parse::<u8>().ok())
            .ok_or_else(|| {
                NegotiationError::new(format!("extension: invalid extmap id on {mid}"))
            })?;
        if id == 0 || (id > 14 && !allow_mixed) {
            return Err(NegotiationError::new(format!(
                "extension: invalid extmap id {id} on {mid}"
            )));
        }
        let direction = match id_parts.next() {
            None | Some("sendrecv") => MediaDirection::Bidirectional,
            Some("sendonly") => MediaDirection::ReceiveOnly,
            Some("recvonly") => MediaDirection::SendOnly,
            Some("inactive") => MediaDirection::Inactive,
            Some(_) => {
                return Err(NegotiationError::new(format!(
                    "extension: invalid extmap direction on {mid}"
                )));
            }
        };
        if id_parts.next().is_some() {
            return Err(NegotiationError::new(format!(
                "extension: invalid extmap direction on {mid}"
            )));
        }
        let uri = uri.trim().to_owned();
        if uri.is_empty() {
            return Err(NegotiationError::new(format!(
                "extension: empty URI on {mid}"
            )));
        }
        if !uris.insert(uri.clone()) {
            return Err(NegotiationError::new(format!(
                "extension: duplicate URI on {mid}"
            )));
        }
        if !ids.insert(id) {
            return Err(NegotiationError::new(format!(
                "extension: duplicate extmap id {id} on {mid}"
            )));
        }
        if is_supported_extension(&uri) {
            found.insert(id, (uri, direction));
        }
    }
    let mut result = found
        .into_iter()
        .map(|(id, (uri, direction))| (id, uri, direction))
        .collect::<Vec<_>>();
    result.sort_by_key(|(id, _, _)| *id);
    Ok(result)
}

fn is_supported_extension(uri: &str) -> bool {
    matches!(
        uri,
        "urn:ietf:params:rtp-hdrext:sdes:mid"
            | "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id"
            | "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id"
            | "urn:ietf:params:rtp-hdrext:ssrc-audio-level"
            | "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time"
            | "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"
            | "http://www.webrtc.org/experiments/rtp-hdrext/transport-wide-cc-01"
            | "http://www.webrtc.org/experiments/rtp-hdrext/video-dependency-descriptor"
            | "urn:ietf:params:rtp-hdrext:video-dependency-descriptor"
            | "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension"
            | "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00"
            | "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation01"
            | "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay"
    )
}

fn rid_ids(
    raw: &[String],
    mid: &str,
    kind: MediaKind,
    media_direction: Direction,
) -> Result<Vec<String>, NegotiationError> {
    let mut ids = HashSet::new();
    let mut result = Vec::new();
    for line in raw.iter().skip(1) {
        let Some(value) = line.strip_prefix("a=rid:") else {
            continue;
        };
        if kind != MediaKind::Video {
            return Err(NegotiationError::new(format!(
                "RID is only valid on video section {mid}"
            )));
        }
        let mut fields = value.split_whitespace();
        let id = fields
            .next()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| NegotiationError::new(format!("invalid RID on {mid}")))?
            .to_owned();
        if result.len() >= MAX_RIDS || !valid_rid_token(&id) {
            return Err(NegotiationError::new(format!(
                "RID exceeds limits on {mid}"
            )));
        }
        let rid_direction = fields
            .next()
            .ok_or_else(|| NegotiationError::new(format!("invalid RID on {mid}")))?;
        let expected = match media_direction {
            Direction::SendOnly => Some("send"),
            Direction::RecvOnly => Some("recv"),
            Direction::SendRecv | Direction::Inactive => {
                return Err(NegotiationError::new(format!(
                    "RID requires a unidirectional video section on {mid}"
                )));
            }
        };
        if expected.is_some_and(|expected| rid_direction != expected) {
            return Err(NegotiationError::new(format!(
                "RID direction mismatch on {mid}"
            )));
        }
        if !ids.insert(id.clone()) {
            return Err(NegotiationError::new(format!(
                "duplicate RID {id} on {mid}"
            )));
        }
        result.push(id);
    }
    Ok(result)
}

fn validate_simulcast(
    raw: &[String],
    mid: &str,
    kind: MediaKind,
    media_direction: Direction,
    rids: &[String],
) -> Result<(), NegotiationError> {
    let mut seen = false;
    for line in raw.iter().skip(1) {
        let Some(value) = line.strip_prefix("a=simulcast:") else {
            continue;
        };
        if kind != MediaKind::Video {
            return Err(NegotiationError::new(format!(
                "simulcast is only valid on video section {mid}"
            )));
        }
        if seen {
            return Err(NegotiationError::new(format!(
                "duplicate simulcast on {mid}"
            )));
        }
        seen = true;
        let (direction, groups) = value
            .split_once(char::is_whitespace)
            .ok_or_else(|| NegotiationError::new(format!("invalid simulcast on {mid}")))?;
        let expected = match media_direction {
            Direction::SendOnly => "send",
            Direction::RecvOnly => "recv",
            Direction::SendRecv | Direction::Inactive => {
                return Err(NegotiationError::new(format!(
                    "simulcast requires a unidirectional video section on {mid}"
                )));
            }
        };
        if direction != expected {
            return Err(NegotiationError::new(format!(
                "simulcast direction mismatch on {mid}"
            )));
        }
        let mut item_count = 0_usize;
        let mut referenced = HashSet::new();
        for rid in groups.split([';', ',']) {
            item_count = item_count.checked_add(1).ok_or_else(|| {
                NegotiationError::new(format!("simulcast exceeds limits on {mid}"))
            })?;
            if item_count > MAX_SIMULCAST_ITEMS {
                return Err(NegotiationError::new(format!(
                    "simulcast exceeds limits on {mid}"
                )));
            }
            let rid = rid.trim().trim_start_matches('~');
            if !valid_rid_token(rid) || !referenced.insert(rid) {
                return Err(NegotiationError::new(format!(
                    "invalid simulcast item on {mid}"
                )));
            }
            if !rids.iter().any(|declared| declared == rid) {
                return Err(NegotiationError::new(format!(
                    "simulcast references undeclared RID on {mid}"
                )));
            }
        }
    }
    Ok(())
}

fn valid_rid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_RID_TOKEN_BYTES
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn ssrc_ids(raw: &[String], mid: &str) -> Result<(Vec<u32>, bool), NegotiationError> {
    let mut ids = HashSet::new();
    let mut attributes = HashMap::<(u32, String), String>::new();
    let mut result = Vec::new();
    let mut has_probe = false;
    for line in raw.iter().skip(1) {
        let value = line.strip_prefix("a=ssrc:");
        let Some(value) = value else { continue };
        if attributes.len() >= MAX_SSRC_ATTRIBUTES {
            return Err(NegotiationError::new(format!(
                "SSRC attributes exceed limits on {mid}"
            )));
        }
        let mut fields = value.split_whitespace();
        let id = fields
            .next()
            .and_then(|token| token.parse::<u32>().ok())
            .ok_or_else(|| NegotiationError::new(format!("invalid SSRC on {mid}")))?;
        let attribute = fields.collect::<Vec<_>>().join(" ");
        let key = attribute
            .split_once(':')
            .map(|(key, _)| key)
            .filter(|key| !key.is_empty())
            .ok_or_else(|| NegotiationError::new(format!("invalid SSRC attribute on {mid}")))?
            .to_owned();
        if attributes
            .insert((id, key), attribute.clone())
            .is_some_and(|old| old != attribute)
        {
            return Err(NegotiationError::new(format!(
                "conflicting SSRC attribute on {mid}"
            )));
        }
        if ids.insert(id) {
            if id == 0 {
                has_probe = true;
            } else {
                result.push(id);
            }
        }
    }
    Ok((result, has_probe))
}

fn ssrc_groups(
    raw: &[String],
    mid: &str,
    ssrcs: &[u32],
    kind: MediaKind,
) -> Result<Vec<SsrcGroup>, NegotiationError> {
    let mut groups = Vec::new();
    let mut seen = HashSet::new();
    let mut sim_members = HashSet::new();
    let mut fid_primaries = HashSet::new();
    let mut fid_rtx = HashSet::new();
    for line in raw.iter().skip(1) {
        let Some(value) = line.strip_prefix("a=ssrc-group:") else {
            continue;
        };
        if groups.len() >= MAX_SSRC_GROUPS {
            return Err(NegotiationError::new(format!(
                "SSRC groups exceed limits on {mid}"
            )));
        }
        let mut fields = value.split_whitespace();
        let semantics = fields
            .next()
            .ok_or_else(|| NegotiationError::new(format!("invalid SSRC group on {mid}")))?;
        let members = fields
            .map(|member| {
                member
                    .parse::<u32>()
                    .map_err(|_| NegotiationError::new(format!("invalid SSRC group on {mid}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let is_sim = semantics.eq_ignore_ascii_case("SIM");
        let is_fid = semantics.eq_ignore_ascii_case("FID");
        if (!is_sim && !is_fid)
            || (is_sim && (kind != MediaKind::Video || members.len() < 2))
            || (is_fid && members.len() != 2)
            || members.contains(&0)
        {
            return Err(NegotiationError::new(format!(
                "invalid SSRC group on {mid}"
            )));
        }
        let key = format!("{}:{members:?}", semantics.to_ascii_uppercase());
        let mut unique_members = HashSet::with_capacity(members.len());
        if !seen.insert(key)
            || members.iter().any(|member| !unique_members.insert(member))
            || members.iter().any(|member| !ssrcs.contains(member))
        {
            return Err(NegotiationError::new(format!(
                "invalid SSRC group on {mid}"
            )));
        }
        if is_sim {
            if members
                .iter()
                .any(|member| sim_members.contains(member) || fid_rtx.contains(member))
            {
                return Err(NegotiationError::new(format!(
                    "ambiguous SIM SSRC ownership on {mid}"
                )));
            }
            sim_members.extend(members.iter().copied());
        } else {
            let primary = members
                .first()
                .copied()
                .ok_or_else(|| NegotiationError::new(format!("invalid SSRC group on {mid}")))?;
            let rtx = members
                .get(1)
                .copied()
                .ok_or_else(|| NegotiationError::new(format!("invalid SSRC group on {mid}")))?;
            if fid_primaries.contains(&primary)
                || fid_rtx.contains(&rtx)
                || fid_rtx.contains(&primary)
                || fid_primaries.contains(&rtx)
                || sim_members.contains(&rtx)
            {
                return Err(NegotiationError::new(format!(
                    "ambiguous FID SSRC ownership on {mid}"
                )));
            }
            fid_primaries.insert(primary);
            fid_rtx.insert(rtx);
        }
        groups.push(SsrcGroup::new(
            semantics.to_owned(),
            members.into_boxed_slice(),
        ));
    }
    Ok(groups)
}

fn data_channel(
    line: &str0m::sdp::MediaLine,
    raw: &[String],
    kind: MediaKind,
    mid: &str,
    direction: Direction,
) -> Result<Option<DataChannelParameters>, NegotiationError> {
    if kind != MediaKind::Application {
        return Ok(None);
    }
    if !modern_application_m_line(raw) || line.proto != str0m::sdp::Proto::Sctp {
        return Err(NegotiationError::new(format!(
            "sctp: invalid application protocol or direction on {mid}"
        )));
    }
    if direction != Direction::SendRecv {
        return Err(NegotiationError::new(format!(
            "sctp: invalid application protocol or direction on {mid}"
        )));
    }
    let ports = line
        .attrs
        .iter()
        .filter_map(|attr| {
            if let MediaAttribute::SctpPort(port) = attr {
                Some(*port)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let [port] = ports.as_slice() else {
        return Err(NegotiationError::new(format!(
            "sctp: application section {mid} must contain exactly one SCTP port"
        )));
    };
    let port = *port;
    if port == 0 {
        return Err(NegotiationError::new(format!(
            "sctp: invalid SCTP parameters on {mid}"
        )));
    }
    let maxes = line
        .attrs
        .iter()
        .filter_map(|attr| {
            if let MediaAttribute::MaxMessageSize(size) = attr {
                Some(*size)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    if maxes.len() > 1 {
        return Err(NegotiationError::new(format!(
            "sctp: conflicting max-message-size on {mid}"
        )));
    }
    let max = match maxes.first().copied() {
        None => MaxMessageSize::Default,
        Some(0) => MaxMessageSize::Unlimited,
        Some(value) => MaxMessageSize::finite(value).ok_or_else(|| {
            NegotiationError::new(format!("sctp: invalid SCTP parameters on {mid}"))
        })?,
    };
    DataChannelParameters::new(port, max)
        .map(Some)
        .ok_or_else(|| NegotiationError::new(format!("sctp: invalid SCTP parameters on {mid}")))
}

fn validate_candidates(candidates: &[IceCandidate]) -> Result<Vec<IceCandidate>, NegotiationError> {
    if candidates.len() > MAX_CANDIDATES {
        return Err(NegotiationError::new("too many local ICE candidates"));
    }
    candidates
        .iter()
        .map(|candidate| {
            str0m::Candidate::from_sdp_string(candidate.as_sdp())
                .map(|_| candidate.clone())
                .map_err(|error| {
                    NegotiationError::new(format!("invalid local ICE candidate: {error}"))
                })
        })
        .collect()
}

fn remote_candidates(offer: &str) -> Result<Vec<IceCandidate>, NegotiationError> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for line in offer.lines().map(|line| line.trim_end_matches('\r').trim()) {
        let Some(value) = line.strip_prefix("a=") else {
            continue;
        };
        if !value.starts_with("candidate:") {
            continue;
        }
        let candidate = str0m::Candidate::from_sdp_string(value).map_err(|error| {
            NegotiationError::new(format!("invalid remote ICE candidate: {error}"))
        })?;
        let value = candidate.to_sdp_string();
        if seen.insert(value.clone()) {
            if result.len() >= MAX_CANDIDATES {
                return Err(NegotiationError::new("too many remote ICE candidates"));
            }
            result.push(
                IceCandidate::new(value)
                    .ok_or_else(|| NegotiationError::new("invalid remote ICE candidate"))?,
            );
        }
    }
    Ok(result)
}

fn raw_sections(offer: &str) -> Result<Vec<Vec<String>>, NegotiationError> {
    let mut sections = Vec::new();
    let mut current = None;
    for raw in offer.lines() {
        let line = raw.trim_end_matches('\r').trim().to_owned();
        if line.starts_with("m=")
            && let Some(section) = current.replace(Vec::new())
        {
            sections.push(section);
        }
        if let Some(section) = &mut current {
            section.push(line);
        }
    }
    if let Some(section) = current {
        sections.push(section);
    }
    if sections.is_empty() {
        return Err(NegotiationError::new("invalid SDP: no media sections"));
    }
    Ok(sections)
}

#[allow(
    clippy::too_many_arguments,
    reason = "answer formatting receives immutable transport and media facts"
)]
fn format_answer_section(
    raw: &[String],
    kind: MediaKind,
    direction: MediaDirection,
    payload_types: &[u8],
    ice: &IceCredentials,
    fingerprint: &DtlsFingerprint,
    setup: Setup,
    bundle_tag: bool,
    local_candidates: &[IceCandidate],
    extensions: &[(u8, String, MediaDirection)],
    disabled: bool,
) -> String {
    let fields = raw
        .first()
        .and_then(|line| line.strip_prefix("m="))
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>();
    let offered_payload_types = fields
        .iter()
        .skip(3)
        .filter_map(|value| value.parse::<u8>().ok())
        .collect::<Vec<_>>();
    let mut out = String::new();
    if kind == MediaKind::Application {
        out.push_str(&format!(
            "m=application {} UDP/DTLS/SCTP webrtc-datachannel\r\n",
            if disabled { 0 } else { 9 }
        ));
    } else {
        let media = fields.first().copied().unwrap_or("audio");
        let proto = fields.get(2).copied().unwrap_or("UDP/TLS/RTP/SAVPF");
        out.push_str(&format!(
            "m={media} {} {proto}",
            if disabled { 0 } else { 9 }
        ));
        if disabled {
            for pt in fields.iter().skip(3) {
                out.push(' ');
                out.push_str(pt);
            }
        } else {
            for pt in payload_types {
                out.push_str(&format!(" {pt}"));
            }
        }
        out.push_str("\r\n");
    }
    out.push_str("c=IN IP4 0.0.0.0\r\n");
    let mut copied_ext = HashSet::new();
    for line in raw.iter().skip(1) {
        let keep = if disabled {
            line.starts_with("a=mid:")
                || line.starts_with("a=rtpmap:")
                || line.starts_with("a=fmtp:")
                || line.starts_with("a=rtcp-fb:")
        } else if line.starts_with("a=mid:")
            || line == "a=rtcp-mux"
            || line == "a=rtcp-mux-only"
            || line == "a=rtcp-rsize"
            || line.starts_with("a=extmap:")
            || line.starts_with("a=rid:")
            || line.starts_with("a=simulcast:")
            || line.starts_with("a=sctp-port:")
            || line.starts_with("a=max-message-size:")
        {
            true
        } else if let Some(value) = line
            .strip_prefix("a=rtpmap:")
            .or_else(|| line.strip_prefix("a=fmtp:"))
            .or_else(|| line.strip_prefix("a=rtcp-fb:"))
        {
            let pt = value
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u8>().ok());
            pt.is_some_and(|pt| {
                payload_types.contains(&pt) || (disabled && offered_payload_types.contains(&pt))
            })
        } else {
            false
        };
        if keep
            && !line.starts_with("a=ice-")
            && !line.starts_with("a=fingerprint:")
            && !line.starts_with("a=setup:")
            && !line.starts_with("a=send")
            && !line.starts_with("a=recv")
            && !line.starts_with("a=inactive")
            && !line.starts_with("a=candidate:")
            && line != "a=end-of-candidates"
        {
            if line.starts_with("a=extmap:") {
                let uri = line.split_whitespace().nth(1).unwrap_or_default();
                let id = line
                    .strip_prefix("a=extmap:")
                    .and_then(|v| v.split_whitespace().next())
                    .and_then(|v| v.split('/').next())
                    .and_then(|v| v.parse::<u8>().ok());
                if let Some(id) = id
                    && let Some((_, _, direction)) =
                        extensions.iter().find(|(accepted_id, accepted_uri, _)| {
                            *accepted_id == id && accepted_uri == uri
                        })
                    && copied_ext.insert(id)
                {
                    out.push_str(&format_extmap(id, uri, *direction));
                    out.push_str("\r\n");
                }
            } else if line == "a=rtcp-mux-only" {
                out.push_str("a=rtcp-mux");
                out.push_str("\r\n");
            } else {
                if line.starts_with("a=rid:") {
                    out.push_str(&invert_rid(line));
                } else if line.starts_with("a=simulcast:") {
                    out.push_str(&invert_simulcast(line));
                } else {
                    out.push_str(line);
                }
                out.push_str("\r\n");
            }
        }
    }
    out.push_str(&format!(
        "a=ice-ufrag:{}\r\na=ice-pwd:{}\r\na=fingerprint:{}\r\na=setup:{}\r\n",
        ice.ufrag(),
        ice.password(),
        fingerprint_sdp(fingerprint),
        setup
    ));
    out.push_str(match direction {
        MediaDirection::SendOnly => "a=sendonly\r\n",
        MediaDirection::ReceiveOnly => "a=recvonly\r\n",
        MediaDirection::Inactive => "a=inactive\r\n",
        MediaDirection::Bidirectional => "a=sendrecv\r\n",
    });
    if bundle_tag {
        for candidate in local_candidates {
            out.push_str("a=");
            out.push_str(candidate.as_sdp());
            out.push_str("\r\n");
        }
        if !local_candidates.is_empty() {
            out.push_str("a=end-of-candidates\r\n");
        }
    }
    out
}

fn invert_rid(line: &str) -> String {
    let Some(value) = line.strip_prefix("a=rid:") else {
        return line.to_owned();
    };
    let mut fields = value.splitn(3, ' ');
    let id = fields.next().unwrap_or_default();
    let direction = fields.next().unwrap_or_default();
    let direction = match direction {
        "send" => "recv",
        "recv" => "send",
        _ => direction,
    };
    let suffix = fields
        .next()
        .map(|rest| format!(" {rest}"))
        .unwrap_or_default();
    format!("a=rid:{id} {direction}{suffix}")
}

fn invert_simulcast(line: &str) -> String {
    let Some(value) = line.strip_prefix("a=simulcast:") else {
        return line.to_owned();
    };
    let Some((direction, rest)) = value.split_once(' ') else {
        return line.to_owned();
    };
    let direction = match direction {
        "send" => "recv",
        "recv" => "send",
        _ => direction,
    };
    format!("a=simulcast:{direction} {rest}")
}

fn format_extmap(id: u8, uri: &str, direction: MediaDirection) -> String {
    let suffix = match direction {
        MediaDirection::SendOnly => "/sendonly",
        MediaDirection::ReceiveOnly => "/recvonly",
        MediaDirection::Inactive => "/inactive",
        MediaDirection::Bidirectional => "",
    };
    format!("a=extmap:{id}{suffix} {uri}")
}

fn fingerprint_sdp(fingerprint: &DtlsFingerprint) -> String {
    format!(
        "{} {}",
        fingerprint.algorithm(),
        fingerprint
            .value()
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    )
}

fn format_answer(mids: &[String], sections: &[String], allow_mixed_extmaps: bool) -> String {
    debug_assert_eq!(sections.len(), mids.len());
    let mut out = format!(
        "v=0\r\no=- 0 2 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE {}\r\na=ice-lite\r\n",
        mids.join(" ")
    );
    if allow_mixed_extmaps {
        out.push_str("a=extmap-allow-mixed\r\n");
    }
    for section in sections {
        out.push_str(section);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parameters() -> NegotiationParameters {
        let ice = IceCredentials::new(
            "unitufrag".to_owned(),
            "unitpasswordlongenough22".to_owned(),
        )
        .expect("unit ICE credentials");
        let fingerprint = DtlsFingerprint::new("SHA-256".to_owned(), Box::new([7; 32]))
            .expect("unit fingerprint");
        let candidate =
            IceCandidate::new("candidate:1 1 UDP 2130706431 127.0.0.1 9000 typ host".to_owned())
                .expect("unit candidate");
        NegotiationParameters::new(ice, fingerprint, Box::new([candidate]))
    }

    #[test]
    fn untrusted_input_and_rid_tokens_are_bounded() {
        let error = negotiate(
            &"x".repeat(MAX_SDP_BYTES + 1),
            &RtcConfiguration::default(),
            &parameters(),
        )
        .expect_err("oversized SDP");
        assert!(error.to_string().contains("size"));
        assert!(valid_rid_token("q_1"));
        assert!(!valid_rid_token("q!"));
        assert!(!valid_rid_token("é"));
        assert!(!valid_rid_token(&"q".repeat(MAX_RID_TOKEN_BYTES + 1)));
    }

    #[test]
    fn extension_facts_are_structured_and_filtered() {
        let raw = vec![
            "m=audio 9 UDP/TLS/RTP/SAVPF 111".to_owned(),
            "a=extmap:1/sendonly urn:ietf:params:rtp-hdrext:sdes:something".to_owned(),
            "a=extmap:2 urn:ietf:params:rtp-hdrext:sdes:mid".to_owned(),
        ];
        let facts = extensions(&raw, "audio", false).expect("valid extension map");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].0, 2);
        assert_eq!(facts[0].2, MediaDirection::Bidirectional);
    }

    #[test]
    fn generated_answer_reparses_with_connection_lines() {
        let offer = include_str!("../tests/fixtures/chrome-representative.sdp");
        let result = negotiate(offer, &RtcConfiguration::default(), &parameters())
            .expect("representative offer");
        let answer = result.answer().to_owned();
        let parsed = Sdp::parse(&answer).expect("generated answer");
        parsed.assert_consistency().expect("consistent answer");
        assert_eq!(answer.matches("c=IN IP4 0.0.0.0").count(), 3);
    }
}

use super::{
    Codec, CodecParameters, Direction, Encoding, Error, Extension, Kind, Limits, MAX_SDP_BYTES,
    RepairCodec, RuntimeEncoding, Section,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Transport {
    pub ufrag: String,
    pub password: String,
    pub fingerprint: String,
    pub setup: String,
    pub candidates: Vec<String>,
}

impl Transport {
    pub fn same_peer(&self, other: &Self) -> bool {
        self.ufrag == other.ufrag
            && self.password == other.password
            && self.fingerprint == other.fingerprint
            && self.setup == other.setup
    }
    pub fn render(&self, lines: &mut Vec<String>) {
        lines.extend([
            format!("a=ice-ufrag:{}", self.ufrag),
            format!("a=ice-pwd:{}", self.password),
            format!("a=fingerprint:{}", self.fingerprint),
            format!("a=setup:{}", self.setup),
        ]);
        lines.extend(self.candidates.iter().cloned());
    }
    fn attribute(&mut self, line: &str) -> Result<bool, Error> {
        let field = if let Some(v) = line.strip_prefix("a=ice-ufrag:") {
            Some((&mut self.ufrag, v))
        } else if let Some(v) = line.strip_prefix("a=ice-pwd:") {
            Some((&mut self.password, v))
        } else if let Some(v) = line.strip_prefix("a=fingerprint:") {
            Some((&mut self.fingerprint, v))
        } else if let Some(v) = line.strip_prefix("a=setup:") {
            Some((&mut self.setup, v))
        } else {
            None
        };
        if let Some((field, value)) = field {
            if value.is_empty() || value.contains(['\r', '\n']) {
                return Err(Error::Malformed("transport"));
            }
            value.clone_into(field);
            return Ok(true);
        }
        if line.starts_with("a=candidate:") || line == "a=end-of-candidates" {
            self.candidates.push(line.to_owned());
            return Ok(true);
        }
        Ok(false)
    }
    fn validate(&self) -> Result<(), Error> {
        let fingerprint = self
            .fingerprint
            .strip_prefix("sha-256 ")
            .ok_or(Error::Malformed("fingerprint"))?;
        let digest: Vec<_> = fingerprint.split(':').collect();
        if digest.len() != 32
            || digest
                .iter()
                .any(|byte| byte.len() != 2 || !byte.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(Error::Malformed("fingerprint"));
        }
        if !(4..=256).contains(&self.ufrag.len())
            || !(22..=256).contains(&self.password.len())
            || !self
                .ufrag
                .bytes()
                .chain(self.password.bytes())
                .all(|b| b.is_ascii_alphanumeric() || b"+/".contains(&b))
            || !matches!(self.setup.as_str(), "active" | "passive" | "actpass")
        {
            return Err(Error::Malformed("transport"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(super) struct Media {
    pub section: Section,
    pub proto: String,
    pub formats: Vec<String>,
    pub transport: Transport,
    pub sctp: Vec<String>,
    pub signaled: BTreeMap<NonZeroU32, Option<NonZeroU32>>,
}

#[derive(Clone, Debug)]
pub(super) struct Document {
    pub origin: Vec<String>,
    pub version: u64,
    pub media: Vec<Media>,
    pub bundle: Vec<String>,
    pub ice_lite: bool,
}

pub(super) fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_+./".contains(&b))
}

fn number<T: std::str::FromStr>(s: &str, attr: &'static str) -> Result<T, Error> {
    s.parse().map_err(|_| Error::Malformed(attr))
}
fn pt(s: &str) -> Result<u8, Error> {
    let v = number(s, "payload type")?;
    if v > 127 {
        return Err(Error::Malformed("payload type"));
    }
    Ok(v)
}
fn ssrc(s: &str) -> Result<NonZeroU32, Error> {
    NonZeroU32::new(number(s, "ssrc")?).ok_or(Error::Malformed("ssrc"))
}
fn direction(line: &str) -> Option<Direction> {
    match line {
        "a=sendrecv" => Some(Direction::SendRecv),
        "a=sendonly" => Some(Direction::SendOnly),
        "a=recvonly" => Some(Direction::RecvOnly),
        "a=inactive" => Some(Direction::Inactive),
        _ => None,
    }
}

impl Document {
    pub fn parse(text: &str, limits: Limits) -> Result<Self, Error> {
        if text.len() > MAX_SDP_BYTES {
            return Err(Error::Capacity);
        }
        let mut session = Vec::new();
        let mut sections: Vec<Vec<&str>> = Vec::new();
        for raw in text.lines() {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if line.is_empty() {
                continue;
            }
            if line.contains(['\r', '\0']) || line.len() > 8192 {
                return Err(Error::Malformed("line"));
            }
            if line.starts_with("m=") {
                if sections.len() >= limits.max_sections {
                    return Err(Error::Capacity);
                }
                sections.push(vec![line]);
            } else if let Some(section) = sections.last_mut() {
                if section.len() >= 1024 {
                    return Err(Error::Capacity);
                }
                section.push(line);
            } else {
                if session.len() >= 2048 {
                    return Err(Error::Capacity);
                }
                session.push(line);
            }
        }
        if session.first() != Some(&"v=0") {
            return Err(Error::Malformed("version"));
        }
        let origin: Vec<String> = unique_value(&session, "o=")?
            .ok_or(Error::Malformed("origin"))?
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        if origin.len() != 6 {
            return Err(Error::Malformed("origin"));
        }
        number::<u64>(origin.get(1).ok_or(Error::Malformed("origin"))?, "origin")?;
        let version = number(origin.get(2).ok_or(Error::Malformed("origin"))?, "origin")?;
        let bundle: Vec<_> = unique_value(&session, "a=group:BUNDLE ")?
            .ok_or(Error::Malformed("BUNDLE"))?
            .split_whitespace()
            .collect();
        validate_unique_transport(&session)?;
        let mut transport = Transport::default();
        let mut inherited_direction = Direction::SendRecv;
        let mut inherited_extensions = Vec::new();
        for line in &session {
            transport.attribute(line)?;
            if let Some(d) = direction(line) {
                inherited_direction = d;
            }
            if line.starts_with("a=extmap:") {
                inherited_extensions.push(parse_extension(line)?);
            }
        }
        let mut media = Vec::new();
        let mut mids = BTreeSet::new();
        let mut identities = BTreeSet::new();
        for lines in sections {
            let mut m = parse_media(
                &lines,
                &transport,
                inherited_direction,
                &inherited_extensions,
            )?;
            if !mids.insert(m.section.mid.clone()) {
                return Err(Error::Identifier);
            }
            if !m.section.stopped {
                m.transport.validate()?;
                for (primary, repair) in &m.signaled {
                    for id in [Some(*primary), *repair].into_iter().flatten() {
                        if !identities.insert(id) {
                            return Err(Error::Collision);
                        }
                    }
                }
            } else {
                m.section.direction = Direction::Inactive;
            }
            media.push(m);
        }
        let bundled: BTreeSet<_> = bundle.iter().copied().collect();
        if bundled.len() != bundle.len()
            || media
                .iter()
                .any(|m| !m.section.stopped && !bundled.contains(m.section.mid.as_str()))
            || bundled.iter().any(|mid| !mids.contains(*mid))
        {
            return Err(Error::Malformed("BUNDLE"));
        }
        let mut live = media.iter().filter(|m| !m.section.stopped);
        if let Some(first) = live.next() {
            if live.any(|m| !m.transport.same_peer(&first.transport)) {
                return Err(Error::TransportChange);
            }
        }
        let count: usize = media.iter().map(|m| m.section.encodings.len()).sum();
        if count > limits.max_encodings {
            return Err(Error::Capacity);
        }
        Ok(Self {
            origin,
            version,
            media,
            bundle: bundle.into_iter().map(str::to_owned).collect(),
            ice_lite: session.contains(&"a=ice-lite"),
        })
    }
}

fn unique_value<'a>(lines: &[&'a str], prefix: &str) -> Result<Option<&'a str>, Error> {
    let mut values = lines.iter().filter_map(|line| line.strip_prefix(prefix));
    let first = values.next();
    if values.next().is_some() {
        return Err(Error::Malformed("duplicate attribute"));
    }
    Ok(first)
}

fn validate_unique_transport(lines: &[&str]) -> Result<(), Error> {
    for prefix in ["a=ice-ufrag:", "a=ice-pwd:", "a=fingerprint:", "a=setup:"] {
        unique_value(lines, prefix)?;
    }
    if lines
        .iter()
        .filter(|line| direction(line).is_some())
        .count()
        > 1
    {
        return Err(Error::Malformed("direction"));
    }
    Ok(())
}

fn parse_extension(line: &str) -> Result<Extension, Error> {
    let mut words = line
        .strip_prefix("a=extmap:")
        .ok_or(Error::Malformed("extmap"))?
        .split_whitespace();
    let mut identity = words.next().ok_or(Error::Malformed("extmap"))?.split('/');
    let id: u8 = number(identity.next().ok_or(Error::Malformed("extmap"))?, "extmap")?;
    if id == 0 || id == 15 {
        return Err(Error::Malformed("extmap"));
    }
    let direction = match identity.next() {
        None | Some("sendrecv") => Direction::SendRecv,
        Some("sendonly") => Direction::SendOnly,
        Some("recvonly") => Direction::RecvOnly,
        Some("inactive") => Direction::Inactive,
        _ => return Err(Error::Malformed("extmap")),
    };
    if identity.next().is_some() {
        return Err(Error::Malformed("extmap"));
    }
    let uri = words.next().ok_or(Error::Malformed("extmap"))?.to_owned();
    if uri.contains(['\r', '\n']) || words.next().is_some() {
        return Err(Error::Malformed("extmap attributes"));
    }
    Ok(Extension { id, direction, uri })
}

fn parse_media(
    lines: &[&str],
    inherited: &Transport,
    inherited_direction: Direction,
    inherited_extensions: &[Extension],
) -> Result<Media, Error> {
    validate_unique_transport(lines)?;
    for prefix in ["a=sctp-port:", "a=max-message-size:"] {
        unique_value(lines, prefix)?;
    }
    let words: Vec<_> = lines
        .first()
        .ok_or(Error::Malformed("media"))?
        .split_whitespace()
        .collect();
    let kind = match words.first().copied() {
        Some("m=audio") => Kind::Audio,
        Some("m=video") => Kind::Video,
        Some("m=application") => Kind::Application,
        _ => return Err(Error::Malformed("media kind")),
    };
    let port: u16 = number(words.get(1).ok_or(Error::Malformed("port"))?, "port")?;
    let proto = words
        .get(2)
        .ok_or(Error::Malformed("protocol"))?
        .to_string();
    if (kind == Kind::Application && proto != "UDP/DTLS/SCTP")
        || (kind != Kind::Application && proto != "UDP/TLS/RTP/SAVPF")
    {
        return Err(Error::Malformed("protocol"));
    }
    let formats: Vec<_> = words.iter().skip(3).map(|v| (*v).to_owned()).collect();
    if formats.is_empty() {
        return Err(Error::Malformed("formats"));
    }
    let mid = unique_value(lines, "a=mid:")?
        .ok_or(Error::Identifier)?
        .to_owned();
    if !token(&mid) {
        return Err(Error::Identifier);
    }
    let stopped = port == 0 && !lines.contains(&"a=bundle-only");
    if kind != Kind::Application && !stopped && !lines.contains(&"a=rtcp-mux") {
        return Err(Error::Malformed("rtcp-mux"));
    }
    let mut transport = inherited.clone();
    let mut selected_direction = None;
    let mut codecs: BTreeMap<u8, CodecParameters> = BTreeMap::new();
    let mut fmtps = BTreeMap::new();
    let mut feedback: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut extensions = Vec::new();
    let mut primaries = BTreeSet::new();
    let mut repairs = BTreeMap::new();
    let mut rids = BTreeSet::new();
    let mut simulcast = Vec::new();
    let mut sctp = Vec::new();
    for line in lines.iter().skip(1) {
        transport.attribute(line)?;
        if let Some(d) = direction(line) {
            if selected_direction.replace(d).is_some() {
                return Err(Error::Malformed("direction"));
            }
        } else if let Some(value) = line.strip_prefix("a=rtpmap:") {
            let (payload, format) = value.split_once(' ').ok_or(Error::Malformed("rtpmap"))?;
            let fields: Vec<_> = format.split('/').collect();
            let name = fields
                .first()
                .ok_or(Error::Malformed("rtpmap"))?
                .to_ascii_lowercase();
            let clock_rate = number(fields.get(1).ok_or(Error::Malformed("rtpmap"))?, "rtpmap")?;
            let channels = fields.get(2).map(|v| number(v, "channels")).transpose()?;
            if !token(&name) || clock_rate == 0 || channels == Some(0) || fields.len() > 3 {
                return Err(Error::Malformed("rtpmap"));
            }
            if codecs
                .insert(
                    pt(payload)?,
                    CodecParameters {
                        name,
                        clock_rate,
                        channels,
                        fmtp: BTreeMap::new(),
                        feedback: BTreeSet::new(),
                    },
                )
                .is_some()
            {
                return Err(Error::Malformed("rtpmap"));
            }
        } else if let Some(value) = line.strip_prefix("a=fmtp:") {
            let (payload, parameters) = value.split_once(' ').ok_or(Error::Malformed("fmtp"))?;
            let mut parsed = BTreeMap::new();
            for parameter in parameters.split(';').map(str::trim) {
                let (key, value) = parameter.split_once('=').unwrap_or((parameter, ""));
                if !token(key)
                    || value.contains(['\r', '\n'])
                    || parsed
                        .insert(key.to_ascii_lowercase(), value.to_owned())
                        .is_some()
                {
                    return Err(Error::Malformed("fmtp"));
                }
            }
            if fmtps.insert(pt(payload)?, parsed).is_some() {
                return Err(Error::Malformed("fmtp"));
            }
        } else if let Some(value) = line.strip_prefix("a=rtcp-fb:") {
            let (payload, value) = value.split_once(' ').ok_or(Error::Malformed("rtcp-fb"))?;
            if payload != "*" {
                pt(payload)?;
            }
            if value.is_empty() {
                return Err(Error::Malformed("rtcp-fb"));
            }
            feedback
                .entry(payload.to_owned())
                .or_default()
                .insert(value.to_owned());
        } else if line.starts_with("a=extmap:") {
            extensions.push(parse_extension(line)?);
        } else if let Some(value) = line.strip_prefix("a=ssrc:") {
            let (id, attribute) = value.split_once(' ').ok_or(Error::Malformed("ssrc"))?;
            if !attribute.contains(':') {
                return Err(Error::Malformed("ssrc"));
            }
            primaries.insert(ssrc(id)?);
        } else if let Some(value) = line.strip_prefix("a=ssrc-group:") {
            let fields: Vec<_> = value.split_whitespace().collect();
            match fields.first().copied() {
                Some("FID") if fields.len() == 3 => {
                    let primary = ssrc(fields.get(1).ok_or(Error::Malformed("FID"))?)?;
                    let repair = ssrc(fields.get(2).ok_or(Error::Malformed("FID"))?)?;
                    if primary == repair || repairs.insert(primary, repair).is_some() {
                        return Err(Error::Malformed("FID"));
                    }
                }
                Some("SIM") => {
                    for id in fields.iter().skip(1) {
                        ssrc(id)?;
                    }
                }
                _ => return Err(Error::Malformed("ssrc-group")),
            }
        } else if let Some(value) = line.strip_prefix("a=rid:") {
            let mut fields = value.split_whitespace();
            let rid = fields.next().ok_or(Error::Malformed("rid"))?;
            let dir = fields.next().ok_or(Error::Malformed("rid"))?;
            if !token(rid) || rid.len() > 32 || !matches!(dir, "send" | "recv") {
                return Err(Error::Malformed("rid"));
            }
            if dir == "send" && !rids.insert(rid.to_owned()) {
                return Err(Error::Malformed("rid"));
            }
        } else if let Some(value) = line.strip_prefix("a=simulcast:") {
            let mut fields = value.split_whitespace();
            while let Some(dir) = fields.next() {
                let list = fields.next().ok_or(Error::Malformed("simulcast"))?;
                if !matches!(dir, "send" | "recv") {
                    return Err(Error::Malformed("simulcast"));
                }
                if dir == "send" {
                    for rid in list.split(';') {
                        if rid.contains(',') {
                            return Err(Error::Malformed("simulcast alternatives"));
                        }
                        let (paused, rid) =
                            rid.strip_prefix('~').map_or((false, rid), |v| (true, v));
                        if !token(rid) || simulcast.iter().any(|(prior, _)| prior == rid) {
                            return Err(Error::Malformed("simulcast"));
                        }
                        simulcast.push((rid.to_owned(), paused));
                    }
                }
            }
        } else if line.starts_with("a=sctp-port:") || line.starts_with("a=max-message-size:") {
            let (_, value) = line.split_once(':').ok_or(Error::Malformed("sctp"))?;
            let number: u64 = number(value, "sctp")?;
            if line.starts_with("a=sctp-port:") && (number == 0 || number > u64::from(u16::MAX)) {
                return Err(Error::Malformed("sctp"));
            }
            sctp.push((*line).to_owned());
        }
    }
    if extensions.is_empty() {
        extensions.extend_from_slice(inherited_extensions);
    }
    let mut ids = BTreeSet::new();
    let mut uris = BTreeSet::new();
    if extensions
        .iter()
        .any(|e| !ids.insert(e.id) || !uris.insert(&e.uri))
    {
        return Err(Error::Malformed("extmap"));
    }
    let offered: BTreeSet<_> = if kind == Kind::Application {
        BTreeSet::new()
    } else {
        formats.iter().map(|v| pt(v)).collect::<Result<_, _>>()?
    };
    for (payload, codec) in &mut codecs {
        codec.fmtp = fmtps.remove(payload).unwrap_or_default();
        codec.feedback = feedback
            .get(&payload.to_string())
            .cloned()
            .unwrap_or_default();
        codec
            .feedback
            .extend(feedback.get("*").into_iter().flatten().cloned());
    }
    let mut selected = Vec::new();
    for payload in &offered {
        let Some(parameters) = codecs.get(payload) else {
            continue;
        };
        if parameters.name == "rtx" {
            continue;
        }
        let mut repair = None;
        for (rtx_pt, rtx) in &codecs {
            if rtx.name == "rtx"
                && offered.contains(rtx_pt)
                && rtx.fmtp.get("apt").and_then(|v| v.parse::<u8>().ok()) == Some(*payload)
            {
                if rtx.clock_rate != parameters.clock_rate
                    || repair
                        .replace(RepairCodec {
                            payload_type: *rtx_pt,
                            clock_rate: rtx.clock_rate,
                        })
                        .is_some()
                {
                    return Err(Error::Malformed("RTX"));
                }
            }
        }
        selected.push(Codec {
            payload_type: *payload,
            parameters: parameters.clone(),
            repair,
        });
    }
    for (primary, repair) in &repairs {
        if !primaries.contains(primary) || !primaries.contains(repair) {
            return Err(Error::Malformed("FID"));
        }
    }
    for repair in repairs.values() {
        primaries.remove(repair);
    }
    let signaled = primaries
        .iter()
        .map(|primary| (*primary, repairs.get(primary).copied()))
        .collect();
    let mut encodings = Vec::new();
    if !rids.is_empty() {
        if !primaries.is_empty() && primaries.len() != rids.len() {
            return Err(Error::Malformed("RID SSRC association"));
        }
        // SDP does not define a positional RID-to-SSRC association. Runtime
        // observations resolve it, even when both collections are present.
        if simulcast.is_empty() {
            simulcast.extend(rids.iter().map(|r| (r.clone(), false)));
        }
        if simulcast.len() != rids.len() || simulcast.iter().any(|(r, _)| !rids.contains(r)) {
            return Err(Error::Malformed("simulcast"));
        }
        for (rid, paused) in simulcast {
            encodings.push(Encoding {
                rid: Some(rid),
                primary: None,
                repair: None,
                paused,
                runtime: RuntimeEncoding::default(),
            });
        }
    } else {
        if !simulcast.is_empty() {
            return Err(Error::Malformed("simulcast"));
        }
        for primary in primaries {
            encodings.push(Encoding {
                rid: None,
                primary: Some(primary),
                repair: repairs.get(&primary).copied(),
                paused: false,
                runtime: RuntimeEncoding::default(),
            });
        }
    }
    Ok(Media {
        section: Section {
            mid,
            kind,
            direction: selected_direction.unwrap_or(inherited_direction),
            stopped,
            codecs: selected,
            extensions,
            encodings,
        },
        proto,
        formats,
        transport,
        sctp,
        signaled,
    })
}

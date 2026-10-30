//! Test-only provenance carried inside genuine prerecorded H.264 access units.
//! This is an on-wire fixture marker, not a catalog/SSRC attribution heuristic
//! or a claim that two publishers have visually different pictures.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct FixturePublisher {
    pub participant_id: String,
    pub frames: BTreeSet<Vec<u8>>,
    pub ledger: Option<Arc<Mutex<SourceLedger>>>,
}

/// Keys identify application source lifetimes, including replacements within one admission.
pub type FixturePublishers = BTreeMap<String, FixturePublisher>;

pub fn source_key(admission: &str, slot: &str) -> String {
    format!("{admission}/{slot}")
}

pub fn source_instance_key(admission: &str, slot: &str, incarnation: u64) -> String {
    let base = source_key(admission, slot);
    if incarnation == 0 {
        base
    } else {
        format!("{base}/{incarnation}")
    }
}

pub fn current_source_key(
    publishers: &FixturePublishers,
    admission: &str,
    slot: &str,
) -> Option<String> {
    let base = source_key(admission, slot);
    let prefix = format!("{base}/");
    publishers
        .keys()
        .filter_map(|key| {
            let incarnation = if key == &base {
                0
            } else {
                key.strip_prefix(&prefix)?.parse::<u64>().ok()?
            };
            Some((incarnation, key))
        })
        .max_by_key(|(incarnation, _)| *incarnation)
        .map(|(_, key)| key.clone())
}
const UUID: &[u8; 16] = b"PulseBeamFixture";
const EVENT_UUID: &[u8; 16] = b"PulseBeamEvents!";
const MAX_SOURCE_PICTURES: usize = 65_536;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct SourceEvent {
    pub source: String,
    pub capture: u64,
    pub encoding: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedSourceEvent {
    pub event: SourceEvent,
    pub captured: Instant,
    pub dimensions: (u16, u16),
    pub pixel_sample: super::decoder::PixelSample,
    pub native_temporal_index: Option<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Marker {
    source: String,
    event: Option<SourceEvent>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct H264InputSize {
    pub bytes: u64,
    pub nals: u64,
    pub vcl_bytes: u64,
}

impl H264InputSize {
    pub fn of(data: &[u8]) -> Self {
        let nals = units(data);
        Self {
            bytes: u64::try_from(data.len()).unwrap(),
            nals: u64::try_from(nals.len()).unwrap(),
            // Exclude Annex-B prefixes and the NAL header: fragmentation may
            // replace those, but must carry the coded slice bytes.
            vcl_bytes: nals
                .iter()
                .filter(|(_, nal)| nal.first().is_some_and(|byte| matches!(byte & 31, 1..=5)))
                .map(|(_, nal)| u64::try_from(nal.len() - 1).unwrap())
                .sum(),
        }
    }

    pub fn with_event(self, stamped_bytes: usize) -> Self {
        Self {
            bytes: u64::try_from(stamped_bytes).unwrap(),
            nals: self.nals + 1,
            ..self
        }
    }

    /// Controlled native H264 profile only: 1200-byte MTU, no negotiated FEC,
    /// <=256-byte input header/RTX reservation gives >=900-byte FU payload.
    /// Count NALs separately (ignore aggregation). On rebuilt SFU egress allow
    /// 128 bytes/packet for RTP, <=25-byte DD, MID/RID/TWCC, capture/playout,
    /// SRTP and even IP/UDP (the shaper counts datagrams only). VLA is at most
    /// 64 extra bytes on the first packet of each AU, not on every fragment.
    pub fn native_wire_upper(self) -> u64 {
        self.bytes + (self.nals + self.bytes.div_ceil(900)) * 128 + 64
    }
}

#[derive(Debug, Default)]
pub struct InputLoad {
    pub wire_bytes: u64,
    pub vcl_bytes: u64,
    pub max_wire_au: u64,
    pub offers: Vec<(Instant, u64)>,
}

/// Input traffic envelope, not a prediction of allocator or egress queue state.
/// Offer each entire AU at its capture instant, conservatively ignoring pacing.
/// Unlike the largest AU alone, this accounts for a complete synchronized train.
pub fn required_input_backlog(offers: &mut [(Instant, u64)], bps: u64) -> anyhow::Result<Duration> {
    use anyhow::Context;
    anyhow::ensure!(bps > 0, "input serialization needs positive capacity");
    offers.sort_unstable_by_key(|(at, _)| *at);
    let mut tail = None;
    let mut required = Duration::ZERO;
    for (at, bytes) in offers {
        let nanos = (u128::from(*bytes) * 8_000_000_000).div_ceil(u128::from(bps));
        let duration = Duration::from_nanos(u64::try_from(nanos)?);
        let next = tail
            .unwrap_or(*at)
            .max(*at)
            .checked_add(duration)
            .context("input serialization overflow")?;
        required = required.max(next.duration_since(*at));
        tail = Some(next);
    }
    Ok(required)
}

#[derive(Clone, Debug)]
pub struct SuppliedPicture {
    pub captured: Instant,
    pub dimensions: (u16, u16),
    pub temporal_index: Option<u8>,
    pub admitted: bool,
    pub canonical: Arc<[u8]>,
    pub input_size: H264InputSize,
    pub pixel_sample: super::decoder::PixelSample,
}

/// Actual fixture submissions, not a model of sender/allocator outcomes.
/// A full ledger fails explicitly; it never evicts an unmeasured source event.
#[derive(Debug)]
pub struct SourceLedger {
    pub source: String,
    pub period: Duration,
    pub pictures: BTreeMap<(u64, String), SuppliedPicture>,
    failure: Option<String>,
}

impl SourceLedger {
    pub fn new(source: &str, period: Duration) -> Self {
        assert!(!source.is_empty() && !period.is_zero());
        Self {
            source: source.to_owned(),
            period,
            pictures: BTreeMap::new(),
            failure: None,
        }
    }

    pub fn supply(
        &mut self,
        event: &SourceEvent,
        mut picture: SuppliedPicture,
    ) -> anyhow::Result<()> {
        let key = (event.capture, event.encoding.clone());
        if event.source != self.source
            || self.pictures.len() >= MAX_SOURCE_PICTURES
            || self.pictures.contains_key(&key)
        {
            self.failure.get_or_insert_with(|| {
                "source picture ledger full or duplicate capture/encoding".into()
            });
            return self.require_healthy();
        }
        picture.admitted = false;
        self.pictures.insert(key, picture);
        Ok(())
    }

    pub fn admit(&mut self, capture: u64, encoding: &str) {
        match self.pictures.get_mut(&(capture, encoding.to_owned())) {
            Some(picture) => picture.admitted = true,
            None => {
                self.failure
                    .get_or_insert_with(|| "admission has no source submission".into());
            }
        }
    }

    pub fn authenticate(&self, event: &SourceEvent, canonical: &[u8]) -> Option<&SuppliedPicture> {
        if event.source != self.source {
            return None;
        }
        let picture = self
            .pictures
            .get(&(event.capture, event.encoding.clone()))?;
        (picture.canonical.as_ref() == canonical).then_some(picture)
    }

    pub fn input_load(
        &self,
        encoding: &str,
        max_temporal: Option<u8>,
        start: Instant,
        end: Instant,
    ) -> anyhow::Result<InputLoad> {
        self.require_healthy()?;
        anyhow::ensure!(end > start, "input load needs a positive interval");
        let mut load = InputLoad::default();
        for ((_, rid), picture) in &self.pictures {
            if rid != encoding
                || picture.captured < start
                || picture.captured >= end
                || max_temporal.is_some_and(|max| !picture.temporal_index.is_some_and(|t| t <= max))
            {
                continue;
            }
            anyhow::ensure!(
                picture.admitted && picture.input_size.nals > 0,
                "input load needs settled actual submissions"
            );
            let wire = picture.input_size.native_wire_upper();
            load.wire_bytes += wire;
            load.vcl_bytes += picture.input_size.vcl_bytes;
            load.max_wire_au = load.max_wire_au.max(wire);
            load.offers.push((picture.captured, wire));
        }
        anyhow::ensure!(
            load.vcl_bytes > 0,
            "input load needs genuine supplied slices"
        );
        Ok(load)
    }

    pub fn require_healthy(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.failure.is_none(),
            "source collection failed: {:?}",
            self.failure
        );
        Ok(())
    }
}

fn units(data: &[u8]) -> Vec<(usize, &[u8])> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        let length = if data[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        starts.push((i, length));
        i += length;
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, &(start, prefix))| {
            let end = starts.get(index + 1).map_or(data.len(), |next| next.0);
            (start, &data[start + prefix..end])
        })
        .collect()
}

fn escape(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut zeros = 0;
    for &byte in data {
        if zeros == 2 && byte <= 3 {
            result.push(3);
            zeros = 0;
        }
        result.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    result
}

fn unescape(data: &[u8]) -> Result<Vec<u8>, ()> {
    let mut result = Vec::new();
    let mut zeros = 0;
    let mut i = 0;
    while i < data.len() {
        let byte = data[i];
        if zeros == 2 && byte == 3 {
            if data.get(i + 1).is_none_or(|next| *next > 3) {
                return Err(());
            }
            zeros = 0;
            i += 1;
            continue;
        }
        result.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        i += 1;
    }
    Ok(result)
}

fn extended(value: usize, output: &mut Vec<u8>) {
    output.extend(std::iter::repeat_n(255, value / 255));
    output.push(u8::try_from(value % 255).unwrap());
}

fn read_extended(data: &[u8], offset: &mut usize) -> Result<usize, ()> {
    let mut value = 0usize;
    loop {
        let byte = *data.get(*offset).ok_or(())?;
        *offset += 1;
        value = value.checked_add(usize::from(byte)).ok_or(())?;
        if byte != 255 {
            return Ok(value);
        }
    }
}

fn sei_identity(unit: &[u8]) -> Result<Option<Marker>, ()> {
    let rbsp = unescape(&unit[1..])?;
    let mut offset = 0;
    let mut identity = None;
    let mut messages = 0;
    while offset < rbsp.len() {
        if rbsp[offset] == 0x80 && rbsp[offset + 1..].iter().all(|byte| *byte == 0) {
            return if identity.is_some() && messages != 1 {
                Err(())
            } else {
                Ok(identity)
            };
        }
        let kind = read_extended(&rbsp, &mut offset)?;
        let size = read_extended(&rbsp, &mut offset)?;
        let end = offset.checked_add(size).ok_or(())?;
        let payload = rbsp.get(offset..end).ok_or(())?;
        messages += 1;
        if kind == 5 && (payload.starts_with(UUID) || payload.starts_with(EVENT_UUID)) {
            if identity.is_some() {
                return Err(());
            }
            let event = payload.starts_with(EVENT_UUID);
            let mut body = &payload[UUID.len()..];
            let size = if event {
                let size = usize::from(*body.first().ok_or(())?);
                body = &body[1..];
                size
            } else {
                body.len()
            };
            let source = std::str::from_utf8(body.get(..size).ok_or(())?).map_err(|_| ())?;
            if source.is_empty() || source.len() > 128 {
                return Err(());
            }
            let event = if event {
                body = &body[size..];
                let capture =
                    u64::from_be_bytes(body.get(..8).ok_or(())?.try_into().map_err(|_| ())?);
                body = &body[8..];
                let size = usize::from(*body.first().ok_or(())?);
                let encoding = std::str::from_utf8(body.get(1..).ok_or(())?).map_err(|_| ())?;
                if size != encoding.len() || size > 16 {
                    return Err(());
                }
                Some(SourceEvent {
                    source: source.to_owned(),
                    capture,
                    encoding: encoding.to_owned(),
                })
            } else {
                None
            };
            identity = Some(Marker {
                source: source.to_owned(),
                event,
            });
        }
        offset = end;
    }
    Err(())
}

pub fn stamp(data: &[u8], publisher: &str) -> Vec<u8> {
    assert!(!publisher.is_empty() && publisher.len() <= 128);
    stamp_payload(data, &[UUID.as_slice(), publisher.as_bytes()].concat())
}

pub fn stamp_event(data: &[u8], event: &SourceEvent) -> Vec<u8> {
    assert!(!event.source.is_empty() && event.source.len() <= 128 && event.encoding.len() <= 16);
    let mut payload = EVENT_UUID.to_vec();
    payload.push(u8::try_from(event.source.len()).unwrap());
    payload.extend_from_slice(event.source.as_bytes());
    payload.extend_from_slice(&event.capture.to_be_bytes());
    payload.push(u8::try_from(event.encoding.len()).unwrap());
    payload.extend_from_slice(event.encoding.as_bytes());
    stamp_payload(data, &payload)
}

fn stamp_payload(data: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut rbsp = Vec::new();
    extended(5, &mut rbsp);
    extended(payload.len(), &mut rbsp);
    rbsp.extend_from_slice(payload);
    rbsp.push(0x80);
    let mut marker = vec![0, 0, 0, 1, 6];
    marker.extend(escape(&rbsp));
    // Preserve AUD/SPS/PPS ordering and put SEI before the first coded slice.
    let position = units(data)
        .into_iter()
        .find(|(_, unit)| unit.first().is_some_and(|byte| matches!(byte & 31, 1..=5)))
        .map_or(data.len(), |(start, _)| start);
    let mut result = Vec::with_capacity(data.len() + marker.len());
    result.extend_from_slice(&data[..position]);
    result.extend(marker);
    result.extend_from_slice(&data[position..]);
    result
}

/// Compare coded slices, not native-managed parameter/SEI/filler NALs.
/// Libwebrtc rewrites SPS VUI at receive for low-delay decoding. Those legal
/// header changes are not a different source picture. The marker plus exact
/// VCL codebook proves fixture origin; the independent decoder still receives
/// the ORIGINAL complete payload and checks its actual pixels/dimensions.
pub fn inspect(data: &[u8]) -> Result<(Option<String>, Vec<u8>), ()> {
    inspect_marker(data).map(|(marker, canonical)| (marker.map(|marker| marker.source), canonical))
}

pub fn inspect_event(data: &[u8]) -> Result<(Option<SourceEvent>, Vec<u8>), ()> {
    inspect_marker(data)
        .map(|(marker, canonical)| (marker.and_then(|marker| marker.event), canonical))
}

fn inspect_marker(data: &[u8]) -> Result<(Option<Marker>, Vec<u8>), ()> {
    let mut publisher = None;
    let mut canonical = Vec::new();
    let units = units(data);
    if units.is_empty() {
        return Err(());
    }
    for (_, unit) in units {
        let kind = unit.first().ok_or(())? & 31;
        if kind == 6
            && let Some(identity) = sei_identity(unit)?
        {
            if publisher
                .as_ref()
                .is_some_and(|previous| *previous != identity)
            {
                return Err(());
            }
            publisher = Some(identity);
            continue;
        }
        if matches!(kind, 1..=5) {
            canonical.extend_from_slice(&[0, 0, 0, 1]);
            canonical.extend_from_slice(unit);
        }
    }
    if canonical.is_empty() {
        return Err(());
    }
    Ok((publisher, canonical))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_source_comes_from_constructed_input_lifetimes_not_lexical_order() {
        let mut inputs = FixturePublishers::new();
        assert_eq!(current_source_key(&inputs, "admission", "v0"), None);
        for (admission, slot, incarnation) in [
            ("admission", "v0", 0),
            ("admission", "v0", 2),
            ("admission", "v0", 10),
            ("admission", "v1", 20),
            ("other", "v0", 30),
        ] {
            inputs.insert(
                source_instance_key(admission, slot, incarnation),
                FixturePublisher::default(),
            );
        }
        assert_eq!(
            source_instance_key("admission", "v0", 0),
            source_key("admission", "v0")
        );
        assert_eq!(
            current_source_key(&inputs, "admission", "v0"),
            Some(source_instance_key("admission", "v0", 10))
        );
        assert_eq!(
            current_source_key(&inputs, "admission", "v1"),
            Some(source_instance_key("admission", "v1", 20))
        );
    }

    #[test]
    fn actual_fixture_marker_round_trips_and_remains_independently_decodable() {
        use crate::tests::common::decoder::H264ReferenceDecoder;
        let frames =
            crate::tests::common::media::video_frames(pulsebeam_testdata::RAW_H264_QUARTER_CBR);
        let original = &frames[0];
        let event = SourceEvent {
            source: "publisher-a".into(),
            capture: 0x0100_0003,
            encoding: "q".into(),
        };
        let stamped = stamp_event(original, &event);
        assert_eq!(inspect_event(&stamped).unwrap().0, Some(event));
        let (publisher, bare) = inspect(&stamped).unwrap();
        assert_eq!(publisher.as_deref(), Some("publisher-a"));
        assert_eq!(bare, inspect(original).unwrap().1);
        let expected = H264ReferenceDecoder::new("original", "fixture")
            .try_decode_observation(original, None)
            .unwrap();
        let observed = H264ReferenceDecoder::new("marked", "fixture")
            .try_decode_observation(&stamped, None)
            .unwrap();
        assert_eq!(
            (observed.width, observed.height),
            (expected.width, expected.height)
        );
    }

    #[test]
    fn input_backlog_rejects_a_train_even_when_each_individual_au_fits() {
        let start = Instant::now();
        let mut train = [(start, 2000), (start + Duration::from_millis(1), 2000)];
        let backlog = required_input_backlog(&mut train, 8000).unwrap();
        assert_eq!(backlog, Duration::from_millis(3999));
        assert!(required_input_backlog(&mut train[..1], 8000).unwrap() < Duration::from_secs(3));
        assert!(backlog > Duration::from_secs(3));
        train[1].0 = start + Duration::from_secs(3);
        assert_eq!(
            required_input_backlog(&mut train, 8000).unwrap(),
            Duration::from_secs(2)
        );
        train[1].0 = start;
        assert_eq!(
            required_input_backlog(&mut train, 8000).unwrap(),
            Duration::from_secs(4)
        );
        assert!(required_input_backlog(&mut train, 0).is_err());
    }

    #[test]
    fn input_size_counts_coded_bytes_without_annex_b_or_nal_headers() {
        let size = H264InputSize::of(&[0, 0, 1, 0x67, 3, 0, 0, 0, 1, 0x65, 4, 5]);
        assert_eq!((size.bytes, size.nals, size.vcl_bytes), (12, 2, 2));
        let stamped = size.with_event(1012);
        assert_eq!(
            (stamped.bytes, stamped.nals, stamped.vcl_bytes),
            (1012, 3, 2)
        );
        assert_eq!(stamped.native_wire_upper(), 1012 + (3 + 2) * 128 + 64);
    }

    #[test]
    fn event_identity_authenticates_actual_submission_not_just_known_fixture_bytes() {
        let original =
            crate::tests::common::media::video_frames(pulsebeam_testdata::RAW_H264_QUARTER_CBR);
        let canonical: Arc<[u8]> = inspect(&original[0]).unwrap().1.into();
        let event = SourceEvent {
            source: source_instance_key("admission", "source-owner", 1),
            capture: 7,
            encoding: "f".into(),
        };
        let mut ledger = SourceLedger::new(&event.source, Duration::from_millis(33));
        let captured = Instant::now();
        let pixel_sample =
            crate::tests::common::decoder::H264ReferenceDecoder::new("ledger", "fixture")
                .try_decode_observation(&original[0], None)
                .unwrap()
                .pixel_sample;
        let picture = SuppliedPicture {
            captured,
            dimensions: (320, 180),
            temporal_index: Some(0),
            canonical: canonical.clone(),
            input_size: H264InputSize::of(&original[0]),
            admitted: false,
            pixel_sample,
        };
        ledger.supply(&event, picture.clone()).unwrap();
        let supplied = ledger.authenticate(&event, &canonical).unwrap();
        assert_eq!(supplied.captured, captured);
        assert!(!supplied.admitted);
        let end = captured + ledger.period;
        assert!(ledger.input_load("f", Some(1), captured, end).is_err());
        ledger.admit(event.capture, &event.encoding);
        assert!(ledger.authenticate(&event, &canonical).unwrap().admitted);
        let load = ledger.input_load("f", Some(1), captured, end).unwrap();
        assert_eq!(load.wire_bytes, picture.input_size.native_wire_upper());
        assert_eq!(load.vcl_bytes, picture.input_size.vcl_bytes);
        assert_eq!(load.max_wire_au, load.wire_bytes);
        assert!(ledger.input_load("q", None, captured, end).is_err());
        assert!(
            ledger
                .input_load("f", None, end, end + ledger.period)
                .is_err()
        );
        assert!(ledger.input_load("f", None, captured, captured).is_err());
        let mut wrong = event.clone();
        wrong.capture += 1;
        assert!(ledger.authenticate(&wrong, &canonical).is_none());
        wrong = event.clone();
        wrong.source = "another-admission/source-owner".into();
        assert!(ledger.authenticate(&wrong, &canonical).is_none());
        wrong.source = source_instance_key("admission", "source-owner", 0);
        assert!(ledger.authenticate(&wrong, &canonical).is_none());
        wrong = event.clone();
        wrong.encoding = "q".into();
        assert!(ledger.authenticate(&wrong, &canonical).is_none());
        let mut substituted = canonical.to_vec();
        *substituted.last_mut().unwrap() ^= 1;
        assert!(ledger.authenticate(&event, &substituted).is_none());
        assert!(ledger.supply(&event, picture).is_err());
        assert!(
            ledger.require_healthy().is_err(),
            "collection failure remains sticky"
        );
    }

    #[test]
    fn marker_escaping_and_reconstructed_start_codes_are_supported() {
        let frame = [0, 0, 1, 0x41, 0xab, 0xcd];
        let stamped = stamp(&frame, "source\0\0\u{1}tail");
        assert_eq!(
            inspect(&stamped).unwrap().0.as_deref(),
            Some("source\0\0\u{1}tail")
        );
        assert_eq!(inspect(&stamped).unwrap().1, [0, 0, 0, 1, 0x41, 0xab, 0xcd]);
        let mut conflicting = stamp(&frame, "other");
        conflicting.splice(0..0, stamped[..stamped.len() - frame.len()].iter().copied());
        assert!(inspect(&conflicting).is_err());
    }

    #[test]
    fn native_parameter_rewriting_is_not_unknown_coded_picture_content() {
        // Parser-only values, never submitted as RTC media or decoder input.
        let original = [
            0, 0, 1, 0x67, 0xaa, 0, 0, 1, 0x68, 0xbb, 0, 0, 1, 0x65, 0xcc,
        ];
        let rewritten = [
            0, 0, 0, 1, 0x67, 0xdd, 0xee, 0, 0, 1, 0x68, 0xbb, 0, 0, 0, 1, 0x65, 0xcc,
        ];
        assert_eq!(
            inspect(&original).unwrap().1,
            inspect(&rewritten).unwrap().1
        );
        let mut changed_picture = rewritten;
        *changed_picture.last_mut().unwrap() ^= 1;
        assert_ne!(
            inspect(&original).unwrap().1,
            inspect(&changed_picture).unwrap().1
        );
        assert!(inspect(&[0, 0, 1, 0x67, 0xaa]).is_err());
    }

    #[test]
    fn padding_is_not_picture_identity_but_malformed_markers_fail_closed() {
        let frame = [0, 0, 0, 1, 0x65, 0xaa];
        let mut stamped = stamp(&frame, "source");
        let bare = inspect(&stamped).unwrap().1;
        stamped.extend_from_slice(&[0, 0, 0, 1, 12, 255, 255, 128]);
        assert_eq!(inspect(&stamped).unwrap().1, bare);
        assert!(inspect(&[0, 0, 1, 6, 5, 255, 128]).is_err());
    }
}

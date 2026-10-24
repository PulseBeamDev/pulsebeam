//! Test-only provenance carried inside genuine prerecorded H.264 access units.
//! This is an on-wire fixture marker, not a catalog/SSRC attribution heuristic
//! or a claim that two publishers have visually different pictures.
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct FixturePublisher {
    pub participant_id: String,
    pub frames: BTreeSet<Vec<u8>>,
}

/// Keys identify source admissions, not just reusable logical participant IDs.
pub type FixturePublishers = BTreeMap<String, FixturePublisher>;

pub fn source_key(admission: &str, slot: &str) -> String {
    format!("{admission}/{slot}")
}
const UUID: &[u8; 16] = b"PulseBeamFixture";

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

fn sei_identity(unit: &[u8]) -> Result<Option<String>, ()> {
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
        if kind == 5 && payload.starts_with(UUID) {
            let source = std::str::from_utf8(&payload[UUID.len()..]).map_err(|_| ())?;
            if source.is_empty() || source.len() > 128 || identity.is_some() {
                return Err(());
            }
            identity = Some(source.to_owned());
        }
        offset = end;
    }
    Err(())
}

pub fn stamp(data: &[u8], publisher: &str) -> Vec<u8> {
    assert!(!publisher.is_empty() && publisher.len() <= 128);
    let mut rbsp = Vec::new();
    extended(5, &mut rbsp);
    extended(UUID.len() + publisher.len(), &mut rbsp);
    rbsp.extend_from_slice(UUID);
    rbsp.extend_from_slice(publisher.as_bytes());
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
    fn actual_fixture_marker_round_trips_and_remains_independently_decodable() {
        use crate::tests::common::decoder::H264ReferenceDecoder;
        let frames =
            crate::tests::common::media::video_frames(pulsebeam_testdata::RAW_H264_QUARTER_CBR);
        let original = &frames[0];
        let stamped = stamp(original, "publisher-a");
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

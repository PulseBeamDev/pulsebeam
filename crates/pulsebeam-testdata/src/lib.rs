//! H.264 Annex-B test fixtures.
//!
//! Overflow is explicit here, denied workspace-wide, so
//! the start-code scan states its bounds rather than relying on the `while`
//! guard three lines up.
//!
//! Indexing exception, crate-wide: this crate only ever parses the fixture
//! files compiled into it, and it is only ever linked by tests. An index that
//! leaves the buffer is a broken fixture, and failing the test run is the
//! reporting mechanism — unlike the SFU, there is no session here to protect.
#![allow(clippy::indexing_slicing)]
#![cfg_attr(
    test,
    allow(
        clippy::unreachable,
        clippy::string_slice,
        clippy::disallowed_methods,
        clippy::float_cmp,
        clippy::arithmetic_side_effects,
    )
)]

pub const RAW_H264_FULL_CBR: &[u8] = include_bytes!("full_f_cbr.h264");
pub const RAW_H264_HALF_CBR: &[u8] = include_bytes!("half_h_cbr.h264");
pub const RAW_H264_QUARTER_CBR: &[u8] = include_bytes!("quarter_q_cbr.h264");
pub const RAW_H264_SCREEN_FULL_VBR: &[u8] = include_bytes!("screen_f_vbr.h264");
pub const RAW_H264_SCREEN_FULL_TIMING: &str = include_str!("screen_f_vbr.timing");
pub const QUALITY_CORPUS_MANIFEST: &str = include_str!("quality-corpus.manifest");

const QUALITY_S0_180P_H264: &[u8] = include_bytes!("quality_s0_180p.h264");
const QUALITY_S0_360P_H264: &[u8] = include_bytes!("quality_s0_360p.h264");
const QUALITY_S0_720P_H264: &[u8] = include_bytes!("quality_s0_720p.h264");
const QUALITY_S1_180P_H264: &[u8] = include_bytes!("quality_s1_180p.h264");
const QUALITY_S1_360P_H264: &[u8] = include_bytes!("quality_s1_360p.h264");
const QUALITY_S1_720P_H264: &[u8] = include_bytes!("quality_s1_720p.h264");
const QUALITY_S0_180P_REFERENCE: &[u8] = include_bytes!("quality_s0_180p.yuv420p.zst");
const QUALITY_S0_360P_REFERENCE: &[u8] = include_bytes!("quality_s0_360p.yuv420p.zst");
const QUALITY_S0_720P_REFERENCE: &[u8] = include_bytes!("quality_s0_720p.yuv420p.zst");
const QUALITY_S1_180P_REFERENCE: &[u8] = include_bytes!("quality_s1_180p.yuv420p.zst");
const QUALITY_S1_360P_REFERENCE: &[u8] = include_bytes!("quality_s1_360p.yuv420p.zst");
const QUALITY_S1_720P_REFERENCE: &[u8] = include_bytes!("quality_s1_720p.yuv420p.zst");
const QUALITY_A0_OPUS: &[u8] = include_bytes!("quality_a0_48k_mono.opus");
const QUALITY_A1_OPUS: &[u8] = include_bytes!("quality_a1_48k_mono.opus");
const QUALITY_A0_REFERENCE: &[u8] = include_bytes!("quality_a0_48k_mono.s16le.zst");
const QUALITY_A1_REFERENCE: &[u8] = include_bytes!("quality_a1_48k_mono.s16le.zst");

pub const QUALITY_VIDEO_FPS: u32 = 30;
pub const QUALITY_VIDEO_RTP_CLOCK_RATE: u64 = 90_000;
pub const QUALITY_AUDIO_SAMPLE_RATE: usize = 48_000;
pub const QUALITY_AUDIO_FRAME_SAMPLES: usize = 960;
pub const QUALITY_CORPUS_VIDEO_FRAME_COUNT: usize = 180;
pub const QUALITY_CORPUS_EPOCH_FRAME_COUNT: usize = 90;
pub const QUALITY_CORPUS_AUDIO_FRAME_COUNT: usize = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum QualityVideoSource {
    Zero,
    One,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum QualityVideoLayer {
    P180,
    P360,
    P720,
}

impl QualityVideoLayer {
    pub const fn dimensions(self) -> (usize, usize) {
        match self {
            Self::P180 => (320, 180),
            Self::P360 => (640, 360),
            Self::P720 => (1280, 720),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QualityVideoIdentity {
    pub source: QualityVideoSource,
    pub layer: QualityVideoLayer,
    pub epoch: usize,
    pub frame: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct QualityCorpusVideoFrame<'a> {
    pub identity: QualityVideoIdentity,
    pub index: usize,
    pub rtp_timestamp: u64,
    pub encoded: &'a [u8],
}

#[derive(Debug)]
pub struct QualityCorpusVideo {
    source: QualityVideoSource,
    layer: QualityVideoLayer,
    access_units: Vec<&'static [u8]>,
    reference_zstd: &'static [u8],
}

impl QualityCorpusVideo {
    pub const fn source(&self) -> QualityVideoSource {
        self.source
    }

    pub const fn layer(&self) -> QualityVideoLayer {
        self.layer
    }

    pub fn len(&self) -> usize {
        self.access_units.len()
    }

    pub fn is_empty(&self) -> bool {
        self.access_units.is_empty()
    }

    pub fn frame(&self, index: usize) -> Option<QualityCorpusVideoFrame<'static>> {
        let encoded = *self.access_units.get(index)?;
        let epoch = index.checked_div(QUALITY_CORPUS_EPOCH_FRAME_COUNT)?;
        let frame = index.checked_rem(QUALITY_CORPUS_EPOCH_FRAME_COUNT)?;
        let rtp_timestamp = u64::try_from(index)
            .ok()?
            .checked_mul(QUALITY_VIDEO_RTP_CLOCK_RATE)?
            .checked_div(u64::from(QUALITY_VIDEO_FPS))?;
        Some(QualityCorpusVideoFrame {
            identity: QualityVideoIdentity {
                source: self.source,
                layer: self.layer,
                epoch,
                frame,
            },
            index,
            rtp_timestamp,
            encoded,
        })
    }

    pub fn frame_for_rtp_timestamp(
        &self,
        timestamp: u64,
    ) -> Option<QualityCorpusVideoFrame<'static>> {
        let numerator = timestamp.checked_mul(u64::from(QUALITY_VIDEO_FPS))?;
        if numerator.checked_rem(QUALITY_VIDEO_RTP_CLOCK_RATE)? != 0 {
            return None;
        }
        let index = numerator.checked_div(QUALITY_VIDEO_RTP_CLOCK_RATE)?;
        let index = index.checked_rem(u64::try_from(self.len()).ok()?)?;
        self.frame(usize::try_from(index).ok()?)
    }

    pub fn decode_reference(&self) -> std::io::Result<Vec<u8>> {
        zstd::stream::decode_all(self.reference_zstd)
    }

    pub fn reference_frame<'a>(&self, decoded: &'a [u8], index: usize) -> Option<&'a [u8]> {
        let (width, height) = self.layer.dimensions();
        let frame_bytes = width.checked_mul(height)?.checked_mul(3)?.checked_div(2)?;
        let start = index.checked_mul(frame_bytes)?;
        let end = start.checked_add(frame_bytes)?;
        debug_assert!(start <= end);
        decoded.get(start..end)
    }
}

pub fn quality_corpus_video(
    source: QualityVideoSource,
    layer: QualityVideoLayer,
) -> QualityCorpusVideo {
    let (encoded, reference_zstd) = quality_video_bytes(source, layer);
    let Some(access_units) = h264_access_units(encoded) else {
        std::process::abort();
    };
    debug_assert_eq!(access_units.len(), QUALITY_CORPUS_VIDEO_FRAME_COUNT);
    QualityCorpusVideo {
        source,
        layer,
        access_units,
        reference_zstd,
    }
}

fn quality_video_bytes(
    source: QualityVideoSource,
    layer: QualityVideoLayer,
) -> (&'static [u8], &'static [u8]) {
    match (source, layer) {
        (QualityVideoSource::Zero, QualityVideoLayer::P180) => {
            (QUALITY_S0_180P_H264, QUALITY_S0_180P_REFERENCE)
        }
        (QualityVideoSource::Zero, QualityVideoLayer::P360) => {
            (QUALITY_S0_360P_H264, QUALITY_S0_360P_REFERENCE)
        }
        (QualityVideoSource::Zero, QualityVideoLayer::P720) => {
            (QUALITY_S0_720P_H264, QUALITY_S0_720P_REFERENCE)
        }
        (QualityVideoSource::One, QualityVideoLayer::P180) => {
            (QUALITY_S1_180P_H264, QUALITY_S1_180P_REFERENCE)
        }
        (QualityVideoSource::One, QualityVideoLayer::P360) => {
            (QUALITY_S1_360P_H264, QUALITY_S1_360P_REFERENCE)
        }
        (QualityVideoSource::One, QualityVideoLayer::P720) => {
            (QUALITY_S1_720P_H264, QUALITY_S1_720P_REFERENCE)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum QualityAudioSource {
    Zero,
    One,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualityAudioRegion {
    Active,
    SilenceDtx,
}

#[derive(Clone, Copy, Debug)]
pub struct QualityCorpusAudioFrame<'a> {
    pub source: QualityAudioSource,
    pub region: QualityAudioRegion,
    pub index: usize,
    pub symbol: u8,
    pub rtp_timestamp: u64,
    pub opus_packet: &'a [u8],
}

#[derive(Debug)]
pub struct QualityCorpusAudio {
    source: QualityAudioSource,
    packets: Vec<&'static [u8]>,
    reference_zstd: &'static [u8],
}

impl QualityCorpusAudio {
    pub const fn source(&self) -> QualityAudioSource {
        self.source
    }

    pub fn len(&self) -> usize {
        self.packets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    pub fn frame(&self, index: usize) -> Option<QualityCorpusAudioFrame<'static>> {
        let opus_packet = *self.packets.get(index)?;
        let region = if matches!(index.checked_div(50)?, 2 | 5) {
            QualityAudioRegion::SilenceDtx
        } else {
            QualityAudioRegion::Active
        };
        let rtp_timestamp = u64::try_from(index)
            .ok()?
            .checked_mul(u64::try_from(QUALITY_AUDIO_FRAME_SAMPLES).ok()?)?;
        Some(QualityCorpusAudioFrame {
            source: self.source,
            region,
            index,
            symbol: u8::try_from(index.checked_rem(4)?).ok()?,
            rtp_timestamp,
            opus_packet,
        })
    }

    pub fn frame_for_rtp_timestamp(
        &self,
        timestamp: u64,
    ) -> Option<QualityCorpusAudioFrame<'static>> {
        let frame_samples = u64::try_from(QUALITY_AUDIO_FRAME_SAMPLES).ok()?;
        if timestamp.checked_rem(frame_samples)? != 0 {
            return None;
        }
        let frame_count = u64::try_from(self.len()).ok()?;
        let index = timestamp
            .checked_div(frame_samples)?
            .checked_rem(frame_count)?;
        self.frame(usize::try_from(index).ok()?)
    }

    pub fn decode_reference(&self) -> std::io::Result<Vec<u8>> {
        zstd::stream::decode_all(self.reference_zstd)
    }

    pub fn reference_frame<'a>(&self, decoded: &'a [u8], index: usize) -> Option<&'a [u8]> {
        let frame_bytes = QUALITY_AUDIO_FRAME_SAMPLES.checked_mul(2)?;
        let start = index.checked_mul(frame_bytes)?;
        let end = start.checked_add(frame_bytes)?;
        debug_assert!(start <= end);
        decoded.get(start..end)
    }
}

pub fn quality_corpus_audio(source: QualityAudioSource) -> QualityCorpusAudio {
    let (encoded, reference_zstd) = match source {
        QualityAudioSource::Zero => (QUALITY_A0_OPUS, QUALITY_A0_REFERENCE),
        QualityAudioSource::One => (QUALITY_A1_OPUS, QUALITY_A1_REFERENCE),
    };
    let Some(mut packets) = ogg_opus_packets(encoded) else {
        std::process::abort();
    };
    debug_assert!(packets.len() > QUALITY_CORPUS_AUDIO_FRAME_COUNT);
    packets.truncate(QUALITY_CORPUS_AUDIO_FRAME_COUNT);
    QualityCorpusAudio {
        source,
        packets,
        reference_zstd,
    }
}

// 16 video and 5 audio downstream slots
pub const RAW_CHROME_SDP: &str = include_str!("chrome.sdp");

/// Parse an Annex-B H.264 byte-stream and return **per-frame** (per access
/// unit) byte sizes.
///
/// A new access unit starts when a VCL NAL unit (type 1 or 5) whose
/// `first_mb_in_slice == 0` is encountered.  In CBR-HRD streams (`nal-hrd=cbr`)
/// x264 emits multiple slice NALUs per frame plus SEI / filler padding; grouping
/// them correctly keeps the simulated bytes-per-second equal to the actual
/// encoded bitrate.
///
/// `first_mb_in_slice == 0` ↔ MSB of the first payload byte is `1`
/// (Exp-Golomb code-word for 0 is the single bit `1`).
pub fn h264_frame_sizes(data: &[u8]) -> Vec<usize> {
    let n = data.len();
    let mut sc_positions: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i.saturating_add(2) < n {
        if data[i] == 0 && data[i.saturating_add(1)] == 0 {
            if i.saturating_add(3) < n
                && data[i.saturating_add(2)] == 0
                && data[i.saturating_add(3)] == 1
            {
                sc_positions.push(i);
                i = i.saturating_add(4);
                continue;
            }
            if data[i.saturating_add(2)] == 1 {
                sc_positions.push(i);
                i = i.saturating_add(3);
                continue;
            }
        }
        i = i.saturating_add(1);
    }
    if sc_positions.is_empty() {
        return vec![];
    }

    let mut frames: Vec<usize> = Vec::new();
    let mut current_frame_bytes: usize = 0;
    let mut seen_vcl = false;

    for (k, &sc_pos) in sc_positions.iter().enumerate() {
        let sc_len = if sc_pos.saturating_add(3) < n && data[sc_pos.saturating_add(2)] == 0 {
            4
        } else {
            3
        };
        let nalu_start = sc_pos.saturating_add(sc_len);
        let nalu_end = if k.saturating_add(1) < sc_positions.len() {
            sc_positions[k.saturating_add(1)]
        } else {
            n
        };
        if nalu_start >= nalu_end {
            continue;
        }
        let nalu = &data[nalu_start..nalu_end];
        let nal_type = nalu[0] & 0x1f;
        let nalu_size = nalu_end.saturating_sub(nalu_start);

        let is_vcl = matches!(nal_type, 1..=5);
        // first_mb_in_slice == 0  ↔  MSB of byte[1] set (Exp-Golomb "1" prefix).
        let starts_new_au = is_vcl && nalu.len() >= 2 && (nalu[1] & 0x80) != 0;

        if starts_new_au && seen_vcl {
            frames.push(current_frame_bytes);
            current_frame_bytes = 0;
        }
        if is_vcl {
            seen_vcl = true;
        }
        current_frame_bytes = current_frame_bytes.saturating_add(nalu_size);
    }
    if current_frame_bytes > 0 {
        frames.push(current_frame_bytes);
    }
    frames
}

fn h264_access_units(data: &'static [u8]) -> Option<Vec<&'static [u8]>> {
    let mut start_codes = Vec::new();
    let mut index = 0usize;
    while index < data.len() {
        let end4 = index.checked_add(4);
        let end3 = index.checked_add(3);
        let start_code_len = if end4.and_then(|end| data.get(index..end)) == Some(&[0, 0, 0, 1]) {
            Some(4usize)
        } else if end3.and_then(|end| data.get(index..end)) == Some(&[0, 0, 1]) {
            Some(3usize)
        } else {
            None
        };
        if let Some(start_code_len) = start_code_len {
            start_codes.push((index, start_code_len));
            index = index.checked_add(start_code_len)?;
        } else {
            index = index.checked_add(1)?;
        }
    }

    let mut units = Vec::new();
    let mut access_unit_start = start_codes.first().map(|(offset, _)| *offset);
    let mut seen_vcl = false;
    let has_aud = start_codes.iter().any(|(offset, length)| {
        offset
            .checked_add(*length)
            .and_then(|header| data.get(header))
            .is_some_and(|header| header & 0x1f == 9)
    });
    for (position, (offset, start_code_len)) in start_codes.iter().enumerate() {
        let header = offset.checked_add(*start_code_len)?;
        let next_offset = start_codes
            .get(position.saturating_add(1))
            .map(|(next, _)| *next)
            .unwrap_or(data.len());
        let &nal_header = data.get(header)?;
        let nal_type = nal_header & 0x1f;
        if nal_type == 9 {
            if let Some(start) = access_unit_start
                && start < *offset
                && let Some(access_unit) = data.get(start..*offset)
                && !access_unit.is_empty()
            {
                units.push(access_unit);
            }
            access_unit_start = Some(*offset);
            seen_vcl = false;
        }
        let starts_new_access_unit = matches!(nal_type, 1..=5)
            && header
                .checked_add(1)
                .and_then(|payload| data.get(payload))
                .is_some_and(|byte| byte & 0x80 != 0);
        if !has_aud && starts_new_access_unit && seen_vcl {
            let Some(start) = access_unit_start else {
                continue;
            };
            let Some(access_unit) = data.get(start..*offset) else {
                continue;
            };
            if !access_unit.is_empty() {
                units.push(access_unit);
            }
            access_unit_start = Some(*offset);
        }
        if matches!(nal_type, 1..=5) {
            seen_vcl = true;
        }
        debug_assert!(header < next_offset || header == data.len());
    }
    if seen_vcl
        && let Some(start) = access_unit_start
        && let Some(access_unit) = data.get(start..)
        && !access_unit.is_empty()
    {
        units.push(access_unit);
    }
    (!units.is_empty()).then_some(units)
}

fn ogg_opus_packets(data: &'static [u8]) -> Option<Vec<&'static [u8]>> {
    let mut packets = Vec::new();
    let mut page = 0usize;
    while page < data.len() {
        let header_end = page.checked_add(27)?;
        let header = data.get(page..header_end)?;
        if header.get(..4) != Some(b"OggS") {
            return None;
        }
        let segment_count = usize::from(*header.get(26)?);
        let lacing_start = page.checked_add(27)?;
        let lacing_end = lacing_start.checked_add(segment_count)?;
        let lacing = data.get(lacing_start..lacing_end)?;
        let mut packet_start = lacing_end;
        let mut packet_len = 0usize;
        for &segment_len in lacing {
            let segment_len = usize::from(segment_len);
            let packet_end = packet_start.checked_add(segment_len)?;
            data.get(packet_start..packet_end)?;
            packet_len = packet_len.checked_add(segment_len)?;
            packet_start = packet_end;
            if segment_len < 255 {
                let start = packet_start.saturating_sub(packet_len);
                let packet = data.get(start..packet_start)?;
                if !packet.starts_with(b"OpusHead") && !packet.starts_with(b"OpusTags") {
                    packets.push(packet);
                }
                packet_len = 0;
            }
        }
        if packet_len != 0 {
            return None;
        }
        debug_assert!(packet_start >= page);
        page = packet_start;
    }
    (!packets.is_empty()).then_some(packets)
}

pub fn validate_quality_corpus_manifest(
    manifest: &str,
    files: &[(&str, &[u8])],
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    use std::collections::HashSet;

    let available: HashSet<&str> = files.iter().map(|(name, _)| *name).collect();
    if available.len() != files.len() {
        return Err("duplicate supplied fixture name".to_owned());
    }
    let mut claimed = HashSet::new();
    for line in manifest
        .lines()
        .filter_map(|line| line.strip_prefix("file="))
    {
        let mut fields = line.split(',');
        let expected_hash = fields
            .next()
            .ok_or_else(|| "manifest hash missing".to_owned())?;
        let expected_len = fields
            .next()
            .ok_or_else(|| "manifest length missing".to_owned())?
            .parse::<usize>()
            .map_err(|_| "manifest length is not numeric".to_owned())?;
        let name = fields
            .next()
            .ok_or_else(|| "manifest filename missing".to_owned())?;
        if fields.next().is_some() {
            return Err(format!("extra manifest fields for {name}"));
        }
        if !available.contains(name) {
            return Err(format!("unknown manifest file {name}"));
        }
        if !claimed.insert(name) {
            return Err(format!("duplicate manifest file {name}"));
        }
        let (_, bytes) = files
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .ok_or_else(|| format!("unknown manifest file {name}"))?;
        if bytes.len() != expected_len {
            return Err(format!("length mismatch for {name}"));
        }
        let actual_hash = format!("{:x}", Sha256::digest(bytes));
        if actual_hash != expected_hash {
            return Err(format!("digest mismatch for {name}"));
        }
    }
    if claimed.len() != available.len() {
        return Err(format!(
            "manifest has {} files, expected {}",
            claimed.len(),
            files.len()
        ));
    }
    Ok(())
}

pub fn frame_timestamps_micros(data: &str) -> Vec<u64> {
    let timestamps: Vec<u64> = data
        .lines()
        .map(|line| line.parse().unwrap_or_default())
        .collect();
    debug_assert!(!timestamps.is_empty());
    debug_assert!(timestamps.windows(2).all(|pair| pair[0] < pair[1]));
    timestamps
}

pub fn h264_sps_profile_level_id(data: &[u8]) -> Option<[u8; 3]> {
    let mut i = 0usize;
    while i.saturating_add(4) < data.len() {
        let short =
            data[i] == 0 && data[i.saturating_add(1)] == 0 && data[i.saturating_add(2)] == 1;
        let long = data[i] == 0
            && data[i.saturating_add(1)] == 0
            && data[i.saturating_add(2)] == 0
            && data[i.saturating_add(3)] == 1;
        if short || long {
            let header = i.saturating_add(if short { 3 } else { 4 });
            if data.get(header).is_some_and(|byte| byte & 0x1f == 7) {
                return Some([
                    *data.get(header.saturating_add(1))?,
                    *data.get(header.saturating_add(2))?,
                    *data.get(header.saturating_add(3))?,
                ]);
            }
            i = header.saturating_add(1);
        } else {
            i = i.saturating_add(1);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    use super::*;

    fn annex_b_nalu_types(access_unit: &[u8]) -> Vec<u8> {
        let mut starts = Vec::new();
        let mut offset = 0usize;
        while offset.saturating_add(3) < access_unit.len() {
            let start_code_len =
                if access_unit.get(offset..offset.saturating_add(4)) == Some(&[0, 0, 0, 1]) {
                    Some(4usize)
                } else if access_unit.get(offset..offset.saturating_add(3)) == Some(&[0, 0, 1]) {
                    Some(3usize)
                } else {
                    None
                };
            if let Some(start_code_len) = start_code_len {
                let header = offset.saturating_add(start_code_len);
                debug_assert!(header < access_unit.len());
                starts.push(header);
                offset = header;
            } else {
                offset = offset.saturating_add(1);
            }
        }
        starts
            .into_iter()
            .filter_map(|header| access_unit.get(header).map(|byte| byte & 0x1f))
            .collect()
    }

    fn corpus_files() -> [(&'static str, &'static [u8]); 16] {
        [
            ("quality_s0_180p.h264", QUALITY_S0_180P_H264),
            ("quality_s0_360p.h264", QUALITY_S0_360P_H264),
            ("quality_s0_720p.h264", QUALITY_S0_720P_H264),
            ("quality_s1_180p.h264", QUALITY_S1_180P_H264),
            ("quality_s1_360p.h264", QUALITY_S1_360P_H264),
            ("quality_s1_720p.h264", QUALITY_S1_720P_H264),
            ("quality_s0_180p.yuv420p.zst", QUALITY_S0_180P_REFERENCE),
            ("quality_s0_360p.yuv420p.zst", QUALITY_S0_360P_REFERENCE),
            ("quality_s0_720p.yuv420p.zst", QUALITY_S0_720P_REFERENCE),
            ("quality_s1_180p.yuv420p.zst", QUALITY_S1_180P_REFERENCE),
            ("quality_s1_360p.yuv420p.zst", QUALITY_S1_360P_REFERENCE),
            ("quality_s1_720p.yuv420p.zst", QUALITY_S1_720P_REFERENCE),
            ("quality_a0_48k_mono.opus", QUALITY_A0_OPUS),
            ("quality_a1_48k_mono.opus", QUALITY_A1_OPUS),
            ("quality_a0_48k_mono.s16le.zst", QUALITY_A0_REFERENCE),
            ("quality_a1_48k_mono.s16le.zst", QUALITY_A1_REFERENCE),
        ]
    }

    #[test]
    fn quality_corpus_manifest_is_fixed_and_complete() {
        let files = corpus_files();
        validate_quality_corpus_manifest(QUALITY_CORPUS_MANIFEST, &files)
            .expect("checked-in corpus bytes match their manifest");
        let expected = [
            (
                "quality_s0_180p.h264",
                152_886,
                "d618a5e2ed380f5b461fabbece22e74546233d40cf3f081344f4f208684fc9d7",
            ),
            (
                "quality_s0_360p.h264",
                480_618,
                "a9e798570a9f013619b42a5e1a8fdbe04010c605fdc111674433590c9740c680",
            ),
            (
                "quality_s0_720p.h264",
                1_425_399,
                "81fc2271d97f607a401d98e427af310b082489ee6a2b58c268e8959060154b46",
            ),
            (
                "quality_s1_180p.h264",
                71_369,
                "d7b32a76af4a251776e1c1b97d919047bea100edf6bdf0e9fc281c77afb98612",
            ),
            (
                "quality_s1_360p.h264",
                158_551,
                "dfe5c8bbc0a8f0272add2f94ad0007d519cda0b5704ecbb4b49dd07d8029ccc8",
            ),
            (
                "quality_s1_720p.h264",
                335_978,
                "75dd83f17e8f88ee3267aa6ef449ef59c08871fa9d711450ade8a408f4d81746",
            ),
            (
                "quality_s0_180p.yuv420p.zst",
                1_022_499,
                "f28e918ffc87dcaabe38a2d56435c2e628b3b6048baf6d0d2d018f0fc324f280",
            ),
            (
                "quality_s0_360p.yuv420p.zst",
                3_059_728,
                "df5a4bda9483e21254b9f4d6a83fc6f53b80eb79b949fead796a1c1bf7efa28b",
            ),
            (
                "quality_s0_720p.yuv420p.zst",
                8_331_816,
                "deb86bd1f2cde163873cf2ad98b16839ccdf599464519690d31b4960f120daa6",
            ),
            (
                "quality_s1_180p.yuv420p.zst",
                73_572,
                "f87ca4603f1d0dca1ce73a42fd276f3df608e9d7e3dd9fb1dfbeda4023241658",
            ),
            (
                "quality_s1_360p.yuv420p.zst",
                104_088,
                "fc0992ac25675e618f3cc9f1a7ccd2d3058cc09e78f59106f3d2669cdfa938d5",
            ),
            (
                "quality_s1_720p.yuv420p.zst",
                223_162,
                "3c8359ae69a16565d796d1efab3ab2b9518f5360467bb37a63bdacc9971cc782",
            ),
            (
                "quality_a0_48k_mono.opus",
                38_731,
                "4f9375fd29444175c53c9dbc8c7fc0d020354ef23b7b83698a9ca1284ec11b92",
            ),
            (
                "quality_a1_48k_mono.opus",
                38_420,
                "2911a254b8a65dcdc59302c12eabbc9c955585f793e81f9d6ed9172ebddaff93",
            ),
            (
                "quality_a0_48k_mono.s16le.zst",
                343_903,
                "d4ff7f4c1480ac4f677bbb22a0a50c44cd24620c6a7529ab2fec87ee386ca2e7",
            ),
            (
                "quality_a1_48k_mono.s16le.zst",
                356_286,
                "c9f7a14c68b58fcde66c746bd47e7bb4a26767d4600456019a08c5b4ff2c75e1",
            ),
        ];
        let claims: Vec<_> = QUALITY_CORPUS_MANIFEST
            .lines()
            .filter_map(|line| line.strip_prefix("file="))
            .collect();
        assert_eq!(claims.len(), expected.len());
        for (name, length, digest) in expected {
            let claim = claims
                .iter()
                .find(|claim| claim.ends_with(name))
                .unwrap_or_else(|| panic!("manifest is missing {name}"));
            let mut fields = claim.split(',');
            assert_eq!(fields.next(), Some(digest), "digest for {name}");
            let length = length.to_string();
            assert_eq!(fields.next(), Some(length.as_str()), "length for {name}");
            assert_eq!(fields.next(), Some(name));
            assert!(fields.next().is_none(), "extra fields for {name}");
        }
    }

    #[test]
    fn quality_corpus_validation_rejects_corruption_truncation_and_metadata_mismatch() {
        let files = corpus_files();
        let corrupt = |index: usize| {
            let mut bytes = files[index].1.to_vec();
            let first = bytes.first_mut().expect("fixture is non-empty");
            *first ^= 1;
            let mut candidates: Vec<(&str, &[u8])> = files.to_vec();
            let name = candidates[index].0;
            candidates[index] = (name, bytes.as_slice());
            validate_quality_corpus_manifest(QUALITY_CORPUS_MANIFEST, &candidates).is_err()
        };
        assert!(corrupt(0), "encoded H.264 corruption must fail validation");
        assert!(
            corrupt(6),
            "compressed video-reference corruption must fail validation"
        );

        let truncate = |index: usize| {
            let (name, bytes) = files[index];
            debug_assert!(bytes.len() > 1);
            let truncated = bytes
                .get(..bytes.len().checked_sub(1).expect("non-empty fixture"))
                .expect("truncated fixture slice");
            let mut candidates: Vec<(&str, &[u8])> = files.to_vec();
            candidates[index] = (name, truncated);
            validate_quality_corpus_manifest(QUALITY_CORPUS_MANIFEST, &candidates).is_err()
        };
        assert!(truncate(13), "truncated Opus fixture must fail validation");

        let mismatched =
            QUALITY_CORPUS_MANIFEST.replacen("quality_s0_180p.h264", "quality_s1_180p.h264", 1);
        assert!(
            validate_quality_corpus_manifest(&mismatched, &files).is_err(),
            "source/layer metadata mismatch must fail validation"
        );

        let duplicate = QUALITY_CORPUS_MANIFEST.replacen(
            "file=d7b32a76af4a251776e1c1b97d919047bea100edf6bdf0e9fc281c77afb98612,71369,quality_s1_180p.h264",
            "file=d618a5e2ed380f5b461fabbece22e74546233d40cf3f081344f4f208684fc9d7,152886,quality_s0_180p.h264",
            1,
        );
        assert!(
            validate_quality_corpus_manifest(&duplicate, &files).is_err(),
            "duplicate manifest claims must fail validation"
        );
    }

    #[test]
    fn quality_video_corpus_maps_every_source_layer_epoch_and_rtp_frame() {
        for source in [QualityVideoSource::Zero, QualityVideoSource::One] {
            for layer in [
                QualityVideoLayer::P180,
                QualityVideoLayer::P360,
                QualityVideoLayer::P720,
            ] {
                let fixture = quality_corpus_video(source, layer);
                assert_eq!(fixture.len(), QUALITY_CORPUS_VIDEO_FRAME_COUNT);
                let (width, height) = layer.dimensions();
                let reference = fixture.decode_reference().expect("video reference");
                let frame_bytes = width
                    .checked_mul(height)
                    .and_then(|bytes| bytes.checked_mul(3))
                    .and_then(|bytes| bytes.checked_div(2))
                    .expect("video reference frame size");
                let reference_size = frame_bytes
                    .checked_mul(QUALITY_CORPUS_VIDEO_FRAME_COUNT)
                    .expect("video reference size");
                assert_eq!(reference.len(), reference_size);
                let mut source_packets = Vec::new();
                for index in 0..fixture.len() {
                    let frame = fixture.frame(index).expect("declared video frame");
                    assert_eq!(frame.index, index);
                    assert_eq!(frame.identity.source, source);
                    assert_eq!(frame.identity.layer, layer);
                    assert_eq!(
                        frame.identity.epoch,
                        index / QUALITY_CORPUS_EPOCH_FRAME_COUNT
                    );
                    assert_eq!(
                        frame.identity.frame,
                        index % QUALITY_CORPUS_EPOCH_FRAME_COUNT
                    );
                    assert_eq!(
                        frame.rtp_timestamp,
                        u64::try_from(index)
                            .expect("video index")
                            .checked_mul(QUALITY_VIDEO_RTP_CLOCK_RATE)
                            .and_then(|timestamp| {
                                timestamp.checked_div(u64::from(QUALITY_VIDEO_FPS))
                            })
                            .expect("video RTP timestamp")
                    );
                    assert!(!frame.encoded.is_empty());
                    source_packets.extend(annex_b_nalu_types(frame.encoded));
                }
                assert!(
                    source_packets.contains(&5),
                    "{source:?} {layer:?} has no IDR"
                );
                assert!(
                    source_packets.contains(&7),
                    "{source:?} {layer:?} has no SPS"
                );
                assert!(
                    source_packets.contains(&8),
                    "{source:?} {layer:?} has no PPS"
                );
                assert!(fixture.frame(QUALITY_CORPUS_VIDEO_FRAME_COUNT).is_none());
                assert_eq!(
                    fixture
                        .frame_for_rtp_timestamp(
                            QUALITY_VIDEO_RTP_CLOCK_RATE
                                .checked_mul(6)
                                .expect("video loop timestamp"),
                        )
                        .expect("looped video timestamp")
                        .index,
                    0
                );
            }
        }
        assert_ne!(
            quality_corpus_video(QualityVideoSource::Zero, QualityVideoLayer::P180)
                .frame(0)
                .expect("source zero frame")
                .encoded,
            quality_corpus_video(QualityVideoSource::One, QualityVideoLayer::P180)
                .frame(0)
                .expect("source one frame")
                .encoded
        );
    }

    #[test]
    fn quality_audio_corpus_maps_sources_symbols_cadence_and_silence_dtx() {
        let mut first_packets = Vec::new();
        for source in [QualityAudioSource::Zero, QualityAudioSource::One] {
            let fixture = quality_corpus_audio(source);
            assert_eq!(fixture.len(), QUALITY_CORPUS_AUDIO_FRAME_COUNT);
            let reference = fixture.decode_reference().expect("audio reference");
            let reference_size = 48_000usize
                .checked_mul(6)
                .and_then(|samples| samples.checked_mul(2))
                .expect("audio reference size");
            assert_eq!(reference.len(), reference_size);
            let mut active = 0usize;
            let mut silence_dtx = 0usize;
            for index in 0..fixture.len() {
                let frame = fixture.frame(index).expect("declared audio frame");
                assert_eq!(frame.source, source);
                assert_eq!(frame.index, index);
                assert_eq!(frame.symbol, u8::try_from(index % 4).expect("audio symbol"));
                assert_eq!(
                    frame.rtp_timestamp,
                    u64::try_from(index)
                        .expect("audio index")
                        .checked_mul(u64::try_from(QUALITY_AUDIO_FRAME_SAMPLES).expect("samples"))
                        .expect("audio RTP timestamp")
                );
                assert!(!frame.opus_packet.is_empty());
                match frame.region {
                    QualityAudioRegion::Active => active += 1,
                    QualityAudioRegion::SilenceDtx => silence_dtx += 1,
                }
            }
            assert_eq!(active, 200);
            assert_eq!(silence_dtx, 100);
            assert!(fixture.frame(QUALITY_CORPUS_AUDIO_FRAME_COUNT).is_none());
            assert_eq!(
                fixture
                    .frame_for_rtp_timestamp(
                        u64::try_from(QUALITY_CORPUS_AUDIO_FRAME_COUNT)
                            .expect("audio count")
                            .checked_mul(
                                u64::try_from(QUALITY_AUDIO_FRAME_SAMPLES).expect("samples"),
                            )
                            .expect("audio loop timestamp"),
                    )
                    .expect("looped audio timestamp")
                    .index,
                0
            );
            first_packets.push(fixture.frame(0).expect("first audio frame").opus_packet);
        }
        assert_ne!(first_packets[0], first_packets[1]);
    }

    #[test]
    fn quality_corpus_accessors_reject_corrupt_truncated_and_mismatched_data() {
        let video = quality_corpus_video(QualityVideoSource::Zero, QualityVideoLayer::P180);
        assert!(video.frame(QUALITY_CORPUS_VIDEO_FRAME_COUNT).is_none());
        assert!(video.frame_for_rtp_timestamp(u64::MAX).is_none());
        assert!(video.reference_frame(&[], 0).is_none());
        assert!(video.reference_frame(&[0; 3], 0).is_none());

        let audio = quality_corpus_audio(QualityAudioSource::Zero);
        assert!(audio.frame(QUALITY_CORPUS_AUDIO_FRAME_COUNT).is_none());
        assert!(audio.frame_for_rtp_timestamp(u64::MAX).is_none());
        assert!(audio.reference_frame(&[], 0).is_none());
        assert!(audio.reference_frame(&[0; 2], 0).is_none());
    }

    #[test]
    fn screen_share_fixtures_have_variable_cadence_and_low_static_bitrate() {
        let sizes = h264_frame_sizes(RAW_H264_SCREEN_FULL_VBR);
        let timestamps = frame_timestamps_micros(RAW_H264_SCREEN_FULL_TIMING);
        assert_eq!(sizes.len(), timestamps.len(), "f frame schedule");

        let gaps: Vec<u64> = timestamps
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .collect();
        assert!(
            gaps.iter().any(|gap| *gap <= 67_000),
            "f has 15fps motion cadence"
        );
        assert!(
            gaps.iter().any(|gap| *gap >= 2_000_000),
            "f has 0.5fps static cadence"
        );

        let mut static_window_rates = Vec::new();
        for second in 0..timestamps.last().copied().unwrap() / 1_000_000 {
            let indices: Vec<usize> = timestamps
                .iter()
                .enumerate()
                .filter(|(_, timestamp)| **timestamp / 1_000_000 == second)
                .map(|(index, _)| index)
                .collect();
            if indices.len() <= 1 {
                static_window_rates.push(
                    indices
                        .iter()
                        .map(|index| sizes[*index] as u64)
                        .sum::<u64>()
                        * 8
                        / 1_000,
                );
            }
        }
        assert!(!static_window_rates.is_empty(), "f has static windows");
        static_window_rates.sort_unstable();
        let median = static_window_rates[static_window_rates.len() / 2];
        assert!(median <= 20, "f static median {median}kbps exceeds 20kbps");
    }

    #[test]
    fn bench_h264_fixtures_fit_the_chrome_baseline_contract() {
        assert!(
            RAW_CHROME_SDP.contains("packetization-mode=1;profile-level-id=42e01f"),
            "recorded Chrome SDP must offer constrained-baseline packetization mode 1 at level 3.1"
        );
        for (rid, fixture) in [
            ("f", RAW_H264_FULL_CBR),
            ("h", RAW_H264_HALF_CBR),
            ("q", RAW_H264_QUARTER_CBR),
        ] {
            let [profile, constraints, level] = h264_sps_profile_level_id(fixture)
                .unwrap_or_else(|| panic!("{rid} fixture has no SPS"));
            assert_eq!(profile, 0x42, "{rid} must remain H.264 baseline");
            assert_eq!(
                constraints & 0xc0,
                0xc0,
                "{rid} must be constrained baseline"
            );
            assert!(
                level <= 0x1f,
                "{rid} level {level:#04x} exceeds Chrome's level 3.1 contract"
            );
        }
    }
}

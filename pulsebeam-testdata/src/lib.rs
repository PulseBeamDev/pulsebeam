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
pub const QUALITY_H264_320X180_30: &[u8] = include_bytes!("quality_320x180_30.h264");
pub const QUALITY_H264_320X180_30_YUV420P: &[u8] = include_bytes!("quality_320x180_30.yuv");
pub const QUALITY_OPUS_48K_MONO: &[u8] = include_bytes!("quality_48k_mono.opus");
pub const QUALITY_OPUS_48K_MONO_PCM_S16LE: &[u8] = include_bytes!("quality_48k_mono.s16le");
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

pub const QUALITY_VIDEO_WIDTH: usize = 320;
pub const QUALITY_VIDEO_HEIGHT: usize = 180;
pub const QUALITY_VIDEO_FPS: u32 = 30;
pub const QUALITY_VIDEO_FRAME_COUNT: usize = 90;
pub const QUALITY_VIDEO_RTP_CLOCK_RATE: u64 = 90_000;
pub const QUALITY_AUDIO_SAMPLE_RATE: usize = 48_000;
pub const QUALITY_AUDIO_FRAME_SAMPLES: usize = 960;
pub const QUALITY_AUDIO_FRAME_COUNT: usize = 150;
pub const QUALITY_AUDIO_RTP_CLOCK_RATE: u64 = 48_000;
pub const QUALITY_CORPUS_VIDEO_FRAME_COUNT: usize = 180;
pub const QUALITY_CORPUS_EPOCH_FRAME_COUNT: usize = 90;
pub const QUALITY_CORPUS_AUDIO_FRAME_COUNT: usize = 300;
pub const RAW_OPUS_20MS_MONO: &[u8] = &[
    0x08, 0x83, 0x6d, 0x82, 0xd0, 0x1c, 0xfd, 0xed, 0xc4, 0xec, 0xe7, 0xf3, 0x8f, 0xa4, 0x92, 0x47,
    0x98,
];

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

    pub const fn height(self) -> u32 {
        match self {
            Self::P180 => 180,
            Self::P360 => 360,
            Self::P720 => 720,
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

    pub fn encoded(&self) -> &'static [u8] {
        quality_video_bytes(self.source, self.layer).0
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
        decoded.get(start..start.checked_add(frame_bytes)?)
    }
}

pub fn quality_corpus_video(
    source: QualityVideoSource,
    layer: QualityVideoLayer,
) -> QualityCorpusVideo {
    let (encoded, reference_zstd) = quality_video_bytes(source, layer);
    let access_units = h264_access_units(encoded);
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
        let second = index.checked_div(50)?;
        let region = if matches!(second, 2 | 5) {
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
        decoded.get(start..start.checked_add(frame_bytes)?)
    }
}

pub fn quality_corpus_audio(source: QualityAudioSource) -> QualityCorpusAudio {
    let (encoded, reference_zstd) = match source {
        QualityAudioSource::Zero => (QUALITY_A0_OPUS, QUALITY_A0_REFERENCE),
        QualityAudioSource::One => (QUALITY_A1_OPUS, QUALITY_A1_REFERENCE),
    };
    let mut packets = ogg_opus_packets(encoded);
    debug_assert!(packets.len() > QUALITY_CORPUS_AUDIO_FRAME_COUNT);
    packets.truncate(QUALITY_CORPUS_AUDIO_FRAME_COUNT);
    QualityCorpusAudio {
        source,
        packets,
        reference_zstd,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct QualityVideoFrame<'a> {
    pub index: usize,
    pub rtp_timestamp: u64,
    pub encoded: &'a [u8],
    pub reference_yuv420p: &'a [u8],
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct QualityAudioFrame<'a> {
    pub index: usize,
    pub rtp_timestamp: u64,
    pub opus_packet: &'a [u8],
    pub reference_pcm_s16le: &'a [u8],
    pub sample_rate: usize,
    pub samples: usize,
}

#[derive(Debug)]
pub struct QualityAudioFixture {
    packets: Vec<&'static [u8]>,
}

impl QualityAudioFixture {
    pub fn frame(&self, index: usize) -> Option<QualityAudioFrame<'static>> {
        let opus_packet = *self.packets.get(index)?;
        let pcm_bytes = QUALITY_AUDIO_FRAME_SAMPLES.checked_mul(2)?;
        let pcm_start = index.checked_mul(pcm_bytes)?;
        let pcm_end = pcm_start.checked_add(pcm_bytes)?;
        let reference_pcm_s16le = QUALITY_OPUS_48K_MONO_PCM_S16LE.get(pcm_start..pcm_end)?;
        let rtp_timestamp = u64::try_from(index)
            .ok()?
            .checked_mul(u64::try_from(QUALITY_AUDIO_FRAME_SAMPLES).ok()?)?;
        Some(QualityAudioFrame {
            index,
            rtp_timestamp,
            opus_packet,
            reference_pcm_s16le,
            sample_rate: QUALITY_AUDIO_SAMPLE_RATE,
            samples: QUALITY_AUDIO_FRAME_SAMPLES,
        })
    }

    pub fn len(&self) -> usize {
        self.packets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    pub fn frame_for_rtp_timestamp(&self, timestamp: u64) -> Option<QualityAudioFrame<'static>> {
        let frame_samples = u64::try_from(QUALITY_AUDIO_FRAME_SAMPLES).ok()?;
        let index = timestamp.checked_div(frame_samples)?;
        let frame_count = u64::try_from(self.len()).ok()?;
        let index = usize::try_from(index.checked_rem(frame_count)?).ok()?;
        self.frame(index)
    }
}

pub fn quality_video_frame(index: usize) -> Option<QualityVideoFrame<'static>> {
    let encoded = *h264_access_units(QUALITY_H264_320X180_30).get(index)?;
    let frame_bytes = QUALITY_VIDEO_WIDTH
        .checked_mul(QUALITY_VIDEO_HEIGHT)?
        .checked_mul(3)?
        .checked_div(2)?;
    let reference_start = index.checked_mul(frame_bytes)?;
    let reference_end = reference_start.checked_add(frame_bytes)?;
    let reference_yuv420p = QUALITY_H264_320X180_30_YUV420P.get(reference_start..reference_end)?;
    let rtp_timestamp = u64::try_from(index)
        .ok()?
        .checked_mul(QUALITY_VIDEO_RTP_CLOCK_RATE)?
        .checked_div(u64::from(QUALITY_VIDEO_FPS))?;
    Some(QualityVideoFrame {
        index,
        rtp_timestamp,
        encoded,
        reference_yuv420p,
        width: QUALITY_VIDEO_WIDTH,
        height: QUALITY_VIDEO_HEIGHT,
    })
}

pub fn quality_video_frame_for_rtp_timestamp(timestamp: u64) -> Option<QualityVideoFrame<'static>> {
    let numerator = timestamp.checked_mul(u64::from(QUALITY_VIDEO_FPS))?;
    if numerator.checked_rem(QUALITY_VIDEO_RTP_CLOCK_RATE)? != 0 {
        return None;
    }
    let index = numerator.checked_div(QUALITY_VIDEO_RTP_CLOCK_RATE)?;
    let index = index.checked_rem(u64::try_from(QUALITY_VIDEO_FRAME_COUNT).ok()?)?;
    quality_video_frame(usize::try_from(index).ok()?)
}

pub fn quality_audio_fixture() -> QualityAudioFixture {
    let mut packets = ogg_opus_packets(QUALITY_OPUS_48K_MONO);
    debug_assert!(packets.len() >= QUALITY_AUDIO_FRAME_COUNT);
    packets.truncate(QUALITY_AUDIO_FRAME_COUNT);
    debug_assert_eq!(packets.len(), QUALITY_AUDIO_FRAME_COUNT);
    QualityAudioFixture { packets }
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

fn h264_access_units(data: &'static [u8]) -> Vec<&'static [u8]> {
    let mut start_codes = Vec::new();
    let mut index = 0usize;
    while index.saturating_add(3) < data.len() {
        let start_code_len = if data[index] == 0
            && data[index.saturating_add(1)] == 0
            && data[index.saturating_add(2)] == 0
            && data[index.saturating_add(3)] == 1
        {
            Some(4usize)
        } else if data[index] == 0
            && data[index.saturating_add(1)] == 0
            && data[index.saturating_add(2)] == 1
        {
            Some(3usize)
        } else {
            None
        };
        if let Some(start_code_len) = start_code_len {
            start_codes.push((index, start_code_len));
            index = index.saturating_add(start_code_len);
        } else {
            index = index.saturating_add(1);
        }
    }

    let mut units = Vec::new();
    let mut access_unit_start = start_codes.first().map(|(offset, _)| *offset);
    let mut seen_vcl = false;
    let has_aud = start_codes.iter().any(|(offset, start_code_len)| {
        data.get(offset.saturating_add(*start_code_len))
            .is_some_and(|header| header & 0x1f == 9)
    });
    for (position, (offset, start_code_len)) in start_codes.iter().enumerate() {
        let header = offset.saturating_add(*start_code_len);
        let next_offset = start_codes
            .get(position.saturating_add(1))
            .map(|(next, _)| *next)
            .unwrap_or(data.len());
        let Some(&nal_header) = data.get(header) else {
            continue;
        };
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
            && data
                .get(header.saturating_add(1))
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
    units
}

fn ogg_opus_packets(data: &'static [u8]) -> Vec<&'static [u8]> {
    let mut packets = Vec::new();
    let mut page = 0usize;
    while page < data.len() {
        let Some(header) = data.get(page..page.saturating_add(27)) else {
            break;
        };
        if header.get(..4) != Some(b"OggS") {
            break;
        }
        let segment_count = usize::from(*header.get(26).unwrap_or(&0));
        let lacing_start = page.saturating_add(27);
        let Some(lacing) = data.get(lacing_start..lacing_start.saturating_add(segment_count))
        else {
            break;
        };
        let mut packet_start = lacing_start.saturating_add(segment_count);
        let mut packet_len = 0usize;
        for &segment_len in lacing {
            let segment_len = usize::from(segment_len);
            let Some(_) = data.get(packet_start..packet_start.saturating_add(segment_len)) else {
                return packets;
            };
            packet_len = packet_len.saturating_add(segment_len);
            packet_start = packet_start.saturating_add(segment_len);
            if segment_len < 255 {
                let start = packet_start.saturating_sub(packet_len);
                let Some(packet) = data.get(start..packet_start) else {
                    return packets;
                };
                if !packet.starts_with(b"OpusHead") && !packet.starts_with(b"OpusTags") {
                    packets.push(packet);
                }
                packet_len = 0;
            }
        }
        if packet_len != 0 {
            return packets;
        }
        page = packet_start;
    }
    packets
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
    use super::*;
    use openh264::decoder::Decoder as H264Decoder;
    use openh264::formats::YUVSource;
    use sha2::{Digest, Sha256};

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

    fn annex_b_nalus(access_unit: &[u8]) -> Vec<&[u8]> {
        let mut starts = Vec::new();
        let mut index = 0usize;
        while index.saturating_add(3) < access_unit.len() {
            let header = if access_unit.get(index..index.saturating_add(4)) == Some(&[0, 0, 0, 1]) {
                Some(index.saturating_add(4))
            } else if access_unit.get(index..index.saturating_add(3)) == Some(&[0, 0, 1]) {
                Some(index.saturating_add(3))
            } else {
                None
            };
            if let Some(header) = header {
                starts.push((index, header));
                index = header;
            } else {
                index = index.saturating_add(1);
            }
        }
        starts
            .iter()
            .enumerate()
            .filter_map(|(position, (_, header))| {
                let end = starts
                    .get(position.saturating_add(1))
                    .map(|(start, _)| *start)
                    .unwrap_or(access_unit.len());
                access_unit
                    .get(*header..end)
                    .filter(|nalu| !nalu.is_empty())
            })
            .collect()
    }

    fn decoded_video_error(image: &impl YUVSource, reference: &[u8]) -> (u64, usize, u8) {
        let (width, height) = image.dimensions();
        let (y_stride, u_stride, v_stride) = image.strides();
        let y_len = width.saturating_mul(height);
        let chroma_len = y_len / 4;
        assert_eq!(
            reference.len(),
            y_len.saturating_add(chroma_len.saturating_mul(2))
        );
        let planes = [
            (&reference[..y_len], image.y(), y_stride, width, height),
            (
                &reference[y_len..y_len + chroma_len],
                image.u(),
                u_stride,
                width / 2,
                height / 2,
            ),
            (
                &reference[y_len + chroma_len..],
                image.v(),
                v_stride,
                width / 2,
                height / 2,
            ),
        ];
        let mut sum = 0u64;
        let mut samples = 0usize;
        let mut max = 0u8;
        for (expected, actual, stride, plane_width, plane_height) in planes {
            for row in 0..plane_height {
                let expected = &expected[row * plane_width..(row + 1) * plane_width];
                let actual = &actual[row * stride..row * stride + plane_width];
                for (&expected, &actual) in expected.iter().zip(actual) {
                    let error = expected.abs_diff(actual);
                    sum = sum.saturating_add(u64::from(error));
                    samples = samples.saturating_add(1);
                    max = max.max(error);
                }
            }
        }
        (sum, samples, max)
    }

    fn pcm_i16(bytes: &[u8]) -> Vec<i16> {
        bytes
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
            .collect()
    }

    fn mean_square(samples: &[i16]) -> u64 {
        samples
            .iter()
            .map(|sample| i64::from(*sample).unsigned_abs().pow(2))
            .sum::<u64>()
            / u64::try_from(samples.len()).expect("non-empty PCM window")
    }

    #[test]
    fn quality_corpus_manifest_hashes_every_checked_in_file() {
        let files = corpus_files();
        let claims: Vec<_> = QUALITY_CORPUS_MANIFEST
            .lines()
            .filter_map(|line| line.strip_prefix("file="))
            .collect();
        assert_eq!(claims.len(), files.len());
        for claim in claims {
            let mut fields = claim.split(',');
            let expected_hash = fields.next().expect("manifest hash");
            let expected_len: usize = fields
                .next()
                .expect("manifest length")
                .parse()
                .expect("numeric manifest length");
            let name = fields.next().expect("manifest filename");
            assert!(fields.next().is_none(), "extra manifest fields for {name}");
            let (_, bytes) = files
                .iter()
                .find(|(candidate, _)| *candidate == name)
                .unwrap_or_else(|| panic!("unknown manifest file {name}"));
            assert_eq!(bytes.len(), expected_len, "length of {name}");
            assert_eq!(
                format!("{:x}", Sha256::digest(bytes)),
                expected_hash,
                "hash of {name}"
            );
        }
    }

    #[test]
    fn quality_video_corpus_has_aligned_epochs_and_packetization_shapes() {
        for source in [QualityVideoSource::Zero, QualityVideoSource::One] {
            for layer in [
                QualityVideoLayer::P180,
                QualityVideoLayer::P360,
                QualityVideoLayer::P720,
            ] {
                let fixture = quality_corpus_video(source, layer);
                assert_eq!(fixture.len(), QUALITY_CORPUS_VIDEO_FRAME_COUNT);
                let mut idrs = Vec::new();
                let mut has_single_nalu = false;
                let mut has_aggregation_set = false;
                let mut has_fragmentation_nalu = false;
                for index in 0..fixture.len() {
                    let frame = fixture.frame(index).expect("declared corpus frame");
                    assert_eq!(
                        frame.identity.epoch,
                        index / QUALITY_CORPUS_EPOCH_FRAME_COUNT
                    );
                    assert_eq!(
                        frame.identity.frame,
                        index % QUALITY_CORPUS_EPOCH_FRAME_COUNT
                    );
                    assert_eq!(frame.identity.source, source);
                    assert_eq!(frame.identity.layer, layer);
                    let nalus = annex_b_nalus(frame.encoded);
                    let types: Vec<_> = nalus.iter().map(|nalu| nalu[0] & 0x1f).collect();
                    if types.contains(&5) {
                        idrs.push(index);
                        assert!(types.contains(&7), "IDR {index} lacks SPS");
                        assert!(types.contains(&8), "IDR {index} lacks PPS");
                    }
                    has_single_nalu |= nalus.iter().any(|nalu| nalu.len() <= 1_200);
                    has_aggregation_set |= types.contains(&7) && types.contains(&8);
                    has_fragmentation_nalu |= nalus.iter().any(|nalu| nalu.len() > 1_200);
                }
                assert_eq!(idrs, [0, QUALITY_CORPUS_EPOCH_FRAME_COUNT]);
                assert!(
                    has_single_nalu,
                    "{source:?} {layer:?} lacks single-NAL input"
                );
                assert!(
                    has_aggregation_set,
                    "{source:?} {layer:?} lacks SPS/PPS aggregation input"
                );
                assert!(
                    has_fragmentation_nalu,
                    "{source:?} {layer:?} lacks FU-A input"
                );
                assert!(fixture.frame(QUALITY_CORPUS_VIDEO_FRAME_COUNT).is_none());
                assert_eq!(
                    fixture
                        .frame_for_rtp_timestamp(QUALITY_VIDEO_RTP_CLOCK_RATE * 6)
                        .expect("looped timestamp")
                        .index,
                    0
                );
            }
        }
    }

    #[test]
    fn openh264_decodes_every_corpus_frame_against_its_reference() {
        for source in [QualityVideoSource::Zero, QualityVideoSource::One] {
            for layer in [
                QualityVideoLayer::P180,
                QualityVideoLayer::P360,
                QualityVideoLayer::P720,
            ] {
                let fixture = quality_corpus_video(source, layer);
                let reference = fixture.decode_reference().expect("zstd video reference");
                let (width, height) = layer.dimensions();
                assert_eq!(
                    reference.len(),
                    width * height * 3 / 2 * QUALITY_CORPUS_VIDEO_FRAME_COUNT
                );
                let mut decoder = H264Decoder::new().expect("OpenH264 decoder");
                for index in 0..fixture.len() {
                    let frame = fixture.frame(index).expect("corpus frame");
                    let image = decoder
                        .decode(frame.encoded)
                        .unwrap_or_else(|error| {
                            panic!("decode {source:?} {layer:?} frame {index}: {error}")
                        })
                        .unwrap_or_else(|| {
                            panic!("no image for {source:?} {layer:?} frame {index}")
                        });
                    assert_eq!(image.dimensions(), (width, height));
                    let expected = fixture
                        .reference_frame(&reference, index)
                        .expect("reference frame");
                    let (sum, samples, max) = decoded_video_error(&image, expected);
                    let mean = sum / u64::try_from(samples).expect("sample count");
                    assert!(
                        mean <= 2,
                        "{source:?} {layer:?} frame {index} mean error {mean}, max {max}"
                    );
                    assert!(
                        max <= 32,
                        "{source:?} {layer:?} frame {index} max error {max}"
                    );
                }
            }
        }
    }

    #[test]
    fn opus_corpus_has_exact_cadence_dtx_and_distinct_reference_envelopes() {
        let mut active_levels = Vec::new();
        for source in [QualityAudioSource::Zero, QualityAudioSource::One] {
            let fixture = quality_corpus_audio(source);
            assert_eq!(fixture.len(), QUALITY_CORPUS_AUDIO_FRAME_COUNT);
            let reference = fixture.decode_reference().expect("zstd audio reference");
            assert_eq!(reference.len(), QUALITY_AUDIO_SAMPLE_RATE * 6 * 2);
            let pcm = pcm_i16(&reference);
            let mut decoder =
                opus::Decoder::new(48_000, opus::Channels::Mono).expect("bundled Opus decoder");
            let mut output = [0i16; QUALITY_AUDIO_FRAME_SAMPLES];
            let mut dtx_packets = 0usize;
            let mut active_packets = 0usize;
            for index in 0..fixture.len() {
                let frame = fixture.frame(index).expect("corpus audio frame");
                assert_eq!(
                    frame.rtp_timestamp,
                    u64::try_from(index * QUALITY_AUDIO_FRAME_SAMPLES).unwrap()
                );
                assert_eq!(frame.symbol, u8::try_from(index % 4).unwrap());
                assert_eq!(
                    decoder
                        .decode(frame.opus_packet, &mut output, false)
                        .expect("Opus packet"),
                    QUALITY_AUDIO_FRAME_SAMPLES
                );
                match frame.region {
                    QualityAudioRegion::Active => {
                        active_packets += usize::from(frame.opus_packet.len() > 3);
                    }
                    QualityAudioRegion::SilenceDtx => {
                        dtx_packets += usize::from(frame.opus_packet.len() <= 3);
                    }
                }
            }
            assert!(
                active_packets >= 195,
                "{source:?} active packets were not encoded normally"
            );
            assert!(dtx_packets >= 90, "{source:?} silence did not exercise DTX");
            let active = mean_square(&pcm[48_000..96_000]);
            let silence = mean_square(&pcm[120_000..144_000]);
            assert!(
                active > silence.saturating_mul(1_000),
                "{source:?} active/silence envelope is ambiguous"
            );
            active_levels.push(active);
        }
        assert_ne!(
            quality_corpus_audio(QualityAudioSource::Zero)
                .frame(0)
                .unwrap()
                .opus_packet,
            quality_corpus_audio(QualityAudioSource::One)
                .frame(0)
                .unwrap()
                .opus_packet
        );
        assert_ne!(active_levels[0], active_levels[1]);
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

    #[test]
    fn quality_video_references_match_the_encoded_sequence() {
        assert_eq!(
            h264_access_units(QUALITY_H264_320X180_30).len(),
            QUALITY_VIDEO_FRAME_COUNT
        );
        let first = quality_video_frame(0).expect("first quality video frame");
        let last = quality_video_frame(QUALITY_VIDEO_FRAME_COUNT.saturating_sub(1))
            .expect("last quality video frame");
        assert_eq!(first.rtp_timestamp, 0);
        assert_eq!(
            last.rtp_timestamp,
            QUALITY_VIDEO_RTP_CLOCK_RATE
                .saturating_mul(QUALITY_VIDEO_FRAME_COUNT.saturating_sub(1) as u64)
                / u64::from(QUALITY_VIDEO_FPS)
        );
        assert_eq!(first.width, QUALITY_VIDEO_WIDTH);
        assert_eq!(first.height, QUALITY_VIDEO_HEIGHT);
        assert_eq!(
            first.reference_yuv420p.len(),
            QUALITY_VIDEO_WIDTH
                .saturating_mul(QUALITY_VIDEO_HEIGHT)
                .saturating_mul(3)
                / 2
        );
        assert!(!first.encoded.is_empty());
        assert!(quality_video_frame(QUALITY_VIDEO_FRAME_COUNT).is_none());
        assert_eq!(
            quality_video_frame_for_rtp_timestamp(
                QUALITY_VIDEO_RTP_CLOCK_RATE
                    .checked_mul(3)
                    .expect("quality video loop timestamp"),
            )
            .expect("quality video loop frame")
            .index,
            0
        );
    }

    #[test]
    fn quality_audio_references_match_the_opus_sequence() {
        let fixture = quality_audio_fixture();
        assert_eq!(fixture.len(), QUALITY_AUDIO_FRAME_COUNT);
        let first = fixture.frame(0).expect("first quality audio frame");
        let last = fixture
            .frame(QUALITY_AUDIO_FRAME_COUNT.saturating_sub(1))
            .expect("last quality audio frame");
        assert_eq!(first.rtp_timestamp, 0);
        assert_eq!(
            last.rtp_timestamp,
            QUALITY_AUDIO_FRAME_SAMPLES.saturating_mul(QUALITY_AUDIO_FRAME_COUNT.saturating_sub(1))
                as u64
        );
        assert_eq!(first.sample_rate, QUALITY_AUDIO_SAMPLE_RATE);
        assert_eq!(first.samples, QUALITY_AUDIO_FRAME_SAMPLES);
        assert_eq!(
            first.reference_pcm_s16le.len(),
            QUALITY_AUDIO_FRAME_SAMPLES.saturating_mul(2)
        );
        assert!(!first.opus_packet.is_empty());
        assert!(fixture.frame(QUALITY_AUDIO_FRAME_COUNT).is_none());
        assert_eq!(
            fixture
                .frame_for_rtp_timestamp(
                    u64::try_from(QUALITY_AUDIO_FRAME_COUNT)
                        .expect("quality audio frame count")
                        .checked_mul(u64::try_from(QUALITY_AUDIO_FRAME_SAMPLES).expect("samples"))
                        .expect("quality audio loop timestamp"),
                )
                .expect("quality audio loop frame")
                .index,
            0
        );
    }
}

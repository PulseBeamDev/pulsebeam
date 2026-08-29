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

pub const QUALITY_VIDEO_WIDTH: usize = 320;
pub const QUALITY_VIDEO_HEIGHT: usize = 180;
pub const QUALITY_VIDEO_FPS: u32 = 30;
pub const QUALITY_VIDEO_FRAME_COUNT: usize = 90;
pub const QUALITY_VIDEO_RTP_CLOCK_RATE: u64 = 90_000;
pub const QUALITY_AUDIO_SAMPLE_RATE: usize = 48_000;
pub const QUALITY_AUDIO_FRAME_SAMPLES: usize = 960;
pub const QUALITY_AUDIO_FRAME_COUNT: usize = 150;
pub const QUALITY_AUDIO_RTP_CLOCK_RATE: u64 = 48_000;
pub const RAW_OPUS_20MS_MONO: &[u8] = &[
    0x08, 0x83, 0x6d, 0x82, 0xd0, 0x1c, 0xfd, 0xed, 0xc4, 0xec, 0xe7, 0xf3, 0x8f, 0xa4, 0x92, 0x47,
    0x98,
];

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
    let mut access_unit_start = start_codes.first().map(|(offset, _)| *offset).unwrap_or(0);
    let mut seen_vcl = false;
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
        let starts_new_access_unit = matches!(nal_type, 1..=5)
            && data
                .get(header.saturating_add(1))
                .is_some_and(|byte| byte & 0x80 != 0);
        if starts_new_access_unit && seen_vcl {
            let Some(access_unit) = data.get(access_unit_start..*offset) else {
                continue;
            };
            if !access_unit.is_empty() {
                units.push(access_unit);
            }
            access_unit_start = *offset;
        }
        if matches!(nal_type, 1..=5) {
            seen_vcl = true;
        }
        debug_assert!(header < next_offset || header == data.len());
    }
    if seen_vcl
        && let Some(access_unit) = data.get(access_unit_start..)
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
    // Tests assert by panicking; the process ending is the mechanism.
    use super::*;

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

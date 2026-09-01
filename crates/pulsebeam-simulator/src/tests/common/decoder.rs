use openh264::decoder::Decoder as H264Decoder;
use openh264::formats::YUVSource;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Decoder(String),
    ReferenceMismatch(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decoder(error) | Self::ReferenceMismatch(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for DecodeError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReferenceError {
    pub sum: u64,
    pub samples: u64,
    pub max: u16,
}

pub const MAX_REFERENCE_MEAN_ABSOLUTE: u64 = 5_500;
pub const MAX_REFERENCE_PEAK: u16 = 32;
pub const MAX_AUDIO_REFERENCE_PEAK: u16 = 24_000;

impl ReferenceError {
    pub fn mean_absolute(self) -> Option<u64> {
        self.sum.checked_div(self.samples)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedVideo {
    pub width: usize,
    pub height: usize,
    pub reference_error: ReferenceError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedVideoObservation {
    pub width: usize,
    pub height: usize,
    pub reference_error: Option<ReferenceError>,
}

pub struct H264ReferenceDecoder {
    source: String,
    stream: String,
    decoder: H264Decoder,
}

impl H264ReferenceDecoder {
    pub fn new(source: impl Into<String>, stream: impl Into<String>) -> Self {
        let source = source.into();
        let stream = stream.into();
        let decoder = H264Decoder::new().unwrap_or_else(|error| {
            panic!(
                "OpenH264 initialization failed for source {source:?}, stream {stream:?}: {error}"
            )
        });
        Self {
            source,
            stream,
            decoder,
        }
    }

    pub fn reset(&mut self) {
        self.decoder = H264Decoder::new().unwrap_or_else(|error| {
            panic!(
                "OpenH264 reinitialization failed for source {:?}, stream {:?}: {error}",
                self.source, self.stream
            )
        });
    }

    pub fn decode(&mut self, access_unit: &[u8], reference_yuv420p: &[u8]) -> DecodedVideo {
        let decoded = self
            .try_decode_observation(access_unit, Some(reference_yuv420p))
            .unwrap_or_else(|error| {
                panic!(
                    "OpenH264 decode failed for source {:?}, stream {:?}: {error}",
                    self.source, self.stream
                )
            });
        let reference_error = decoded.reference_error.unwrap_or_else(|| {
            panic!(
                "H.264 reference mismatch for source {:?}, stream {:?}",
                self.source, self.stream
            )
        });
        DecodedVideo {
            width: decoded.width,
            height: decoded.height,
            reference_error,
        }
    }

    pub fn try_decode_observation(
        &mut self,
        access_unit: &[u8],
        reference_yuv420p: Option<&[u8]>,
    ) -> Result<DecodedVideoObservation, DecodeError> {
        debug_assert!(!access_unit.is_empty());
        if access_unit.is_empty() {
            return Err(DecodeError::Decoder(format!(
                "empty H.264 access unit for source {:?}, stream {:?}",
                self.source, self.stream
            )));
        }
        let image = self.decoder.decode(access_unit).map_err(|error| {
            DecodeError::Decoder(format!(
                "OpenH264 decode failed for source {:?}, stream {:?}: {error}",
                self.source, self.stream
            ))
        })?.ok_or_else(|| {
            DecodeError::Decoder(format!(
                "OpenH264 produced no picture for complete access unit from source {:?}, stream {:?}",
                self.source, self.stream
            ))
        })?;
        let (width, height) = image.dimensions();
        if width == 0 || height == 0 {
            return Err(DecodeError::Decoder(format!(
                "OpenH264 returned invalid dimensions {width}x{height} for source {:?}, stream {:?}",
                self.source, self.stream
            )));
        }
        let reference_error = match reference_yuv420p {
            Some(reference) => Some(reference_error_yuv420p(&image, reference).ok_or_else(|| {
                DecodeError::ReferenceMismatch(format!(
                    "H.264 reference mismatch for source {:?}, stream {:?}: decoded dimensions {width}x{height}, reference bytes {}",
                    self.source,
                    self.stream,
                    reference.len()
                ))
            })?),
            None => None,
        };
        Ok(DecodedVideoObservation {
            width,
            height,
            reference_error,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedAudio {
    pub samples: usize,
    pub reference_error: ReferenceError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedAudioObservation {
    pub samples: usize,
    pub reference_error: Option<ReferenceError>,
}

pub struct OpusReferenceDecoder {
    source: String,
    stream: String,
    decoder: opus::Decoder,
    pcm: Box<[i16]>,
}

impl OpusReferenceDecoder {
    pub fn new(source: impl Into<String>, stream: impl Into<String>) -> Self {
        let source = source.into();
        let stream = stream.into();
        let decoder = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap_or_else(|error| {
            panic!("Opus initialization failed for source {source:?}, stream {stream:?}: {error}")
        });
        Self {
            source,
            stream,
            decoder,
            pcm: vec![0; 5_760].into_boxed_slice(),
        }
    }

    pub fn decode(&mut self, packet: &[u8], reference_pcm_s16le: &[u8]) -> DecodedAudio {
        let decoded = self
            .try_decode_observation(packet, Some(reference_pcm_s16le))
            .unwrap_or_else(|error| {
                panic!(
                    "Opus decode failed for source {:?}, stream {:?}: {error}",
                    self.source, self.stream
                )
            });
        let reference_error = decoded.reference_error.unwrap_or_else(|| {
            panic!(
                "Opus reference mismatch for source {:?}, stream {:?}",
                self.source, self.stream
            )
        });
        DecodedAudio {
            samples: decoded.samples,
            reference_error,
        }
    }

    pub fn try_decode_observation(
        &mut self,
        packet: &[u8],
        reference_pcm_s16le: Option<&[u8]>,
    ) -> Result<DecodedAudioObservation, DecodeError> {
        debug_assert!(!packet.is_empty());
        if packet.is_empty() {
            return Err(DecodeError::Decoder(format!(
                "empty Opus packet for source {:?}, stream {:?}",
                self.source, self.stream
            )));
        }
        let samples = self
            .decoder
            .decode(packet, &mut self.pcm, false)
            .map_err(|error| {
                DecodeError::Decoder(format!(
                    "Opus decode failed for source {:?}, stream {:?}: {error}",
                    self.source, self.stream
                ))
            })?;
        let pcm = self.pcm.get(..samples).ok_or_else(|| {
            DecodeError::Decoder(format!(
                "Opus returned {samples} samples outside its output buffer for source {:?}, stream {:?}",
                self.source, self.stream
            ))
        })?;
        let reference_error = match reference_pcm_s16le {
            Some(reference) => Some(reference_error_pcm(pcm, reference).ok_or_else(|| {
                DecodeError::ReferenceMismatch(format!(
                    "Opus reference mismatch for source {:?}, stream {:?}: decoded samples {samples}, reference bytes {}",
                    self.source,
                    self.stream,
                    reference.len()
                ))
            })?),
            None => None,
        };
        Ok(DecodedAudioObservation {
            samples,
            reference_error,
        })
    }
}

fn reference_error_yuv420p(image: &impl YUVSource, reference: &[u8]) -> Option<ReferenceError> {
    let (width, height) = image.dimensions();
    debug_assert!(width > 0 && height > 0);
    if width % 2 != 0 || height % 2 != 0 {
        return None;
    }
    let y_len = width.checked_mul(height)?;
    let chroma_len = y_len.checked_div(4)?;
    let u_start = y_len;
    let u_end = u_start.checked_add(chroma_len)?;
    let v_end = u_end.checked_add(chroma_len)?;
    if reference.len() != v_end {
        return None;
    }
    let (y_stride, u_stride, v_stride) = image.strides();
    debug_assert!(y_stride >= width);
    debug_assert!(u_stride >= width / 2);
    debug_assert!(v_stride >= width / 2);
    let y_reference = reference.get(..y_len)?;
    let u_reference = reference.get(y_len..u_end)?;
    let v_reference = reference.get(u_end..v_end)?;
    let y_error = plane_error(y_reference, image.y(), y_stride, width, height)?;
    let u_error = plane_error(u_reference, image.u(), u_stride, width / 2, height / 2)?;
    let v_error = plane_error(v_reference, image.v(), v_stride, width / 2, height / 2)?;
    Some(combine_errors([y_error, u_error, v_error]))
}

fn plane_error(
    reference: &[u8],
    decoded: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> Option<ReferenceError> {
    debug_assert!(width > 0 && height > 0);
    debug_assert!(width <= stride);
    let expected_len = width.checked_mul(height)?;
    if reference.len() != expected_len {
        return None;
    }
    let mut sum = 0u64;
    let mut max = 0u16;
    for row in 0..height {
        let decoded_start = row.checked_mul(stride)?;
        let decoded_end = decoded_start.checked_add(width)?;
        let reference_start = row.checked_mul(width)?;
        let reference_end = reference_start.checked_add(width)?;
        let decoded_row = decoded.get(decoded_start..decoded_end)?;
        let reference_row = reference.get(reference_start..reference_end)?;
        for (&expected, &actual) in reference_row.iter().zip(decoded_row) {
            let error = u16::from(expected.abs_diff(actual));
            sum = sum.checked_add(u64::from(error))?;
            max = max.max(error);
        }
    }
    Some(ReferenceError {
        sum,
        samples: u64::try_from(expected_len).ok()?,
        max,
    })
}

fn reference_error_pcm(pcm: &[i16], reference: &[u8]) -> Option<ReferenceError> {
    let expected_len = pcm.len().checked_mul(2)?;
    if reference.len() != expected_len {
        return None;
    }
    let mut sum = 0u64;
    let mut max = 0u16;
    for (actual, bytes) in pcm.iter().zip(reference.chunks_exact(2)) {
        let bytes = <[u8; 2]>::try_from(bytes).ok()?;
        let expected = i16::from_le_bytes(bytes);
        let error = actual.abs_diff(expected);
        sum = sum.checked_add(u64::from(error))?;
        max = max.max(error);
    }
    Some(ReferenceError {
        sum,
        samples: u64::try_from(pcm.len()).ok()?,
        max,
    })
}

fn combine_errors(errors: [ReferenceError; 3]) -> ReferenceError {
    let mut combined = ReferenceError::default();
    for error in errors {
        combined.sum = combined
            .sum
            .checked_add(error.sum)
            .unwrap_or_else(|| panic!("reference error sum overflowed"));
        combined.samples = combined
            .samples
            .checked_add(error.samples)
            .unwrap_or_else(|| panic!("reference error sample count overflowed"));
        combined.max = combined.max.max(error.max);
    }
    combined
}

#[cfg(test)]
mod tests {
    use super::{H264ReferenceDecoder, OpusReferenceDecoder};
    use pulsebeam_agent::{FrameReceiver, FrameSender, MediaFrame, MediaTime};
    use pulsebeam_testdata::{
        QualityVideoLayer, QualityVideoSource, RAW_H264_FULL_CBR, RAW_H264_HALF_CBR,
        RAW_H264_QUARTER_CBR, quality_corpus_audio, quality_corpus_video,
    };
    use std::sync::Arc;
    use tokio::time::Instant;

    #[test]
    fn decoder_h264_returns_dimensions_and_reference_facts() {
        let corpus = quality_corpus_video(QualityVideoSource::Zero, QualityVideoLayer::P180);
        let frame = corpus.frame(0).expect("quality H.264 frame");
        let reference = corpus.decode_reference().expect("quality H.264 reference");
        let reference = corpus
            .reference_frame(&reference, frame.index)
            .expect("quality H.264 reference frame");
        let mut decoder = H264ReferenceDecoder::new("quality-zero", "video-180p");
        let decoded = decoder.decode(frame.encoded, reference);
        assert_eq!(
            (decoded.width, decoded.height),
            QualityVideoLayer::P180.dimensions()
        );
        assert!(decoded.reference_error.samples > 0);
        assert!(
            decoded.reference_error.mean_absolute().unwrap_or(u64::MAX) <= 5_500,
            "unexpected H.264 fixture error: {:?}",
            decoded.reference_error
        );
        assert!(decoded.reference_error.max <= 32);
    }

    #[test]
    fn decoder_opus_returns_sample_count_and_reference_facts() {
        let corpus = quality_corpus_audio(pulsebeam_testdata::QualityAudioSource::Zero);
        let frame = corpus.frame(0).expect("quality Opus frame");
        let reference = corpus.decode_reference().expect("quality Opus reference");
        let reference = corpus
            .reference_frame(&reference, frame.index)
            .expect("quality Opus reference frame");
        let mut decoder = OpusReferenceDecoder::new("quality-zero", "audio-mono");
        let decoded = decoder.decode(frame.opus_packet, reference);
        assert_eq!(decoded.samples, 960);
        assert!(decoded.reference_error.samples > 0);
        assert!(
            decoded.reference_error.mean_absolute().unwrap_or(u64::MAX) <= 5_500,
            "unexpected Opus fixture error: {:?}",
            decoded.reference_error
        );
    }

    #[test]
    fn raw_h264_round_trips_through_agent_pipeline_and_decodes() {
        let access_units: Vec<_> = pulsebeam_agent::media::H264FrameSlicer::new(RAW_H264_FULL_CBR)
            .take(180)
            .collect();
        assert!(access_units.len() >= 100);
        let mut sender = FrameSender::h264(pulsebeam_agent::Mid::from("v0"), None, 1, 1);
        let mut receiver = FrameReceiver::with_h264();
        let mut decoder = H264ReferenceDecoder::new("raw", "video");
        let mut decoded = 0usize;
        let mut received_frames = 0usize;
        for (index, access_unit) in access_units.iter().enumerate() {
            let frame = MediaFrame {
                ts: MediaTime::from_90khz(u64::try_from(index).unwrap_or(0) * 3_000),
                data: Arc::from(*access_unit),
                capture_time: Instant::now(),
                abs_capture_time: None,
                contiguous: true,
                is_keyframe: index == 0,
                audio_level: None,
                voice_activity: None,
                target_bitrate_bps: None,
                resolution: None,
                dependency_descriptor: None,
                temporal_layers: None,
            };
            for packet in sender.packetize(&frame) {
                for received in receiver.push(packet) {
                    received_frames += 1;
                    if decoder.try_decode_observation(&received.data, None).is_ok() {
                        decoded += 1;
                    }
                }
            }
        }
        for received in receiver.flush() {
            received_frames += 1;
            if decoder.try_decode_observation(&received.data, None).is_ok() {
                decoded += 1;
            }
        }
        assert_eq!(received_frames, access_units.len());
        assert_eq!(decoded, access_units.len());
    }

    #[test]
    fn raw_h264_fixture_keyframes_decode_standalone() {
        for (name, data) in [
            ("full", RAW_H264_FULL_CBR),
            ("half", RAW_H264_HALF_CBR),
            ("quarter", RAW_H264_QUARTER_CBR),
        ] {
            let frame = pulsebeam_agent::media::H264FrameSlicer::new(data)
                .next()
                .expect("raw H264 fixture has a first access unit");
            let mut decoder = H264ReferenceDecoder::new(name, "video");
            assert!(
                decoder.try_decode_observation(frame, None).is_ok(),
                "{name} raw fixture keyframe must decode"
            );
        }
    }
}

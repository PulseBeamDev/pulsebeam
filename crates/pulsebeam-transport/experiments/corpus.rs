//! One corpus capture ledger and codec inputs shared by every adaptive arm.
use super::*;
use pulsebeam_testdata::{
    QUALITY_CORPUS_EPOCH_FRAME_COUNT, QUALITY_VIDEO_FPS, QualityCaptureIdentity,
    QualitySourceCapture, QualitySourceSequence, QualityVideoLayer, QualityVideoSource,
    quality_common_view,
};

pub(super) const LAYERS: [QualityVideoLayer; 3] = [
    QualityVideoLayer::P180,
    QualityVideoLayer::P360,
    QualityVideoLayer::P720,
];
pub(super) const CAPS: [u32; 3] = [200_000, 600_000, 1_400_000];
const MINIMUM: [u32; 3] = [50_000, 150_000, 400_000];
pub(super) const FPS: u32 = QUALITY_VIDEO_FPS;
pub(super) const RTP_STEP: u32 = 90_000 / FPS;
pub(super) const SOURCE_POLICY: &str = "corpus-p720-box-floor-crc16-luma-v1";
pub(super) const ALLOCATION_POLICY: &str = "aggregate-rung-minima-low-first-v1";

pub(super) struct Corpus {
    sequence: QualitySourceSequence,
    source: u64,
    lifetime: u64,
}
pub(super) struct Capture {
    pub(super) offered: presentation::Capture,
    pub(super) full: Vec<u8>,
}
impl Corpus {
    pub(super) fn new(source: QualityVideoSource, lifetime: u64) -> Self {
        Self {
            sequence: QualitySourceSequence::decode(source).unwrap(),
            source: match source {
                QualityVideoSource::Zero => 0,
                QualityVideoSource::One => 1,
            },
            lifetime,
        }
    }
    pub(super) fn len(&self) -> usize {
        self.sequence.len()
    }
    pub(super) fn offered(&self, index: usize) -> presentation::Capture {
        let capture = self.sequence.capture(index).unwrap();
        presentation::Capture {
            id: presentation::CaptureId {
                source: self.source,
                lifetime: self.lifetime,
                epoch: u64::try_from(capture.identity.epoch).unwrap(),
                capture: u64::try_from(index).unwrap(),
            },
            at_ns: capture.capture_ns,
        }
    }
    pub(super) fn frame(&self, index: usize) -> Capture {
        let mut full = self.sequence.capture(index).unwrap().i420.to_vec();
        let offered = self.offered(index);
        stamp(&mut full, offered.id);
        Capture { offered, full }
    }
}
impl Capture {
    pub(super) fn rung(&self, layer: QualityVideoLayer) -> Vec<u8> {
        let id = self.offered.id;
        QualitySourceCapture {
            identity: QualityCaptureIdentity {
                source: match id.source {
                    0 => QualityVideoSource::Zero,
                    1 => QualityVideoSource::One,
                    _ => panic!("unknown corpus source"),
                },
                epoch: usize::try_from(id.epoch).unwrap(),
                frame: usize::try_from(id.capture).unwrap() % QUALITY_CORPUS_EPOCH_FRAME_COUNT,
            },
            index: usize::try_from(id.capture).unwrap(),
            capture_ns: self.offered.at_ns,
            i420: &self.full,
        }
        .representation(layer)
        .unwrap()
    }
}

fn checksum(id: presentation::CaptureId) -> u16 {
    let mut crc = 0xffffu16;
    for byte in id
        .source
        .to_be_bytes()
        .into_iter()
        .chain(id.lifetime.to_be_bytes())
        .chain(id.epoch.to_be_bytes())
        .chain(id.capture.to_be_bytes())
    {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x1021
            };
        }
    }
    crc
}
fn stamp(full: &mut [u8], id: presentation::CaptureId) {
    let word = u32::try_from(id.capture).unwrap() | (u32::from(checksum(id)) << 16);
    // Stamp once in the canonical view before deriving any rung/reference.
    for row in 0..2 {
        for bit in 0..32 {
            let high = (word & (1 << bit) != 0) ^ (row == 1);
            for y in row * 64..(row + 1) * 64 {
                full[y * 1280 + bit * 32..y * 1280 + (bit + 1) * 32].fill(if high {
                    235
                } else {
                    16
                });
            }
        }
    }
}

/// Decode pixels, then validate the entire scope/capture codeword. No catalog inference.
pub(super) fn identify(
    frame: &VideoFrame,
    source: u64,
    lifetime: u64,
) -> Option<presentation::CaptureId> {
    if frame.width < 320 || frame.height < 180 {
        return None;
    }
    let mut word = 0u32;
    for bit in 0..32 {
        let sample = |row: usize| {
            let mut sum = 0u32;
            for y in row * 16 + 6..row * 16 + 10 {
                for x in bit * 8 + 2..bit * 8 + 6 {
                    sum += u32::from(
                        frame.buffer.as_bytes()[(y * frame.height as usize / 180)
                            * frame.width as usize
                            + x * frame.width as usize / 320],
                    );
                }
            }
            sum / 16
        };
        let a = sample(0);
        let b = sample(1);
        if a.abs_diff(b) < 100 {
            return None;
        }
        if a > b {
            word |= 1 << bit;
        }
    }
    let capture = u64::from(word & 0xffff);
    let id = presentation::CaptureId {
        source,
        lifetime,
        epoch: capture / u64::try_from(QUALITY_CORPUS_EPOCH_FRAME_COUNT).unwrap(),
        capture,
    };
    (u32::from(checksum(id)) == word >> 16).then_some(id)
}

/// One budget, including every active representation. No rung gets a fresh controller.
pub(super) fn allocate(budget_bps: u64) -> [u32; 3] {
    let mut count = 0;
    let mut required = 0u64;
    for minimum in MINIMUM {
        required += u64::from(minimum);
        if budget_bps >= required {
            count += 1;
        }
    }
    let mut rates = [0; 3];
    let mut remaining = budget_bps;
    for index in 0..count {
        rates[index] = MINIMUM[index];
        remaining -= u64::from(rates[index]);
    }
    for index in 0..count {
        let extra = remaining.min(u64::from(CAPS[index] - rates[index]));
        rates[index] += u32::try_from(extra).unwrap();
        remaining -= extra;
    }
    assert!(rates.iter().map(|rate| u64::from(*rate)).sum::<u64>() <= budget_bps);
    rates
}

pub(super) struct Encoded {
    pub(super) id: presentation::CaptureId,
    pub(super) layer: usize,
    pub(super) timestamp: u32,
    pub(super) bytes: Vec<u8>,
    pub(super) key: bool,
}
pub(super) struct Source {
    encoders: [encoder::SourceEncoder; 3],
    active: [bool; 3],
    pub(super) omissions: [u64; 3],
    pub(super) suspended: [u64; 3],
}
impl Source {
    pub(super) fn new() -> Self {
        Self {
            encoders: std::array::from_fn(|index| {
                let (width, height) = LAYERS[index].dimensions();
                encoder::SourceEncoder::with_format(
                    u32::try_from(width).unwrap(),
                    u32::try_from(height).unwrap(),
                    FPS,
                    MINIMUM[index],
                    CAPS[index],
                    true,
                )
            }),
            active: [false; 3],
            omissions: [0; 3],
            suspended: [0; 3],
        }
    }
    pub(super) fn encode(
        &mut self,
        capture: &Capture,
        budget: u64,
        request: [bool; 3],
    ) -> Vec<Encoded> {
        let rates = allocate(budget);
        let mut output = Vec::new();
        for index in 0..3 {
            if rates[index] == 0 {
                self.active[index] = false;
                self.suspended[index] += 1;
                continue;
            }
            let (width, height) = LAYERS[index].dimensions();
            let timestamp = u32::try_from(capture.offered.id.capture).unwrap() * RTP_STEP;
            let raw = VideoFrame::i420(
                u32::try_from(width).unwrap(),
                u32::try_from(height).unwrap(),
                capture.rung(LAYERS[index]),
                i64::try_from(capture.offered.at_ns / 1000).unwrap(),
                timestamp,
            )
            .unwrap();
            let key = !self.active[index]
                || request[index]
                || capture
                    .offered
                    .id
                    .capture
                    .is_multiple_of(u64::try_from(QUALITY_CORPUS_EPOCH_FRAME_COUNT).unwrap());
            self.active[index] = true;
            if let Some((bytes, key)) =
                self.encoders[index].encode(&raw, rates[index], f64::from(FPS), key)
            {
                output.push(Encoded {
                    id: capture.offered.id,
                    layer: index,
                    timestamp,
                    bytes,
                    key,
                });
            } else {
                self.omissions[index] += 1;
            }
        }
        output
    }
}

pub(super) fn common_view(frame: &VideoFrame, layer: usize) -> Vec<u8> {
    let (width, height) = LAYERS[layer].dimensions();
    assert_eq!(
        (frame.width as usize, frame.height as usize),
        (width, height)
    );
    quality_common_view(frame.buffer.as_bytes(), LAYERS[layer]).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aggregate_allocations_and_real_codec_corpus_identity_cover_resumption_and_epochs() {
        assert_eq!(SOURCE_POLICY, "corpus-p720-box-floor-crc16-luma-v1");
        assert_eq!(ALLOCATION_POLICY, "aggregate-rung-minima-low-first-v1");
        for budget in [
            0,
            49_999,
            50_000,
            199_999,
            200_000,
            300_000,
            599_999,
            600_000,
            2_200_000,
            u64::MAX,
        ] {
            let rates = allocate(budget);
            assert!(rates.iter().map(|rate| u64::from(*rate)).sum::<u64>() <= budget);
            for index in 0..3 {
                assert!(rates[index] <= CAPS[index]);
            }
        }
        assert_eq!(allocate(300_000), [150_000, 150_000, 0]);
        assert_eq!(allocate(2_200_000), CAPS);
        let corpus = Corpus::new(QualityVideoSource::Zero, 7);
        assert_eq!(corpus.len(), 180);
        assert_eq!(corpus.offered(90).at_ns, 3_000_000_000);
        let early = receiver::Receiver::new(&corpus, measurement::CodecClock::Direct90k);
        let incomplete = comparison::summary(&corpus, &early, 10_000_000, false);
        assert_eq!(incomplete["qualified"], false);
        assert_eq!(incomplete["unknown"], 180);
        assert!(incomplete["presented_ns"].is_null());
        let mut source = Source::new();
        let mut observations = measurement::Observations::new(&corpus);
        let mut decoders: [_; 3] = std::array::from_fn(|_| decoder::Decoder::new());
        for index in [0, 89, 90, 179] {
            let capture = corpus.frame(index);
            let encoded = source.encode(&capture, 2_200_000, [true; 3]);
            assert_eq!(encoded.len(), 3);
            let reference = capture.rung(QualityVideoLayer::P180);
            for image in encoded {
                assert!(image.key);
                let mut input = EncodedImage::default();
                input.set_buffer(&EncodedVideoBuffer::from_bytes(&image.bytes));
                let mut metadata = input.metadata();
                let (width, height) = LAYERS[image.layer].dimensions();
                metadata.width = u32::try_from(width).unwrap();
                metadata.height = u32::try_from(height).unwrap();
                metadata.rtp_timestamp = image.timestamp;
                metadata.frame_type = EncodedImageFrameType::Key;
                input.set_metadata(&metadata).unwrap();
                let decoder = &mut decoders[image.layer];
                assert!(decoder.decode_image(
                    &input,
                    false,
                    i64::try_from(capture.offered.at_ns / 1_000_000).unwrap()
                ));
                let output = decoder.output.try_recv().unwrap();
                assert_eq!(identify(&output, 0, 7), Some(image.id));
                assert_eq!(identify(&output, 1, 7), None);
                assert_eq!(identify(&output, 0, 8), None);
                assert_eq!(output.rtp_timestamp, image.timestamp);
                observations.decoded(
                    &output,
                    image.layer,
                    capture.offered.at_ns + 10_000_000,
                    measurement::CodecClock::Direct90k,
                );
                let view = common_view(&output, image.layer);
                assert_eq!(view.len(), reference.len());
                let squared_error = view
                    .iter()
                    .zip(&reference)
                    .map(|(a, b)| u64::from(a.abs_diff(*b)).pow(2))
                    .sum::<u64>();
                // Fixed clean codec calibration, not an adaptive ranking threshold.
                assert!(
                    squared_error < 650 * u64::try_from(view.len()).unwrap(),
                    "common-view codec calibration: layer={}, capture={index}, squared_error={squared_error}",
                    image.layer
                );
            }
            if index == 0 {
                assert!(source.encode(&corpus.frame(1), 0, [false; 3]).is_empty());
                assert_eq!(source.suspended, [1; 3]);
                let resumed = source.encode(&corpus.frame(2), 2_200_000, [false; 3]);
                assert_eq!(resumed.len(), 3);
                assert!(resumed.iter().all(|image| image.key));
            }
        }
        let report = observations.finish(&corpus, 6_000_000_000, true);
        assert!(report.timeline.qualified);
        assert_eq!(
            report
                .timeline
                .decode
                .iter()
                .filter(|state| **state == presentation::DecodeStatus::Timely)
                .count(),
            4
        );
        assert_eq!(
            report
                .timeline
                .decode
                .iter()
                .filter(|state| **state == presentation::DecodeStatus::Missing)
                .count(),
            176
        );
        assert_eq!(report.timeline.startup_blank_ns, 10_000_000);
        assert!(report.timeline.held_ns > 0 && report.timeline.freeze_ns > 0);
        assert!(
            report.presented_squared_error_times_ns
                * u128::from(report.decoded_representation_samples)
                > u128::from(report.decoded_representation_squared_error)
                    * report.presented_samples_times_ns,
            "held pictures must be scored against current offered truth, not their own old reference"
        );
        let mut corrupt = corpus.frame(0).rung(QualityVideoLayer::P180);
        for row in 0..2 {
            for y in row * 16..(row + 1) * 16 {
                for x in 32..40 {
                    corrupt[y * 320 + x] = if row == 0 { 235 } else { 16 };
                }
            }
        }
        let wrong_capture = VideoFrame::i420(320, 180, corrupt, 0, 0).unwrap();
        assert_eq!(
            identify(&wrong_capture, 0, 7),
            None,
            "a stale but complementary identity bit must fail the scope/capture checksum"
        );
    }
}

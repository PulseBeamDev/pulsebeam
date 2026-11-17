//! Corpus-derived common source, distinct from independently labelled layer files.
use super::{
    QUALITY_CORPUS_EPOCH_FRAME_COUNT, QUALITY_CORPUS_VIDEO_FRAME_COUNT, QUALITY_VIDEO_FPS,
    QualityVideoLayer, QualityVideoSource, quality_corpus_video,
};
use std::io;

pub const QUALITY_COMMON_VIEW_POLICY: &str = "decoded-720p-i420-box-floor-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QualityCaptureIdentity {
    pub source: QualityVideoSource,
    pub epoch: usize,
    pub frame: usize,
}

/// One offered capture. Alternative representations share this identity and time.
#[derive(Clone, Copy, Debug)]
pub struct QualitySourceCapture<'a> {
    pub identity: QualityCaptureIdentity,
    pub index: usize,
    pub capture_ns: u64,
    /// Tightly packed 1280x720 I420, from the existing decoded P720 reference.
    pub i420: &'a [u8],
}

impl QualitySourceCapture<'_> {
    /// Derive raw encoder input, not the corpus's independently encoded layer file.
    pub fn representation(self, layer: QualityVideoLayer) -> Option<Vec<u8>> {
        scale_i420(self.i420, QualityVideoLayer::P720, layer)
    }

    pub fn common_view(self) -> Option<Vec<u8>> {
        self.representation(QualityVideoLayer::P180)
    }
}

/// One bounded six-second source lifetime. No implicit looping or epoch relabelling.
pub struct QualitySourceSequence {
    source: QualityVideoSource,
    decoded: Vec<u8>,
}

impl QualitySourceSequence {
    pub fn decode(source: QualityVideoSource) -> io::Result<Self> {
        let decoded = quality_corpus_video(source, QualityVideoLayer::P720).decode_reference()?;
        let expected = frame_bytes(QualityVideoLayer::P720)
            .and_then(|size| size.checked_mul(QUALITY_CORPUS_VIDEO_FRAME_COUNT));
        if Some(decoded.len()) != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "canonical source length",
            ));
        }
        Ok(Self { source, decoded })
    }

    pub const fn len(&self) -> usize {
        QUALITY_CORPUS_VIDEO_FRAME_COUNT
    }

    pub const fn is_empty(&self) -> bool {
        false
    }

    pub fn capture(&self, index: usize) -> Option<QualitySourceCapture<'_>> {
        if index >= self.len() {
            return None;
        }
        let size = frame_bytes(QualityVideoLayer::P720)?;
        let start = index.checked_mul(size)?;
        Some(QualitySourceCapture {
            identity: QualityCaptureIdentity {
                source: self.source,
                epoch: index.checked_div(QUALITY_CORPUS_EPOCH_FRAME_COUNT)?,
                frame: index.checked_rem(QUALITY_CORPUS_EPOCH_FRAME_COUNT)?,
            },
            index,
            capture_ns: u64::try_from(index)
                .ok()?
                .checked_mul(1_000_000_000)?
                .checked_div(u64::from(QUALITY_VIDEO_FPS))?,
            i420: self.decoded.get(start..start.checked_add(size)?)?,
        })
    }
}

/// Project tightly packed decoded I420 to the shared 320x180 quality view.
/// Each plane is box-averaged independently, with integer division rounding down.
pub fn quality_common_view(i420: &[u8], layer: QualityVideoLayer) -> Option<Vec<u8>> {
    scale_i420(i420, layer, QualityVideoLayer::P180)
}

fn frame_bytes(layer: QualityVideoLayer) -> Option<usize> {
    let (width, height) = layer.dimensions();
    width.checked_mul(height)?.checked_mul(3)?.checked_div(2)
}

fn scale_i420(input: &[u8], from: QualityVideoLayer, to: QualityVideoLayer) -> Option<Vec<u8>> {
    if input.len() != frame_bytes(from)? {
        return None;
    }
    if from == to {
        return Some(input.to_vec());
    }
    let (width, height) = from.dimensions();
    let (out_width, out_height) = to.dimensions();
    if out_width > width || out_height > height {
        return None;
    }
    let factor = width.checked_div(out_width)?;
    let mut output = Vec::with_capacity(frame_bytes(to)?);
    let mut offset = 0usize;
    for divisor in [1, 2, 2] {
        let plane_width = width.checked_div(divisor)?;
        let plane_height = height.checked_div(divisor)?;
        let end = offset.checked_add(plane_width.checked_mul(plane_height)?)?;
        let plane = input.get(offset..end)?;
        match factor {
            2 => box2(plane, plane_width, &mut output)?,
            4 => box4(plane, plane_width, &mut output)?,
            _ => return None,
        }
        offset = end;
    }
    Some(output)
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "four u8 samples sum to at most 1020, within u16"
)]
fn sum4([a, b, c, d]: [u8; 4]) -> u16 {
    u16::from(a) + u16::from(b) + u16::from(c) + u16::from(d)
}

fn box2(plane: &[u8], width: usize, output: &mut Vec<u8>) -> Option<()> {
    for block in plane.chunks_exact(width.checked_mul(2)?) {
        let mut rows = block.chunks_exact(width);
        let a = rows.next()?.as_chunks::<2>().0;
        let b = rows.next()?.as_chunks::<2>().0;
        for ([a, b], [c, d]) in a.iter().zip(b) {
            output.push(u8::try_from(sum4([*a, *b, *c, *d]) / 4).ok()?);
        }
    }
    Some(())
}

fn box4(plane: &[u8], width: usize, output: &mut Vec<u8>) -> Option<()> {
    for block in plane.chunks_exact(width.checked_mul(4)?) {
        let mut rows = block.chunks_exact(width);
        let a = rows.next()?.as_chunks::<4>().0;
        let b = rows.next()?.as_chunks::<4>().0;
        let c = rows.next()?.as_chunks::<4>().0;
        let d = rows.next()?.as_chunks::<4>().0;
        for ((a, b), (c, d)) in a.iter().zip(b).zip(c.iter().zip(d)) {
            let sum = sum4(*a)
                .checked_add(sum4(*b))?
                .checked_add(sum4(*c))?
                .checked_add(sum4(*d))?;
            output.push(u8::try_from(sum / 16).ok()?);
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_projection_has_exact_plane_boundaries_rounding_and_shape_checks() {
        let mut input = vec![0; 640 * 360 * 3 / 2];
        for (index, value) in input[..640 * 360].iter_mut().enumerate() {
            *value = u8::try_from((index % 640) % 4).expect("pattern");
        }
        input[640 * 360..640 * 360 * 5 / 4].fill(17);
        input[640 * 360 * 5 / 4..].fill(201);
        let view = quality_common_view(&input, QualityVideoLayer::P360).expect("I420 view");
        assert_eq!(view.len(), 320 * 180 * 3 / 2);
        assert_eq!(&view[..4], &[0, 2, 0, 2]);
        assert!(view[320 * 180..320 * 180 * 5 / 4].iter().all(|v| *v == 17));
        assert!(view[320 * 180 * 5 / 4..].iter().all(|v| *v == 201));
        assert!(quality_common_view(&input[..input.len() - 1], QualityVideoLayer::P360).is_none());
        assert_eq!(
            quality_common_view(&view, QualityVideoLayer::P180),
            Some(view)
        );
    }

    #[test]
    fn blocked_projection_matches_scalar_box_floor_for_every_rung_pair() {
        for from in [
            QualityVideoLayer::P180,
            QualityVideoLayer::P360,
            QualityVideoLayer::P720,
        ] {
            let input: Vec<_> = (0..frame_bytes(from).expect("size"))
                .map(|index| u8::try_from((index * 73 + index / 7) % 256).expect("pattern"))
                .collect();
            let (width, height) = from.dimensions();
            for to in [
                QualityVideoLayer::P180,
                QualityVideoLayer::P360,
                QualityVideoLayer::P720,
            ] {
                let (out_width, out_height) = to.dimensions();
                let actual = scale_i420(&input, from, to);
                if out_width > width {
                    assert!(actual.is_none());
                    continue;
                }
                let factor = width / out_width;
                let mut expected = Vec::new();
                let mut offset = 0;
                for divisor in [1, 2, 2] {
                    let plane_width = width / divisor;
                    for y in 0..out_height / divisor {
                        for x in 0..out_width / divisor {
                            let mut sum = 0u32;
                            for dy in 0..factor {
                                for dx in 0..factor {
                                    sum += u32::from(
                                        input[offset
                                            + (y * factor + dy) * plane_width
                                            + x * factor
                                            + dx],
                                    );
                                }
                            }
                            expected.push(
                                u8::try_from(sum / u32::try_from(factor * factor).expect("factor"))
                                    .expect("mean"),
                            );
                        }
                    }
                    offset += plane_width * (height / divisor);
                }
                assert_eq!(actual, Some(expected), "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn canonical_captures_share_content_time_and_identity_across_rungs() {
        for source in [QualityVideoSource::Zero, QualityVideoSource::One] {
            let sequence = QualitySourceSequence::decode(source).expect("canonical source");
            assert_eq!(sequence.len(), 180);
            assert!(!sequence.is_empty());
            assert!(sequence.capture(180).is_none());
            assert!(sequence.capture(usize::MAX).is_none());
            for index in [0, 89, 90, 179] {
                let capture = sequence.capture(index).expect("capture");
                assert_eq!(
                    capture.identity,
                    QualityCaptureIdentity {
                        source,
                        epoch: index / 90,
                        frame: index % 90,
                    }
                );
                assert_eq!(
                    capture.capture_ns,
                    u64::try_from(index).expect("index") * 1_000_000_000 / 30
                );
                let reference = capture.common_view().expect("common view");
                assert_eq!(
                    reference,
                    capture.representation(QualityVideoLayer::P180).expect("q")
                );
                assert_eq!(
                    capture.i420,
                    capture.representation(QualityVideoLayer::P720).expect("f")
                );
                let half = capture.representation(QualityVideoLayer::P360).expect("h");
                let projected =
                    quality_common_view(&half, QualityVideoLayer::P360).expect("h view");
                // Two floor-averaging stages can differ from one by at most one.
                assert!(
                    reference
                        .iter()
                        .zip(projected)
                        .all(|(a, b)| a.abs_diff(b) <= 1)
                );
            }
        }
    }
}

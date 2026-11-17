//! Shared decoder admission and actual callback collection, never packet-as-decode credit.
use super::*;
pub(super) struct Receiver {
    decoders: [decoder::Decoder; 3],
    pub(super) observations: measurement::Observations,
    clock: measurement::CodecClock,
    pub(super) waiting: [bool; 3],
    pub(super) complete: [u64; 3],
    pub(super) dependency_discarded: [u64; 3],
    pub(super) rejected: [u64; 3],
}
impl Receiver {
    pub(super) fn new(corpus: &corpus::Corpus, clock: measurement::CodecClock) -> Self {
        Self {
            decoders: std::array::from_fn(|_| decoder::Decoder::new()),
            observations: measurement::Observations::new(corpus),
            clock,
            waiting: [true; 3],
            complete: [0; 3],
            dependency_discarded: [0; 3],
            rejected: [0; 3],
        }
    }
    pub(super) fn unit(
        &mut self,
        layer: usize,
        bytes: &[u8],
        timestamp: u32,
        key: bool,
        missing: bool,
        at_ns: u64,
    ) {
        self.complete[layer] += 1;
        self.waiting[layer] |= missing;
        if self.waiting[layer] && !key {
            self.dependency_discarded[layer] += 1;
            return;
        }
        let mut image = EncodedImage::default();
        image.set_buffer(&EncodedVideoBuffer::from_bytes(bytes));
        let mut metadata = image.metadata();
        let (width, height) = corpus::LAYERS[layer].dimensions();
        metadata.width = u32::try_from(width).unwrap();
        metadata.height = u32::try_from(height).unwrap();
        metadata.rtp_timestamp = timestamp;
        metadata.frame_type = if key {
            EncodedImageFrameType::Key
        } else {
            EncodedImageFrameType::Delta
        };
        image.set_metadata(&metadata).unwrap();
        self.waiting[layer] = !self.decoders[layer].decode_image(
            &image,
            false,
            i64::try_from(at_ns / 1_000_000).unwrap(),
        );
        if self.waiting[layer] {
            self.rejected[layer] += 1;
        }
        self.collect(at_ns);
    }
    pub(super) fn collect(&mut self, at_ns: u64) {
        for layer in 0..3 {
            while let Ok(frame) = self.decoders[layer].output.try_recv() {
                self.observations.decoded(&frame, layer, at_ns, self.clock);
            }
        }
    }
}

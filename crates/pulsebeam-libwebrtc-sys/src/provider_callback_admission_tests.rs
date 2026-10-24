//! Dispatcher admission checks only. These invoke real standalone native
//! adapters, not an installed media consumer or an SDK codec-queue origin.
use crate::*;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<(&'static str, bool)>>>);
impl Probe {
    fn record(&self, label: &'static str) {
        self.0
            .lock()
            .unwrap()
            .push((label, crate::provider_panic::callback_active()));
    }
}
struct Selector(Probe);
impl VideoEncoderSelectorProvider for Selector {
    fn on_current_encoder(&mut self, _: SdpVideoFormat) {
        self.0.record("selector-current");
    }
    fn on_available_bitrate(&mut self, _: DataRate) -> Option<SdpVideoFormat> {
        self.0.record("selector-bitrate");
        None
    }
    fn on_encoder_broken(&mut self) -> Option<SdpVideoFormat> {
        self.0.record("selector-broken");
        None
    }
}
impl Drop for Selector {
    fn drop(&mut self) {
        self.0.record("selector-drop");
    }
}
struct Encoded(Probe);
impl EncodedImageCallbackProvider for Encoded {
    fn encoded(
        &self,
        _: EncodedImage,
        _: Option<OwnedCodecSpecificInfo>,
    ) -> EncodedImageCallbackResult {
        self.0.record("encoded");
        EncodedImageCallbackResult {
            error: EncodedImageCallbackError::Ok,
            frame_id: 0,
            drop_next_frame: false,
        }
    }
    fn frame_dropped(&self, _: u32, _: i32, _: bool) {
        self.0.record("encoded-dropped");
    }
}
impl Drop for Encoded {
    fn drop(&mut self) {
        self.0.record("encoded-drop");
    }
}
struct Decoded(Probe);
impl DecodedImageCallbackProvider for Decoded {
    fn decoded(&self, _: &mut RawVideoFrame) -> i32 {
        self.0.record("decoded");
        0
    }
}
impl Drop for Decoded {
    fn drop(&mut self) {
        self.0.record("decoded-drop");
    }
}
struct Fec(Probe);
impl FecControllerOverrideProvider for Fec {
    fn set_fec_allowed(&self, _: bool) {
        self.0.record("fec");
    }
}
impl Drop for Fec {
    fn drop(&mut self) {
        self.0.record("fec-drop");
    }
}

#[test]
fn standalone_native_video_dispatchers_scope_callbacks_and_provider_destruction() {
    let probe = Probe::default();
    let selector = VideoEncoderSelector::from_provider(Selector(probe.clone()));
    selector.on_current_encoder(&SdpVideoFormat::new("VP8"));
    assert!(
        selector
            .on_available_bitrate(DataRate::BitsPerSecond(100))
            .unwrap()
            .is_none()
    );
    assert!(selector.on_encoder_broken().is_none());
    assert_eq!(selector.failure(), None);
    drop(selector);

    let encoded = EncodedImageCallbackSink::new(Encoded(probe.clone()));
    assert_eq!(
        encoded.encoded(&EncodedImage::default(), None).error,
        EncodedImageCallbackError::Ok
    );
    encoded.frame_dropped(0, 0, true);
    assert_eq!(encoded.failure_observer().callback_panics(), 0);
    drop(encoded);

    let decoded = DecodedImageCallbackSink::new(Decoded(probe.clone()));
    let mut pixels = WritableI420Buffer::new(2, 2).unwrap();
    pixels.set_black();
    let mut frame = RawVideoFrame::from_buffer(&pixels.freeze().unwrap());
    assert_eq!(decoded.decoded(&mut frame), 0);
    assert_eq!(decoded.decoded_with_time(&mut frame, 0), 0);
    decoded.decoded_with_metadata(&mut frame, None, None);
    assert_eq!(decoded.failure_observer().callback_panics(), 0);
    drop(decoded);

    let fec = FecControllerOverride::new(Fec(probe.clone()));
    fec.set_fec_allowed(true).unwrap();
    assert_eq!(fec.failure_observer().unwrap().callback_panics(), 0);
    drop(fec);

    let seen = probe.0.lock().unwrap();
    let labels: Vec<_> = seen.iter().map(|(label, _)| *label).collect();
    assert_eq!(
        labels,
        [
            "selector-current",
            "selector-bitrate",
            "selector-broken",
            "selector-drop",
            "encoded",
            "encoded-dropped",
            "encoded-drop",
            "decoded",
            "decoded",
            "decoded",
            "decoded-drop",
            "fec",
            "fec-drop"
        ]
    );
    assert!(seen.iter().all(|(_, active)| *active));
    assert!(!crate::provider_panic::callback_active());
}

// Metadata-only probe. No codec output, parsing or concealment is exercised or
// presented as evidence of media behavior.
struct AudioMetadata(Probe);
impl AudioDecoderProvider for AudioMetadata {
    fn sample_rate_hz(&mut self) -> i32 {
        self.0.record("audio-sample");
        8000
    }
    fn channels(&mut self) -> usize {
        1
    }
    fn reset(&mut self) -> Result<(), AudioDecoderError> {
        Ok(())
    }
    fn parse_payload(
        &mut self,
        _: Vec<u8>,
        _: u32,
    ) -> Result<Vec<AudioDecoderProviderParseResult>, AudioDecoderError> {
        unreachable!("metadata-only admission probe")
    }
    fn generate_plc(
        &mut self,
        _: usize,
        _: &[i16],
        _: usize,
    ) -> Result<Vec<i16>, AudioDecoderError> {
        unreachable!("metadata-only admission probe")
    }
}
impl Drop for AudioMetadata {
    fn drop(&mut self) {
        self.0.record("audio-drop");
    }
}

#[test]
fn caller_audio_product_roots_remain_unscoped_but_sdk_entries_and_drop_are_scoped() {
    let probe = Probe::default();
    let environment = Environment::builder().build().unwrap();
    let decoder = CallerAudioDecoder::from_provider(&environment, AudioMetadata(probe.clone()));
    assert_eq!(decoder.sample_rate_hz().unwrap(), 8000);
    let sdk = decoder.sdk_decoder();
    assert_eq!(sdk.sample_rate_hz().unwrap(), 8000);
    assert_eq!(decoder.sample_rate_hz().unwrap(), 8000);
    drop(sdk);
    drop(decoder);
    assert_eq!(
        *probe.0.lock().unwrap(),
        [
            ("audio-sample", false),
            ("audio-sample", true),
            ("audio-sample", false),
            ("audio-drop", true),
        ]
    );
    assert!(!crate::provider_panic::callback_active());
}

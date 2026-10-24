use pulsebeam_webrtc_sys::*;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    registrations: Vec<Option<DecodedImageCallback>>,
    inputs: Vec<(bool, bool, i64, u32)>,
    outputs: Vec<(u8, i64, Option<i32>, Option<u8>)>,
    settings: Vec<VideoDecoderSettingsFields>,
}
struct Factory(Arc<Mutex<State>>);
impl VideoDecoderFactory for Factory {
    fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        vec![VideoCodecFormat::new("VP8")]
    }
    fn query_support(
        &self,
        _: &VideoCodecFormat,
        _: bool,
        _: Option<VideoResolution>,
    ) -> CodecSupport {
        CodecSupport {
            supported: true,
            power_efficient: false,
        }
    }
    fn create(&self, _: &VideoCodecFormat) -> Result<Box<dyn VideoDecoder>, CodecError> {
        Ok(Box::new(Decoder(self.0.clone())))
    }
}
struct Decoder(Arc<Mutex<State>>);
impl VideoDecoder for Decoder {
    fn configure_value(&mut self, settings: OwnedVideoDecoderSettings) -> Result<bool, CodecError> {
        self.0.lock().unwrap().settings.push(settings.fields());
        Ok(true)
    }
    fn register_callback_value(
        &mut self,
        callback: Option<DecodedImageCallback>,
    ) -> Result<i32, CodecError> {
        let status = if callback.is_some() { -73 } else { 0 };
        self.0.lock().unwrap().registrations.push(callback);
        Ok(status) // The negative status does not undo actual installation.
    }
    fn decode_value(
        &mut self,
        frame: EncodedImage,
        time: i64,
        callback: DecodedImageCallback,
    ) -> Result<i32, CodecError> {
        self.0
            .lock()
            .unwrap()
            .inputs
            .push((false, false, time, frame.metadata().rtp_timestamp));
        let mut writer = WritableI420Buffer::new(2, 2).unwrap();
        writer.set_black();
        let mut raw = RawVideoFrame::from_buffer(&writer.freeze().unwrap());
        assert_eq!(callback.decoded(&mut raw)?, 31);
        assert_eq!(raw.metadata().id, 1);
        assert_eq!(callback.decoded_with_time(&mut raw, i64::MAX)?, -91);
        assert_eq!(raw.metadata().id, 2);
        callback.decoded_with_metadata(&mut raw, Some(i32::MIN), Some(255))?;
        assert_eq!(raw.metadata().id, 3);
        Ok(57)
    }
    fn decode_with_missing_frames_value(
        &mut self,
        frame: EncodedImage,
        missing: bool,
        time: i64,
        _: DecodedImageCallback,
    ) -> Result<i32, CodecError> {
        self.0
            .lock()
            .unwrap()
            .inputs
            .push((true, missing, time, frame.metadata().rtp_timestamp));
        Ok(-27)
    }
    fn release(&mut self) -> Result<(), CodecError> {
        Ok(())
    }
}
struct Output(Arc<Mutex<State>>);
impl Output {
    fn mutate(&self, frame: &mut RawVideoFrame, event: (u8, i64, Option<i32>, Option<u8>)) {
        self.0.lock().unwrap().outputs.push(event);
        let mut metadata = frame.metadata();
        metadata.id += 1;
        frame.set_metadata(&metadata).unwrap();
    }
}
impl DecodedImageCallbackProvider for Output {
    fn decoded(&self, frame: &mut RawVideoFrame) -> i32 {
        self.mutate(frame, (0, 0, None, None));
        31
    }
    fn decoded_with_time(&self, frame: &mut RawVideoFrame, time: i64) -> i32 {
        self.mutate(frame, (1, time, None, None));
        -91
    }
    fn decoded_with_metadata(&self, frame: &mut RawVideoFrame, time: Option<i32>, qp: Option<u8>) {
        self.mutate(frame, (2, 0, time, qp));
    }
}
#[test]
fn selected_decoder_preserves_overloads_statuses_mutations_and_registration_epochs() {
    let state = Arc::new(Mutex::new(State::default()));
    let factory = VideoDecoderFactoryHandle::new(Factory(state.clone())).unwrap();
    let environment = Environment::builder().build().unwrap();
    let mut decoder = factory
        .create_decoder(&environment, &SdpVideoFormat::new("VP8"))
        .unwrap();
    let sink = DecodedImageCallbackSink::new(Output(state.clone()));
    assert_eq!(decoder.register_callback(Some(&sink)), -73);
    let old = state.lock().unwrap().registrations[0].clone().unwrap();
    assert!(!old.is_retired()); // Before the first input, despite error-after-install.
    let mut settings = OwnedVideoDecoderSettings::default();
    let mut fields = settings.fields();
    fields.number_of_cores = 17;
    fields.buffer_pool_size = Some(-17); // Full field absent from the legacy projection.
    settings.set_fields(fields.clone()).unwrap();
    assert!(decoder.configure(&settings));
    let mut image = EncodedImage::default();
    image.set_buffer(&EncodedVideoBuffer::from_bytes(&[0, 255, 7]));
    let mut metadata = image.metadata();
    metadata.rtp_timestamp = 417;
    image.set_metadata(&metadata).unwrap();
    assert_eq!(decoder.decode(&image, i64::MIN).unwrap(), 57);
    assert_eq!(
        decoder
            .decode_with_missing_frames(&image, true, i64::MAX)
            .unwrap(),
        -27
    );
    {
        let state = state.lock().unwrap();
        assert_eq!(state.settings, [fields]);
        assert_eq!(
            state.inputs,
            [(false, false, i64::MIN, 417), (true, true, i64::MAX, 417)]
        );
        assert_eq!(
            state.outputs,
            [
                (0, 0, None, None),
                (1, i64::MAX, None, None),
                (2, 0, Some(i32::MIN), Some(255))
            ]
        );
    }
    assert_eq!(decoder.register_callback(Some(&sink)), -73);
    assert!(old.is_retired());
    let current = state.lock().unwrap().registrations[1].clone().unwrap();
    assert!(!current.is_retired());
    assert_eq!(decoder.register_callback(None), 0);
    assert!(current.is_retired());
    assert!(state.lock().unwrap().registrations[2].is_none());
    drop(decoder);
    drop(factory);
    drop(environment);
    assert!(old.is_retired());
    assert_eq!(sink.failure_observer().callback_panics(), 0);
}

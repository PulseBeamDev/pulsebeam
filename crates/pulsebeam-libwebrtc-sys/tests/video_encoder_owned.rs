use pulsebeam_webrtc_sys::*;
use std::sync::{Arc, Mutex};

#[test]
fn owning_rates_and_info_preserve_full_storage_without_consumer_clamping() {
    let mut allocation = OwnedVideoBitrateAllocation::default();
    assert_eq!(allocation.bitrate(0, 0).unwrap(), None);
    assert!(allocation.set_bitrate(0, 0, 0).unwrap());
    assert_eq!(allocation.bitrate(0, 0).unwrap(), Some(0));
    assert!(allocation.set_bitrate(1, 2, 120_000).unwrap());
    allocation.set_bandwidth_limited(true);
    let mut rates =
        OwnedVideoRateControl::new(allocation.fields(), 29.97, Some(DataRate::PositiveInfinity))
            .unwrap();
    let mut fields = rates.fields();
    fields.target.bandwidth_limited = false;
    fields.target.layer_bitrates_bps[1][2] = Some(80_000);
    rates.set_fields(fields.clone()).unwrap();
    assert_eq!(rates.fields(), fields);
    assert_eq!(rates.clone(), rates);
    let mut info = OwnedVideoEncoderInfo::default();
    let mut fields = info.fields();
    fields.implementation_name = vec![255, 0, 128];
    fields.requested_resolution_alignment = u32::MAX;
    fields.fps_allocation[0] = vec![1, 2, 3, 4, 5, 6, 7, 255];
    fields.preferred_pixel_formats = vec![VideoEncoderPixelFormat::NativeValue(255)];
    fields.min_qp = Some(i32::MIN);
    fields.mapped_resolution = Some(VideoEncoderMappedResolution {
        width: -1,
        height: i32::MAX,
    });
    info.set_fields(fields.clone());
    assert_eq!(info.clone().fields(), fields);
    assert_eq!(
        info.to_string_bytes(),
        Err(VideoEncoderInfoValueError::UnsafePixelFormatNaming)
    );
}

#[derive(Default)]
struct State {
    initializes: usize,
    raw_ids: Vec<u16>,
    results: Vec<EncodedImageCallbackResult>,
    rates: Vec<VideoRateControlFields>,
    aliases: Vec<EncodedImageCallback>,
}
struct Factory(Arc<Mutex<State>>);
impl VideoEncoderFactory for Factory {
    fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        vec![VideoCodecFormat::new("H264")]
    }
    fn query_support(
        &self,
        _: &VideoCodecFormat,
        _: Option<&str>,
        _: Option<VideoResolution>,
    ) -> CodecSupport {
        CodecSupport {
            supported: true,
            power_efficient: false,
        }
    }
    fn create(&self, _: &VideoCodecFormat) -> Result<Box<dyn VideoEncoder>, CodecError> {
        Ok(Box::new(Encoder(self.0.clone())))
    }
}
struct Encoder(Arc<Mutex<State>>);
impl VideoEncoder for Encoder {
    fn initialize(&mut self, _: VideoEncoderSettings) -> Result<(), CodecError> {
        panic!("lossy initialization")
    }
    fn initialize_value(
        &mut self,
        codec: OwnedVideoCodec,
        settings: VideoEncoderSettingsValue,
    ) -> Result<i32, CodecError> {
        assert_eq!(codec.fields().width, 16);
        assert_eq!(settings.encoder_thread_limit, Some(2));
        self.0.lock().unwrap().initializes += 1;
        Ok(0)
    }
    fn encode(
        &mut self,
        _: VideoFrame,
        _: &[VideoFrameType],
        _: EncodedImageCallback,
    ) -> Result<(), CodecError> {
        panic!("lossy frame")
    }
    fn encode_value(
        &mut self,
        frame: RawVideoFrame,
        types: Option<Vec<EncodedImageFrameType>>,
        callback: EncodedImageCallback,
    ) -> Result<i32, CodecError> {
        // The actual selected SDK adapter turns a null caller vector into
        // its per-stream default Delta vector. Do not bypass that SDK behavior.
        assert_eq!(types, Some(vec![EncodedImageFrameType::Delta]));
        let mut image = EncodedImage::default();
        image.set_buffer(&EncodedVideoBuffer::from_bytes(&[1, 2, 3]));
        let mut metadata = image.metadata();
        metadata.width = 16;
        metadata.height = 16;
        metadata.frame_type = EncodedImageFrameType::Key;
        metadata.rtp_timestamp = 123;
        image.set_metadata(&metadata).unwrap();
        let result = callback.encoded(&image, None)?;
        let mut state = self.0.lock().unwrap();
        state.raw_ids.push(frame.metadata().id);
        state.results.push(result);
        state.aliases.push(callback);
        Ok(73)
    }
    fn set_rates(&mut self, _: VideoRateControl) -> Result<(), CodecError> {
        panic!("lossy rates")
    }
    fn set_rates_value(&mut self, rates: OwnedVideoRateControl) -> Result<(), CodecError> {
        self.0.lock().unwrap().rates.push(rates.fields());
        Ok(())
    }
    fn release(&mut self) -> Result<(), CodecError> {
        Ok(())
    }
    fn info(&self) -> VideoEncoderInfo {
        VideoEncoderInfo {
            implementation_name: "full-value-fixture".into(),
            hardware_accelerated: false,
            supports_native_handle: false,
            supports_simulcast: true,
            fps_allocation: None,
        }
    }
}
struct Output;
impl EncodedImageCallbackProvider for Output {
    fn encoded(
        &self,
        image: EncodedImage,
        info: Option<OwnedCodecSpecificInfo>,
    ) -> EncodedImageCallbackResult {
        assert_eq!(image.to_vec(), [1, 2, 3]);
        assert!(info.is_none());
        EncodedImageCallbackResult {
            error: EncodedImageCallbackError::SendFailed,
            frame_id: u32::MAX,
            drop_next_frame: true,
        }
    }
    fn frame_dropped(&self, _: u32, _: i32, _: bool) {}
}
#[test]
fn captive_encoder_dispatch_and_pending_callback_retirement_survive_migration() {
    let state = Arc::new(Mutex::new(State::default()));
    let factory = VideoEncoderFactoryHandle::new(Factory(state.clone())).unwrap();
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .video_encoder_factory(&factory)
        .build()
        .unwrap();
    let encoder = session
        .create_video_encoder(&SdpVideoFormat::from(&VideoCodecFormat::new("H264")))
        .unwrap();
    let callback = session
        .create_encoded_image_callback(Arc::new(Output))
        .unwrap();
    assert_eq!(
        session
            .register_video_encoder_callback(encoder, Some(callback))
            .unwrap(),
        0
    );
    let mut codec = OwnedVideoCodec::default();
    let mut fields = codec.fields();
    fields.codec_type = VideoCodecType::H264;
    fields.width = 16;
    fields.height = 16;
    fields.max_framerate = 30;
    fields.start_bitrate_kbps = 100;
    fields.min_bitrate_kbps = 10;
    fields.max_bitrate_kbps = 200;
    fields.codec_specific = Some(VideoCodecSpecificConfiguration::H264 {
        key_frame_interval: 0,
        temporal_layers: 1,
    });
    codec.set_fields(&fields).unwrap();
    assert_eq!(
        session
            .initialize_video_encoder(
                encoder,
                &codec,
                VideoEncoderSettingsValue {
                    loss_notification: true,
                    number_of_cores: 1,
                    max_payload_size: 1200,
                    encoder_thread_limit: Some(2)
                }
            )
            .unwrap(),
        0
    );
    let writer = session.create_writable_i420_buffer(16, 16).unwrap();
    session.initialize_i420_buffer(writer, true).unwrap();
    let buffer = session.freeze_i420_buffer(writer).unwrap();
    let frame = session.native_video_frame_from_buffer(buffer).unwrap();
    let mut metadata = session.native_video_frame_metadata(frame).unwrap();
    metadata.id = 417;
    session
        .set_native_video_frame_metadata(frame, &metadata)
        .unwrap();
    drop(factory);
    std::thread::spawn(move || {
        assert_eq!(
            session.encode_video_frame(encoder, frame, None).unwrap(),
            73
        );
        assert_eq!(state.lock().unwrap().raw_ids, [417]);
        assert_eq!(
            session
                .register_video_encoder_callback(encoder, None)
                .unwrap(),
            0
        );
        assert!(state.lock().unwrap().aliases[0].is_retired());
        session.shutdown().unwrap();
        assert!(
            state.lock().unwrap().aliases[0]
                .encoded(&EncodedImage::default(), None)
                .is_err()
        );
    })
    .join()
    .unwrap();
}

#[test]
fn selected_native_encoder_dispatches_full_input_rates_output_and_retirement() {
    let state = Arc::new(Mutex::new(State::default()));
    let factory = VideoEncoderFactoryHandle::new(Factory(state.clone())).unwrap();
    let environment = Environment::builder().build().unwrap();
    let mut product = factory
        .create_encoder(
            &environment,
            &SdpVideoFormat::from(&VideoCodecFormat::new("H264")),
        )
        .unwrap();
    let output = EncodedImageCallbackSink::new(Output);
    assert_eq!(product.register_callback(Some(&output)), 0);
    let mut codec = OwnedVideoCodec::default();
    let mut fields = codec.fields();
    fields.codec_type = VideoCodecType::H264;
    fields.width = 16;
    fields.height = 16;
    fields.max_framerate = 30;
    fields.start_bitrate_kbps = 100;
    fields.min_bitrate_kbps = 10;
    fields.max_bitrate_kbps = 200;
    fields.codec_specific = Some(VideoCodecSpecificConfiguration::H264 {
        key_frame_interval: 0,
        temporal_layers: 1,
    });
    codec.set_fields(&fields).unwrap();
    assert_eq!(
        product.initialize(
            &codec,
            VideoEncoderSettingsValue {
                loss_notification: true,
                number_of_cores: 1,
                max_payload_size: 1200,
                encoder_thread_limit: Some(2)
            }
        ),
        0
    );
    let mut writer = WritableI420Buffer::new(16, 16).unwrap();
    writer.set_black();
    let buffer = writer.freeze().unwrap();
    let mut frame = RawVideoFrame::from_buffer(&buffer);
    let mut metadata = frame.metadata();
    metadata.id = 417;
    frame.set_metadata(&metadata).unwrap();
    let mut allocation = OwnedVideoBitrateAllocation::default();
    allocation.set_bitrate(0, 0, 100_000).unwrap();
    let rates = OwnedVideoRateControl::new(allocation.fields(), 30.0, None).unwrap();
    product.set_rates(&rates);
    assert_eq!(product.encode(&frame, None), 73);
    {
        let state = state.lock().unwrap();
        assert_eq!(state.initializes, 1);
        assert_eq!(state.raw_ids, [417]);
        assert_eq!(
            state.results[0],
            EncodedImageCallbackResult {
                error: EncodedImageCallbackError::SendFailed,
                frame_id: u32::MAX,
                drop_next_frame: true
            }
        );
        assert!(!state.rates.is_empty());
    }
    assert_eq!(product.register_callback(None), 0);
    assert!(state.lock().unwrap().aliases[0].is_retired());
    drop(output);
    drop(product);
    drop(factory);
    drop(environment);
    assert!(
        state.lock().unwrap().aliases[0]
            .encoded(&EncodedImage::default(), None)
            .is_err()
    );
}

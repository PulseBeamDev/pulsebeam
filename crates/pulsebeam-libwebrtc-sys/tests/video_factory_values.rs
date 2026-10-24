use pulsebeam_webrtc_sys::*;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq)]
struct Query {
    format: SdpVideoFormatFields,
    mode: Option<Vec<u8>>,
    resolution: Option<(i32, i32)>,
    reference_scaling: bool,
}
fn format() -> SdpVideoFormat {
    let mut format = SdpVideoFormat::new([0xff, 0]);
    let mut fields = format.fields();
    fields.parameters.insert(vec![0xfe, 0], vec![0xfd]);
    fields.packetization = Some(vec![]);
    fields.tx_mode = Some(vec![0xfc]);
    fields.scalability_modes = vec![ScalabilityMode::from_raw(255)];
    format.set_fields(fields);
    format
}
struct Encoder(Arc<Mutex<Vec<Query>>>);
impl VideoEncoderFactoryProvider for Encoder {
    fn supported_formats(&self) -> Vec<SdpVideoFormat> {
        vec![format()]
    }
    fn query_support(
        &self,
        format: &SdpVideoFormat,
        mode: Option<&[u8]>,
        resolution: Option<&Resolution>,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        self.0.lock().unwrap().push(Query {
            format: format.fields(),
            mode: mode.map(<[u8]>::to_vec),
            resolution: resolution.map(Resolution::to_pair),
            reference_scaling: false,
        });
        Ok(CodecSupport {
            supported: mode.is_none(),
            power_efficient: mode.is_some(),
        })
    }
    fn create(&self, _: VideoCodecCreationContext) -> Result<Box<dyn VideoEncoder>, CodecError> {
        Err(CodecError::ConstructionFailed)
    }
}
struct Decoder(Arc<Mutex<Vec<Query>>>);
impl VideoDecoderFactoryProvider for Decoder {
    fn supported_formats(&self) -> Vec<SdpVideoFormat> {
        vec![format()]
    }
    fn query_support(
        &self,
        format: &SdpVideoFormat,
        reference_scaling: bool,
        resolution: Option<&Resolution>,
    ) -> Result<CodecSupport, VideoFactoryQueryError> {
        self.0.lock().unwrap().push(Query {
            format: format.fields(),
            mode: None,
            resolution: resolution.map(Resolution::to_pair),
            reference_scaling,
        });
        Ok(CodecSupport {
            supported: !reference_scaling,
            power_efficient: reference_scaling,
        })
    }
    fn create(&self, _: VideoCodecCreationContext) -> Result<Box<dyn VideoDecoder>, CodecError> {
        Err(CodecError::ConstructionFailed)
    }
}
#[test]
fn owning_queries_preserve_all_bytes_raw_modes_optionals_and_signed_geometry() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let factory = VideoEncoderFactoryHandle::from_provider(Encoder(calls.clone())).unwrap();
    let input = format();
    assert_eq!(factory.supported_format_values()[0], input);
    // Modern default GetImplementations delegates actual full declarations.
    assert_eq!(factory.implementation_values()[0], input);
    let resolution = Resolution::new(i32::MIN, i32::MAX);
    assert_eq!(
        factory.query_support_value(&input, None, None).unwrap(),
        CodecSupport {
            supported: true,
            power_efficient: false
        }
    );
    assert_eq!(
        factory
            .query_support_value(&input, Some(&[]), Some(&Resolution::new(0, 0)))
            .unwrap(),
        CodecSupport {
            supported: false,
            power_efficient: true
        }
    );
    assert_eq!(
        factory
            .query_support_value(&input, Some(&[0xff, 0]), Some(&resolution))
            .unwrap(),
        CodecSupport {
            supported: false,
            power_efficient: true
        }
    );
    let expected = input.fields();
    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            Query {
                format: expected.clone(),
                mode: None,
                resolution: None,
                reference_scaling: false
            },
            Query {
                format: expected.clone(),
                mode: Some(vec![]),
                resolution: Some((0, 0)),
                reference_scaling: false
            },
            Query {
                format: expected.clone(),
                mode: Some(vec![0xff, 0]),
                resolution: Some((i32::MIN, i32::MAX)),
                reference_scaling: false
            },
        ]
    );
    assert_eq!(factory.failure_observer().unwrap().failure(), None);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let decoder = VideoDecoderFactoryHandle::from_provider(Decoder(calls.clone())).unwrap();
    assert_eq!(
        decoder
            .query_support_value(&input, true, Some(&resolution))
            .unwrap(),
        CodecSupport {
            supported: false,
            power_efficient: true
        }
    );
    assert_eq!(
        *calls.lock().unwrap(),
        vec![Query {
            format: expected,
            mode: None,
            resolution: Some((i32::MIN, i32::MAX)),
            reference_scaling: true
        }]
    );
    let snapshot = decoder.supported_format_values();
    drop(decoder);
    assert_eq!(snapshot[0], input);
}
struct Legacy;
impl VideoEncoderFactory for Legacy {
    fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        vec![VideoCodecFormat::new("VP8")]
    }
    fn query_support(
        &self,
        _: &VideoCodecFormat,
        _: Option<&str>,
        _: Option<VideoResolution>,
    ) -> CodecSupport {
        CodecSupport {
            supported: false,
            power_efficient: true,
        }
    }
    fn create(&self, _: &VideoCodecFormat) -> Result<Box<dyn VideoEncoder>, CodecError> {
        Err(CodecError::UnsupportedFormat)
    }
}
#[test]
fn legacy_projection_failure_is_not_native_unsupported_or_poison() {
    let factory = VideoEncoderFactoryHandle::new(Legacy).unwrap();
    let observer = factory.failure_observer().unwrap();
    assert_eq!(
        factory
            .query_support_value(&format(), None, None)
            .unwrap_err(),
        VideoFactoryQueryError::LegacyOptionalFormatFields
    );
    let input = SdpVideoFormat::new("VP8");
    assert_eq!(
        factory
            .query_support_value(&input, Some(&[0xff]), None)
            .unwrap_err(),
        VideoFactoryQueryError::LegacyModeBytes
    );
    assert_eq!(
        factory
            .query_support_value(&input, None, Some(&Resolution::new(-1, 0)))
            .unwrap_err(),
        VideoFactoryQueryError::LegacySignedResolution
    );
    // Projection rejection does not disable independent native-valid queries.
    assert_eq!(
        factory
            .query_support_value(&input, None, Some(&Resolution::new(0, 0)))
            .unwrap(),
        CodecSupport {
            supported: false,
            power_efficient: true
        }
    );
    drop(factory);
    assert_eq!(
        observer.failure(),
        Some(VideoFactoryQueryError::LegacyOptionalFormatFields)
    );
}
#[test]
fn builtin_queries_do_not_invent_a_positive_resolution_domain() {
    let encoder = VideoEncoderFactoryHandle::builtin().unwrap();
    let decoder = VideoDecoderFactoryHandle::builtin().unwrap();
    let input = SdpVideoFormat::well_known(WellKnownSdpVideoFormat::Vp8);
    let negative = Resolution::new(i32::MIN, i32::MAX);
    assert_eq!(
        encoder.query_support_value(&input, None, None).unwrap(),
        encoder
            .query_support_value(&input, None, Some(&negative))
            .unwrap()
    );
    assert_eq!(
        decoder.query_support_value(&input, false, None).unwrap(),
        decoder
            .query_support_value(&input, false, Some(&negative))
            .unwrap()
    );
    assert!(
        !encoder
            .query_support_value(&input, Some(&[0xff, 0]), None)
            .unwrap()
            .supported
    );
}

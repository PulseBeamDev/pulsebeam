use pulsebeam_webrtc_sys::*;

#[test]
fn owned_decoder_settings_preserve_sdk_metadata_without_configure_policy() {
    let mut value = OwnedVideoDecoderSettings::default();
    assert_eq!(
        value.fields(),
        VideoDecoderSettingsFields {
            buffer_pool_size: None,
            max_render_width: 0,
            max_render_height: 0,
            number_of_cores: 1,
            codec_type: 0,
        }
    );
    for codec_type in 0..=7 {
        let fields = VideoDecoderSettingsFields {
            buffer_pool_size: Some(i32::MIN),
            max_render_width: i32::MIN,
            max_render_height: i32::MAX,
            number_of_cores: i32::MAX,
            codec_type,
        };
        value.set_fields(fields.clone()).unwrap();
        assert_eq!(value.fields(), fields);
        let copied = value.clone();
        let mut assigned = OwnedVideoDecoderSettings::default();
        assigned.clone_from(&copied);
        assert_eq!(assigned.fields(), fields);
    }
    let original = value.fields();
    for codec_type in [-1, 8, i32::MIN, i32::MAX] {
        let mut fields = original.clone();
        fields.codec_type = codec_type;
        assert_eq!(
            value.set_fields(fields),
            Err(VideoDecoderSettingsValueError::CodecTypeNotRepresentable)
        );
        assert_eq!(value.fields(), original);
    }
    let copy = value.clone();
    let mut fields = value.fields();
    fields.buffer_pool_size = Some(0);
    value.set_fields(fields.clone()).unwrap();
    assert_eq!(value.fields(), fields);
    fields.buffer_pool_size = None;
    value.set_fields(fields.clone()).unwrap();
    assert_eq!(value.fields(), fields);
    drop(value);
    assert_eq!(copy.fields(), original);
}

#[test]
fn core_setter_checks_only_the_reached_artifact_assertion_and_is_atomic() {
    let mut value = OwnedVideoDecoderSettings::default();
    let previous = value.fields();
    let mut fields = previous.clone();
    fields.number_of_cores = -1;
    match value.set_fields(fields.clone()) {
        Ok(()) => assert_eq!(value.fields(), fields),
        Err(VideoDecoderSettingsValueError::CoreAssertion) => assert_eq!(value.fields(), previous),
        other => panic!("unexpected core metadata result: {other:?}"),
    }
}

#[test]
fn decoder_info_sdk_copy_assignment_equality_and_formatting_keep_bytes() {
    let mut info = OwnedVideoDecoderInfo::default();
    assert_eq!(
        info.fields(),
        VideoDecoderInfoFields {
            implementation_name: vec![],
            is_hardware_accelerated: false
        }
    );
    let fields = VideoDecoderInfoFields {
        implementation_name: vec![b'x', 0, 0xff],
        is_hardware_accelerated: true,
    };
    info.set_fields(fields.clone());
    let copy = info.clone();
    assert_eq!(copy, info);
    let mut assigned = OwnedVideoDecoderInfo::default();
    assigned.clone_from(&info);
    assert_eq!(assigned, info);
    let formatted = info.to_string_bytes();
    assert!(
        formatted
            .windows(fields.implementation_name.len())
            .any(|value| value == fields.implementation_name)
    );
    assert!(formatted.windows(4).any(|value| value == b"true"));
    info.set_fields(VideoDecoderInfoFields {
        implementation_name: b"changed".to_vec(),
        is_hardware_accelerated: false,
    });
    assert_ne!(info, copy);
    drop(info);
    drop(assigned);
    std::thread::spawn(move || {
        assert_eq!(copy.fields(), fields);
        assert_eq!(copy.to_string_bytes(), formatted);
    })
    .join()
    .unwrap();
}

use pulsebeam_webrtc_sys::{AudioCodecInfo, AudioDecoderFactory, CodecError, SdpAudioFormat};

#[test]
fn audio_format_native_matching_and_parameter_identity() {
    let mut left = SdpAudioFormat::new(b"OpUs".to_vec(), 48_000, 2);
    let mut right = SdpAudioFormat::new(b"opus".to_vec(), 48_000, 2);
    left.parameters
        .insert(b"UseInbandFec".to_vec(), b"1".to_vec());
    right
        .parameters
        .insert(b"useinbandfec".to_vec(), b"1".to_vec());
    assert!(left.matches(&right));
    assert_ne!(left, right);
    right.parameters = left.parameters.clone();
    assert_eq!(left, right);
    right.num_channels = 1;
    assert!(!left.matches(&right));
}

#[test]
fn audio_format_description_preserves_raw_native_strings_and_widths() {
    let name = vec![b'x', 0xff, 0, b'y'];
    let key = vec![b'k', 0xfe, 0, b'1'];
    let value = vec![b'v', 0xfd, 0, b'2'];
    let mut format = SdpAudioFormat::new(name.clone(), i32::MIN, usize::MAX);
    format.parameters.insert(key.clone(), value.clone());
    let description = format.to_native_description();
    for bytes in [name, key, value] {
        assert!(description.windows(bytes.len()).any(|part| part == bytes));
    }
    for number in [i32::MIN.to_string(), usize::MAX.to_string()] {
        assert!(
            description
                .windows(number.len())
                .any(|part| part == number.as_bytes())
        );
    }
}

#[test]
fn audio_codec_info_preserves_native_constructor_and_fixed_rate_constraints() {
    let fixed = AudioCodecInfo::new(1, usize::MAX, i32::MAX).unwrap();
    assert!(fixed.has_fixed_bitrate().unwrap());
    assert!(fixed.allow_comfort_noise);
    assert!(!fixed.supports_network_adaption);
    let mut variable =
        AudioCodecInfo::with_bitrate_range(48_000, 2, 64_000, 6_000, 510_000).unwrap();
    assert!(!variable.has_fixed_bitrate().unwrap());
    variable.sample_rate_hz = 0;
    variable.num_channels = 0;
    // HasFixedBitrate's own preconditions concern bitrates only.
    assert!(!variable.has_fixed_bitrate().unwrap());
    variable.min_bitrate_bps = -1;
    match variable.has_fixed_bitrate() {
        Err(CodecError::InvalidConfiguration) => {} // Actual DCHECK-enabled artifact.
        Ok(value) => assert_eq!(value, variable.min_bitrate_bps == variable.max_bitrate_bps),
        other => panic!("unexpected native fixed-bitrate result: {other:?}"),
    }
    let mut invalid_fixed = variable.clone();
    invalid_fixed.min_bitrate_bps = -1;
    invalid_fixed.max_bitrate_bps = -1;
    match invalid_fixed.has_fixed_bitrate() {
        Ok(value) => assert!(value),
        Err(CodecError::InvalidConfiguration) => {}
        other => panic!("unexpected native fixed-bitrate result: {other:?}"),
    }
    // SDK constructors have only DCHECKs, not arithmetic. Disabled-DCHECK
    // artifacts must retain even these standalone metadata fields unchanged.
    let constructor_checks_enabled = AudioCodecInfo::new(0, 1, 0).is_err();
    for args in [
        (0, 1, 0, 0, 0),
        (i32::MIN, usize::MAX, 0, 0, 0),
        (1, 0, 0, 0, 0),
        (1, 1, 0, -1, 0),
        (1, 1, 0, 1, 1),
        (1, 1, 1, 0, 0),
        (i32::MIN, 0, i32::MIN, i32::MAX, i32::MIN),
    ] {
        let result = AudioCodecInfo::with_bitrate_range(args.0, args.1, args.2, args.3, args.4);
        if constructor_checks_enabled {
            assert_eq!(result, Err(CodecError::InvalidConfiguration));
        } else {
            let value = result.unwrap();
            assert_eq!(
                (
                    value.sample_rate_hz,
                    value.num_channels,
                    value.default_bitrate_bps,
                    value.min_bitrate_bps,
                    value.max_bitrate_bps
                ),
                args
            );
            assert!(value.allow_comfort_noise);
            assert!(!value.supports_network_adaption);
        }
    }
    let negative_fixed = AudioCodecInfo::new(i32::MIN, 0, i32::MIN);
    if constructor_checks_enabled {
        assert_eq!(negative_fixed, Err(CodecError::InvalidConfiguration));
    } else {
        let value = negative_fixed.unwrap();
        assert_eq!(value.sample_rate_hz, i32::MIN);
        assert_eq!(value.num_channels, 0);
        assert_eq!(value.default_bitrate_bps, i32::MIN);
        assert_eq!(value.min_bitrate_bps, i32::MIN);
        assert_eq!(value.max_bitrate_bps, i32::MIN);
        assert!(value.has_fixed_bitrate().unwrap());
    }
    // Description is observation, not consumer validation, even for modified values.
    assert!(!variable.to_native_description().is_empty());
}

#[test]
fn builtin_decoder_query_rejects_aborting_or_overflowing_configurations() {
    let factory = AudioDecoderFactory::builtin().unwrap();
    for format in [
        SdpAudioFormat::new(b"PCMU".to_vec(), 8000, usize::MAX),
        SdpAudioFormat::new(b"PCMA".to_vec(), 8000, 17),
        SdpAudioFormat::new(b"L16".to_vec(), 48_000, usize::MAX),
        SdpAudioFormat::new(b"unknown".to_vec(), 48_000, usize::MAX),
    ] {
        assert_eq!(
            factory.is_supported_decoder(&format),
            Err(CodecError::InvalidConfiguration)
        );
    }
    let mut multi = SdpAudioFormat::new(b"multiopus".to_vec(), 48_000, 1);
    multi
        .parameters
        .insert(b"num_streams".to_vec(), i32::MAX.to_string().into_bytes());
    multi.parameters.insert(
        b"coupled_streams".to_vec(),
        i32::MAX.to_string().into_bytes(),
    );
    multi
        .parameters
        .insert(b"channel_mapping".to_vec(), b"0".to_vec());
    assert_eq!(
        factory.is_supported_decoder(&multi),
        Err(CodecError::InvalidConfiguration)
    );
    // Ordinary Opus resolves first and must ignore those irrelevant parameters.
    let mut ordinary = SdpAudioFormat::new(b"opus".to_vec(), 48_000, 2);
    ordinary.parameters = multi.parameters;
    ordinary
        .parameters
        .insert(b"channel_mapping".to_vec(), b"0,1".to_vec());
    assert_eq!(factory.is_supported_decoder(&ordinary), Ok(true));

    let opus_only = AudioDecoderFactory::builtin_opus().unwrap();
    // This factory filters names before entering the builtin parser at all.
    assert_eq!(
        opus_only.is_supported_decoder(&SdpAudioFormat::new(b"PCMU".to_vec(), 8000, usize::MAX)),
        Ok(false)
    );
}

#[test]
fn builtin_decoder_queries_return_complete_native_opus_spec() {
    for factory in [
        AudioDecoderFactory::builtin().unwrap(),
        AudioDecoderFactory::builtin_opus().unwrap(),
    ] {
        let specs = factory.supported_decoders();
        let opus = specs
            .iter()
            .find(|s| s.format.name.eq_ignore_ascii_case(b"opus"))
            .unwrap();
        assert_eq!(opus.format.clockrate_hz, 48_000);
        assert_eq!(opus.format.num_channels, 2);
        assert_eq!(
            opus.format.parameters.get(b"minptime".as_slice()).unwrap(),
            b"10"
        );
        assert_eq!(
            opus.format
                .parameters
                .get(b"useinbandfec".as_slice())
                .unwrap(),
            b"1"
        );
        assert_eq!(
            opus.info,
            AudioCodecInfo {
                sample_rate_hz: 48_000,
                num_channels: 1,
                default_bitrate_bps: 64_000,
                min_bitrate_bps: 6_000,
                max_bitrate_bps: 510_000,
                allow_comfort_noise: false,
                supports_network_adaption: true,
            }
        );
        assert_eq!(factory.is_supported_decoder(&opus.format), Ok(true));
        assert_eq!(
            factory.is_supported_decoder(&SdpAudioFormat::new(b"not-a-codec".to_vec(), 48_000, 2)),
            Ok(false)
        );
        assert!(!opus.to_native_description().is_empty());
        let retained = specs.clone();
        drop(factory);
        assert_eq!(retained, specs);
    }
}

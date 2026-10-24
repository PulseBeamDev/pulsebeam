use pulsebeam_webrtc_sys::{
    AudioCodecPairId, AudioDecoder, AudioDecoderError, AudioDecoderFactory, AudioSpeechType,
    CodecError, DecodableAudioFrame, Environment, FieldTrials, SdpAudioFormat,
};

fn opus(stereo: bool) -> SdpAudioFormat {
    let mut format = SdpAudioFormat::new(b"opus".to_vec(), 48_000, 2);
    format.parameters.insert(
        b"stereo".to_vec(),
        if stereo { b"1" } else { b"0" }.to_vec(),
    );
    format
}

#[test]
fn owning_factory_creation_preserves_optional_native_results_and_parser_errors() {
    let env = Environment::builder().build().unwrap();
    let factory = AudioDecoderFactory::builtin().unwrap();
    assert!(
        factory
            .create(&env, &SdpAudioFormat::new(b"unknown".to_vec(), 1, 1))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        factory
            .create(
                &env,
                &SdpAudioFormat::new(b"PCMU".to_vec(), 8000, usize::MAX)
            )
            .err(),
        Some(CodecError::InvalidConfiguration)
    );
    let opus_only = AudioDecoderFactory::builtin_opus().unwrap();
    assert!(
        opus_only
            .create(
                &env,
                &SdpAudioFormat::new(b"PCMU".to_vec(), 8000, usize::MAX)
            )
            .unwrap()
            .is_none()
    );

    // Native multiopus selection deliberately ignores the name/rate and follows
    // ordinary Opus in the stock template. Do not curate this supported input.
    let mut multi = SdpAudioFormat::new(b"unusual-name".to_vec(), 123, 1);
    for (key, value) in [
        (b"num_streams".as_slice(), b"1".as_slice()),
        (b"coupled_streams", b"0"),
        (b"channel_mapping", b"0"),
    ] {
        multi.parameters.insert(key.to_vec(), value.to_vec());
    }
    assert!(factory.is_supported_decoder(&multi).unwrap());
    let decoder = factory.create(&env, &multi).unwrap().unwrap();
    assert_eq!((decoder.sample_rate_hz(), decoder.channels()), (48_000, 1));
    multi
        .parameters
        .insert(b"num_streams".to_vec(), b"0".to_vec());
    multi
        .parameters
        .insert(b"channel_mapping".to_vec(), b"255".to_vec());
    // IsSupportedDecoder accepts this, but libopus's constructor returns null.
    assert!(factory.is_supported_decoder(&multi).unwrap());
    assert!(factory.create(&env, &multi).unwrap().is_none());
}

#[test]
fn pair_identity_is_native_generated_and_both_factory_overloads_remain_callable() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<AudioCodecPairId>();
    let id = AudioCodecPairId::create();
    let other = AudioCodecPairId::create();
    let retained = id.clone();
    assert_eq!(id, retained);
    assert_ne!(id, other);
    assert_eq!(
        id.cmp(&other),
        id.numeric_representation()
            .cmp(&other.numeric_representation())
    );
    let env = Environment::builder().build().unwrap();
    for factory in [
        AudioDecoderFactory::builtin().unwrap(),
        AudioDecoderFactory::builtin_opus().unwrap(),
    ] {
        for pair in [None, Some(&id)] {
            let decoder = factory
                .create_with_codec_pair_id(&env, &opus(false), pair)
                .unwrap()
                .unwrap();
            assert_eq!((decoder.sample_rate_hz(), decoder.channels()), (48_000, 1));
        }
    }
    drop((id, other));
    std::thread::spawn(move || retained.numeric_representation())
        .join()
        .unwrap();
}

#[test]
fn parsed_frames_retain_decoder_environment_and_enforce_one_decode_attempt() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<AudioDecoder>();
    send_sync::<DecodableAudioFrame>();
    let mut env = Environment::builder().build().unwrap();
    let factory = AudioDecoderFactory::builtin().unwrap();
    let decoder = factory
        .create(&env, &SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 2))
        .unwrap()
        .unwrap();
    let info = decoder.packet_info(&vec![0xff; 321]).unwrap();
    assert_eq!(
        (info.duration, info.redundant_duration, info.has_fec),
        (160, 160, false)
    );
    let mut parsed = decoder.parse_payload(vec![0xff; 321], u32::MAX).unwrap();
    assert_eq!(parsed.len(), 1);
    let parsed = parsed.pop().unwrap();
    assert_eq!((parsed.timestamp, parsed.priority), (u32::MAX, 0));
    let frame = parsed.frame.unwrap();
    assert_eq!(frame.duration(), 160);
    assert!(!frame.is_dtx_packet());
    env.close();
    drop((env, factory, decoder));
    std::thread::spawn(move || {
        assert_eq!((frame.sample_rate_hz(), frame.channels()), (8000, 2));
        let decoded = frame.decode().unwrap().unwrap();
        assert_eq!(decoded.samples, vec![0; 320]);
        assert_eq!(decoded.speech_type, AudioSpeechType::SPEECH);
        assert_eq!(frame.decode(), Err(AudioDecoderError::FrameAlreadyDecoded));
        assert_eq!(frame.duration(), 160);
    })
    .join()
    .unwrap();
}

#[test]
fn stock_split_frames_preserve_samples_priorities_and_wrapping_timestamps() {
    let env = Environment::builder().build().unwrap();
    let factory = AudioDecoderFactory::builtin().unwrap();
    let decoder = factory
        .create(&env, &SdpAudioFormat::new(b"L16".to_vec(), 48_000, 1))
        .unwrap()
        .unwrap();
    let payload: Vec<_> = (0..20_000).flat_map(|_| [0_u8, 1]).collect();
    let mut timestamp = u32::MAX - 1000;
    let parsed = decoder.parse_payload(payload, timestamp).unwrap();
    assert!(parsed.len() > 1);
    let mut count = 0;
    for parsed in parsed {
        assert_eq!((parsed.timestamp, parsed.priority), (timestamp, 0));
        let frame = parsed.frame.unwrap();
        let duration = frame.duration();
        assert!(duration < 5760);
        timestamp = timestamp.wrapping_add(duration as u32);
        let decoded = frame.decode().unwrap().unwrap();
        assert_eq!(decoded.samples, vec![1; duration]);
        count += decoded.samples.len();
    }
    assert_eq!(count, 20_000);
    decoder.reset();
    assert_eq!(decoder.error_code(), 0);
    let empty = decoder
        .parse_payload(Vec::new(), 0)
        .unwrap()
        .pop()
        .unwrap()
        .frame
        .unwrap();
    assert_eq!(empty.duration(), 0);
    assert!(empty.decode().unwrap().unwrap().samples.is_empty());
}

#[test]
fn g722_stereo_split_outputs_fit_the_stock_frame_bound() {
    let env = Environment::builder().build().unwrap();
    let factory = AudioDecoderFactory::builtin().unwrap();
    let decoder = factory
        .create(&env, &SdpAudioFormat::new(b"G722".to_vec(), 8000, 2))
        .unwrap()
        .unwrap();
    assert_eq!((decoder.sample_rate_hz(), decoder.channels()), (16_000, 2));
    for parsed in decoder.parse_payload(vec![0; 10_001], 0).unwrap() {
        let frame = parsed.frame.unwrap();
        let expected = frame.duration() * frame.channels();
        let decoded = frame.decode().unwrap().unwrap();
        assert_eq!(decoded.samples.len(), expected);
        assert!(decoded.samples.len() <= 5760 * decoder.channels());
    }
}

#[test]
fn opus_empty_and_malformed_frames_and_plc_keep_native_outcomes() {
    let env = Environment::builder().build().unwrap();
    let factory = AudioDecoderFactory::builtin_opus().unwrap();
    let decoder = factory.create(&env, &opus(false)).unwrap().unwrap();
    assert_eq!((decoder.sample_rate_hz(), decoder.channels()), (48_000, 1));
    let empty = decoder
        .parse_payload(Vec::new(), 0)
        .unwrap()
        .pop()
        .unwrap()
        .frame
        .unwrap();
    // Empty Opus payloads represent the native ten-millisecond PLC frame.
    assert_eq!(empty.duration(), 480);
    assert!(empty.is_dtx_packet());
    let decoded = empty.decode().unwrap().unwrap();
    assert_eq!(decoded.samples.len(), 480);
    let invalid = decoder
        .parse_payload(vec![0xff], 0)
        .unwrap()
        .pop()
        .unwrap()
        .frame
        .unwrap();
    assert!(invalid.decode().unwrap().is_none());
    assert_eq!(
        invalid.decode(),
        Err(AudioDecoderError::FrameAlreadyDecoded)
    );
    let mut prefix = vec![7, 8, 9];
    decoder.generate_plc(usize::MAX, &mut prefix).unwrap();
    assert_eq!(prefix, [7, 8, 9]);

    let env = Environment::builder()
        .field_trials(FieldTrials::parse("WebRTC-Audio-OpusGeneratePlc/Enabled/").unwrap())
        .build()
        .unwrap();
    let stereo = factory.create(&env, &opus(true)).unwrap().unwrap();
    let mut prefix = vec![7, 8];
    stereo.generate_plc(usize::MAX, &mut prefix).unwrap();
    assert_eq!(&prefix[..2], &[7, 8]);
    // SDK AppendData returns the per-channel count, not the full stereo count.
    // Preserve that native quirk rather than silently repairing its output.
    assert_eq!(prefix.len(), 2 + 480);
}

#[test]
fn frame_shared_readers_race_decode_but_only_one_attempt_enters_native() {
    let env = Environment::builder().build().unwrap();
    let factory = AudioDecoderFactory::builtin().unwrap();
    let decoder = factory
        .create(&env, &SdpAudioFormat::new(b"PCMA".to_vec(), 8000, 1))
        .unwrap()
        .unwrap();
    let frame = std::sync::Arc::new(
        decoder
            .parse_payload(vec![0xd5; 160], 0)
            .unwrap()
            .pop()
            .unwrap()
            .frame
            .unwrap(),
    );
    let results: Vec<_> = (0..4)
        .map(|_| {
            let frame = frame.clone();
            std::thread::spawn(move || {
                assert_eq!(frame.duration(), 160);
                frame.decode()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(Some(_))))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(AudioDecoderError::FrameAlreadyDecoded)))
            .count(),
        3
    );
}

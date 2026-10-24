//! Stock NetEq packet, removal, initialized output and factory-selection contracts.
use pulsebeam_webrtc_sys::{
    AudioDecoderFactory, AudioFrame, AudioFrameSpeechType, Environment, NetEq, NetEqConfig,
    NetEqError, NetEqOperation, NetEqReturnCode, RtpHeaderValues, RtpReceivedPacketInfo,
    SdpAudioFormat, Timestamp,
};
use std::collections::BTreeMap;

fn stock() -> NetEq {
    let environment = Environment::builder().build().unwrap();
    let factory = AudioDecoderFactory::builtin().unwrap();
    // Both dependencies intentionally drop before the caller uses the engine.
    NetEq::stock(&environment, &NetEqConfig::default(), &factory).unwrap()
}
#[test]
fn absent_factory_is_native_stock_without_an_implicit_decoder() {
    let environment = Environment::builder().build().unwrap();
    let config = NetEqConfig {
        sample_rate_hz: 8000,
        ..Default::default()
    };
    let mut neteq = NetEq::stock_with_decoder_factory(&environment, &config, None).unwrap();
    assert!(!neteq.is_failed());
    assert!(neteq.register_payload_type(0, &pcmu()));
    // The pinned stock product inherits the deprecated getter's absent result.
    assert!(neteq.decoder_format(0).is_none());
    assert!(!neteq.create_decoder(0));
    assert_eq!(
        neteq.insert_packet(&header(0, 1, 0), &[0xff; 80]),
        NetEqReturnCode::FAIL
    );
    assert!(neteq.playout_timestamp().is_none());
    assert!(neteq.remove_payload_type(0) == NetEqReturnCode::OK);
    // Empty playout still runs the stock engine, not a permanently failed substitute.
    let mut frame = AudioFrame::new();
    assert_eq!(
        neteq.get_audio(&mut frame, None, None, None),
        Ok(NetEqReturnCode::OK)
    );
    assert_eq!(frame.fields().samples_per_channel, 80);
    assert_eq!(neteq.lifetime_statistics().total_samples_received, 0);
}

#[test]
fn absent_factory_preserves_native_internal_comfort_noise() {
    let environment = Environment::builder().build().unwrap();
    let config = NetEqConfig {
        sample_rate_hz: 8000,
        ..Default::default()
    };
    let mut neteq = NetEq::stock_with_decoder_factory(&environment, &config, None).unwrap();
    let cn = SdpAudioFormat::new(b"CN".to_vec(), 8000, 1);
    assert!(neteq.register_payload_type(13, &cn));
    assert_eq!(
        neteq.insert_packet(&header(13, 1, 0), &[42]),
        NetEqReturnCode::OK
    );
    let mut frame = AudioFrame::new();
    assert_eq!(
        neteq.get_audio(&mut frame, None, None, None),
        Ok(NetEqReturnCode::OK)
    );
    assert_eq!(frame.fields().sample_rate_hz, 8000);
    assert_eq!(frame.fields().samples_per_channel, 80);
    assert!(neteq.current_decoder_format().is_none());
    assert_eq!(frame.fields().speech_type, AudioFrameSpeechType::CNG);
    assert!(frame.data_view().unwrap().iter().any(|&sample| sample != 0));
}

fn pcmu() -> SdpAudioFormat {
    SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1)
}
fn header(payload_type: u8, sequence_number: u16, timestamp: u32) -> RtpHeaderValues {
    RtpHeaderValues {
        payload_type,
        sequence_number,
        timestamp,
        ssrc: 73,
        ..Default::default()
    }
}

#[test]
fn lazy_registration_preserves_unsupported_and_supported_but_null_outcomes() {
    let mut engine = stock();
    assert!(engine.register_payload_type(96, &SdpAudioFormat::new(b"unknown".to_vec(), 123, 1)));
    assert!(!engine.create_decoder(96));
    assert_eq!(
        engine.insert_packet(&header(96, 1, 1000), &[1]),
        NetEqReturnCode::FAIL
    );
    let mut format = SdpAudioFormat::new(b"unusual-name".to_vec(), 123, 1);
    for (key, value) in [
        (b"num_streams".as_slice(), b"0".as_slice()),
        (b"coupled_streams", b"0"),
        (b"channel_mapping", b"255"),
    ] {
        format.parameters.insert(key.to_vec(), value.to_vec());
    }
    assert!(
        AudioDecoderFactory::builtin()
            .unwrap()
            .is_supported_decoder(&format)
            .unwrap()
    );
    assert!(engine.register_payload_type(97, &format));
    assert!(!engine.create_decoder(97));
    assert_eq!(
        engine.insert_packet(&header(97, 2, 1160), &[1]),
        NetEqReturnCode::FAIL
    );
    assert!(engine.register_payload_type(98, &pcmu()));
    assert!(engine.create_decoder(98));
    // Preserve the SDK's int-to-uint8 compatibility conversion, not an allowlist.
    assert!(engine.create_decoder(98 + 256));
}

#[test]
fn same_format_payload_switches_recreate_destroyed_instances() {
    let mut engine = stock();
    assert!(engine.register_payload_type(96, &pcmu()));
    assert!(engine.register_payload_type(97, &pcmu()));
    let mut frame = AudioFrame::new();
    for (index, pt) in [96, 97, 96, 97, 96].into_iter().enumerate() {
        assert_eq!(
            engine.insert_packet(
                &header(pt, index as u16, 1000 + index as u32 * 160),
                &[0xff; 160]
            ),
            NetEqReturnCode::OK
        );
        assert_eq!(
            engine.get_audio(&mut frame, None, None, None).unwrap(),
            NetEqReturnCode::OK
        );
        assert_eq!(
            (
                frame.sample_rate_hz(),
                frame.num_channels(),
                frame.samples_per_channel()
            ),
            (8000, 1, 80)
        );
        assert_eq!(
            engine.current_decoder_format().unwrap().payload_type,
            i32::from(pt)
        );
    }
}

#[test]
fn remove_all_refuses_queued_frames_without_implicit_flush_or_codec_mutation() {
    let mut engine = stock();
    assert!(engine.register_payload_type(96, &pcmu()));
    // PCMU 0xff decodes to silence. Use nonzero audio to prove that the
    // rejected removal retains the queued decoder data, not just a zero frame.
    assert_eq!(
        engine.insert_packet(&header(96, 1, 1000), &[0; 160]),
        NetEqReturnCode::OK
    );
    let before = engine.operations_and_state();
    // next_packet_available also requires sync-buffer timestamp alignment;
    // the removal guard below, not that metadata bit, proves a queued packet.
    assert!(before.current_buffer_size_ms > 0);
    assert_eq!(
        engine.remove_all_payload_types(),
        Err(NetEqError::BufferedPackets)
    );
    assert_eq!(engine.operations_and_state(), before);
    assert!(engine.create_decoder(96));
    let mut retained = AudioFrame::new();
    assert_eq!(
        engine.get_audio(&mut retained, None, None, None),
        Ok(NetEqReturnCode::OK)
    );
    assert_eq!(
        retained.fields().speech_type,
        AudioFrameSpeechType::NORMAL_SPEECH
    );
    assert!(
        retained
            .data_view()
            .unwrap()
            .iter()
            .any(|&sample| sample != 0)
    );
    engine.flush_buffers();
    assert_eq!(engine.remove_all_payload_types(), Ok(()));
    assert!(!engine.create_decoder(96));
    assert!(engine.register_payload_type(96, &pcmu()));
    assert_eq!(
        engine.insert_packet(&header(96, 2, 1160), &[0xff; 160]),
        NetEqReturnCode::OK
    );
    assert_eq!(
        engine
            .get_audio(&mut AudioFrame::new(), None, None, None)
            .unwrap(),
        NetEqReturnCode::OK
    );
}

#[test]
fn unsafe_codec_maps_and_buffer_resize_are_atomic_refusals() {
    let mut engine = stock();
    assert!(engine.register_payload_type(96, &pcmu()));
    for (pt, format) in [
        (128, pcmu()),
        (-1, pcmu()),
        (13, SdpAudioFormat::new(b"cN".to_vec(), 123, 1)),
    ] {
        assert_eq!(
            engine.set_codecs(&BTreeMap::from([(pt, format)])),
            Err(NetEqError::InvalidCodecMap)
        );
        assert!(engine.create_decoder(96));
    }
    assert!(!engine.register_payload_type(13, &SdpAudioFormat::new(b"CN".to_vec(), 123, 1)));
    assert_eq!(
        engine.insert_packet(&header(96, 1, 1000), &[0xff; 160]),
        NetEqReturnCode::OK
    );
    let before = engine.operations_and_state();
    assert_eq!(
        engine.set_maximum_buffer_packets(usize::MAX),
        Err(NetEqError::InvalidStockConfiguration)
    );
    assert_eq!(engine.operations_and_state(), before);
    assert_eq!(engine.set_maximum_buffer_packets(0), Ok(()));
    assert!(!engine.operations_and_state().next_packet_available);
}

#[test]
fn get_audio_guard_keeps_outputs_and_allows_stock_override_normalization() {
    let mut engine = stock();
    let mut frame = AudioFrame::new();
    let mut muted = true;
    let mut rate = -73;
    let before = frame.fields();
    assert_eq!(
        engine.get_audio(
            &mut frame,
            Some(&mut muted),
            Some(&mut rate),
            Some(NetEqOperation(-1))
        ),
        Err(NetEqError::InvalidOperation)
    );
    assert_eq!(
        engine
            .get_audio(
                &mut frame,
                Some(&mut muted),
                Some(&mut rate),
                Some(NetEqOperation::UNDEFINED)
            )
            .unwrap(),
        NetEqReturnCode::FAIL
    );
    assert_eq!((muted, rate), (true, -73));
    assert_eq!(frame.fields(), before);
    assert!(engine.register_payload_type(96, &pcmu()));
    assert_eq!(
        engine.insert_packet(&header(96, 1, 1000), &[0xff; 160]),
        NetEqReturnCode::OK
    );
    // A speech packet with insufficient decoded samples must not reach RFC CNG.
    assert_eq!(
        engine
            .get_audio(&mut frame, None, None, Some(NetEqOperation::RFC3389_CNG))
            .unwrap(),
        NetEqReturnCode::FAIL
    );
    assert_eq!(
        engine.get_audio(&mut frame, None, None, None).unwrap(),
        NetEqReturnCode::OK
    );
    // Native has another 10 ms available and normalizes this override to Normal.
    assert!(engine.sync_buffer_size_ms() >= 10);
    assert_eq!(
        engine
            .get_audio(&mut frame, None, None, Some(NetEqOperation::RFC3389_CNG))
            .unwrap(),
        NetEqReturnCode::OK
    );
}

#[test]
fn receive_time_rejection_precedes_insertion_and_info_fields_remain_independent() {
    let mut engine = stock();
    assert!(engine.register_payload_type(96, &pcmu()));
    let packet = header(96, 1, 1000);
    let before = engine.operations_and_state();
    assert_eq!(
        engine.insert_packet_at(&packet, &[0xff; 160], Timestamp::plus_infinity()),
        Err(NetEqError::ReceiveTimeOutOfRange)
    );
    assert_eq!(engine.operations_and_state(), before);
    let info = RtpReceivedPacketInfo {
        ssrc: 42,
        csrcs: vec![43, 44],
        rtp_timestamp: packet.timestamp,
        sequence_number: packet.sequence_number,
        audio_level: Some(65),
        receive_time: Timestamp::minus_infinity(),
        ..Default::default()
    };
    assert_eq!(
        engine
            .insert_packet_with_info(&packet, &[0xff; 160], &info)
            .unwrap(),
        NetEqReturnCode::OK
    );
    let mut frame = AudioFrame::new();
    assert_eq!(
        engine.get_audio(&mut frame, None, None, None).unwrap(),
        NetEqReturnCode::OK
    );
    assert_eq!(&*frame.fields().packet_infos, &[info]);
}

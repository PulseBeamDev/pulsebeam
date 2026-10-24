use pulsebeam_webrtc_sys::*;

fn native_error<T>(result: Result<T, RtpSenderOperationError>, kind: RtcErrorType, message: &[u8]) {
    match result {
        Err(RtpSenderOperationError::Native(error)) => {
            assert_eq!(error.kind(), kind);
            assert_eq!(error.message_bytes(), message);
            assert_eq!(error.error_detail(), RtcErrorDetailType::None);
            assert_eq!(error.sctp_cause_code(), None);
        }
        Err(error) => panic!("expected native {kind:?}, got {error:?}"),
        Ok(_) => panic!("expected native {kind:?}, got success"),
    }
}

#[test]
fn trackless_creation_installs_full_init_and_preserves_native_normalization() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    for kind in [MediaType::Audio, MediaType::Video] {
        let mut init = RtpTransceiverInit::default();
        init.set_direction(RtpTransceiverDirectionValue::ReceiveOnly);
        let ids = vec![b"stream-a".to_vec(), b"opaque\0\xff".to_vec()];
        init.set_stream_ids(&ids);
        let mut first = RtpEncodingParameters::default();
        first.set_rid(b"first");
        first.set_max_bitrate_bps(Some(64_000));
        first.set_active(false);
        let mut second = first.clone();
        second.set_rid(b"second");
        init.set_send_encodings(&[first, second]);
        let transceiver = peer.add_transceiver(kind, &init).unwrap();
        assert_eq!(transceiver.media_type(), kind);
        assert_eq!(transceiver.direction_value(), init.direction());
        let sender = transceiver.sender();
        assert!(sender.track_reference().is_none());
        assert_eq!(sender.stream_id_bytes(), ids);
        let installed = sender.init_send_encodings();
        // Native audio drops all but the first encoding, then removes its RID.
        assert_eq!(
            installed.len(),
            if kind == MediaType::Audio { 1 } else { 2 }
        );
        assert_eq!(
            installed[0].rid_bytes(),
            if kind == MediaType::Audio {
                b"".as_slice()
            } else {
                b"first"
            }
        );
        assert_eq!(installed[0].max_bitrate_bps(), Some(64_000));
        assert!(!installed[0].active());
        // Installation never mutates the supplied value or its owned children.
        assert_eq!(init.stream_ids(), ids);
        assert_eq!(init.send_encodings().len(), 2);
        assert_eq!(init.send_encodings()[0].rid_bytes(), b"first");
        assert!(
            peer.transceivers()
                .unwrap()
                .iter()
                .any(|v| v.same_resource(&transceiver))
        );
    }
}

#[test]
fn native_rejections_and_binding_safety_failures_are_distinct_and_non_mutating() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let mut init = RtpTransceiverInit::default();
    for kind in [MediaType::Data, MediaType::Any, MediaType::Other(i32::MIN)] {
        native_error(
            peer.add_transceiver(kind, &init),
            RtcErrorType::InvalidParameter,
            b"media type is not audio or video",
        );
    }
    let mut encoding = RtpEncodingParameters::default();
    encoding.set_ssrc(Some(123));
    init.set_send_encodings(&[encoding]);
    native_error(
        peer.add_transceiver(MediaType::Video, &init),
        RtcErrorType::UnsupportedParameter,
        b"Attempted to set an unimplemented parameter of RtpParameters.",
    );
    assert_eq!(init.send_encodings()[0].ssrc(), Some(123));
    assert!(peer.transceivers().unwrap().is_empty());
    init.set_send_encodings(&[]);
    init.set_direction(RtpTransceiverDirectionValue::Other(i32::MIN));
    assert!(matches!(
        peer.add_transceiver(MediaType::Video, &init),
        Err(RtpSenderOperationError::Binding(PeerError {
            kind: PeerErrorKind::InvalidParameter,
            ..
        }))
    ));
    init.set_direction(RtpTransceiverDirectionValue::ReceiveOnly);
    assert!(peer.add_transceiver(MediaType::Video, &init).is_ok());
    peer.close();
    assert!(matches!(
        peer.add_transceiver(MediaType::Audio, &init),
        Err(RtpSenderOperationError::Binding(PeerError {
            kind: PeerErrorKind::Closed,
            ..
        }))
    ));

    // AddTransceiver on Plan B would terminate inside the pinned SDK.
    let mut config = RtcConfiguration::default();
    config.options.sdp_semantics = SdpSemantics::PlanB;
    let plan_b = factory
        .create_peer_connection_with_configuration(config)
        .unwrap();
    assert!(matches!(
        plan_b.add_transceiver(MediaType::Audio, &init),
        Err(RtpSenderOperationError::Binding(PeerError {
            kind: PeerErrorKind::InvalidParameter,
            ..
        }))
    ));
}

#[test]
fn audio_track_init_retains_track_and_forwards_complete_native_fields() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let source = factory.create_audio_source().unwrap();
    let track = factory.create_audio_track("pcm", &source).unwrap();
    let mut init = RtpTransceiverInit::default();
    init.set_stream_ids(&[b"first-stream".to_vec(), b"second-stream".to_vec()]);
    init.set_direction(RtpTransceiverDirectionValue::SendOnly);
    let mut encoding = RtpEncodingParameters::default();
    encoding.set_max_bitrate_bps(Some(48_000));
    init.set_send_encodings(&[encoding]);
    let mut rejected_init = init.clone();
    let mut rejected_encoding = RtpEncodingParameters::default();
    rejected_encoding.set_ssrc(Some(123));
    rejected_init.set_send_encodings(&[rejected_encoding]);
    native_error(
        peer.add_audio_transceiver_with_init(&track, &rejected_init),
        RtcErrorType::UnsupportedParameter,
        b"Attempted to set an unimplemented parameter of RtpParameters.",
    );
    assert!(peer.transceivers().unwrap().is_empty());
    assert_eq!(rejected_init.send_encodings()[0].ssrc(), Some(123));
    let transceiver = peer
        .add_media_transceiver_with_init(&track.as_media_track(), &init)
        .unwrap();
    let sender = transceiver.sender();
    assert_eq!(sender.stream_id_bytes(), init.stream_ids());
    assert_eq!(
        sender.init_send_encodings()[0].max_bitrate_bps(),
        Some(48_000)
    );
    assert_eq!(
        transceiver.direction_value(),
        RtpTransceiverDirectionValue::SendOnly
    );
    drop(track);
    drop(source);
    drop(factory);
    assert_eq!(sender.track_reference().unwrap().id(), b"pcm");
    assert!(sender.dtmf_sender().is_some());
    assert_eq!(peer.transceivers().unwrap().len(), 1);
}

#[test]
fn video_track_init_preserves_native_error_and_retains_the_successful_product() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let source = factory.create_video_source().unwrap();
    let track = factory.create_video_track("camera", &source).unwrap();
    let mut init = RtpTransceiverInit::default();
    init.set_direction(RtpTransceiverDirectionValue::SendOnly);
    init.set_stream_ids(&[b"camera-stream".to_vec()]);
    let mut encoding = RtpEncodingParameters::default();
    encoding.set_ssrc(Some(123));
    init.set_send_encodings(&[encoding.clone()]);
    native_error(
        peer.add_video_transceiver_with_init(&track, &init),
        RtcErrorType::UnsupportedParameter,
        b"Attempted to set an unimplemented parameter of RtpParameters.",
    );
    assert!(peer.transceivers().unwrap().is_empty());
    assert_eq!(init.send_encodings()[0].ssrc(), Some(123));

    encoding.set_ssrc(None);
    encoding.set_rid(b"main");
    encoding.set_max_bitrate_bps(Some(256_000));
    let mut smaller = encoding.clone();
    smaller.set_rid(b"small");
    smaller.set_max_bitrate_bps(Some(64_000));
    smaller.set_scale_resolution_down_by(Some(2.0));
    init.set_send_encodings(&[encoding, smaller]);
    let transceiver = peer
        .add_media_transceiver_with_init(&track.as_media_track(), &init)
        .unwrap();
    let sender = transceiver.sender();
    assert_eq!(transceiver.media_type(), MediaType::Video);
    assert_eq!(transceiver.direction_value(), init.direction());
    assert_eq!(sender.stream_id_bytes(), init.stream_ids());
    let installed = sender.init_send_encodings();
    assert_eq!(installed.len(), 2);
    assert_eq!(installed[0].rid_bytes(), b"main");
    assert_eq!(installed[0].max_bitrate_bps(), Some(256_000));
    assert_eq!(installed[1].rid_bytes(), b"small");
    assert_eq!(installed[1].max_bitrate_bps(), Some(64_000));
    assert_eq!(installed[1].scale_resolution_down_by(), Some(2.0));
    drop(track);
    drop(source);
    drop(factory);
    assert_eq!(sender.track_reference().unwrap().id(), b"camera");
    assert!(sender.dtmf_sender().is_none());
    assert!(
        peer.transceivers()
            .unwrap()
            .iter()
            .any(|value| value.same_resource(&transceiver))
    );
}

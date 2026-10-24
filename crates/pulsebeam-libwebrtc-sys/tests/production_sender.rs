use pulsebeam_webrtc_sys::*;

#[test]
fn migrated_sender_observations_and_stream_updates_use_native_state() {
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .encoded_audio()
        .build()
        .unwrap();
    let peer = session.create_peer(PeerConfiguration::default()).unwrap();
    let source = session.create_opus_source(1).unwrap();
    let transceiver = session
        .publish_opus(
            peer,
            source,
            "actor-stream-values",
            RtpTransceiverDirection::SendOnly,
        )
        .unwrap();
    let sender = session.sender(transceiver).unwrap();
    let native_id = session.sender_native_id(sender).unwrap();
    assert!(!native_id.is_empty());

    let (mut session, mut initial) = std::thread::spawn(move || {
        assert_eq!(session.sender_media_type(sender).unwrap(), MediaType::Audio);
        assert_eq!(session.sender_ssrc(sender).unwrap(), 0);
        assert_eq!(session.sender_native_id(sender).unwrap(), native_id);
        let error = session
            .generate_sender_keyframe(sender, &[vec![0xff, 0]])
            .unwrap_err();
        let RtpSenderOperationError::Native(error) = error else {
            panic!("actual audio native error required")
        };
        assert_eq!(error.kind(), RtcErrorType::UnsupportedOperation);
        assert!(!error.message_bytes().is_empty());
        let initial = session.sender_initial_encodings(sender).unwrap();
        assert_eq!(initial, vec![RtpEncodingParameters::default()]);
        // Native SetStreams de-duplicates by first occurrence, including empty
        // IDs. Embedded NUL and non-ASCII UTF-8 survive as exact bytes.
        session
            .set_sender_streams(
                sender,
                &["α\0one".into(), "".into(), "α\0one".into(), "two".into()],
            )
            .unwrap();
        assert_eq!(
            session.sender_stream_ids(sender).unwrap(),
            vec!["α\0one".as_bytes().to_vec(), vec![], b"two".to_vec()]
        );
        (session, initial)
    })
    .join()
    .unwrap();

    initial[0].set_min_bitrate_bps(Some(-7));
    assert_eq!(
        session.sender_initial_encodings(sender).unwrap()[0].min_bitrate_bps(),
        None
    );
    std::thread::spawn(move || {
        session.set_sender_streams(sender, &[]).unwrap();
        assert!(session.sender_stream_ids(sender).unwrap().is_empty());
        session.shutdown().unwrap();
        assert_eq!(
            session.sender_ssrc(sender).unwrap_err().kind,
            PeerErrorKind::Closed
        );
        assert_eq!(initial[0].min_bitrate_bps(), Some(-7));
        drop(session);
        // The actual SDK value copy remains usable after actor/factory release.
        assert_eq!(initial[0].min_bitrate_bps(), Some(-7));
    })
    .join()
    .unwrap();
}

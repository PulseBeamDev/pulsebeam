//! The same application operations, with dependencies selected by the harness.
//! This is constructor/operation evidence, not software-codec replay evidence.
use pulsebeam_webrtc_sys::*;
use std::{
    net::IpAddr,
    time::{Duration, Instant},
};

fn application<R: SessionRuntime>(runtime: R, mut progress: impl FnMut()) -> SessionDescription {
    let mut session = Session::with_runtime(runtime, SessionConfig::default())
        .event_logging(true)
        .audio_processing(AudioProcessingConfig::default())
        .build()
        .unwrap();
    let peer = session.create_peer(PeerConfiguration::default()).unwrap();
    for kind in [MediaType::Audio, MediaType::Video] {
        assert!(
            !session
                .get_rtp_sender_capabilities(kind)
                .unwrap()
                .codecs()
                .is_empty()
        );
        assert!(
            !session
                .get_rtp_receiver_capabilities(kind)
                .unwrap()
                .codecs()
                .is_empty()
        );
    }
    let pcm = session.create_pcm_source().unwrap();
    let video = session.create_raw_video_source().unwrap();
    session
        .publish_pcm(peer, pcm, "pcm", RtpTransceiverDirection::SendOnly, None)
        .unwrap();
    session
        .publish_video(
            peer,
            video,
            "raw-video",
            RtpTransceiverDirection::SendOnly,
            &[],
        )
        .unwrap();
    let mut receive = RtpTransceiverInit::default();
    receive.set_direction(RtpTransceiverDirectionValue::ReceiveOnly);
    let forwarding_peer = session.create_peer(PeerConfiguration::default()).unwrap();
    let mut send = RtpTransceiverInit::default();
    send.set_direction(RtpTransceiverDirectionValue::SendOnly);
    for kind in [MediaType::Audio, MediaType::Video] {
        let id = session.add_transceiver(peer, kind, &receive).unwrap();
        assert_eq!(session.transceiver_media_type(id).unwrap(), kind);
        assert_eq!(
            session.direction_value(id).unwrap(),
            RtpTransceiverDirectionValue::ReceiveOnly
        );
        let receiver = session.receiver(id).unwrap();
        let track = session.receiver_track_reference(receiver).unwrap().unwrap();
        let track_id = session.media_track_id(track).unwrap();
        let mut invalid = send.clone();
        let mut encoding = RtpEncodingParameters::default();
        encoding.set_ssrc(Some(123));
        invalid.set_send_encodings(&[encoding]);
        match session.add_media_transceiver_with_init(forwarding_peer, track, &invalid) {
            Err(RtpSenderOperationError::Native(error)) => {
                assert_eq!(error.kind(), RtcErrorType::UnsupportedParameter);
                assert_eq!(
                    error.message_bytes(),
                    b"Attempted to set an unimplemented parameter of RtpParameters."
                );
                assert_eq!(error.error_detail(), RtcErrorDetailType::None);
                assert_eq!(error.sctp_cause_code(), None);
            }
            result => panic!("expected the native initialization error, got {result:?}"),
        }
        let forwarded = session
            .add_media_transceiver_with_init(forwarding_peer, track, &send)
            .unwrap();
        session.release_media_track_reference(track).unwrap();
        assert_eq!(session.transceiver_media_type(forwarded).unwrap(), kind);
        let sender = session.sender(forwarded).unwrap();
        let retained = session.sender_track_reference(sender).unwrap().unwrap();
        assert_eq!(session.media_track_id(retained).unwrap(), track_id);
        session.release_media_track_reference(retained).unwrap();
    }
    let transceivers = session.transceivers(peer).unwrap();
    for &transceiver in &transceivers {
        assert_eq!(session.transceiver_mid(transceiver).unwrap(), None);
    }
    let operation = session.create_offer(peer, false).unwrap();
    let offer = loop {
        if let Some(SessionEvent::OperationComplete(done)) = session.try_peer_event(peer).unwrap() {
            if done.operation_id == operation {
                break done.result.unwrap().expect("native offer");
            }
        }
        progress();
    };
    assert!(offer.sdp.contains("m=audio "));
    assert!(offer.sdp.contains("m=video "));
    let operation = session.set_local_description(peer, offer.clone()).unwrap();
    loop {
        if let Some(SessionEvent::OperationComplete(done)) = session.try_peer_event(peer).unwrap() {
            if done.operation_id == operation {
                done.result.unwrap();
                break;
            }
        }
        progress();
    }
    let mut mids = std::collections::HashSet::new();
    for transceiver in transceivers {
        let mid = session.transceiver_mid(transceiver).unwrap().unwrap();
        assert!(offer.sdp.lines().any(|line| line == format!("a=mid:{mid}")));
        assert!(mids.insert(mid), "native transceivers have distinct MIDs");
    }
    session.shutdown().unwrap();
    offer
}

#[test]
fn common_application_constructs_stock_media_and_trackless_receivers_in_both_runtimes() {
    {
        let deadline = Instant::now() + Duration::from_secs(15);
        application(ThreadedRuntime, || {
            assert!(
                Instant::now() < deadline,
                "threaded native offer did not complete"
            );
            std::thread::yield_now();
        });
    }
    // Repeat acquisition to exercise full dependent teardown and hook restore.
    for _ in 0..2 {
        let world = ControlledWorld::acquire(17, Duration::from_secs(7)).unwrap();
        let network = world.create_network().unwrap();
        let endpoint = network
            .register_endpoint("192.0.2.1".parse::<IpAddr>().unwrap())
            .unwrap();
        let runtime = world.runtime().unwrap().with_endpoint(&endpoint).unwrap();
        let mut turns = 0;
        application(runtime, || {
            turns += 1;
            assert!(
                turns <= 4096,
                "managed offer stalled without advancing time"
            );
            assert!(!world.pump(64).blocked);
        });
        assert_eq!(world.now(), Duration::from_secs(7));
        drop(endpoint);
        drop(network);
        drop(world);
    }
}

#[test]
fn only_the_threaded_session_is_movable() {
    fn send<T: Send>() {}
    send::<Session<ThreadedRuntime>>();
    trait AmbiguousIfSend<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    struct IsSend;
    impl<T: ?Sized + Send> AmbiguousIfSend<IsSend> for T {}
    let _ = <Session<ManagedRuntime> as AmbiguousIfSend<_>>::check;

    trait AmbiguousIfSync<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfSync<()> for T {}
    struct IsSync;
    impl<T: ?Sized + Sync> AmbiguousIfSync<IsSync> for T {}
    let _ = <Session<ThreadedRuntime> as AmbiguousIfSync<_>>::check;
    let _ = <Session<ManagedRuntime> as AmbiguousIfSync<_>>::check;
}

#[test]
fn encoded_audio_is_an_explicit_mode_independent_adapter() {
    let mut raw = Session::with_runtime(ThreadedRuntime, SessionConfig::default())
        .build()
        .unwrap();
    assert!(raw.create_pcm_source().is_ok());
    assert!(raw.create_opus_source(1).is_err());
    raw.shutdown().unwrap();
    drop(raw);
    let mut encoded = Session::with_runtime(ThreadedRuntime, SessionConfig::default())
        .encoded_audio()
        .build()
        .unwrap();
    assert!(encoded.create_opus_source(1).is_ok());
    assert!(encoded.create_pcm_source().is_err());
}

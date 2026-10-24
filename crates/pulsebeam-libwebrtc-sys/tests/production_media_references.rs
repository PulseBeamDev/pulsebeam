use pulsebeam_webrtc_sys::*;
use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};

fn finish(
    session: &mut ProductionSession,
    peer: SessionPeerId,
    id: OperationId,
    receiver: &mut Option<SessionReceiverId>,
) -> Option<SessionDescriptionBytes> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match session.try_peer_event(peer).unwrap() {
            Some(SessionEvent::Track {
                receiver: value, ..
            }) => *receiver = Some(value),
            Some(SessionEvent::OperationComplete(done)) if done.operation_id == id => {
                return done.result.unwrap().map(Into::into);
            }
            Some(SessionEvent::OperationCompleteBytes {
                operation_id,
                description,
            }) if operation_id == id => return Some(description),
            Some(SessionEvent::OperationFailedBytes {
                operation_id,
                error,
            }) if operation_id == id => panic!("native SDP error: {error:?}"),
            _ => {}
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
}
#[test]
fn migrated_actor_owns_native_stream_source_and_bounded_notifier_references() {
    let mut session = ProductionSession::builder(Default::default())
        .encoded_audio()
        .build()
        .unwrap();
    let left = session.create_peer(Default::default()).unwrap();
    let right = session.create_peer(Default::default()).unwrap();
    let input = session.create_opus_source(1).unwrap();
    let tx = session
        .publish_opus(
            left,
            input,
            "actor-reference-track",
            RtpTransceiverDirection::SendOnly,
        )
        .unwrap();
    let sender = session.sender(tx).unwrap();
    session
        .set_sender_stream_id_bytes(sender, &[b"actor-stream-\xff".to_vec()])
        .unwrap();
    let track = session.sender_track_reference(sender).unwrap().unwrap();
    let local_source = session.media_track_source(track).unwrap().unwrap();
    assert_eq!(
        session.media_track_source(track).unwrap(),
        Some(local_source)
    );
    assert!(!session.track_source_remote(local_source).unwrap());
    assert_eq!(
        session.track_source_kind(local_source).unwrap(),
        MediaTrackKind::Audio
    );
    assert!(
        session
            .media_track_audio_processor(track)
            .unwrap()
            .is_none()
    );
    assert_eq!(session.media_track_signal_level(track).unwrap(), None);
    assert_eq!(session.media_track_audio_volume(track).unwrap(), Some(1.0));
    assert!(
        session
            .track_source_audio_options(local_source)
            .unwrap()
            .is_some()
    );
    let outcome = session
        .set_media_track_audio_options(
            track,
            &AudioSourceOptions {
                highpass_filter: Some(true),
                highpass_filter_mode: Some(AudioProcessingMode::Software),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(outcome.code, TrackAudioOptionsCode::Stored);
    assert_eq!(
        session
            .track_source_audio_options(local_source)
            .unwrap()
            .unwrap()
            .highpass_filter,
        Some(true)
    );
    let mut received = None;
    let op = session.create_offer(left, false).unwrap();
    let offer = finish(&mut session, left, op, &mut received).unwrap();
    let op = session
        .set_local_description_bytes(left, offer.clone())
        .unwrap();
    finish(&mut session, left, op, &mut received);
    let op = session.set_remote_description_bytes(right, offer).unwrap();
    finish(&mut session, right, op, &mut received);
    let receiver = received.unwrap();
    let op = session.create_answer(right).unwrap();
    let answer = finish(&mut session, right, op, &mut received).unwrap();
    let op = session
        .set_local_description_bytes(right, answer.clone())
        .unwrap();
    finish(&mut session, right, op, &mut received);
    let op = session.set_remote_description_bytes(left, answer).unwrap();
    finish(&mut session, left, op, &mut received);
    let remote_track = session.receiver_track_reference(receiver).unwrap().unwrap();
    let remote_source = session.media_track_source(remote_track).unwrap().unwrap();
    let streams = session.receiver_streams(receiver).unwrap();
    assert_eq!(streams.len(), 1);
    let stream = streams[0];
    assert_eq!(session.receiver_streams(receiver).unwrap(), streams);
    assert_eq!(
        session.media_stream_id(stream).unwrap(),
        b"actor-stream-\xff"
    );
    assert_eq!(
        session.receiver_stream_ids(receiver).unwrap(),
        vec![b"actor-stream-\xff".to_vec()]
    );
    assert_eq!(
        session.receiver_media_type(receiver).unwrap(),
        MediaType::Audio
    );
    assert!(!session.receiver_native_id(receiver).unwrap().is_empty());
    assert!(!session.sender_native_id_bytes(sender).unwrap().is_empty());
    assert_eq!(
        session.media_stream_audio_tracks(stream).unwrap(),
        vec![remote_track]
    );
    let id = session.media_track_id(remote_track).unwrap();
    assert_eq!(
        session.media_stream_find_audio_track(stream, &id).unwrap(),
        Some(remote_track)
    );
    assert!(
        session
            .media_stream_find_video_track(stream, &id)
            .unwrap()
            .is_none()
    );
    assert!(
        session
            .media_stream_video_tracks(stream)
            .unwrap()
            .is_empty()
    );
    assert!(
        session
            .register_media_track_observer(track, NonZeroUsize::new(1).unwrap())
            .unwrap()
    );
    assert!(
        !session
            .register_media_track_observer(track, NonZeroUsize::new(9).unwrap())
            .unwrap()
    );
    assert!(
        session
            .register_media_stream_observer(stream, NonZeroUsize::new(2).unwrap())
            .unwrap()
    );
    std::thread::spawn(move || {
        let mut session = session;
        assert_eq!(session.sender_track_reference(sender).unwrap(), Some(track));
        for enabled in [false, true, false] {
            assert!(session.set_media_track_enabled(track, enabled).unwrap());
        }
        assert!(session.take_media_track_event(track).unwrap().is_some());
        assert!(session.take_media_track_event(track).unwrap().is_none());
        assert_eq!(session.take_media_track_dropped_events(track).unwrap(), 2);
        assert_eq!(session.take_media_track_dropped_events(track).unwrap(), 0);
        assert!(session.track_source_remote(remote_source).unwrap());
        session.set_media_track_volume(remote_track, 3.25).unwrap();
        assert_eq!(
            session.media_track_audio_volume(remote_track).unwrap(),
            Some(3.25)
        );
        assert_eq!(
            session
                .set_media_track_audio_options(remote_track, &AudioSourceOptions::default())
                .unwrap()
                .code,
            TrackAudioOptionsCode::RejectedRemoteTrack
        );
        session.shutdown().unwrap();
        assert_eq!(
            session.media_track_state(remote_track).unwrap(),
            MediaTrackState::Ended
        );
        assert_eq!(
            session.track_source_state(remote_source).unwrap(),
            MediaSourceState::Ended
        );
        assert_eq!(
            session.media_stream_id(stream).unwrap(),
            b"actor-stream-\xff"
        );
        session.unregister_media_stream_observer(stream).unwrap();
        session.release_media_stream(stream).unwrap();
        assert!(session.media_stream_id(stream).is_err());
        session.release_media_track_reference(track).unwrap();
        assert!(session.take_media_track_event(track).is_err());
        session.release_media_track_reference(remote_track).unwrap();
        assert_eq!(
            session.track_source_state(remote_source).unwrap(),
            MediaSourceState::Ended
        );
        session.release_track_source(local_source).unwrap();
        session.release_track_source(remote_source).unwrap();
        drop(session);
    })
    .join()
    .unwrap();
}

#[test]
fn migrated_actor_forwards_video_track_controls_to_the_actual_sdk() {
    let mut session = ProductionSession::new(ProductionSessionConfig {
        video_format: Some(VideoCodecFormat::new("VP8")),
    })
    .unwrap();
    let peer = session.create_peer(Default::default()).unwrap();
    let input = session.create_video_source().unwrap();
    let tx = session
        .publish_video(
            peer,
            input,
            "actor-video-controls",
            RtpTransceiverDirection::SendOnly,
            &[],
        )
        .unwrap();
    let sender = session.sender(tx).unwrap();
    let track = session.sender_track_reference(sender).unwrap().unwrap();
    let source = session.media_track_source(track).unwrap().unwrap();
    std::thread::spawn(move || {
        assert_eq!(
            session.media_track_should_receive(track).unwrap(),
            Some(true)
        );
        session
            .set_media_track_should_receive(track, false)
            .unwrap();
        assert_eq!(
            session.media_track_should_receive(track).unwrap(),
            Some(false)
        );
        session
            .set_media_track_content_hint(track, VideoContentHint::Detailed)
            .unwrap();
        assert_eq!(
            session.media_track_content_hint(track).unwrap(),
            Some(VideoContentHint::Detailed)
        );
        assert_eq!(
            session.track_source_kind(source).unwrap(),
            MediaTrackKind::Video
        );
        assert!(session.track_source_video_info(source).unwrap().is_some());
        session.close_source(input).unwrap();
        assert_eq!(
            session.track_source_state(source).unwrap(),
            MediaSourceState::Ended
        );
        session.release_media_track_reference(track).unwrap();
        assert_eq!(
            session.track_source_state(source).unwrap(),
            MediaSourceState::Ended
        );
        session.release_track_source(source).unwrap();
        session.shutdown().unwrap();
    })
    .join()
    .unwrap();
}

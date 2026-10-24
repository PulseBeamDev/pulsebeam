use pulsebeam_webrtc_sys::*;
use std::{num::NonZeroUsize, thread};

#[test]
fn native_container_preserves_order_duplicate_ids_id_removal_and_origin_ownership() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let other = PeerConnectionFactory::builder().build().unwrap();
    let source = factory.create_audio_source().unwrap();
    let other_source = other.create_audio_source().unwrap();
    let video_source = other.create_video_source().unwrap();
    let a = factory.create_audio_track("same", &source).unwrap();
    let duplicate = other.create_audio_track("same", &other_source).unwrap();
    let b = other.create_audio_track("second", &other_source).unwrap();
    let video = other.create_video_track("same", &video_source).unwrap();
    assert!(!a.same_resource(&duplicate));
    let stream = factory.create_media_stream("container\0id").unwrap();
    assert_eq!(stream.id(), "container\0id");
    assert!(stream.audio_tracks().is_empty() && stream.video_tracks().is_empty());
    assert!(stream.find_audio_track("absent").is_none());
    let observer = stream
        .observe_changes(NonZeroUsize::new(2).unwrap())
        .unwrap();
    assert!(stream.add_audio_track(&a));
    assert!(!stream.add_audio_track(&duplicate));
    assert!(!stream.add_audio_track(&a));
    assert!(stream.add_audio_track(&b));
    assert!(stream.add_video_track(&video)); // Duplicate IDs are per kind.
    assert_eq!(observer.take_dropped_events(), 1);
    assert_eq!(observer.take_dropped_events(), 0);
    let tracks = stream.audio_tracks();
    assert_eq!(
        tracks.iter().map(AudioTrack::id).collect::<Vec<_>>(),
        ["same", "second"]
    );
    assert!(tracks[0].same_resource(&a) && tracks[1].same_resource(&b));
    assert!(stream.find_audio_track("same").unwrap().same_resource(&a));
    assert!(
        stream
            .find_video_track("same")
            .unwrap()
            .same_resource(&video)
    );
    assert!(tracks[0].set_enabled(false));
    assert!(!a.enabled());
    observer.close();
    observer.close();
    // Native Remove matches ID even when the argument is a different object
    // that native Add rejected. Do not prune dependencies using that argument.
    assert!(stream.remove_audio_track(&duplicate));
    assert!(!stream.remove_audio_track(&a));
    assert!(stream.find_audio_track("same").is_none());
    assert!(stream.add_audio_track(&duplicate));
    let retained = stream.find_audio_track("same").unwrap();
    assert!(retained.same_resource(&duplicate));
    assert_eq!(
        stream
            .audio_tracks()
            .iter()
            .map(AudioTrack::id)
            .collect::<Vec<_>>(),
        ["second", "same"]
    );
    let retained_video = stream.video_tracks().pop().unwrap();
    drop((
        factory,
        other,
        source,
        other_source,
        video_source,
        a,
        duplicate,
        b,
        video,
        tracks,
    ));
    drop(stream);
    // The observer retains the container; returned tracks independently retain
    // their exact origins and do not point back to the container.
    assert_eq!(observer.try_next_event(), Some(MediaStreamEvent::Changed));
    assert_eq!(observer.try_next_event(), Some(MediaStreamEvent::Changed));
    assert_eq!(observer.try_next_event(), None);
    drop(observer);
    assert_eq!(retained.id(), "same");
    assert!(retained.enabled());
    assert!(retained_video.set_enabled(false));
    assert!(!retained_video.enabled());
    assert_eq!(retained.source().unwrap().state(), AudioSourceState::Live);
}

#[test]
fn actor_stream_tracks_and_closed_observer_output_survive_identifier_retirement_and_migration() {
    let encoder = AudioEncoderFactory::builtin().unwrap();
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .audio_encoder_factory(&encoder)
        .build()
        .unwrap();
    let a = session.create_pcm_source().unwrap();
    let b = session.create_pcm_source().unwrap();
    let v = session.create_raw_video_source().unwrap();
    let audio = session.create_source_track(a, "same").unwrap();
    let duplicate = session.create_source_track(b, "same").unwrap();
    let video = session.create_source_track(v, "same").unwrap();
    let stream = session.create_media_stream("").unwrap();
    assert_eq!(session.local_stream_id(stream).unwrap(), "");
    let events = session
        .observe_media_stream(stream, NonZeroUsize::new(2).unwrap())
        .unwrap();
    assert!(session.local_add_media_stream_track(stream, audio).unwrap());
    assert!(
        !session
            .local_add_media_stream_track(stream, duplicate)
            .unwrap()
    );
    assert!(session.local_add_media_stream_track(stream, video).unwrap());
    let audio_alias = session.local_stream_audio_tracks(stream).unwrap()[0];
    let video_alias = session.local_stream_video_tracks(stream).unwrap()[0];
    assert!(
        session
            .media_stream_tracks_same(audio_alias, audio)
            .unwrap()
    );
    assert!(
        !session
            .media_stream_tracks_same(audio_alias, duplicate)
            .unwrap()
    );
    assert!(
        session
            .media_stream_tracks_same(video_alias, video)
            .unwrap()
    );
    let found = session
        .find_media_stream_audio_track(stream, "same")
        .unwrap()
        .unwrap();
    assert!(session.media_stream_tracks_same(found, audio).unwrap());
    session.release_media_stream_track(found);
    assert_eq!(
        session
            .find_media_stream_video_track(stream, "absent")
            .unwrap(),
        None
    );
    let session = thread::spawn(move || {
        let mut session = session;
        assert!(
            session
                .local_remove_media_stream_track(stream, duplicate)
                .unwrap()
        );
        assert!(
            !session
                .local_remove_media_stream_track(stream, audio)
                .unwrap()
        );
        assert!(
            session
                .local_stream_audio_tracks(stream)
                .unwrap()
                .is_empty()
        );
        assert_eq!(session.take_media_stream_dropped(events).unwrap(), 1);
        session.close_media_stream_observer(events).unwrap();
        session.close_media_stream_observer(events).unwrap();
        session.local_release_media_stream(stream);
        for id in [audio, duplicate, video] {
            session.release_media_stream_track(id);
        }
        for id in [a, b, v] {
            session.close_source(id).unwrap();
        }
        session
    })
    .join()
    .unwrap();
    thread::spawn(move || {
        let mut session = session;
        assert_eq!(session.media_stream_track_id(audio_alias).unwrap(), "same");
        assert_eq!(
            session.media_stream_track_kind(audio_alias).unwrap(),
            MediaType::Audio
        );
        assert_eq!(
            session.media_stream_track_kind(video_alias).unwrap(),
            MediaType::Video
        );
        assert!(
            session
                .set_media_stream_track_enabled(video_alias, false)
                .unwrap()
        );
        assert!(!session.media_stream_track_enabled(video_alias).unwrap());
        for _ in 0..2 {
            assert_eq!(
                session.try_media_stream_event(events).unwrap(),
                Some(MediaStreamEvent::Changed)
            );
        }
        assert_eq!(session.try_media_stream_event(events).unwrap(), None);
        session.release_media_stream_observer(events);
        session.release_media_stream_track(audio_alias);
        session.release_media_stream_track(video_alias);
        session.shutdown().unwrap();
    })
    .join()
    .unwrap();
}

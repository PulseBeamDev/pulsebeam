use pulsebeam_webrtc_sys::*;
use std::num::NonZeroUsize;

#[test]
fn native_video_views_preserve_defaults_change_notifications_and_exact_origin_lifetime() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut source = factory.create_video_source().unwrap();
    let track = factory
        .create_video_track("observed-video", &source)
        .unwrap();
    let producer = source.as_view();
    let source_alias = source.as_view();
    assert!(producer.same_interface(&source_alias));
    let installed = track.source().unwrap();
    let installed_alias = track.source().unwrap();
    assert!(installed.same_interface(&installed_alias));
    assert_eq!(source.rejected_sink_requests(), 0);
    assert_eq!(producer.rejected_sink_requests(), Some(0));
    assert_eq!(installed.rejected_sink_requests(), Some(0));
    // Source identity is the actual interface. Do not unwrap SDK proxies or
    // guess a join between the producer and the track's source interface.
    for view in [&producer, &installed] {
        assert_eq!(
            view.status(),
            VideoSourceStatus {
                state: VideoSourceState::Live,
                remote: false,
                is_screencast: false,
                needs_denoising: None,
                supports_encoded_output: false,
                stats: None,
            }
        );
    }
    assert_eq!(track.kind(), "video");
    assert!(track.should_receive());
    assert_eq!(track.content_hint(), VideoContentHint::None);
    let cap = NonZeroUsize::new(2).unwrap();
    let source_events = installed.observe_changes(cap).unwrap();
    let track_events = track.observe_changes(cap).unwrap();
    for view in [&producer, &installed] {
        view.request_refresh_frame();
        view.generate_key_frame(); // Native no-op, not fabricated Unsupported.
        view.process_constraints(VideoSourceConstraints {
            min_fps: Some(-0.0),
            max_fps: Some(f64::from_bits(0x7ff8000000000123)),
        });
        view.process_constraints(VideoSourceConstraints::default());
        assert_eq!(view.state(), VideoSourceState::Live);
    }
    track.request_refresh_frame();
    assert_eq!(source_events.try_next_event(), None);
    assert_eq!(track_events.try_next_event(), None);
    track.set_should_receive(false);
    track.set_should_receive(false);
    assert!(!track.should_receive());
    assert_eq!(
        track_events.try_next_event(),
        Some(VideoObserverEvent::Changed)
    );
    assert_eq!(track_events.try_next_event(), None);
    for hint in [
        VideoContentHint::Fluid,
        VideoContentHint::Detailed,
        VideoContentHint::Text,
        VideoContentHint::None,
    ] {
        track.set_content_hint(hint);
        track.set_content_hint(hint);
        assert_eq!(track.content_hint(), hint);
        assert_eq!(
            track_events.try_next_event(),
            Some(VideoObserverEvent::Changed)
        );
        assert_eq!(track_events.try_next_event(), None);
    }
    assert!(track.set_enabled(false));
    assert!(!track.set_enabled(false));
    assert_eq!(
        track_events.try_next_event(),
        Some(VideoObserverEvent::Changed)
    );
    assert_eq!(track_events.try_next_event(), None);
    // The stock push source does not implement GetStats. Seeing a real frame
    // must not turn requested geometry into fabricated native statistics.
    source
        .push_frame(&VideoFrame::i420(4, 4, vec![51; 24], 1234, 77).unwrap())
        .unwrap();
    assert_eq!(producer.stats(), None);
    assert_eq!(installed.stats(), None);
    for hint in [
        VideoContentHint::Fluid,
        VideoContentHint::Detailed,
        VideoContentHint::Text,
    ] {
        track.set_content_hint(hint);
    }
    assert_eq!(track_events.take_dropped_events(), 1);
    assert_eq!(track_events.take_dropped_events(), 0);
    track_events.close();
    track_events.close();
    source.close().unwrap();
    assert_eq!(source.state(), VideoSourceState::Ended);
    assert_eq!(installed.state(), VideoSourceState::Ended);
    assert_eq!(track.state(), VideoTrackState::Ended);
    assert_eq!(
        source_events.try_next_event(),
        Some(VideoObserverEvent::Changed)
    );
    assert_eq!(source_events.try_next_event(), None);
    source_events.close();
    drop((source, track, factory, producer, source_alias, installed));
    // Independently retained SDK source proxies and accepted callbacks survive
    // every original producer, track and factory wrapper.
    assert_eq!(installed_alias.state(), VideoSourceState::Ended);
    assert_eq!(installed_alias.stats(), None);
    assert_eq!(installed_alias.rejected_sink_requests(), Some(0));
    for _ in 0..2 {
        assert_eq!(
            track_events.try_next_event(),
            Some(VideoObserverEvent::Changed)
        );
    }
    assert_eq!(track_events.try_next_event(), None);
    assert_eq!(source_events.take_dropped_events(), 0);
}

#[test]
fn actor_video_view_and_closed_observer_survive_retired_ids_and_migration() {
    let mut session = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
    let source = session.create_raw_video_source().unwrap();
    let track = session.create_source_track(source, "actor-video").unwrap();
    let view = session.video_track_source_view(track).unwrap().unwrap();
    let alias = session.video_track_source_view(track).unwrap().unwrap();
    assert!(
        session
            .video_source_views_same_interface(view, alias)
            .unwrap()
    );
    let source_events = session
        .observe_video_source_changes(view, NonZeroUsize::new(2).unwrap())
        .unwrap();
    let track_events = session
        .observe_video_track_changes(track, NonZeroUsize::new(2).unwrap())
        .unwrap();
    let session = std::thread::spawn(move || {
        let mut session = session;
        session
            .set_video_track_should_receive(track, false)
            .unwrap();
        session
            .set_video_track_should_receive(track, false)
            .unwrap();
        session
            .set_video_track_content_hint(track, VideoContentHint::Text)
            .unwrap();
        session
            .set_video_track_content_hint(track, VideoContentHint::Text)
            .unwrap();
        assert_eq!(
            session.video_source_rejected_sink_requests(view).unwrap(),
            Some(0)
        );
        session.request_video_source_refresh(view).unwrap();
        session.generate_video_source_key_frame(view).unwrap();
        session
            .process_video_source_constraints(
                view,
                VideoSourceConstraints {
                    min_fps: None,
                    max_fps: Some(0.0),
                },
            )
            .unwrap();
        session.request_video_track_refresh(track).unwrap();
        let status = session.video_track_status(track).unwrap();
        assert_eq!(status.kind, "video");
        assert!(!status.should_receive);
        assert_eq!(status.content_hint, VideoContentHint::Text);
        session.close_video_observer(track_events).unwrap();
        session.release_media_stream_track(track);
        session.release_video_source_view(view);
        session.close_source(source).unwrap();
        session.close_video_observer(source_events).unwrap();
        session
    })
    .join()
    .unwrap();
    std::thread::spawn(move || {
        let mut session = session;
        assert_eq!(
            session.video_source_status(alias).unwrap().state,
            VideoSourceState::Ended
        );
        assert_eq!(session.video_source_status(alias).unwrap().stats, None);
        assert_eq!(
            session.video_source_rejected_sink_requests(alias).unwrap(),
            Some(0)
        );
        session.request_video_source_refresh(alias).unwrap();
        session.generate_video_source_key_frame(alias).unwrap();
        session
            .process_video_source_constraints(alias, VideoSourceConstraints::default())
            .unwrap();
        for _ in 0..2 {
            assert_eq!(
                session.try_video_observer_event(track_events).unwrap(),
                Some(VideoObserverEvent::Changed)
            );
        }
        assert_eq!(
            session.try_video_observer_event(track_events).unwrap(),
            None
        );
        assert_eq!(
            session.take_video_observer_dropped(track_events).unwrap(),
            0
        );
        assert_eq!(
            session.try_video_observer_event(source_events).unwrap(),
            Some(VideoObserverEvent::Changed)
        );
        assert_eq!(
            session.try_video_observer_event(source_events).unwrap(),
            None
        );
        session.release_video_source_view(alias);
        session.release_video_observer(track_events);
        session.release_video_observer(source_events);
        session.shutdown().unwrap();
    })
    .join()
    .unwrap();
}

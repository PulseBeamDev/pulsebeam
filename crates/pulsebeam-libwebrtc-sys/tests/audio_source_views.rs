use pulsebeam_webrtc_sys::*;
use std::num::NonZeroUsize;

#[test]
fn native_source_aliases_notifications_and_closed_queues_retain_origins() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut source = factory.create_audio_source().unwrap();
    let track = factory.create_audio_track("observed", &source).unwrap();
    let direct = source.as_source_view();
    let view = track.source().unwrap();
    assert!(direct.same_resource(&view));
    assert!(!view.is_remote());
    let changes = view.observe_changes(NonZeroUsize::new(2).unwrap()).unwrap();
    let volume = view.observe_volume(NonZeroUsize::new(1).unwrap()).unwrap();
    let track_changes = track
        .observe_changes(NonZeroUsize::new(1).unwrap())
        .unwrap();
    let mut options = AudioSourceOptions::default();
    options.stereo_swapping = Some(false);
    for _ in 0..3 {
        view.set_options(&options).unwrap();
    }
    assert_eq!(source.options(), options);
    assert_eq!(changes.take_dropped_events(), 1);
    assert_eq!(changes.take_dropped_events(), 0);
    assert_eq!(changes.try_next_event(), Some(AudioObserverEvent::Changed));
    assert_eq!(changes.try_next_event(), Some(AudioObserverEvent::Changed));
    assert_eq!(changes.try_next_event(), None);
    // Native OnChanged only forwards real track-state transitions.
    assert_eq!(track_changes.try_next_event(), None);
    view.set_volume(0.0).unwrap();
    assert_eq!(view.volume(), 1.0);
    assert_eq!(volume.try_next_event(), None); // Native local implementation is a no-op.
    assert!(track.set_enabled(false));
    assert!(!track.set_enabled(false));
    assert!(track.set_enabled(true));
    assert_eq!(track_changes.take_dropped_events(), 1);
    assert_eq!(
        track_changes.try_next_event(),
        Some(AudioObserverEvent::Changed)
    );
    assert_eq!(track_changes.try_next_event(), None);
    source.close().unwrap();
    assert_eq!(view.state(), AudioSourceState::Ended);
    assert_eq!(track.state(), AudioTrackState::Ended);
    let retained = changes.clone();
    changes.close();
    changes.close();
    track_changes.close();
    volume.close();
    // Close unregisters, but does not discard real accepted callback output.
    assert_eq!(
        track_changes.try_next_event(),
        Some(AudioObserverEvent::Changed)
    );
    view.set_options(&AudioSourceOptions::default()).unwrap();
    drop((
        factory,
        source,
        track,
        direct,
        changes,
        track_changes,
        volume,
    ));
    assert_eq!(retained.try_next_event(), Some(AudioObserverEvent::Changed));
    assert_eq!(retained.try_next_event(), None);
    assert_eq!(view.options(), AudioSourceOptions::default());
    assert_eq!(view.state(), AudioSourceState::Ended);
    retained.close();
}

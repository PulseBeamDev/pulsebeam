use pulsebeam_webrtc_sys::*;
use std::time::Duration;

#[test]
fn local_pcm_handoff_preserves_native_format_capture_presence_and_ownership() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let source = factory.create_audio_source().unwrap();
    let track = factory.create_audio_track("local-pcm", &source).unwrap();
    let mut sink = track.attach_sink().unwrap();
    for (rate, channels) in [(100, 1), (44100, 1), (44100, 6), (48000, 8)] {
        let samples = (0..rate / 100 * u32::from(channels))
            .map(|index| (index * 31 % 23000) as i16 - 11500)
            .collect();
        let frame = AudioPcmFrame::i16_interleaved(rate, channels, 777, samples).unwrap();
        source
            .push_frame_at(&frame, Duration::from_micros(1_234_567))
            .unwrap();
        let observed = sink
            .try_next_frame()
            .expect("native source did not deliver PCM");
        assert_eq!(
            (
                observed.sample_rate_hz,
                observed.channels,
                observed.samples_per_channel
            ),
            (rate, channels, rate / 100)
        );
        assert_eq!(observed.samples, frame.samples);
        assert_eq!(observed.capture_time_ms, Some(1234));
        assert_eq!(observed.capture_time_us, Some(1_234_000));
        assert!(sink.try_next_frame().is_none());
    }
    let mut frame = AudioPcmFrame::i16_interleaved(44100, 2, 0, vec![17; 882]).unwrap();
    frame.samples_per_channel += 1;
    assert_eq!(
        source.push_frame(&frame),
        Err(AudioFrameError::InvalidSampleCount)
    );
    assert!(sink.try_next_frame().is_none());
    frame.samples_per_channel -= 1;
    assert_eq!(
        source.push_frame_at(&frame, Duration::from_secs(u64::MAX)),
        Err(AudioFrameError::InvalidTimestamp)
    );
    assert!(sink.try_next_frame().is_none());
    source.push_frame(&frame).unwrap();
    let unstamped = sink.try_next_frame().unwrap();
    assert_eq!(unstamped.capture_time_ms, None);
    assert_eq!(unstamped.capture_time_us, None);
    source.push_frame_at(&frame, Duration::ZERO).unwrap();
    // Native callback output owns its samples independently of the caller's
    // input buffer and of every outer source/track/factory owner.
    frame.samples.fill(-19);
    drop((source, track, factory));
    let observed = sink.try_next_frame().unwrap();
    assert_eq!(observed.samples, vec![17; 882]);
    assert_eq!(observed.capture_time_ms, Some(0));
    assert_eq!(observed.capture_time_us, Some(0));
    assert_eq!(sink.dropped_frames(), 0);
    sink.close().unwrap();
    sink.close().unwrap();
    assert!(sink.try_next_frame().is_none());
    drop(sink);
    assert_eq!(observed.samples, vec![17; 882]);
}

#[test]
fn custom_clock_does_not_synthesize_a_pcm_capture_mapping() {
    let clock = ManualClock::new(Duration::from_secs(1)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let factory = PeerConnectionFactory::builder()
        .environment(environment)
        .build()
        .unwrap();
    let source = factory.create_audio_source().unwrap();
    let track = factory.create_audio_track("custom-clock", &source).unwrap();
    let sink = track.attach_sink().unwrap();
    let frame = AudioPcmFrame::i16_interleaved(8000, 1, 1_000_000, vec![3; 80]).unwrap();
    assert_eq!(
        source.capture_time_now(),
        Err(AudioFrameError::InvalidTimestamp)
    );
    assert_eq!(
        source.push_frame_at(&frame, Duration::from_secs(1)),
        Err(AudioFrameError::InvalidTimestamp)
    );
    assert!(sink.try_next_frame().is_none());
    source.push_frame(&frame).unwrap();
    let observed = sink.try_next_frame().unwrap();
    assert_eq!(observed.samples, frame.samples);
    assert_eq!(observed.capture_time_ms, None);
    assert_eq!(clock.now(), Duration::from_secs(1));
}

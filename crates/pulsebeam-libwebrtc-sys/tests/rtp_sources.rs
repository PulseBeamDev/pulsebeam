use pulsebeam_webrtc_sys::{
    AbsoluteCaptureTime, RtcError, RtpSource, RtpSourceExtensions, RtpSourceType, StringBuilder,
    TimeDelta, Timestamp,
};

#[test]
fn native_source_construction_copy_and_complete_extensions() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<RtpSource>();
    let extensions = RtpSourceExtensions {
        audio_level: Some(u8::MAX),
        absolute_capture_time: Some(AbsoluteCaptureTime {
            absolute_capture_timestamp: u64::MAX,
            estimated_capture_clock_offset: Some(i64::MIN),
        }),
        local_capture_clock_offset: Some(TimeDelta::minus_infinity()),
    };
    let mut source = RtpSource::new(
        Timestamp::minus_infinity(),
        u32::MAX,
        RtpSourceType::Csrc,
        u32::MAX,
        extensions,
    );
    assert_eq!(source.timestamp(), Timestamp::minus_infinity());
    assert_eq!(source.source_id(), u32::MAX);
    assert_eq!(source.source_type(), RtpSourceType::Csrc);
    assert_eq!(source.rtp_timestamp(), u32::MAX);
    assert_eq!(source.extensions(), extensions);
    assert_eq!(source.audio_level(), Some(u8::MAX));
    assert_eq!(
        source.absolute_capture_time(),
        extensions.absolute_capture_time
    );
    assert_eq!(
        source.local_capture_clock_offset(),
        extensions.local_capture_clock_offset
    );
    let copy = source.clone();
    source.set_audio_level(None);
    assert_eq!(source.audio_level(), None);
    assert_eq!(copy.audio_level(), Some(u8::MAX));
    assert_ne!(source, copy);
    source.clone_from(&copy);
    assert_eq!(source, copy);
    drop(copy);
    assert_eq!(source.extensions(), extensions);
    let detached = std::thread::spawn(move || source).join().unwrap();
    assert_eq!(detached.extensions(), extensions);
}

#[test]
fn native_source_equality_omits_only_local_capture_clock_offset() {
    let extensions = RtpSourceExtensions {
        audio_level: Some(0),
        absolute_capture_time: Some(AbsoluteCaptureTime {
            absolute_capture_timestamp: 0,
            estimated_capture_clock_offset: Some(0),
        }),
        local_capture_clock_offset: None,
    };
    let make = |timestamp, source_id, source_type, rtp_timestamp, extensions| {
        RtpSource::new(timestamp, source_id, source_type, rtp_timestamp, extensions)
    };
    let baseline = make(Timestamp::zero(), 0, RtpSourceType::Ssrc, 0, extensions);
    let changed = RtpSourceExtensions {
        local_capture_clock_offset: Some(TimeDelta::plus_infinity()),
        ..extensions
    };
    let same = make(Timestamp::zero(), 0, RtpSourceType::Ssrc, 0, changed);
    assert_eq!(baseline, same);
    assert_ne!(baseline.extensions(), same.extensions());
    assert_ne!(
        baseline,
        make(
            Timestamp::plus_infinity(),
            0,
            RtpSourceType::Ssrc,
            0,
            extensions
        )
    );
    assert_ne!(
        baseline,
        make(Timestamp::zero(), 1, RtpSourceType::Ssrc, 0, extensions)
    );
    assert_ne!(
        baseline,
        make(Timestamp::zero(), 0, RtpSourceType::Csrc, 0, extensions)
    );
    assert_ne!(
        baseline,
        make(Timestamp::zero(), 0, RtpSourceType::Ssrc, 1, extensions)
    );
    assert_ne!(
        baseline,
        make(
            Timestamp::zero(),
            0,
            RtpSourceType::Ssrc,
            0,
            RtpSourceExtensions {
                audio_level: None,
                ..extensions
            }
        )
    );
    assert_ne!(
        baseline,
        make(
            Timestamp::zero(),
            0,
            RtpSourceType::Ssrc,
            0,
            RtpSourceExtensions {
                absolute_capture_time: None,
                ..extensions
            }
        )
    );
}

#[test]
fn native_source_formatting_and_unknown_metadata_follow_sdk() {
    for (kind, label) in [
        (RtpSourceType::Ssrc, "SSRC"),
        (RtpSourceType::Csrc, "CSRC"),
        (RtpSourceType::Other(i32::MIN), "SSRC"),
        (RtpSourceType::Other(i32::MAX), "SSRC"),
    ] {
        let value = RtpSource::new(
            Timestamp::from_microseconds(-1).unwrap(),
            u32::MAX,
            kind,
            0,
            RtpSourceExtensions::default(),
        );
        assert_eq!(value.source_type(), kind);
        assert_eq!(value.timestamp().microseconds(), Ok(-1));
        assert_eq!(value.audio_level(), None);
        assert_eq!(value.absolute_capture_time(), None);
        assert_eq!(value.local_capture_clock_offset(), None);
        let expected = format!("{{ source_type: {label}, source_id: 4294967295 }}").into_bytes();
        assert_eq!(value.native_string_bytes(), expected);
        let mut builder = StringBuilder::default();
        builder.append_rtp_source(&value);
        assert_eq!(builder.as_bytes(), expected);
        let mut error = RtcError::default();
        error.string_builder().append_rtp_source(&value);
        assert_eq!(error.message_bytes(), expected);
        let error = RtcError::default().with_appended_rtp_source(&value);
        assert_eq!(error.message_bytes(), expected);
    }
}

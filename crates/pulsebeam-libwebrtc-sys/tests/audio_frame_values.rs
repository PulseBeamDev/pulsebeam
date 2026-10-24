use pulsebeam_webrtc_sys::{
    AbsoluteCaptureTime, AudioChannel, AudioFrame, AudioFrameFields,
    AudioFrameSpeechType as Speech, AudioFrameVadActivity as Vad, AudioFrameValueError as Error,
    ChannelLayout, RtpReceivedPacketInfo, RtpReceivedPacketInfos, TimeDelta, Timestamp,
};

#[test]
fn defaults_and_constructor_domains_are_native() {
    let frame = AudioFrame::new();
    assert_eq!(frame.fields(), AudioFrameFields::default());
    assert!(frame.muted());
    assert_eq!(frame.channel_layout(), ChannelLayout::NONE);
    assert_eq!(frame.absolute_capture_timestamp_ms(), None);
    assert!(frame.data_view().unwrap().is_empty());
    assert_eq!(frame.data().len(), 7680);
    assert!(frame.data().iter().all(|value| *value == 0));
    assert_eq!(frame.max_16bit_samples(), AudioFrame::MAX_DATA_SIZE_SAMPLES);
    assert_eq!(
        AudioFrame::sample_rate_to_default_channel_size(0).unwrap(),
        0
    );
    assert_eq!(
        AudioFrame::sample_rate_to_default_channel_size(192000).unwrap(),
        1920
    );
    assert!(AudioFrame::sample_rate_to_default_channel_size(192001).is_err());
    let stereo = AudioFrame::with_format(48000, 2, ChannelLayout::UNSUPPORTED).unwrap();
    assert_eq!(stereo.samples_per_channel(), 480);
    assert_eq!(stereo.channel_layout(), ChannelLayout::STEREO);
    assert_eq!(stereo.data_view().unwrap().len(), 960);
    assert!(stereo.muted());
    assert_eq!(
        AudioFrame::with_format(48000, 2, ChannelLayout::MONO)
            .unwrap()
            .channel_layout(),
        ChannelLayout::MONO
    );
    let unnamed = AudioFrame::with_format(48000, 2, ChannelLayout(63)).unwrap();
    assert_eq!(unnamed.channel_layout(), ChannelLayout(63));
    assert!(unnamed.channel_layout().channel_count().is_err());
    assert!(unnamed.channel_layout().as_string().is_err());
    assert!(AudioFrame::with_format(192000, 4, ChannelLayout::DISCRETE).is_ok());
    for (rate, channels, layout) in [
        (0, 1, 1),
        (99, 1, 1),
        (-1, 1, 1),
        (192001, 1, 1),
        (192000, 5, 1),
        (32000, 24, 1),
        (48000, 2, -1),
        (48000, 2, 64),
    ] {
        assert!(AudioFrame::with_format(rate, channels, ChannelLayout(layout)).is_err());
    }
}

#[test]
fn backing_is_initialized_and_mute_clearing_depends_on_overload() {
    let mut frame = AudioFrame::new();
    frame.mutable_data().fill(7);
    frame
        .set_fields(&AudioFrameFields {
            samples_per_channel: 4,
            num_channels: 2,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(frame.data_view().unwrap(), &[7; 8]);
    frame.mute();
    assert_eq!(frame.data_view().unwrap(), &[0; 8]);
    assert!(frame.data().iter().all(|value| *value == 0));
    assert_eq!(frame.mutable_data_with_shape(2, 2).unwrap(), &[0; 4]);
    assert!(!frame.muted());
    assert_eq!(&frame.data()[4..8], &[7; 4]);
    assert_eq!(frame.channel_layout(), ChannelLayout::NONE);
    frame.mute();
    assert!(frame.mutable_data().iter().all(|value| *value == 0));
    frame.mutable_data()[7679] = 91;
    frame
        .set_fields(&AudioFrameFields {
            samples_per_channel: 480,
            num_channels: 16,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(frame.data_view().unwrap()[7679], 91);
    assert_eq!(frame.mutable_data_with_shape(0, 0).unwrap().len(), 0);
    assert_eq!(
        frame.mutable_data_with_shape(usize::MAX, 0).unwrap().len(),
        0
    );
    assert!(frame.mutable_data_with_shape(0, 1).is_err());
    assert!(frame.mutable_data_with_shape(1, 2).is_err());
    assert!(frame.mutable_data_with_shape(usize::MAX, 2).is_err());
    assert!(frame.mutable_data_with_shape(480, 17).is_err());
}

#[test]
fn update_preserves_untouched_metadata_and_optional_data_presence() {
    let mut frame = AudioFrame::new();
    frame
        .set_fields(&AudioFrameFields {
            elapsed_time_ms: i64::MIN,
            ntp_time_ms: i64::MAX,
            ..Default::default()
        })
        .unwrap();
    frame.set_absolute_capture_timestamp_ms(i64::MIN);
    frame
        .update_frame(
            u32::MAX,
            Some(&[1, 2, 3, 4, 99]),
            2,
            -1,
            Speech(7),
            Vad(3),
            2,
        )
        .unwrap();
    assert_eq!(frame.data_view().unwrap(), &[1, 2, 3, 4]);
    let fields = frame.fields();
    assert_eq!(fields.timestamp, u32::MAX);
    assert_eq!(fields.elapsed_time_ms, i64::MIN);
    assert_eq!(fields.ntp_time_ms, i64::MAX);
    assert_eq!(fields.sample_rate_hz, -1);
    assert_eq!(fields.speech_type, Speech(7));
    assert_eq!(fields.vad_activity, Vad(3));
    assert_eq!(frame.absolute_capture_timestamp_ms(), Some(i64::MIN));
    assert_eq!(frame.channel_layout(), ChannelLayout::STEREO);
    assert!(!frame.muted());
    assert_eq!(
        frame.update_frame(0, Some(&[1]), 2, 48000, Speech::PLC, Vad::UNKNOWN, 2),
        Err(Error::DataTooShort)
    );
    assert_eq!(frame.fields(), fields);
    for (speech, vad) in [(-1, 0), (8, 0), (0, -1), (0, 4)] {
        assert_eq!(
            frame.update_frame(0, None, 0, 0, Speech(speech), Vad(vad), 0),
            Err(Error::InvalidEnumRepresentation)
        );
    }
    frame
        .update_frame(0, None, 2, 0, Speech::PLC, Vad::PASSIVE, 2)
        .unwrap();
    assert!(frame.muted());
    frame
        .update_frame(0, Some(&[]), 0, 0, Speech::UNDEFINED, Vad::UNKNOWN, 0)
        .unwrap();
    assert!(!frame.muted());
    frame
        .update_frame(0, None, 0, 0, Speech::UNDEFINED, Vad::UNKNOWN, 0)
        .unwrap();
    assert!(frame.muted());
}

#[test]
fn fields_copy_packet_metadata_and_frame_can_migrate_independently() {
    let infos = RtpReceivedPacketInfos::from(vec![RtpReceivedPacketInfo {
        sequence_number: u16::MAX,
        ssrc: u32::MAX,
        csrcs: (0..20).collect(),
        rtp_timestamp: u32::MAX,
        receive_time: Timestamp::plus_infinity(),
        audio_level: Some(u8::MAX),
        absolute_capture_time: Some(AbsoluteCaptureTime {
            absolute_capture_timestamp: u64::MAX,
            estimated_capture_clock_offset: Some(i64::MIN),
        }),
        local_capture_clock_offset: Some(TimeDelta::minus_infinity()),
    }]);
    let mut frame = AudioFrame::with_format(48000, 2, ChannelLayout::STEREO).unwrap();
    frame.mutable_data()[0] = 11;
    let fields = AudioFrameFields {
        timestamp: u32::MAX,
        elapsed_time_ms: i64::MAX,
        ntp_time_ms: i64::MIN,
        samples_per_channel: 2,
        sample_rate_hz: i32::MIN,
        num_channels: 2,
        speech_type: Speech(6),
        vad_activity: Vad(3),
        packet_infos: infos,
    };
    frame.set_fields(&fields).unwrap();
    frame.set_absolute_capture_timestamp_ms(i64::MAX);
    let copy = frame.try_clone().unwrap();
    assert_eq!(copy.fields(), fields);
    assert_eq!(copy.channel_layout(), ChannelLayout::STEREO);
    assert_eq!(copy.absolute_capture_timestamp_ms(), Some(i64::MAX));
    assert!(!copy.muted());
    assert_eq!(copy.data_view().unwrap(), &[11, 0, 0, 0]);
    frame.mutable_data()[0] = 19;
    drop(frame);
    let (output, samples) =
        std::thread::spawn(move || (copy.fields(), copy.data_view().unwrap().to_vec()))
            .join()
            .unwrap();
    assert_eq!(output, fields);
    assert_eq!(samples, [11, 0, 0, 0]);
}

#[test]
fn reset_copy_and_public_shape_fields_keep_native_distinctions() {
    let mut frame = AudioFrame::new();
    frame.mutable_data()[0] = 27;
    frame.reset_without_muting();
    assert!(!frame.muted());
    assert_eq!(frame.fields(), AudioFrameFields::default());
    assert_eq!(frame.data()[0], 27);
    frame.set_absolute_capture_timestamp_ms(0);
    frame.reset();
    assert!(frame.muted());
    assert_eq!(frame.absolute_capture_timestamp_ms(), None);
    assert_eq!(frame.fields(), AudioFrameFields::default());
    // Public fields permit shapes that are not legal for an InterleavedView.
    // Retain those fields, but reject the particular view/copy operations.
    let fields = AudioFrameFields {
        samples_per_channel: 0,
        num_channels: usize::MAX,
        ..Default::default()
    };
    frame.set_fields(&fields).unwrap();
    assert_eq!(frame.fields(), fields);
    assert_eq!(frame.data_view(), Err(Error::InvalidShape));
    assert_eq!(frame.data().len(), 7680);
    let mut target = AudioFrame::new();
    assert_eq!(target.copy_from(&frame), Err(Error::InvalidShape));
    assert_eq!(target.fields(), AudioFrameFields::default());
    let mut bad = fields.clone();
    bad.samples_per_channel = 2;
    assert_eq!(frame.set_fields(&bad), Err(Error::InvalidShape));
    assert_eq!(frame.fields(), fields);
    frame
        .set_fields(&AudioFrameFields {
            samples_per_channel: usize::MAX,
            num_channels: 0,
            ..Default::default()
        })
        .unwrap();
    assert!(frame.data_view().unwrap().is_empty());
    assert!(frame.try_clone().unwrap().data_view().unwrap().is_empty());
}

#[test]
fn channel_layout_lookups_and_rate_changes_validate_only_their_own_domains() {
    assert_eq!(ChannelLayout::guess(2), ChannelLayout::STEREO);
    assert_eq!(ChannelLayout::guess(i32::MIN), ChannelLayout::UNSUPPORTED);
    assert_eq!(ChannelLayout::STEREO.channel_count().unwrap(), 2);
    assert_eq!(
        ChannelLayout::STEREO
            .channel_order(AudioChannel::LEFT)
            .unwrap(),
        0
    );
    assert_eq!(
        ChannelLayout::STEREO
            .channel_order(AudioChannel::RIGHT)
            .unwrap(),
        1
    );
    assert_eq!(
        ChannelLayout::STEREO
            .channel_order(AudioChannel::CENTER)
            .unwrap(),
        -1
    );
    assert_eq!(ChannelLayout::STEREO.as_string().unwrap(), "STEREO");
    assert!(ChannelLayout(33).channel_count().is_err());
    assert!(ChannelLayout::MONO.channel_order(AudioChannel(11)).is_err());
    let mut frame = AudioFrame::new();
    frame
        .set_layout_and_num_channels(ChannelLayout::DISCRETE, usize::MAX)
        .unwrap();
    assert_eq!(frame.num_channels(), usize::MAX);
    assert_eq!(
        frame.set_sample_rate_and_channel_size(100),
        Err(Error::InvalidShape)
    );
    assert_eq!(frame.samples_per_channel(), 0);
    frame.set_sample_rate_and_channel_size(0).unwrap();
    frame
        .set_layout_and_num_channels(ChannelLayout::MONO, 1)
        .unwrap();
    assert_eq!(
        frame.set_layout_and_num_channels(ChannelLayout::STEREO, 1),
        Err(Error::InvalidLayoutIndex)
    );
    frame.set_sample_rate_and_channel_size(48000).unwrap();
    assert_eq!(frame.samples_per_channel(), 480);
    assert_eq!(frame.sample_rate_hz(), 48000);
    assert_eq!(frame.data_view().unwrap().len(), 480);
    assert_eq!(
        frame.set_sample_rate_and_channel_size(-1),
        Err(Error::InvalidSampleRate)
    );
    assert_eq!(
        frame.set_sample_rate_and_channel_size(192001),
        Err(Error::InvalidSampleRate)
    );
}

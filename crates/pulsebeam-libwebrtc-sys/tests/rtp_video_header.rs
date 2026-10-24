use pulsebeam_webrtc_sys::{
    AbsoluteCaptureTime, ColorSpace, FrameInstrumentationData, RtpVideoGenericDescriptorInfo,
    RtpVideoHeader, RtpVideoHeaderVp9, RtpVideoTypeHeader, VideoCodecType, VideoFrameMetadata,
    VideoMetadataDecodeTargetIndication, VideoMetadataError, VideoMetadataFrameType,
    VideoMetadataRotation, VideoPlayoutDelay, VideoSendTiming,
};

#[test]
fn full_header_storage_and_copy_are_independent() {
    let mut header = RtpVideoHeader::new();
    let default = header.values();
    assert!(default.generic.is_none());
    assert_eq!(default.frame_type, VideoMetadataFrameType::EMPTY);
    assert_eq!(default.video_type_header, RtpVideoTypeHeader::None);
    assert_eq!(default.video_timing, VideoSendTiming::default());
    assert!(default.is_last_frame_in_picture);
    let mut v = default;
    v.generic = Some(RtpVideoGenericDescriptorInfo {
        frame_id: i64::MIN,
        spatial_index: i32::MIN,
        temporal_index: i32::MAX,
        decode_target_indications: vec![VideoMetadataDecodeTargetIndication(i32::MIN)],
        dependencies: vec![i64::MAX, i64::MIN, i64::MAX],
        chain_diffs: vec![i32::MIN, i32::MAX],
        active_decode_targets: 0x8000_0001,
    });
    v.frame_type = VideoMetadataFrameType(i32::MIN);
    v.width = u16::MAX;
    v.height = 32768;
    v.rotation = VideoMetadataRotation(511);
    v.content_type.0 = u8::MAX;
    v.is_first_packet_in_frame = true;
    v.is_last_packet_in_frame = true;
    v.is_last_frame_in_picture = false;
    v.simulcast_idx = u8::MAX;
    v.codec = VideoCodecType::Vp9;
    v.playout_delay = Some(VideoPlayoutDelay::minimal());
    v.video_timing = VideoSendTiming {
        encode_start_delta_ms: 1,
        encode_finish_delta_ms: 2,
        packetization_finish_delta_ms: 3,
        pacer_exit_delta_ms: 4,
        network_timestamp_delta_ms: 5,
        network2_timestamp_delta_ms: 6,
        flags: 217,
    };
    v.color_space = Some(ColorSpace {
        primaries: 255,
        range: i32::MIN,
        chroma_siting_horizontal: i32::MAX,
        ..ColorSpace::default()
    });
    v.video_frame_tracking_id = Some(u16::MAX);
    let mut vp9 = RtpVideoHeaderVp9::default();
    vp9.num_spatial_layers = usize::MAX;
    vp9.gof.num_frames_in_gof = usize::MAX;
    vp9.gof.pid_start = 531;
    vp9.gof.pid_diff[254] = [1, 217, 255];
    v.video_type_header = RtpVideoTypeHeader::Vp9(Box::new(vp9));
    v.absolute_capture_time = Some(AbsoluteCaptureTime {
        absolute_capture_timestamp: u64::MAX,
        estimated_capture_clock_offset: Some(i64::MIN),
    });
    let mut instrumentation = FrameInstrumentationData::new();
    assert!(instrumentation.set_sequence_index(16383));
    assert!(instrumentation.set_sample_values(&[0.0, f64::NAN, 255.0]));
    v.frame_instrumentation_data = Some(instrumentation);
    header.set_values(&v).unwrap();
    let copy = header.clone();
    let mut assigned = RtpVideoHeader::new();
    assigned.assign_from(&header);
    header.set_values(&RtpVideoHeader::new().values()).unwrap();
    for snapshot in [copy.values(), assigned.values()] {
        assert_eq!(snapshot.generic, v.generic);
        assert_eq!(snapshot.frame_type, v.frame_type);
        assert_eq!(snapshot.width, v.width);
        assert_eq!(snapshot.height, v.height);
        assert_eq!(snapshot.rotation, v.rotation);
        assert_eq!(snapshot.content_type, v.content_type);
        assert!(snapshot.is_first_packet_in_frame && snapshot.is_last_packet_in_frame);
        assert!(!snapshot.is_last_frame_in_picture);
        assert_eq!(snapshot.simulcast_idx, v.simulcast_idx);
        assert_eq!(snapshot.codec, v.codec);
        assert_eq!(snapshot.playout_delay, v.playout_delay);
        assert_eq!(snapshot.video_timing, v.video_timing);
        assert_eq!(snapshot.color_space, v.color_space);
        assert_eq!(snapshot.video_frame_tracking_id, v.video_frame_tracking_id);
        assert_eq!(snapshot.video_type_header, v.video_type_header);
        assert_eq!(snapshot.absolute_capture_time, v.absolute_capture_time);
        let instrument = snapshot.frame_instrumentation_data.unwrap();
        assert_eq!(instrument.sequence_index(), 16383);
        let samples = instrument.sample_values();
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0], 0.0);
        assert!(samples[1].is_nan());
        assert_eq!(samples[2], 255.0);
    }
}

#[test]
fn native_subset_mapping_preserves_extras_and_resets_generic_extras() {
    let mut header = RtpVideoHeader::new();
    let mut v = header.values();
    v.generic = Some(RtpVideoGenericDescriptorInfo {
        frame_id: 7,
        dependencies: vec![4, 3],
        chain_diffs: vec![2, 1],
        active_decode_targets: 5,
        ..RtpVideoGenericDescriptorInfo::default()
    });
    v.video_type_header = RtpVideoTypeHeader::LegacyGeneric {
        picture_id: u16::MAX,
    };
    v.is_first_packet_in_frame = true;
    v.video_frame_tracking_id = Some(19);
    v.video_timing.flags = 127;
    header.set_values(&v).unwrap();
    let mut metadata = header.as_metadata().unwrap();
    assert_eq!(metadata.frame_id(), Some(7));
    assert_eq!(metadata.dependencies(), Some(vec![4, 3]));
    assert_eq!(metadata.ssrc(), 0);
    assert!(metadata.csrcs().is_empty());
    assert_eq!(
        metadata.codec_specifics(),
        pulsebeam_webrtc_sys::RtpVideoHeaderCodecSpecifics::None
    );
    metadata.set_dependencies(None);
    metadata.set_ssrc(999);
    metadata.set_csrcs(vec![1, 2]);
    header.set_from_metadata(&metadata).unwrap();
    let mapped = header.values();
    let generic = mapped.generic.as_ref().unwrap();
    assert_eq!(generic.frame_id, 7);
    assert!(generic.dependencies.is_empty());
    assert!(generic.chain_diffs.is_empty());
    assert_eq!(generic.active_decode_targets, u32::MAX);
    assert_eq!(mapped.video_type_header, v.video_type_header);
    assert!(mapped.is_first_packet_in_frame);
    assert_eq!(mapped.video_frame_tracking_id, Some(19));
    assert_eq!(mapped.video_timing.flags, 127);
    assert_eq!(header.as_metadata().unwrap().dependencies(), Some(vec![]));
    let fresh = RtpVideoHeader::from_metadata(&metadata).unwrap().values();
    assert_eq!(fresh.video_type_header, RtpVideoTypeHeader::None);
    assert!(!fresh.is_first_packet_in_frame);
    assert!(fresh.video_frame_tracking_id.is_none());
    assert_eq!(fresh.video_timing, VideoSendTiming::default());
    metadata.set_frame_id(None);
    header.set_from_metadata(&metadata).unwrap();
    assert!(header.values().generic.is_none());
}

#[test]
fn extreme_metadata_frame_ids_preserve_values_and_reset_chains() {
    let mut header = RtpVideoHeader::new();
    let mut values = header.values();
    values.generic = Some(RtpVideoGenericDescriptorInfo {
        frame_id: 17,
        chain_diffs: vec![1],
        active_decode_targets: 1,
        ..RtpVideoGenericDescriptorInfo::default()
    });
    header.set_values(&values).unwrap();
    let mut metadata = header.as_metadata().unwrap();
    metadata.set_frame_type(VideoMetadataFrameType::DELTA);
    for frame_id in [i64::MIN, i64::MAX] {
        metadata.set_frame_id(Some(frame_id));
        header.set_from_metadata(&metadata).unwrap();
        let updated = header.values().generic.unwrap();
        assert_eq!(updated.frame_id, frame_id);
        assert!(updated.chain_diffs.is_empty());
        assert_eq!(updated.active_decode_targets, u32::MAX);
        let fresh = RtpVideoHeader::from_metadata(&metadata)
            .unwrap()
            .values()
            .generic
            .unwrap();
        assert_eq!(fresh, updated);
        assert_eq!(header.as_metadata().unwrap().frame_id(), Some(frame_id));
    }
}

#[test]
fn association_checks_belong_to_subset_conversion_not_header_storage() {
    let mut header = RtpVideoHeader::new();
    let mut v = header.values();
    v.codec = VideoCodecType::Vp9;
    v.video_type_header = RtpVideoTypeHeader::LegacyGeneric { picture_id: 101 };
    header.set_values(&v).unwrap();
    assert!(matches!(
        header.as_metadata(),
        Err(VideoMetadataError::NativeConsumption(_))
    ));
    assert_eq!(header.values().video_type_header, v.video_type_header);
    let mut rejected = v.clone();
    rejected.rotation = VideoMetadataRotation(-1);
    rejected.video_frame_tracking_id = Some(3);
    assert_eq!(
        header.set_values(&rejected),
        Err(VideoMetadataError::NativeEnumRange("rotation"))
    );
    assert!(header.values().video_frame_tracking_id.is_none());
    assert_eq!(header.values().video_type_header, v.video_type_header);
    let mut metadata = VideoFrameMetadata::new();
    metadata.set_codec(VideoCodecType::Vp8).unwrap();
    assert!(matches!(
        header.set_from_metadata(&metadata),
        Err(VideoMetadataError::NativeConsumption(_))
    ));
    assert!(matches!(
        RtpVideoHeader::from_metadata(&metadata),
        Err(VideoMetadataError::NativeConsumption(_))
    ));
    assert_eq!(header.values().codec, VideoCodecType::Vp9);
    v.codec = VideoCodecType::Native(7);
    header.set_values(&v).unwrap();
    assert_eq!(
        header.as_metadata().unwrap().codec(),
        VideoCodecType::Native(7)
    );
}

use pulsebeam_webrtc_sys::{
    H264NaluInfo, H264PacketizationMode, H264PacketizationType, RtpVideoHeaderCodecSpecifics,
    RtpVideoHeaderH264, RtpVideoHeaderVp8, RtpVideoHeaderVp9, VideoCodecType, VideoFrameMetadata,
    VideoFrameMetadataValues, VideoMetadataContentType, VideoMetadataDecodeTargetIndication,
    VideoMetadataError, VideoMetadataFrameType, VideoMetadataRotation, Vp9GofInfo,
    Vp9TemporalStructureMode,
};

#[test]
fn common_metadata_defaults_optional_lists_and_copy() {
    let mut metadata = VideoFrameMetadata::new();
    let defaults = metadata.values();
    assert_eq!(defaults.frame_type, VideoMetadataFrameType::EMPTY);
    assert_eq!((defaults.width, defaults.height), (0, 0));
    assert_eq!(defaults.rotation, VideoMetadataRotation::NONE);
    assert_eq!(defaults.content_type, VideoMetadataContentType::UNSPECIFIED);
    assert_eq!(defaults.frame_id, None);
    assert_eq!((defaults.spatial_index, defaults.temporal_index), (0, 0));
    assert_eq!(defaults.dependencies, None);
    assert!(defaults.decode_target_indications.is_empty());
    assert!(defaults.is_last_frame_in_picture);
    assert_eq!(defaults.simulcast_idx, 0);
    assert_eq!(defaults.codec, VideoCodecType::Generic);
    assert_eq!(defaults.ssrc, 0);
    assert!(defaults.csrcs.is_empty());
    assert!(metadata.frame_dependencies().is_empty());

    metadata.set_frame_dependencies(&[]);
    assert_eq!(metadata.dependencies(), Some(vec![]));
    let copy = metadata.clone();
    metadata.set_dependencies(None);
    assert_eq!(metadata.dependencies(), None);
    assert_eq!(copy.dependencies(), Some(vec![]));
    metadata.assign_from(&copy);
    assert_eq!(metadata.dependencies(), Some(vec![]));
}

#[test]
fn common_metadata_preserves_native_value_domains_and_order() {
    let fields = VideoFrameMetadataValues {
        frame_type: VideoMetadataFrameType(i32::MIN),
        width: u16::MAX,
        height: 32768,
        rotation: VideoMetadataRotation(511),
        content_type: VideoMetadataContentType(u8::MAX),
        frame_id: Some(i64::MIN),
        spatial_index: i32::MIN,
        temporal_index: i32::MAX,
        dependencies: Some(vec![i64::MAX, i64::MIN, 0, i64::MAX]),
        decode_target_indications: vec![
            VideoMetadataDecodeTargetIndication(i32::MIN),
            VideoMetadataDecodeTargetIndication::REQUIRED,
            VideoMetadataDecodeTargetIndication(i32::MAX),
        ],
        is_last_frame_in_picture: false,
        simulcast_idx: u8::MAX,
        codec: VideoCodecType::Native(7),
        ssrc: u32::MAX,
        csrcs: vec![0, u32::MAX, 0],
    };
    let mut metadata = VideoFrameMetadata::new();
    metadata.set_values(&fields).unwrap();
    assert_eq!(metadata.values(), fields);
    assert_eq!(metadata.clone().values(), fields);
    let mut assigned = VideoFrameMetadata::new();
    assigned.assign_from(&metadata);
    metadata.set_dependencies(Some(vec![7]));
    metadata.set_csrcs(vec![1]);
    assert_eq!(assigned.values(), fields);
}

#[test]
fn codec_specific_metadata_preserves_full_storage_and_independent_codec() {
    let mut vp8 = RtpVideoHeaderVp8::default();
    vp8.non_reference = true;
    vp8.picture_id = i16::MIN;
    vp8.tl0_pic_idx = i16::MAX;
    vp8.temporal_idx = 217;
    vp8.layer_sync = true;
    vp8.key_idx = i32::MIN;
    vp8.partition_id = i32::MAX;
    vp8.beginning_of_partition = true;

    let mut vp9 = RtpVideoHeaderVp9::default();
    vp9.inter_pic_predicted = true;
    vp9.flexible_mode = true;
    vp9.beginning_of_frame = true;
    vp9.end_of_frame = true;
    vp9.ss_data_available = false;
    vp9.non_ref_for_inter_layer_pred = true;
    vp9.picture_id = i16::MIN;
    vp9.max_picture_id = i16::MAX;
    vp9.tl0_pic_idx = -1234;
    vp9.temporal_idx = 219;
    vp9.spatial_idx = 220;
    vp9.temporal_up_switch = true;
    vp9.inter_layer_predicted = true;
    vp9.gof_idx = 221;
    // Full metadata domains; these counts are not safe for all consumers.
    vp9.num_ref_pics = u8::MAX;
    vp9.pid_diff = [0, u8::MAX, 17];
    vp9.ref_picture_id = [i16::MIN, i16::MAX, -27];
    vp9.num_spatial_layers = usize::MAX;
    vp9.first_active_layer = usize::MAX - 1;
    vp9.spatial_layer_resolution_present = true;
    vp9.width = std::array::from_fn(|i| 60000 + i as u16);
    vp9.height = std::array::from_fn(|i| 50000 + i as u16);
    vp9.gof.num_frames_in_gof = usize::MAX;
    vp9.gof.temporal_idx = std::array::from_fn(|i| i as u8);
    vp9.gof.temporal_up_switch = std::array::from_fn(|i| i % 2 == 0);
    vp9.gof.num_ref_pics = std::array::from_fn(|i| 255 - i as u8);
    vp9.gof.pid_diff = std::array::from_fn(|i| [i as u8, 255 - i as u8, 91]);
    vp9.gof.pid_start = u16::MAX;
    vp9.end_of_picture = false;
    let h264 = RtpVideoHeaderH264 {
        nalu_type: u8::MAX,
        packetization_type: H264PacketizationType(3),
        nalus: vec![
            H264NaluInfo {
                type_code: 255,
                sps_id: i32::MIN,
                pps_id: i32::MAX,
            },
            H264NaluInfo {
                type_code: 0,
                sps_id: -7,
                pps_id: 22,
            },
        ],
        packetization_mode: H264PacketizationMode(i32::MIN),
    };
    let mut metadata = VideoFrameMetadata::new();
    for specific in [
        RtpVideoHeaderCodecSpecifics::Vp8(vp8),
        RtpVideoHeaderCodecSpecifics::Vp9(Box::new(vp9)),
        RtpVideoHeaderCodecSpecifics::H264(h264),
        RtpVideoHeaderCodecSpecifics::None,
    ] {
        metadata.set_codec_specifics(&specific).unwrap();
        assert_eq!(metadata.codec(), VideoCodecType::Generic);
        assert_eq!(metadata.codec_specifics(), specific);
        let copy = metadata.clone();
        let mut assigned = VideoFrameMetadata::new();
        assigned.assign_from(&metadata);
        metadata.set_ssrc(19);
        metadata.set_dependencies(Some(vec![]));
        assert_eq!(metadata.codec_specifics(), specific);
        assert_eq!(copy.codec_specifics(), specific);
        assert_eq!(assigned.codec_specifics(), specific);
    }
}

#[test]
fn native_header_init_and_gof_operations_preserve_untouched_storage() {
    let mut vp8 = RtpVideoHeaderVp8::default();
    vp8.partition_id = 99;
    vp8.init();
    assert_eq!(vp8, RtpVideoHeaderVp8::default());
    assert!(vp8.sdk_equals(&vp8.clone()));
    let mut vp9 = RtpVideoHeaderVp9::default();
    vp9.width[7] = 17;
    vp9.pid_diff[2] = 18;
    vp9.spatial_layer_resolution_present = true;
    vp9.gof.pid_start = 19;
    vp9.gof.pid_diff[254] = [20, 21, 22];
    vp9.picture_id = 27;
    vp9.init();
    assert_eq!(vp9.picture_id, RtpVideoHeaderVp9::default().picture_id);
    assert_eq!(vp9.width[7], 17);
    assert_eq!(vp9.pid_diff[2], 18);
    assert!(vp9.spatial_layer_resolution_present);
    assert_eq!(vp9.gof.pid_start, 19);
    assert_eq!(vp9.gof.pid_diff[254], [20, 21, 22]);

    let mut target = Vp9GofInfo::default();
    target.num_frames_in_gof = usize::MAX;
    target.pid_start = 123;
    target.pid_diff[0] = [51, 52, 53];
    target.temporal_idx[254] = 211;
    let mut source = Vp9GofInfo::default();
    source.num_frames_in_gof = 1;
    source.pid_start = 456;
    source.temporal_idx[0] = 9;
    source.temporal_up_switch[0] = true;
    source.num_ref_pics[0] = 2;
    source.pid_diff[0] = [11, 12, 13];
    target.copy_prefix_from(&source).unwrap();
    assert_eq!(target.num_frames_in_gof, 1);
    assert_eq!(target.pid_start, 123);
    assert_eq!(target.temporal_idx[0], 9);
    assert!(target.temporal_up_switch[0]);
    assert_eq!(target.num_ref_pics[0], 2);
    assert_eq!(target.pid_diff[0], [11, 12, 53]);
    assert_eq!(target.temporal_idx[254], 211);
    let before = target.clone();
    source.num_frames_in_gof = 256;
    assert!(matches!(
        target.copy_prefix_from(&source),
        Err(VideoMetadataError::NativeConsumption(_))
    ));
    assert_eq!(target, before);
    source.num_frames_in_gof = 1;
    source.num_ref_pics[0] = 4;
    assert!(matches!(
        target.copy_prefix_from(&source),
        Err(VideoMetadataError::NativeConsumption(_))
    ));
    assert_eq!(target, before);
    target
        .set_structure(Vp9TemporalStructureMode::THREE_LAYERS)
        .unwrap();
    assert_eq!(target.num_frames_in_gof, 4);
    assert_eq!(&target.temporal_idx[..4], &[0, 2, 1, 2]);
    assert_eq!(target.pid_diff[0], [4, 12, 53]);
    assert_eq!(target.pid_start, 123);
    assert_eq!(target.temporal_idx[254], 211);
}

#[test]
fn native_equality_consumes_only_its_actual_selected_prefixes() {
    let mut a = RtpVideoHeaderVp9::default();
    let mut b = a.clone();
    a.num_spatial_layers = usize::MAX;
    a.gof.num_frames_in_gof = usize::MAX;
    b.num_spatial_layers = 0;
    b.gof.num_frames_in_gof = 0;
    a.width[7] = 9;
    assert_ne!(a, b);
    assert_eq!(a.sdk_equals(&b), Ok(true)); // SS absent: ignore all SS fields.
    a.num_ref_pics = 255;
    b.num_ref_pics = 255;
    a.pid_diff[0] = 1;
    assert_eq!(a.sdk_equals(&b), Ok(false)); // short circuit before bad count
    b.pid_diff[0] = 1;
    assert!(matches!(
        a.sdk_equals(&b),
        Err(VideoMetadataError::NativeConsumption(_))
    ));
    b.picture_id += 1;
    assert_eq!(a.sdk_equals(&b), Ok(false)); // scalar short circuit

    let mut meta_a = VideoFrameMetadata::new();
    let mut meta_b = VideoFrameMetadata::new();
    meta_a
        .set_codec_specifics(&RtpVideoHeaderCodecSpecifics::Vp9(Box::new(a.clone())))
        .unwrap();
    meta_b
        .set_codec_specifics(&RtpVideoHeaderCodecSpecifics::Vp9(Box::new(a.clone())))
        .unwrap();
    meta_b.set_ssrc(111);
    assert!(matches!(
        meta_a.equals(&meta_b),
        Err(VideoMetadataError::NativeConsumption(_))
    ));
    meta_b.set_frame_type(VideoMetadataFrameType::KEY);
    assert_eq!(meta_a.equals(&meta_b), Ok(false));
    let mut other = VideoFrameMetadata::new();
    assert_eq!(other.equals(&VideoFrameMetadata::new()), Ok(true));
    other.set_dependencies(Some(vec![]));
    assert_eq!(other.equals(&VideoFrameMetadata::new()), Ok(false));

    let mut gof = Vp9GofInfo::default();
    gof.num_frames_in_gof = usize::MAX;
    let mut other_gof = gof.clone();
    other_gof.pid_start = 1;
    assert_eq!(gof.sdk_equals(&other_gof), Ok(false));
    other_gof.pid_start = 0;
    other_gof.temporal_idx[0] = 1;
    assert_eq!(gof.sdk_equals(&other_gof), Ok(false));
    other_gof.temporal_idx[0] = 0;
    assert!(matches!(
        gof.sdk_equals(&other_gof),
        Err(VideoMetadataError::NativeConsumption(_))
    ));
    gof.num_frames_in_gof = 1;
    other_gof.num_frames_in_gof = 1;
    gof.num_ref_pics[0] = 255;
    other_gof.num_ref_pics[0] = 255;
    assert!(matches!(
        gof.sdk_equals(&other_gof),
        Err(VideoMetadataError::NativeConsumption(_))
    ));
    other_gof.num_ref_pics[0] = 0;
    assert_eq!(gof.sdk_equals(&other_gof), Ok(false));

    let mut spatial = RtpVideoHeaderVp9::default();
    spatial.ss_data_available = true;
    spatial.num_spatial_layers = usize::MAX;
    assert_eq!(spatial.sdk_equals(&spatial), Ok(true)); // resolution absent
    spatial.spatial_layer_resolution_present = true;
    assert!(matches!(
        spatial.sdk_equals(&spatial),
        Err(VideoMetadataError::NativeConsumption(_))
    ));
    let mut spatial_other = spatial.clone();
    spatial_other.width[0] = 1;
    assert_eq!(spatial.sdk_equals(&spatial_other), Ok(false));
    spatial_other.width[0] = 0;
    spatial_other.gof.pid_start = 1;
    assert_eq!(spatial.sdk_equals(&spatial_other), Ok(false));
}

#[test]
fn h264_non_fixed_enum_rejection_does_not_mutate_metadata() {
    let mut metadata = VideoFrameMetadata::new();
    let header = RtpVideoHeaderH264::default();
    metadata
        .set_codec_specifics(&RtpVideoHeaderCodecSpecifics::H264(header.clone()))
        .unwrap();
    for invalid in [-1, 4, i32::MIN, i32::MAX] {
        let mut rejected = header.clone();
        rejected.packetization_type = H264PacketizationType(invalid);
        assert_eq!(
            metadata.set_codec_specifics(&RtpVideoHeaderCodecSpecifics::H264(rejected)),
            Err(VideoMetadataError::NativeEnumRange(
                "H264 packetization type"
            ))
        );
        assert_eq!(
            metadata.codec_specifics(),
            RtpVideoHeaderCodecSpecifics::H264(header.clone())
        );
    }
    assert_eq!(header.sdk_equals(&header), Ok(true));
}

#[test]
fn non_fixed_enum_representation_rejection_is_atomic() {
    let mut metadata = VideoFrameMetadata::new();
    metadata.set_ssrc(42);
    let before = metadata.values();
    for rotation in [-1, 512, i32::MIN, i32::MAX] {
        let mut rejected = before.clone();
        rejected.rotation = VideoMetadataRotation(rotation);
        rejected.ssrc = 99;
        assert_eq!(
            metadata.set_values(&rejected),
            Err(VideoMetadataError::NativeEnumRange("rotation"))
        );
        assert_eq!(metadata.values(), before);
    }
    for codec in [-1, 8, i32::MIN, i32::MAX] {
        let mut rejected = before.clone();
        rejected.codec = VideoCodecType::Native(codec);
        rejected.ssrc = 99;
        assert_eq!(
            metadata.set_values(&rejected),
            Err(VideoMetadataError::NativeEnumRange("codec"))
        );
        assert_eq!(metadata.values(), before);
    }
}

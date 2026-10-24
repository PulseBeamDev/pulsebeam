use super::*;
use crate::RtpReceivedPacketInfo;

fn round_trip(value: &RtpHeaderValues) -> RtpHeaderValues {
    let mut valid = false;
    let output = ffi::media_rtp_header_round_trip(&value.to_native(), &mut valid);
    assert!(valid);
    RtpHeaderValues::from_native(output)
}

#[test]
fn initialized_owned_header_default_round_trips() {
    // Rust's owned input initializes every raw field. A bare SDK RTPHeader
    // constructor leaves timing deltas uninitialized, so it is not an
    // observable complete default and must never be serialized into Rust.
    let header = RtpHeaderValues::default();
    assert!(!header.extension.has_video_timing);
    assert_eq!(header.extension.video_timing, VideoSendTiming::default());
    for _ in 0..256 {
        assert_eq!(round_trip(&header), header);
    }
}

#[test]
fn full_native_header_graph_is_not_a_wire_round_trip() {
    let mut header = RtpHeaderValues {
        marker: true,
        payload_type: u8::MAX,
        sequence_number: u16::MAX,
        timestamp: u32::MAX,
        ssrc: u32::MAX,
        csrc_count: u8::MAX,
        csrcs: std::array::from_fn(|i| i as u32 + 1),
        padding_length: usize::MAX,
        header_length: usize::MAX,
        extension: RtpHeaderExtensionValues {
            // False presence flags must not erase their independently stored values.
            transmission_time_offset: i32::MIN,
            absolute_send_time: u32::MAX,
            absolute_capture_time: Some(AbsoluteCaptureTime {
                absolute_capture_timestamp: u64::MAX,
                estimated_capture_clock_offset: Some(i64::MIN),
            }),
            transport_sequence_number: u16::MAX,
            feedback_request: Some(RtpFeedbackRequest {
                include_timestamps: true,
                sequence_count: i32::MIN,
            }),
            audio_level: Some(AudioLevel::new(true, 127).unwrap()),
            video_rotation: 511,
            video_content_type: u8::MAX,
            video_timing: VideoSendTiming {
                flags: 0x81,
                encode_start_delta_ms: 1,
                encode_finish_delta_ms: 2,
                packetization_finish_delta_ms: 3,
                pacer_exit_delta_ms: 4,
                network_timestamp_delta_ms: 5,
                network2_timestamp_delta_ms: u16::MAX,
            },
            playout_delay: VideoPlayoutDelay::new(
                TimeDelta::from_microseconds(7).unwrap(),
                TimeDelta::from_microseconds(19).unwrap(),
            ),
            stream_id: vec![0, 0xff, 0x80],
            repaired_stream_id: vec![0xfe, 0],
            mid: vec![0x81],
            color_space: Some(ColorSpace {
                primaries: 255,
                transfer: 255,
                matrix: 255,
                range: i32::MIN,
                chroma_siting_horizontal: i32::MAX,
                chroma_siting_vertical: -1,
                hdr_metadata: Some(HdrMetadata {
                    mastering_metadata: HdrMasteringMetadata {
                        primary_r: HdrChromaticity { x: -0.0, y: 2.0 },
                        luminance_max: -1.0,
                        ..Default::default()
                    },
                    max_content_light_level: i32::MIN,
                    max_frame_average_light_level: i32::MAX,
                }),
            }),
            ..Default::default()
        },
    };
    assert_eq!(round_trip(&header), header);
    header.extension.has_transmission_time_offset = true;
    header.extension.has_absolute_send_time = true;
    header.extension.has_transport_sequence_number = true;
    header.extension.has_video_rotation = true;
    header.extension.has_video_content_type = true;
    header.extension.has_video_timing = true;
    assert_eq!(round_trip(&header), header);
    let info = RtpReceivedPacketInfo::from_header(&header, Timestamp::minus_infinity()).unwrap();
    assert_eq!(info.sequence_number, u16::MAX);
    assert_eq!(info.ssrc, u32::MAX);
    assert_eq!(info.csrcs, header.csrcs.to_vec());
    assert_eq!(info.receive_time, Timestamp::minus_infinity());
    assert_eq!(info.audio_level, Some(127));
    assert_eq!(
        info.absolute_capture_time,
        header.extension.absolute_capture_time
    );
    assert_eq!(info.local_capture_clock_offset, None);
}

#[test]
fn hdr_nan_infinity_and_negative_zero_bits_are_retained() {
    let hdr = HdrMetadata {
        mastering_metadata: HdrMasteringMetadata {
            primary_r: HdrChromaticity {
                x: f32::from_bits(0x7fc01234),
                y: -0.0,
            },
            luminance_max: f32::INFINITY,
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(!hdr.validate());
    let header = RtpHeaderValues {
        extension: RtpHeaderExtensionValues {
            color_space: Some(ColorSpace {
                hdr_metadata: Some(hdr),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let output = round_trip(&header)
        .extension
        .color_space
        .unwrap()
        .hdr_metadata
        .unwrap();
    assert_eq!(output.mastering_metadata.primary_r.x.to_bits(), 0x7fc01234);
    assert_eq!(
        output.mastering_metadata.primary_r.y.to_bits(),
        (-0.0f32).to_bits()
    );
    assert_eq!(output.mastering_metadata.luminance_max, f32::INFINITY);
    assert!(HdrMetadata::default().validate());
    assert!(
        !HdrChromaticity {
            x: f32::NAN,
            y: 0.0
        }
        .validate()
    );
}

#[test]
fn constructor_clamping_and_reject_without_mutation_are_distinct() {
    let max = VideoPlayoutDelay::MAXIMUM;
    let mut delay = VideoPlayoutDelay::new(TimeDelta::plus_infinity(), TimeDelta::minus_infinity());
    assert_eq!(delay.minimum(), max);
    assert_eq!(delay.maximum(), max);
    let before = delay;
    assert!(!delay.set(TimeDelta::plus_infinity(), max));
    assert_eq!(delay, before);
    assert!(delay.set(TimeDelta::zero(), TimeDelta::from_microseconds(1).unwrap()));
    assert_eq!(delay.maximum().microseconds().unwrap(), 1);
    let mut color = ColorSpace::default();
    assert!(!color.set_primaries_from_uint8(255));
    assert_eq!(color, ColorSpace::default());
    assert!(color.set_primaries_from_uint8(22));
    assert!(color.set_matrix_from_uint8(0));
    assert!(color.set_transfer_from_uint8(18));
    assert!(color.set_range_from_uint8(3));
    assert!(color.set_chroma_siting_horizontal_from_uint8(2));
    assert!(color.set_chroma_siting_vertical_from_uint8(1));
    assert!(color.as_string().contains("kJEDECP22"));
    assert!(AudioLevel::new(false, -1).is_err());
    assert!(AudioLevel::new(false, 128).is_err());
}

#[test]
fn timing_overloads_keep_native_rounding_and_saturation() {
    assert_eq!(VideoSendTiming::delta_capped_ms(5, 4).unwrap(), 0);
    assert_eq!(
        VideoSendTiming::delta_capped_ms(0, 65_535).unwrap(),
        u16::MAX
    );
    assert_eq!(
        VideoSendTiming::delta_capped_ms(0, i64::MAX).unwrap(),
        u16::MAX
    );
    assert_eq!(VideoSendTiming::delta_capped_ms(0, i64::MIN).unwrap(), 0);
    assert!(VideoSendTiming::delta_capped_ms(-1, i64::MAX).is_err());
    assert!(VideoSendTiming::delta_capped_ms(1, i64::MIN).is_err());
    for (us, expected) in [
        (-1500, 0),
        (499, 0),
        (500, 1),
        (1500, 2),
        (65_535_500, u16::MAX),
    ] {
        assert_eq!(
            VideoSendTiming::duration_capped_ms(TimeDelta::from_microseconds(us).unwrap()).unwrap(),
            expected
        );
    }
    assert!(VideoSendTiming::duration_capped_ms(TimeDelta::plus_infinity()).is_err());
    assert!(VideoSendTiming::duration_capped_ms(TimeDelta::minus_infinity()).is_err());
}

#[test]
fn raw_playout_conversion_delegates_native_constructor_clamping() {
    let mut input = RtpHeaderValues::default().to_native();
    input.extension.playout_delay.minimum_us = i64::MAX;
    input.extension.playout_delay.maximum_us = i64::MIN;
    let mut valid = false;
    let output = ffi::media_rtp_header_round_trip(&input, &mut valid);
    assert!(valid);
    let output = RtpHeaderValues::from_native(output);
    assert_eq!(
        output.extension.playout_delay.minimum(),
        VideoPlayoutDelay::MAXIMUM
    );
    assert_eq!(
        output.extension.playout_delay.maximum(),
        VideoPlayoutDelay::MAXIMUM
    );
}

#[test]
fn unsupported_native_getter_and_enum_cast_inputs_are_errors() {
    let mut header = RtpHeaderValues::default();
    assert_eq!(
        header.extension.absolute_send_timestamp(),
        Err(RtpValueError::InvalidAbsoluteSendTime)
    );
    header.extension.has_absolute_send_time = true;
    header.extension.absolute_send_time = (1 << 18) + 1;
    assert_eq!(
        header
            .extension
            .absolute_send_timestamp()
            .unwrap()
            .microseconds()
            .unwrap(),
        1_000_003
    );
    header.extension.absolute_send_time = 1 << 24;
    assert_eq!(
        header.extension.absolute_send_timestamp(),
        Err(RtpValueError::InvalidAbsoluteSendTime)
    );
    for rotation in [-1, 512, i32::MAX] {
        header.extension.video_rotation = rotation;
        let mut valid = true;
        let _ = ffi::media_rtp_header_round_trip(&header.to_native(), &mut valid);
        assert!(!valid);
        assert_eq!(
            RtpReceivedPacketInfo::from_header(&header, Timestamp::zero()),
            Err(RtpValueError::InvalidRotation)
        );
    }
}

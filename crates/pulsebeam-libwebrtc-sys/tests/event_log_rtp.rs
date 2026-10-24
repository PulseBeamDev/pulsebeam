use pulsebeam_webrtc_sys::*;

fn packet(uri: &str, extension: &[u8]) -> (Vec<u8>, Vec<RtcEventRtpExtension>) {
    // Two-byte extensions also accommodate the 28-byte HDR color-space value.
    let mut bytes = vec![0x90, 96, 0, 7, 0, 0, 0, 19, 0, 0, 0, 23];
    let size = (2 + extension.len()).div_ceil(4) * 4;
    bytes.extend_from_slice(&[0x10, 0, (size / 4 >> 8) as u8, (size / 4) as u8]);
    bytes.extend_from_slice(&[1, extension.len() as u8]);
    bytes.extend_from_slice(extension);
    bytes.resize(16 + size, 0);
    bytes.push(0xbb);
    let map = vec![RtcEventRtpExtension {
        id: 1,
        uri: uri.into(),
        encrypted: false,
    }];
    (bytes, map)
}

fn assert_extension(
    kind: RtcEventRtpExtensionKind,
    uri: &str,
    raw: &[u8],
    decoded: RtcEventDecodedRtpExtension,
) {
    let (bytes, map) = packet(uri, raw);
    let incoming = RtcEvent::rtp_incoming(&bytes, &map, None).unwrap();
    let outgoing = RtcEvent::rtp_outgoing(&bytes, &map, -1, None).unwrap();
    let expected = Some(RtcEventRtpExtensionSnapshot {
        kind,
        raw: raw.to_vec(),
        decoded: Some(decoded),
    });
    assert_eq!(incoming.rtp_extension(kind), expected);
    assert_eq!(outgoing.rtp_extension(kind), expected);
    let copy = outgoing.try_clone().unwrap();
    drop((incoming, outgoing, bytes, map));
    assert_eq!(copy.rtp_extension(kind), expected);
    std::thread::spawn(move || assert_eq!(copy.rtp_extension(kind), expected))
        .join()
        .unwrap();
}

#[test]
fn native_scalar_and_optional_extension_values_survive_packet_copy() {
    use RtcEventDecodedRtpExtension as D;
    use RtcEventRtpExtensionKind as K;
    assert_extension(
        K::AbsoluteSendTime,
        "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
        &[1, 2, 3],
        D::AbsoluteSendTime(0x010203),
    );
    assert_extension(
        K::AbsoluteCaptureTime,
        "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time",
        &[0, 0, 0, 0, 0, 0, 0, 7],
        D::AbsoluteCaptureTime(RtcEventAbsoluteCaptureTime {
            absolute_capture_timestamp: 7,
            estimated_capture_clock_offset: None,
        }),
    );
    assert_extension(
        K::AbsoluteCaptureTime,
        "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time",
        &[
            0, 0, 0, 0, 0, 0, 0, 7, 255, 255, 255, 255, 255, 255, 255, 249,
        ],
        D::AbsoluteCaptureTime(RtcEventAbsoluteCaptureTime {
            absolute_capture_timestamp: 7,
            estimated_capture_clock_offset: Some(-7),
        }),
    );
    assert_extension(
        K::AudioLevel,
        "urn:ietf:params:rtp-hdrext:ssrc-audio-level",
        &[0x85],
        D::AudioLevel(RtcEventAudioLevel {
            voice_activity: true,
            level: 5,
        }),
    );
    assert_extension(
        K::CsrcAudioLevels,
        "urn:ietf:params:rtp-hdrext:csrc-audio-level",
        &[7, 127],
        D::CsrcAudioLevels(vec![7, 127]),
    );
    assert_extension(
        K::TransmissionOffset,
        "urn:ietf:params:rtp-hdrext:toffset",
        &[255, 255, 249],
        D::TransmissionOffset(-7),
    );
    assert_extension(
        K::TransportSequenceNumber,
        "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
        &[0x12, 0x34],
        D::TransportSequenceNumber(0x1234),
    );
    assert_extension(
        K::TransportSequenceNumberV2,
        "http://www.webrtc.org/experiments/rtp-hdrext/transport-wide-cc-02",
        &[0, 7],
        D::TransportSequenceNumberV2 {
            sequence_number: 7,
            feedback_request: None,
        },
    );
    assert_extension(
        K::TransportSequenceNumberV2,
        "http://www.webrtc.org/experiments/rtp-hdrext/transport-wide-cc-02",
        &[0, 7, 0x80, 2],
        D::TransportSequenceNumberV2 {
            sequence_number: 7,
            feedback_request: Some(RtcEventFeedbackRequest {
                include_timestamps: true,
                sequence_count: 2,
            }),
        },
    );
    assert_extension(
        K::VideoOrientation,
        "urn:3gpp:video-orientation",
        &[3],
        D::VideoOrientation {
            rotation_degrees: 270,
        },
    );
    assert_extension(
        K::PlayoutDelay,
        "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay",
        &[0, 0x10, 2],
        D::PlayoutDelay {
            minimum_ms: 10,
            maximum_ms: 20,
        },
    );
    assert_extension(
        K::VideoContentType,
        "http://www.webrtc.org/experiments/rtp-hdrext/video-content-type",
        &[0x21],
        D::VideoContentType(1),
    );
    assert_extension(
        K::VideoTiming,
        "http://www.webrtc.org/experiments/rtp-hdrext/video-timing",
        &[3, 0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6],
        D::VideoTiming(RtcEventVideoTiming {
            flags: 3,
            encode_start_delta_ms: 1,
            encode_finish_delta_ms: 2,
            packetization_finish_delta_ms: 3,
            pacer_exit_delta_ms: 4,
            network_timestamp_delta_ms: 5,
            network2_timestamp_delta_ms: 6,
        }),
    );
    assert_extension(
        K::ColorSpace,
        "http://www.webrtc.org/experiments/rtp-hdrext/color-space",
        &[1, 1, 1, 0x26],
        D::ColorSpace(RtcEventColorSpace {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            range: 2,
            chroma_siting_horizontal: 1,
            chroma_siting_vertical: 2,
            hdr_metadata: None,
        }),
    );
    assert_extension(
        K::RtpStreamId,
        "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id",
        &[b'a', 255],
        D::RtpStreamId(vec![b'a', 255]),
    );
    assert_extension(
        K::RepairedRtpStreamId,
        "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id",
        b"repair",
        D::RepairedRtpStreamId(b"repair".to_vec()),
    );
    assert_extension(
        K::Mid,
        "urn:ietf:params:rtp-hdrext:sdes:mid",
        b"mid",
        D::Mid(b"mid".to_vec()),
    );
    assert_extension(
        K::InbandComfortNoise,
        "http://www.webrtc.org/experiments/rtp-hdrext/inband-cn",
        &[0x80],
        D::InbandComfortNoise(None),
    );
    assert_extension(
        K::InbandComfortNoise,
        "http://www.webrtc.org/experiments/rtp-hdrext/inband-cn",
        &[0],
        D::InbandComfortNoise(Some(0)),
    );
    assert_extension(
        K::VideoFrameTrackingId,
        "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-tracking-id",
        &[0xff, 0xff],
        D::VideoFrameTrackingId(u16::MAX),
    );
}

#[test]
fn present_malformed_extension_is_not_confused_with_absence_or_wrong_event_type() {
    use RtcEventRtpExtensionKind as K;
    let (bytes, map) = packet(
        "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
        &[1, 2],
    );
    let event = RtcEvent::rtp_incoming(&bytes, &map, None).unwrap();
    assert_eq!(
        event.rtp_extension(K::AbsoluteSendTime),
        Some(RtcEventRtpExtensionSnapshot {
            kind: K::AbsoluteSendTime,
            raw: vec![1, 2],
            decoded: None,
        })
    );
    assert_eq!(event.rtp_extension(K::TransmissionOffset), None);
    let unmapped = RtcEvent::rtp_incoming(&bytes, &[], None).unwrap();
    assert_eq!(unmapped.rtp_extension(K::AbsoluteSendTime), None);
    let ordinary = RtcEvent::from_data(RtcEventData::AlrState { in_alr: true }).unwrap();
    assert_eq!(ordinary.rtp_extension(K::AbsoluteSendTime), None);
}

#[test]
fn hdr_mastering_and_light_levels_are_native_normalized_fields() {
    use RtcEventRtpExtensionKind as K;
    let mut raw = vec![9, 16, 9, 0x10];
    // Native wire order: max/min luminance, four xy pairs (1/50000), CLL/FALL.
    for value in [
        1_000u16, 1, 25_000, 12_500, 5_000, 10_000, 15_000, 20_000, 10_000, 15_000, 2_000, 1_000,
    ] {
        raw.extend_from_slice(&value.to_be_bytes());
    }
    let (bytes, map) = packet(
        "http://www.webrtc.org/experiments/rtp-hdrext/color-space",
        &raw,
    );
    let event = RtcEvent::rtp_incoming(&bytes, &map, None).unwrap();
    let extension = event.rtp_extension(K::ColorSpace).unwrap();
    assert_eq!(extension.raw, raw);
    let Some(RtcEventDecodedRtpExtension::ColorSpace(color)) = extension.decoded else {
        panic!("parsed color space")
    };
    assert_eq!(
        (color.primaries, color.transfer, color.matrix, color.range),
        (9, 16, 9, 1)
    );
    let hdr = color.hdr_metadata.unwrap();
    assert_eq!(hdr.primary_r, RtcEventChromaticity { x: 0.5, y: 0.25 });
    assert!((hdr.primary_g.x - 0.1).abs() < 0.00001);
    assert!((hdr.primary_b.y - 0.4).abs() < 0.00001);
    assert_eq!(hdr.luminance_max, 1_000.0);
    assert!((hdr.luminance_min - 0.0001).abs() < 0.000001);
    assert_eq!(
        (
            hdr.max_content_light_level,
            hdr.max_frame_average_light_level
        ),
        (2_000, 1_000)
    );

    // Preserve an invalid HDR payload as present raw bytes, not invented
    // metadata or a missing extension. Native validation rejects >20000 nits.
    raw[4..6].copy_from_slice(&u16::MAX.to_be_bytes());
    let (bytes, map) = packet(
        "http://www.webrtc.org/experiments/rtp-hdrext/color-space",
        &raw,
    );
    let invalid = RtcEvent::rtp_incoming(&bytes, &map, None).unwrap();
    assert_eq!(
        invalid.rtp_extension(K::ColorSpace),
        Some(RtcEventRtpExtensionSnapshot {
            kind: K::ColorSpace,
            raw,
            decoded: None,
        })
    );
}

#[test]
fn additional_native_traits_preserve_defined_fields_and_presence() {
    use RtcEventDecodedRtpExtension as D;
    use RtcEventRtpExtensionKind as K;
    let generic = "http://www.webrtc.org/experiments/rtp-hdrext/generic-frame-descriptor-00";
    assert_extension(
        K::GenericFrameDescriptor,
        generic,
        &[0x40],
        D::GenericFrameDescriptor(RtcEventGenericFrameDescriptor {
            first_packet_in_subframe: false,
            last_packet_in_subframe: true,
            start: None,
        }),
    );
    assert_extension(
        K::GenericFrameDescriptor,
        generic,
        &[0xf2, 4, 0x34, 0x12, 2, 0x80, 1, 0x68],
        D::GenericFrameDescriptor(RtcEventGenericFrameDescriptor {
            first_packet_in_subframe: true,
            last_packet_in_subframe: true,
            start: Some(RtcEventGenericFrameStart {
                temporal_layer: 2,
                spatial_layer: 2,
                spatial_layers_bitmask: 4,
                width: 640,
                height: 360,
                frame_id: 0x1234,
                frame_dependency_diffs: vec![],
            }),
        }),
    );
    let allocation = "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00";
    assert_extension(
        K::VideoLayersAllocation,
        allocation,
        &[0],
        D::VideoLayersAllocation(RtcEventVideoLayersAllocation {
            rtp_stream_index: 0,
            resolution_and_frame_rate_valid: true,
            active_spatial_layers: vec![],
        }),
    );
    for (raw, resolution) in [
        (vec![1, 0, 100], None),
        (
            vec![1, 0, 100, 2, 0x7f, 1, 0x67, 30],
            Some(RtcEventVideoAllocationResolution {
                width: 640,
                height: 360,
                frame_rate_fps: 30,
            }),
        ),
    ] {
        assert_extension(
            K::VideoLayersAllocation,
            allocation,
            &raw,
            D::VideoLayersAllocation(RtcEventVideoLayersAllocation {
                rtp_stream_index: 0,
                resolution_and_frame_rate_valid: resolution.is_some(),
                active_spatial_layers: vec![RtcEventVideoAllocationLayer {
                    rtp_stream_index: 0,
                    spatial_id: 0,
                    target_bitrate_per_temporal_layer: vec![RtcEventDataRate::BitsPerSecond(
                        100_000,
                    )],
                    resolution_and_frame_rate: resolution,
                }],
            }),
        );
    }
    let corruption = "http://www.webrtc.org/experiments/rtp-hdrext/corruption-detection";
    assert_extension(
        K::CorruptionDetection,
        corruption,
        &[0x85],
        D::CorruptionDetection(RtcEventCorruptionDetection {
            sequence_index: 5,
            interpret_sequence_index_as_most_significant_bits: true,
            standard_deviation: 0.0,
            luma_error_threshold: 0,
            chroma_error_threshold: 0,
            sample_values: vec![],
        }),
    );
    assert_extension(
        K::CorruptionDetection,
        corruption,
        &[5, 255, 0x23, 11, 12],
        D::CorruptionDetection(RtcEventCorruptionDetection {
            sequence_index: 5,
            interpret_sequence_index_as_most_significant_bits: false,
            standard_deviation: 40.0,
            luma_error_threshold: 2,
            chroma_error_threshold: 3,
            sample_values: vec![11.0, 12.0],
        }),
    );
    assert_extension(
        K::DependencyDescriptorMandatory,
        "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension",
        &[0xc7, 0x12, 0x34],
        D::DependencyDescriptorMandatory(RtcEventDependencyDescriptorMandatory {
            first_packet_in_frame: true,
            last_packet_in_frame: true,
            frame_number: 0x1234,
            template_id: 7,
        }),
    );
}

fn push_bits(bits: &mut Vec<u8>, value: u32, count: usize) {
    for shift in (0..count).rev() {
        bits.push(((value >> shift) & 1) as u8);
    }
}

#[test]
fn full_dependency_descriptor_uses_and_returns_typed_external_structure() {
    let uri =
        "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";
    let (bytes, map) = packet(uri, &[0xc0, 0, 19]);
    let event = RtcEvent::rtp_incoming(&bytes, &map, None).unwrap();
    let no_context = event.rtp_dependency_descriptor(None).unwrap().unwrap();
    assert_eq!(no_context.raw, vec![0xc0, 0, 19]);
    assert_eq!(no_context.decoded, None);
    let template = RtcEventDependencyTemplate {
        spatial_id: 0,
        temporal_id: 0,
        decode_target_indications: vec![DecodeTargetIndication::Required],
        frame_diffs: vec![1],
        chain_diffs: vec![],
    };
    let structure = RtcEventDependencyStructure {
        structure_id: 0,
        num_decode_targets: 1,
        num_chains: 0,
        decode_target_protected_by_chain: vec![],
        resolutions: vec![RtcEventRenderResolution {
            width: 640,
            height: 360,
        }],
        templates: vec![template.clone()],
    };
    let descriptor = event
        .rtp_dependency_descriptor(Some(&structure))
        .unwrap()
        .unwrap()
        .decoded
        .unwrap();
    assert_eq!(descriptor.frame_number, 19);
    assert_eq!(descriptor.frame_dependencies, template);
    assert_eq!(
        descriptor.resolution,
        Some(RtcEventRenderResolution {
            width: 640,
            height: 360
        })
    );
    assert_eq!(descriptor.active_decode_targets_bitmask, None);
    assert_eq!(descriptor.attached_structure, None);
    let mut invalid = structure.clone();
    invalid.templates[0].spatial_id = 1;
    assert_eq!(
        event.rtp_dependency_descriptor(Some(&invalid)),
        Err(EventLogError::InvalidDependencyStructure)
    );
    invalid = structure.clone();
    invalid.num_decode_targets = 33;
    assert_eq!(
        event.rtp_dependency_descriptor(Some(&invalid)),
        Err(EventLogError::InvalidDependencyStructure)
    );
    invalid = structure.clone();
    invalid.templates[0].decode_target_indications.clear();
    assert_eq!(
        event.rtp_dependency_descriptor(Some(&invalid)),
        Err(EventLogError::InvalidDependencyStructure)
    );

    // Attached structure: one target, one required template, no diffs/chains,
    // and a single resolution. The parser supplies the active target bitmask.
    let mut bits = vec![];
    push_bits(&mut bits, 0b10000, 5); // extended flags: attached structure only
    push_bits(&mut bits, 0, 6); // structure ID
    push_bits(&mut bits, 0, 5); // decode target count minus 1
    push_bits(&mut bits, 3, 2); // no more templates
    push_bits(&mut bits, 3, 2); // required DTI
    push_bits(&mut bits, 0, 1); // no frame diffs
    push_bits(&mut bits, 0, 1); // zero chains
    push_bits(&mut bits, 1, 1); // resolutions present
    push_bits(&mut bits, 639, 16);
    push_bits(&mut bits, 359, 16);
    bits.resize(bits.len().div_ceil(8) * 8, 0);
    let mut raw = vec![0xc0, 0, 20];
    raw.extend(
        bits.chunks_exact(8)
            .map(|chunk| chunk.iter().fold(0u8, |byte, bit| (byte << 1) | bit)),
    );
    let (bytes, map) = packet(uri, &raw);
    let attached = RtcEvent::rtp_outgoing(&bytes, &map, 0, None).unwrap();
    let snapshot = attached.rtp_dependency_descriptor(None).unwrap().unwrap();
    assert_eq!(snapshot.raw, raw);
    let descriptor = snapshot.decoded.unwrap();
    assert_eq!(descriptor.active_decode_targets_bitmask, Some(1));
    let saved = descriptor.attached_structure.unwrap();
    assert_eq!(saved.resolutions, structure.resolutions);
    assert_eq!(saved.num_decode_targets, 1);
    assert_eq!(saved.templates[0].frame_diffs, vec![]);
    assert_eq!(descriptor.frame_dependencies, saved.templates[0]);
    let copy = attached.try_clone().unwrap();
    drop((attached, event, bytes, map));
    assert_eq!(
        copy.rtp_dependency_descriptor(None)
            .unwrap()
            .unwrap()
            .decoded
            .unwrap()
            .attached_structure,
        Some(saved.clone())
    );
    let (bytes, map) = packet(uri, &[0, 0, 21]);
    let next = RtcEvent::rtp_incoming(&bytes, &map, None).unwrap();
    let decoded = next
        .rtp_dependency_descriptor(Some(&saved))
        .unwrap()
        .unwrap()
        .decoded
        .unwrap();
    assert_eq!(decoded.frame_number, 21);
    assert_eq!(decoded.frame_dependencies, saved.templates[0]);
}

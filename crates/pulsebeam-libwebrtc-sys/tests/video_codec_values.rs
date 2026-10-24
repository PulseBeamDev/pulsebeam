use pulsebeam_webrtc_sys::*;
use std::collections::BTreeMap;

#[test]
fn complete_sdp_value_retains_bytes_optional_presence_and_native_value_categories() {
    let parameters = BTreeMap::from([(vec![0xff, 0], vec![0xfe, 0, b'x'])]);
    let mut format = SdpVideoFormat::with_parameters_and_modes(
        [0xff, 0, b'v'],
        &parameters,
        &[
            ScalabilityMode::L1T1,
            ScalabilityMode::from_raw(255),
            ScalabilityMode::L1T1,
        ],
    );
    let mut fields = format.fields();
    fields.packetization = Some(vec![]);
    fields.tx_mode = Some(vec![0xff, 0]);
    format.set_fields(fields.clone());
    let copy = format.clone();
    assert_eq!(copy.fields(), fields);
    assert_eq!(copy, format);
    assert_eq!(
        format.native_string_bytes(),
        Err(SdpVideoFormatError::UnsafeScalabilityModeFormatting)
    );
    // Identity matching/equality do not consume mode names. Exact fuzzy match
    // reaches no native diagnostic formatting, even with unknown modes.
    assert!(format.is_same_codec(&copy));
    assert!(format.is_codec_in_list(&[copy.clone()]));
    assert_eq!(
        format.fuzzy_match(&[copy.clone()]).unwrap(),
        Some(copy.clone())
    );
    let modes_only = format.with_scalability_modes(&[ScalabilityMode::L1T2]);
    assert_eq!(modes_only.fields().packetization, Some(vec![]));
    assert_eq!(modes_only.fields().tx_mode, Some(vec![0xff, 0]));
    assert_eq!(modes_only.fields().name, fields.name);
    assert!(modes_only.native_string_bytes().unwrap().contains(&0xff));
    assert_ne!(modes_only, format);
    assert!(modes_only.is_same_codec(&format));
    let moved = copy.sdk_move();
    assert_eq!(moved.fields(), fields);
    let mut assigned = SdpVideoFormat::new("different");
    assigned.assign_from(&format);
    assert_eq!(assigned, format);
    assigned.move_assign(moved);
    assert_eq!(assigned, format);
    std::thread::spawn(move || {
        assert_eq!(assigned.fields(), fields);
        assert!(assigned.is_same_codec(&format));
    })
    .join()
    .unwrap();
}

#[test]
fn native_codec_identity_and_fuzzy_matching_are_not_exact_equality() {
    let a = SdpVideoFormat::well_known(WellKnownSdpVideoFormat::H264);
    let b = SdpVideoFormat::with_parameters(
        "h264",
        &BTreeMap::from([(b"packetization-mode".to_vec(), b"0".to_vec())]),
    );
    let c = SdpVideoFormat::with_parameters(
        "H264",
        &BTreeMap::from([(b"packetization-mode".to_vec(), b"1".to_vec())]),
    );
    assert_ne!(a, b);
    assert!(a.is_same_codec(&b));
    assert!(!a.is_same_codec(&c));
    let request = SdpVideoFormat::with_parameters(
        "Opaque",
        &BTreeMap::from([(b"k".to_vec(), b"v".to_vec())]),
    );
    let first = SdpVideoFormat::new("opaque");
    let mut second = SdpVideoFormat::new("OPAQUE");
    let mut fields = second.fields();
    fields.tx_mode = Some(b"chosen".to_vec());
    second.set_fields(fields);
    assert_eq!(
        request
            .fuzzy_match(&[first.clone(), second.clone()])
            .unwrap(),
        Some(first)
    );
    let strongest = SdpVideoFormat::with_parameters("opaque", &request.fields().parameters);
    assert_eq!(
        request.fuzzy_match(&[second, strongest.clone()]).unwrap(),
        Some(strongest)
    );
    assert_eq!(
        request
            .fuzzy_match(&[SdpVideoFormat::new("different")])
            .unwrap(),
        None
    );
    let profile0 = SdpVideoFormat::well_known(WellKnownSdpVideoFormat::Vp9Profile0);
    let profile2 = SdpVideoFormat::well_known(WellKnownSdpVideoFormat::Vp9Profile2);
    assert!(!profile0.is_same_codec(&profile2));
    assert_eq!(
        profile2.fields().parameters.get(b"profile-id".as_slice()),
        Some(&b"2".to_vec())
    );
}

#[test]
fn native_typed_insertion_preserves_output_on_guard_rejection() {
    // Native RtcError stream formatting is C-string based. Keep this success
    // path non-NUL while still exercising byte-preserving non-UTF-8 storage.
    let good = SdpVideoFormat::new([0xff]);
    let bad = good.with_scalability_modes(&[ScalabilityMode::from_raw(255)]);
    let mut builder = StringBuilder::new(b"prefix:");
    assert!(!builder.append_sdp_video_format(&bad));
    assert_eq!(builder.as_bytes(), b"prefix:");
    assert!(builder.append_sdp_video_format(&good));
    assert_eq!(
        &builder.as_bytes()[7..],
        good.native_string_bytes().unwrap()
    );
    let mut error = RtcError::invalid_parameter(b"before:");
    assert!(!error.string_builder().append_sdp_video_format(&bad));
    assert_eq!(error.message_bytes(), b"before:");
    let error = error.with_appended_sdp_video_format(&bad).unwrap_err();
    assert_eq!(error.kind(), RtcErrorType::InvalidParameter);
    assert_eq!(error.message_bytes(), b"before:");
    let mut error = error.with_appended_sdp_video_format(&good).unwrap();
    // RTCError::message is a const-char* getter, unlike the owning builder.
    // Verify both native contracts, including the bytes retained after NUL.
    assert_eq!(
        error.message_bytes(),
        b"before:Codec name: \xff, parameters: { }"
    );
    assert_eq!(
        &error.string_builder().as_bytes()[7..],
        good.native_string_bytes().unwrap()
    );
}

#[test]
fn actual_selected_factory_discovery_returns_independent_complete_values() {
    let encoder = VideoEncoderFactoryHandle::builtin().unwrap();
    let decoder = VideoDecoderFactoryHandle::builtin().unwrap();
    let formats = encoder.supported_format_values();
    let implementations = encoder.implementation_values();
    let decoded_formats = decoder.supported_format_values();
    assert!(!formats.is_empty());
    assert!(!implementations.is_empty());
    assert!(!decoded_formats.is_empty());
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        assert!(formats.iter().any(|format| format.fields().name == b"H264"));
        assert!(
            decoded_formats
                .iter()
                .any(|format| format.fields().name == b"H264")
        );
    }
    let snapshots: Vec<_> = formats.iter().map(SdpVideoFormat::fields).collect();
    drop(encoder);
    drop(decoder);
    std::thread::spawn(move || {
        assert_eq!(
            formats
                .iter()
                .map(SdpVideoFormat::fields)
                .collect::<Vec<_>>(),
            snapshots
        );
        for value in implementations.iter().chain(decoded_formats.iter()) {
            assert_eq!(value.clone(), *value);
        }
    })
    .join()
    .unwrap();
}

#[test]
fn render_resolution_preserves_signed_metadata_and_native_validity() {
    let zero = RenderResolution::default();
    assert_eq!((zero.width(), zero.height(), zero.valid()), (0, 0, false));
    let mut value = RenderResolution::new(i32::MIN, i32::MAX);
    assert!(!value.valid());
    let copy = value.clone();
    assert_eq!(copy, value);
    let positive = RenderResolution::new(i32::MAX, i32::MAX);
    assert!(positive.valid()); // No native pixel-count multiplication occurs.
    value.assign_from(&positive);
    assert_eq!(value, positive);
    std::thread::spawn(move || assert_eq!((copy.width(), copy.height()), (i32::MIN, i32::MAX)))
        .join()
        .unwrap();
}

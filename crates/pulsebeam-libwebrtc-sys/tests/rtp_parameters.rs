use pulsebeam_webrtc_sys::{
    DegradationPreference as Degradation, MediaType, Resolution, RtcError, RtcpParameters,
    RtpCodec, RtpCodecParameters, RtpEncodingParameters, RtpExtension, RtpExtensions,
    RtpHeaderExtensionId, RtpParameters, StringBuilder,
};

#[test]
fn complete_parameters_defaults_and_sdk_equality_exclude_transaction_identity() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<RtpParameters>();
    let mut value = RtpParameters::default();
    assert_eq!(value.transaction_id_bytes(), b"");
    assert_eq!(value.mid_bytes(), b"");
    assert!(value.codecs().is_empty());
    assert!(value.encodings().is_empty());
    assert!(value.header_extensions().is_empty());
    assert_eq!(value.rtcp(), RtcpParameters::default());
    assert_eq!(value.degradation_preference(), None);
    assert!(!value.is_mixed_codec());
    assert_eq!(value.native_string_bytes().unwrap(), b"{transaction_id: '', mid: '', codecs: [], header_extensions: [], encodings: [], rtcp: {cname: '', reduced_size: false, mux: true}}");
    let mut copy = value.clone();
    value.set_transaction_id(b"tx\xff\0");
    assert_eq!(value.transaction_id_bytes(), b"tx\xff\0");
    assert_eq!(value, copy);
    value.set_mid(b"mid\xfe\0");
    assert_ne!(value, copy);
    copy.clone_from(&value);
    assert_eq!(copy, value);
    assert_eq!(copy.transaction_id_bytes(), b"tx\xff\0");
    value.set_mid("");
    assert_eq!(copy.mid_bytes(), b"mid\xfe\0");
}

#[test]
fn complete_parameters_children_are_full_independently_owned_sdk_copies() {
    let mut value = RtpParameters::default();
    let mut base = RtpCodec::default();
    base.set_name(b"codec\xff\0");
    base.set_kind(MediaType::Video);
    base.set_clock_rate(Some(i32::MIN));
    let mut codec = RtpCodecParameters::default();
    codec.set_base_codec(&base);
    codec.set_payload_type(i32::MAX);
    value.set_codecs(&[codec.clone()]);
    let mut encoding = RtpEncodingParameters::default();
    encoding.set_codec(Some(&base));
    encoding.set_rid(b"rid\xff\0");
    encoding.set_scalability_mode(Some(b"mode\xfe\0"));
    encoding.set_request_key_frame(true);
    encoding.set_csrcs(Some(&[]));
    encoding.set_ssrc(Some(0));
    encoding.set_scale_resolution_down_to(Some(&Resolution::new(i32::MIN, i32::MAX)));
    value.set_encodings(&[encoding.clone()]);
    let mut rtcp = RtcpParameters::default();
    rtcp.set_ssrc(Some(0));
    rtcp.set_cname(b"cname\xff\0");
    rtcp.set_mux(false);
    value.set_rtcp(&rtcp);
    let mut extension = RtpExtension::default();
    extension.set_uri(b"uri\xff\0");
    extension.set_id(&RtpHeaderExtensionId::new(i32::MIN));
    let mut extensions = RtpExtensions::default();
    extensions.push(&extension);
    value.set_header_extensions(&extensions);
    value.set_degradation_preference(Some(Degradation::Other(i32::MIN)));
    let copy = value.clone();
    codec.set_payload_type(0);
    encoding.set_rid("");
    extensions.clear();
    rtcp.set_ssrc(None);
    assert_eq!(copy.codecs()[0].payload_type(), i32::MAX);
    assert_eq!(copy.encodings()[0].rid_bytes(), b"rid\xff\0");
    assert_eq!(
        copy.encodings()[0].scalability_mode_bytes().unwrap(),
        b"mode\xfe\0"
    );
    assert!(copy.encodings()[0].request_key_frame());
    assert_eq!(copy.encodings()[0].csrcs(), Some(Vec::new()));
    assert_eq!(copy.encodings()[0].ssrc(), Some(0));
    assert_eq!(
        copy.encodings()[0].scale_resolution_down_to().unwrap(),
        Resolution::new(i32::MIN, i32::MAX)
    );
    assert_eq!(copy.rtcp().ssrc(), Some(0));
    assert_eq!(copy.rtcp().cname_bytes(), b"cname\xff\0");
    assert!(!copy.rtcp().mux());
    assert_eq!(
        copy.header_extensions().get(0).unwrap().uri_bytes(),
        b"uri\xff\0"
    );
    assert_eq!(
        copy.header_extensions().get(0).unwrap().id().value(),
        i32::MIN
    );
    assert_eq!(
        copy.degradation_preference(),
        Some(Degradation::Other(i32::MIN))
    );
    assert_eq!(copy.native_string_bytes(), None);
    let detached = copy.encodings();
    drop(copy);
    assert_eq!(detached[0].codec().unwrap().name_bytes(), b"codec\xff\0");
    let mut changed = value.encodings();
    changed[0].set_request_key_frame(false);
    let mut equal = value.clone();
    equal.set_encodings(&changed);
    assert_eq!(value, equal); // SDK encoding equality omits request_key_frame.
    changed[0].set_bitrate_priority(f64::NAN);
    value.set_encodings(&changed);
    assert_ne!(value, value); // SDK floating-point comparison is not reflexive.
}

#[test]
fn mixed_codec_uses_only_active_encodings_and_distinguishes_absence() {
    let mut value = RtpParameters::default();
    let mut a = RtpEncodingParameters::default();
    let mut b = a.clone();
    value.set_encodings(&[a.clone(), b.clone()]);
    assert!(!value.is_mixed_codec());
    b.set_codec(Some(&RtpCodec::default()));
    value.set_encodings(&[a.clone(), b.clone()]);
    assert!(value.is_mixed_codec());
    b.set_active(false);
    value.set_encodings(&[a.clone(), b.clone()]);
    assert!(!value.is_mixed_codec());
    a.set_active(false);
    b.set_active(true);
    value.set_encodings(&[a, b]);
    assert!(!value.is_mixed_codec());
}

#[test]
fn sdk_degradation_aliases_and_full_parameter_formatting_guard_only_consumption() {
    assert_eq!(
        Degradation::DISABLED,
        Degradation::MaintainFramerateAndResolution
    );
    for (raw, text) in [
        (0, "maintain-framerate-and-resolution"),
        (1, "maintain-framerate"),
        (2, "maintain-resolution"),
        (3, "balanced"),
    ] {
        assert_eq!(Degradation::from_native(raw).to_native(), raw);
        assert_eq!(
            Degradation::from_native(raw).native_string_bytes().unwrap(),
            text.as_bytes()
        );
    }
    for raw in [i32::MIN, -1, 4, i32::MAX] {
        assert_eq!(Degradation::from_native(raw), Degradation::Other(raw));
        assert_eq!(Degradation::from_native(raw).native_string_bytes(), None);
    }
    let mut value = RtpParameters::default();
    value.set_degradation_preference(Some(Degradation::Balanced));
    let text = value.native_string_bytes().unwrap();
    let mut output = StringBuilder::default();
    assert!(output.append_rtp_parameters(&value));
    assert_eq!(output.as_bytes(), text);
    let mut borrowed = RtcError::network_error("");
    assert!(borrowed.string_builder().append_rtp_parameters(&value));
    let streamed = RtcError::network_error("")
        .with_appended_rtp_parameters(&value)
        .unwrap();
    value.set_degradation_preference(Some(Degradation::Other(4)));
    assert!(!output.append_rtp_parameters(&value));
    assert!(!borrowed.string_builder().append_rtp_parameters(&value));
    assert_eq!(output.as_bytes(), text);
    assert_eq!(borrowed.string_builder().as_bytes(), text);
    let mut rejected = streamed.with_appended_rtp_parameters(&value).unwrap_err();
    assert_eq!(rejected.string_builder().as_bytes(), text);
    assert_eq!(value.degradation_preference(), Some(Degradation::Other(4)));
    value.set_degradation_preference(None);
    let mut codec = RtpCodecParameters::default();
    let mut base = codec.base_codec();
    base.set_kind(MediaType::Other(i32::MAX));
    codec.set_base_codec(&base);
    value.set_codecs(&[codec]);
    assert_eq!(value.native_string_bytes(), None);
    assert!(!output.append_rtp_parameters(&value));
    value.set_codecs(&[]);
    let mut encoding = RtpEncodingParameters::default();
    encoding.set_codec(Some(&base));
    value.set_encodings(&[encoding]);
    assert_eq!(value.native_string_bytes(), None);
    assert!(!output.append_rtp_parameters(&value));
    assert_eq!(output.as_bytes(), text);
}

use pulsebeam_webrtc_sys::Resolution;
#[test]
fn native_resolution_value_domain_and_consumption_guard() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Resolution>();
    let mut value = Resolution::default();
    assert_eq!(value.to_pair(), (0, 0));
    assert_eq!(value.pixel_count(), Some(0));
    value.set_width(-3);
    value.set_height(7);
    assert_eq!(value.to_pair(), (-3, 7));
    assert_eq!(value.pixel_count(), Some(-21));
    let copy = value.clone();
    value.set_height(8);
    assert_ne!(copy, value);
    value.clone_from(&copy);
    assert_eq!(copy, value);
    assert_eq!(Resolution::new(i32::MIN, 1).pixel_count(), Some(i32::MIN));
    assert_eq!(Resolution::new(i32::MAX, 1).pixel_count(), Some(i32::MAX));
    let overflow = Resolution::new(i32::MIN, -1);
    assert_eq!(overflow.pixel_count(), None);
    assert_eq!(overflow.to_pair(), (i32::MIN, -1));
    assert_eq!(Resolution::new(i32::MAX, 2).pixel_count(), None);
    assert_eq!(Resolution::new(i32::MIN, i32::MIN).pixel_count(), None);
    assert_eq!(Resolution::new(i32::MIN, 0).pixel_count(), Some(0));
}

#[test]
fn native_feedback_constructors_comparison_and_stringifiers() {
    use pulsebeam_webrtc_sys::{RtcpFeedback, RtcpFeedbackMessageType as M, RtcpFeedbackType as T};
    fn send_sync<T: Send + Sync>() {}
    send_sync::<RtcpFeedback>();
    let default = RtcpFeedback::default();
    assert_eq!(default.feedback_type(), T::Ccm);
    assert_eq!(default.message_type(), None);
    let typed = RtcpFeedback::for_type(T::Nack);
    assert_eq!(typed.message_type(), None);
    let mut value = RtcpFeedback::new(T::Nack, M::GenericNack);
    assert_ne!(value, typed);
    let copy = value.clone();
    value.set_feedback_type(T::Other(i32::MIN));
    value.set_message_type(Some(M::Other(i32::MAX)));
    assert_eq!(value.feedback_type(), T::Other(i32::MIN));
    assert_eq!(value.message_type(), Some(M::Other(i32::MAX)));
    value.clone_from(&copy);
    assert_eq!(value, copy);
    value.set_message_type(None);
    assert_eq!(value, typed);
    for (kind, text) in [
        (T::None, "NONE"),
        (T::Ccm, "CCM"),
        (T::Lntf, "LNTF"),
        (T::Nack, "NACK"),
        (T::Remb, "REMB"),
        (T::TransportCc, "TRANSPORT_CC"),
        (T::Ccfb, "CCFB"),
    ] {
        assert_eq!(kind.native_string(), text);
        assert_eq!(T::optional_native_string(Some(kind)), text);
    }
    assert_eq!(T::MAX, T::Ccfb);
    assert_eq!(T::optional_native_string(None), "nullopt");
    assert_eq!(T::Other(-9).native_string(), "");
    assert_eq!(T::optional_native_string(Some(T::Other(-9))), "");
}

#[test]
fn native_codec_preserves_bytes_collections_and_harmless_metadata() {
    use pulsebeam_webrtc_sys::{
        MediaType, RtcpFeedback, RtcpFeedbackMessageType, RtcpFeedbackType, RtpCodec,
        RtpCodecParameter as P,
    };
    fn send_sync<T: Send + Sync>() {}
    send_sync::<RtpCodec>();
    let mut codec = RtpCodec::default();
    assert_eq!(codec.kind(), MediaType::Audio);
    assert_eq!(codec.clock_rate(), None);
    assert_eq!(codec.num_channels(), None);
    assert_eq!(
        codec.mime_type_bytes().as_deref(),
        Some(b"audio/".as_slice())
    );
    codec.set_name(b"x\xff\0y");
    codec.set_kind(MediaType::Video);
    codec.set_clock_rate(Some(i32::MIN));
    codec.set_num_channels(Some(-7));
    assert_eq!(codec.name_bytes(), b"x\xff\0y");
    assert_eq!(codec.clock_rate(), Some(i32::MIN));
    assert_eq!(codec.num_channels(), Some(-7));
    assert_eq!(
        codec.mime_type_bytes().as_deref(),
        Some(b"video/x\xff\0y".as_slice())
    );
    codec.set_parameters(&[
        P {
            key: vec![0xff, 0],
            value: vec![0xfe, 0],
        },
        P {
            key: b"a".to_vec(),
            value: b"old".to_vec(),
        },
        P {
            key: b"a".to_vec(),
            value: b"new".to_vec(),
        },
    ]);
    assert_eq!(
        codec.parameters(),
        vec![
            P {
                key: b"a".to_vec(),
                value: b"new".to_vec()
            },
            P {
                key: vec![0xff, 0],
                value: vec![0xfe, 0]
            }
        ]
    );
    let feedback = RtcpFeedback::new(RtcpFeedbackType::Nack, RtcpFeedbackMessageType::Pli);
    codec.set_rtcp_feedback(&[feedback.clone()]);
    assert_eq!(codec.rtcp_feedback(), vec![feedback]);
    let copy = codec.clone();
    assert_eq!(copy, codec);
    assert!(
        codec
            .native_string_bytes()
            .unwrap()
            .windows(4)
            .any(|bytes| bytes == b"x\xff\0y")
    );
    codec.set_parameters(&[]);
    assert_ne!(copy, codec);
    codec.clone_from(&copy);
    assert_eq!(copy, codec);
    codec.set_kind(MediaType::Other(-9));
    assert_eq!(codec.kind(), MediaType::Other(-9));
    assert_eq!(codec.mime_type_bytes(), None);
    assert_eq!(codec.native_string_bytes(), None);
    assert!(codec.is_media_codec());
    assert_eq!(codec, codec.clone());
    codec.set_name("rtx");
    assert!(codec.is_resiliency_codec());
    assert!(!codec.is_media_codec());
    codec.set_name("CN");
    assert!(!codec.is_resiliency_codec());
    assert!(!codec.is_media_codec());
}

#[test]
fn native_codec_fmtp_helpers_use_native_map_and_builder() {
    use pulsebeam_webrtc_sys::{RtpCodec, RtpCodecParameter as P, StringBuilder};
    let mut codec = RtpCodec::default();
    let mut output = StringBuilder::new("prefix:");
    assert!(!codec.write_fmtp_parameters(&mut output));
    assert_eq!(output.as_bytes(), b"prefix:");
    codec.set_parameters(&[
        P {
            key: b"a".to_vec(),
            value: b"1".to_vec(),
        },
        P {
            key: b"ptime".to_vec(),
            value: b"20".to_vec(),
        },
    ]);
    assert!(codec.write_fmtp_parameters(&mut output));
    assert_eq!(output.as_bytes(), b"prefix:a=1");
    let error = codec.parse_fmtp_parameter_set("b=2;a=3");
    assert!(error.is_ok());
    assert_eq!(
        codec.parameters(),
        vec![
            P {
                key: b"a".to_vec(),
                value: b"3".to_vec()
            },
            P {
                key: b"b".to_vec(),
                value: b"2".to_vec()
            },
            P {
                key: b"ptime".to_vec(),
                value: b"20".to_vec()
            }
        ]
    );
}

#[test]
fn full_native_encoding_fields_and_copy_semantics() {
    use pulsebeam_webrtc_sys::{MediaType, Priority, RtpCodec, RtpEncodingParameters};
    fn send_sync<T: Send + Sync>() {}
    send_sync::<RtpEncodingParameters>();
    let mut value = RtpEncodingParameters::default();
    assert_eq!(value.ssrc(), None);
    assert_eq!(value.csrcs(), None);
    assert_eq!(value.bitrate_priority(), 1.0);
    assert_eq!(value.network_priority(), Priority::Low);
    assert!(value.active());
    assert!(!value.request_key_frame());
    assert!(!value.adaptive_ptime());
    assert_eq!(value.codec(), None);
    assert_eq!(value.scale_resolution_down_to(), None);
    assert_eq!(value.scalability_mode_bytes(), None);
    value.set_ssrc(Some(0));
    value.set_csrcs(Some(&[]));
    value.set_bitrate_priority(-3.0);
    value.set_network_priority(Priority::Other(-9));
    value.set_max_bitrate_bps(Some(i32::MIN));
    value.set_min_bitrate_bps(Some(-7));
    value.set_max_framerate(Some(f64::INFINITY));
    value.set_num_temporal_layers(Some(-8));
    value.set_scale_resolution_down_by(Some(f64::NEG_INFINITY));
    let resolution = Resolution::new(i32::MIN, -1);
    value.set_scale_resolution_down_to(Some(&resolution));
    value.set_scalability_mode(Some(b"x\xff\0y"));
    value.set_rid(b"r\xff\0d");
    value.set_active(false);
    value.set_request_key_frame(true);
    value.set_adaptive_ptime(true);
    let mut codec = RtpCodec::default();
    codec.set_kind(MediaType::Video);
    codec.set_name(b"c\xff\0d");
    value.set_codec(Some(&codec));
    assert_eq!(value.ssrc(), Some(0));
    assert_eq!(value.csrcs(), Some(vec![]));
    assert_eq!(value.bitrate_priority(), -3.0);
    assert_eq!(value.network_priority(), Priority::Other(-9));
    assert_eq!(value.max_bitrate_bps(), Some(i32::MIN));
    assert_eq!(value.min_bitrate_bps(), Some(-7));
    assert_eq!(value.max_framerate(), Some(f64::INFINITY));
    assert_eq!(value.num_temporal_layers(), Some(-8));
    assert_eq!(value.scale_resolution_down_by(), Some(f64::NEG_INFINITY));
    assert_eq!(value.scale_resolution_down_to(), Some(resolution));
    assert_eq!(value.scalability_mode_bytes(), Some(b"x\xff\0y".to_vec()));
    assert_eq!(value.rid_bytes(), b"r\xff\0d");
    assert!(!value.active());
    assert!(value.request_key_frame());
    assert!(value.adaptive_ptime());
    assert_eq!(value.codec(), Some(codec));
    assert!(value.native_string_bytes().is_some()); // no PixelCount/positivity consumption here
    let copy = value.clone();
    value.set_csrcs(None);
    assert_ne!(value, copy);
    value.clone_from(&copy);
    assert_eq!(value, copy);
    assert_eq!(
        value.scalability_mode_bytes(),
        copy.scalability_mode_bytes()
    );
    assert_eq!(value.request_key_frame(), copy.request_key_frame());
    let mut child = value.codec().unwrap();
    child.set_name("changed");
    assert_ne!(value.codec().unwrap(), child); // owned SDK field copy, not an alias
}

#[test]
fn native_encoding_equality_and_formatting_are_not_field_derived() {
    use pulsebeam_webrtc_sys::{MediaType, RtpCodec, RtpEncodingParameters};
    let value = RtpEncodingParameters::default();
    let mut changed = value.clone();
    changed.set_request_key_frame(true);
    assert_eq!(value, changed);
    assert_eq!(value.native_string_bytes(), changed.native_string_bytes());
    changed.set_scalability_mode(Some(b"L1T3"));
    assert_eq!(value, changed);
    assert_ne!(value.native_string_bytes(), changed.native_string_bytes());
    changed.set_scalability_mode(Some(&[]));
    assert_eq!(changed.scalability_mode_bytes(), Some(vec![]));
    changed.set_scalability_mode(None);
    assert_eq!(changed.scalability_mode_bytes(), None);
    let mut codec = RtpCodec::default();
    codec.set_kind(MediaType::Other(-9));
    changed.set_codec(Some(&codec));
    assert_eq!(changed.native_string_bytes(), None);
    assert_eq!(changed.codec().unwrap().kind(), MediaType::Other(-9));
    assert_eq!(changed, changed.clone()); // unknown kind is harmless comparison metadata
    changed.set_codec(None);
    assert!(changed.native_string_bytes().is_some());
    changed.set_bitrate_priority(f64::NAN);
    assert!(changed.bitrate_priority().is_nan());
    assert_ne!(changed, changed); // SDK floating-point equality, not Eq
    assert!(changed.native_string_bytes().is_some());
}

#[test]
fn native_priority_value_strong_alias_members_and_conversion() {
    use pulsebeam_webrtc_sys::{Priority, PriorityValue};
    fn send_sync<T: Send + Sync>() {}
    send_sync::<PriorityValue>();
    for (kind, expected) in [
        (Priority::VeryLow, 128),
        (Priority::Low, 256),
        (Priority::Medium, 512),
        (Priority::High, 1024),
    ] {
        assert_eq!(
            PriorityValue::from_priority(kind).unwrap().value(),
            expected
        );
    }
    assert!(PriorityValue::from_priority(Priority::Other(-9)).is_none());
    assert!(PriorityValue::from_priority(Priority::Other(i32::MAX)).is_none());
    let mut value = PriorityValue::new(0);
    assert_eq!(value.value_ref(), &0);
    assert_eq!(value.arrow_value_ref(), &0);
    assert_eq!(value.as_underlying(), &0);
    *value.value_mut() = u16::MAX;
    assert_eq!(*value, u16::MAX);
    *value.arrow_value_mut() = 42;
    *value = 7;
    let copy = value.clone();
    assert_eq!(value, copy);
    assert_eq!(value.native_hash(), copy.native_hash());
    assert!(value < PriorityValue::new(8));
    assert!(value <= copy);
    assert!(value > PriorityValue::new(6));
    assert!(value >= copy);
    assert_ne!(value, PriorityValue::new(8));
    value.clone_from(&PriorityValue::new(u16::MAX));
    assert_eq!(value.value(), u16::MAX);
    assert_eq!(value.clone().into_value(), u16::MAX);
    assert_eq!(value.clone().into_const_value(), u16::MAX);
    assert_eq!(value.clone().into_deref(), u16::MAX);
    assert_eq!(value.into_const_deref(), u16::MAX);
}

#[test]
fn native_rtp_template_builder_and_consuming_error_streams() {
    use pulsebeam_webrtc_sys::{
        MediaType, RtcError, RtcErrorDetailType, RtcErrorType, RtpCodec, RtpEncodingParameters,
        StringBuilder,
    };
    let mut codec = RtpCodec::default();
    codec.set_name(b"x\xff\0y");
    codec.set_kind(MediaType::Video);
    let codec_text = codec.native_string_bytes().unwrap();
    let mut output = StringBuilder::new("prefix:");
    assert!(output.append_codec(&codec));
    let expected = [b"prefix:".as_slice(), codec_text.as_slice()].concat();
    assert_eq!(output.as_bytes(), expected);

    let mut base = RtcError::with_message(RtcErrorType::Other(-9), "prefix:");
    base.set_error_detail(RtcErrorDetailType::Other(i32::MIN));
    base.set_sctp_cause_code(0);
    let mut streamed = base.clone().with_appended_codec(&codec).unwrap();
    assert_eq!(streamed.kind(), base.kind());
    assert_eq!(streamed.error_detail(), base.error_detail());
    assert_eq!(streamed.sctp_cause_code(), Some(0));
    assert_eq!(streamed.string_builder().as_bytes(), expected);
    let mut borrowed = base.clone();
    assert!(borrowed.string_builder().append_codec(&codec));
    assert_eq!(borrowed.string_builder().as_bytes(), expected);

    let mut encoding = RtpEncodingParameters::default();
    encoding.set_codec(Some(&codec));
    encoding.set_max_framerate(Some(f64::INFINITY));
    encoding.set_scale_resolution_down_to(Some(&Resolution::new(i32::MIN, -1)));
    let encoding_text = encoding.native_string_bytes().unwrap();
    output.clear();
    assert!(output.append_encoding(&encoding));
    assert_eq!(output.as_bytes(), encoding_text);
    let mut streamed = base.clone().with_appended_encoding(&encoding).unwrap();
    let expected = [b"prefix:".as_slice(), encoding_text.as_slice()].concat();
    assert_eq!(streamed.string_builder().as_bytes(), expected);
    let mut borrowed = base.clone();
    assert!(borrowed.string_builder().append_encoding(&encoding));
    assert_eq!(borrowed.string_builder().as_bytes(), expected);

    codec.set_kind(MediaType::Other(-9));
    encoding.set_codec(Some(&codec));
    output.clear();
    output.append_text("kept");
    assert!(!output.append_codec(&codec));
    assert!(!output.append_encoding(&encoding));
    assert_eq!(output.as_bytes(), b"kept");
    assert!(!base.string_builder().append_codec(&codec));
    assert!(!base.string_builder().append_encoding(&encoding));
    assert_eq!(base.string_builder().as_bytes(), b"prefix:");
    let mut rejected = base.clone().with_appended_codec(&codec).unwrap_err();
    assert_eq!(rejected.string_builder().as_bytes(), b"prefix:");
    assert_eq!(rejected.kind(), base.kind());
    assert_eq!(rejected.error_detail(), base.error_detail());
    assert_eq!(rejected.sctp_cause_code(), base.sctp_cause_code());
    let mut rejected = base.with_appended_encoding(&encoding).unwrap_err();
    assert_eq!(rejected.string_builder().as_bytes(), b"prefix:");
}

#[test]
fn native_rtp_enum_template_streams_preserve_safe_empty_unknown_output() {
    use pulsebeam_webrtc_sys::{MediaType, RtcError, RtcpFeedbackType as T, StringBuilder};
    let mut output = StringBuilder::new("prefix:");
    output.append_feedback_type(T::Nack);
    output.append_feedback_type(T::Other(-9));
    output.append_optional_feedback_type(Some(T::Remb));
    output.append_optional_feedback_type(Some(T::Other(-9)));
    output.append_optional_feedback_type(None);
    assert!(output.append_media_type(MediaType::Data));
    assert!(!output.append_media_type(MediaType::Any));
    assert!(!output.append_media_type(MediaType::Other(-9)));
    assert_eq!(output.as_bytes(), b"prefix:NACKREMBnulloptdata");
    let mut streamed = RtcError::network_error("prefix:")
        .with_appended_feedback_type(T::Nack)
        .with_appended_feedback_type(T::Other(-9))
        .with_appended_optional_feedback_type(Some(T::Remb))
        .with_appended_optional_feedback_type(Some(T::Other(-9)))
        .with_appended_optional_feedback_type(None)
        .with_appended_media_type(MediaType::Data)
        .unwrap();
    assert_eq!(streamed.string_builder().as_bytes(), output.as_bytes());
    let mut borrowed = RtcError::network_error("prefix:");
    {
        let mut builder = borrowed.string_builder();
        builder.append_feedback_type(T::Nack);
        builder.append_feedback_type(T::Other(-9));
        builder.append_optional_feedback_type(Some(T::Remb));
        builder.append_optional_feedback_type(Some(T::Other(-9)));
        builder.append_optional_feedback_type(None);
        assert!(builder.append_media_type(MediaType::Data));
        assert!(!builder.append_media_type(MediaType::Unsupported));
    }
    assert_eq!(borrowed.string_builder().as_bytes(), output.as_bytes());
    let mut rejected = streamed
        .with_appended_media_type(MediaType::Other(-9))
        .unwrap_err();
    assert_eq!(rejected.string_builder().as_bytes(), output.as_bytes());
}

#[test]
fn native_codec_parameters_own_inherited_fields_and_preserve_signed_payloads() {
    use pulsebeam_webrtc_sys::{
        MediaType, RtcError, RtcpFeedback, RtcpFeedbackMessageType, RtcpFeedbackType, RtpCodec,
        RtpCodecParameters, StringBuilder,
    };
    fn send_sync<T: Send + Sync>() {}
    send_sync::<RtpCodecParameters>();
    let mut value = RtpCodecParameters::default();
    assert_eq!(value.payload_type(), 0);
    assert_eq!(value.base_codec(), RtpCodec::default());
    let mut base = value.base_codec();
    base.set_name(b"rtx\xff\0");
    base.set_kind(MediaType::Video);
    base.set_clock_rate(Some(-9));
    base.set_num_channels(Some(i32::MIN));
    base.set_rtcp_feedback(&[RtcpFeedback::new(
        RtcpFeedbackType::Nack,
        RtcpFeedbackMessageType::GenericNack,
    )]);
    assert_eq!(value.base_codec(), RtpCodec::default());
    value.set_payload_type(i32::MIN);
    value.set_base_codec(&base);
    assert_eq!(value.payload_type(), i32::MIN);
    assert_eq!(value.base_codec(), base);
    assert_eq!(value.mime_type_bytes(), base.mime_type_bytes());
    assert_eq!(value.is_media_codec(), base.is_media_codec());
    assert_eq!(value.is_resiliency_codec(), base.is_resiliency_codec());
    let copy = value.clone();
    value.set_payload_type(i32::MAX);
    value.set_base_codec(&RtpCodec::default());
    assert_ne!(copy, value);
    value.clone_from(&copy);
    assert_eq!(copy, value);
    let text = value.native_string_bytes().unwrap();
    let mut output = StringBuilder::default();
    assert!(output.append_codec_parameters(&value));
    assert_eq!(output.as_bytes(), text);
    let mut error = RtcError::network_error("")
        .with_appended_codec_parameters(&value)
        .unwrap();
    assert_eq!(error.string_builder().as_bytes(), text);
    let mut borrowed = RtcError::network_error("");
    assert!(borrowed.string_builder().append_codec_parameters(&value));
    assert_eq!(borrowed.string_builder().as_bytes(), text);
    base.set_kind(MediaType::Other(-9));
    value.set_base_codec(&base);
    assert_eq!(value.base_codec().kind(), MediaType::Other(-9));
    assert_eq!(value.native_string_bytes(), None);
    assert_eq!(value.mime_type_bytes(), None);
    assert!(!output.append_codec_parameters(&value));
    assert_eq!(output.as_bytes(), text);
    assert!(!borrowed.string_builder().append_codec_parameters(&value));
    assert_eq!(borrowed.string_builder().as_bytes(), text);
    let mut rejected = error.with_appended_codec_parameters(&value).unwrap_err();
    assert_eq!(rejected.string_builder().as_bytes(), text);
    let detached_base = copy.base_codec();
    drop(copy);
    assert_eq!(detached_base.name_bytes(), b"rtx\xff\0");
}

#[test]
fn native_rtcp_parameters_defaults_optional_zero_bytes_copy_and_template_streams() {
    use pulsebeam_webrtc_sys::{RtcError, RtcpParameters, StringBuilder};
    fn send_sync<T: Send + Sync>() {}
    send_sync::<RtcpParameters>();
    let mut value = RtcpParameters::default();
    assert_eq!(value.ssrc(), None);
    assert_eq!(value.cname_bytes(), b"");
    assert!(!value.reduced_size());
    assert!(value.mux());
    assert_eq!(
        value.native_string_bytes(),
        b"{cname: '', reduced_size: false, mux: true}"
    );
    value.set_ssrc(Some(0));
    value.set_cname(b"c\xff\0n");
    value.set_reduced_size(true);
    value.set_mux(false);
    let copy = value.clone();
    assert_eq!(copy.ssrc(), Some(0));
    assert_eq!(copy.cname_bytes(), b"c\xff\0n");
    value.set_ssrc(Some(u32::MAX));
    value.set_cname("");
    value.set_reduced_size(false);
    value.set_mux(true);
    assert_ne!(copy, value);
    value.clone_from(&copy);
    assert_eq!(value, copy);
    let text = value.native_string_bytes();
    assert_eq!(
        text,
        b"{ssrc: 0, cname: 'c\xff\0n', reduced_size: true, mux: false}"
    );
    let mut output = StringBuilder::default();
    output.append_rtcp_parameters(&value);
    assert_eq!(output.as_bytes(), text);
    let mut error = RtcError::network_error("").with_appended_rtcp_parameters(&value);
    assert_eq!(error.string_builder().as_bytes(), text);
    let mut borrowed = RtcError::network_error("");
    borrowed.string_builder().append_rtcp_parameters(&value);
    assert_eq!(borrowed.string_builder().as_bytes(), text);
    value.set_ssrc(None);
    assert_eq!(value.ssrc(), None);
    assert_eq!(copy.ssrc(), Some(0));
}

#[test]
fn native_header_extension_id_full_signed_domain_and_strong_alias_loans() {
    use pulsebeam_webrtc_sys::RtpHeaderExtensionId as Id;
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Id>();
    assert_eq!(Id::default(), Id::not_set());
    assert_eq!(Id::default().value(), 0);
    assert!(!Id::default().is_set());
    assert!(!Id::default().is_valid());
    assert_eq!(Id::min_id().value(), 1);
    assert_eq!(Id::max_id().value(), 255);
    assert_eq!(Id::one_byte_max_id().value(), 14);
    for raw in [i32::MIN, -1, 0, 1, 14, 255, 256, i32::MAX] {
        let value = Id::new(raw);
        assert_eq!(value.value(), raw);
        assert_eq!(value.to_int(), raw);
        assert_eq!(value.is_set(), raw != 0);
        assert_eq!(value.is_valid(), (1..=255).contains(&raw));
        assert_eq!(value.native_string_bytes(), raw.to_string().as_bytes());
        assert!(std::ptr::eq(value.value_ref(), value.as_underlying()));
        assert!(std::ptr::eq(value.value_ref(), &*value));
        assert!(std::ptr::eq(value.value_ref(), value.arrow_value_ref()));
        let copy = value.clone();
        assert_eq!(value, copy);
        assert_eq!(value.native_hash(), copy.native_hash());
        assert_eq!(value.into_value(), raw);
        assert_eq!(copy.into_const_value(), raw);
        assert_eq!(Id::new(raw).into_deref(), raw);
        assert_eq!(Id::new(raw).into_const_deref(), raw);
    }
    let mut value = Id::new(1);
    *value.value_mut() = i32::MIN;
    *value.arrow_value_mut() = i32::MAX;
    *value = -9;
    assert_eq!(value.to_int(), -9);
    assert!(!value.is_valid());
    assert!(value.is_set());
    value.clone_from(&Id::max_id());
    assert_eq!(value, Id::max_id());
    assert!(Id::min_id() < value);
    assert!(Id::min_id() <= value);
    assert!(value > Id::min_id());
    assert!(value >= Id::min_id());
    assert_ne!(value, Id::min_id());
}

#[test]
fn native_rtp_extension_constructors_fields_constants_and_sdk_formatters() {
    use pulsebeam_webrtc_sys::{RtpExtension as E, RtpHeaderExtensionId as Id};
    fn send_sync<T: Send + Sync>() {}
    send_sync::<E>();
    let mut value = E::default();
    assert_eq!(value.uri_bytes(), b"");
    assert_eq!(value.id(), Id::not_set());
    assert!(!value.encrypt());
    assert_eq!(value.native_string_bytes(), b"[0 ]");
    assert_eq!(value.to_string_bytes(), b"{uri: , id: 0}");
    for id in [i32::MIN, -1, 0, 256, i32::MAX] {
        assert!(E::new(b"x", &Id::new(id)).is_none());
        assert!(E::new_encrypted(b"x", &Id::new(id), true).is_none());
        assert!(E::new_int(b"x", id).is_none());
        assert!(E::new_int_encrypted(b"x", id, false).is_none());
    }
    let uri = E::audio_level_uri();
    let plain = E::new(&uri, &Id::new(14)).unwrap();
    assert_eq!(plain, E::new_int(&uri, 14).unwrap());
    let encrypted = E::new_encrypted(&uri, &Id::new(14), true).unwrap();
    assert_eq!(encrypted, E::new_int_encrypted(&uri, 14, true).unwrap());
    assert_ne!(plain, encrypted);
    assert!(E::is_supported_for_audio(&uri));
    assert!(!E::is_supported_for_video(&uri));
    assert!(E::is_supported_for_video(E::timestamp_offset_uri()));
    assert!(!E::is_supported_for_audio(E::timestamp_offset_uri()));
    assert!(!E::is_encryption_supported(
        E::encrypt_header_extensions_uri()
    ));
    assert!(E::is_encryption_supported(b"unknown\0\xff"));
    assert!(!E::is_supported_for_audio(b"unknown\0\xff"));
    assert!(!E::is_supported_for_video(b"unknown\0\xff"));
    assert_eq!(E::min_id(), Id::min_id());
    assert_eq!(E::max_id(), Id::max_id());
    assert_eq!(E::one_byte_max_id(), Id::one_byte_max_id());
    assert_eq!(E::max_value_size(), 255);
    assert_eq!(E::one_byte_max_value_size(), 16);
    value.set_uri(b"raw\0\xff");
    value.set_id(&Id::new(i32::MIN));
    value.set_encrypt(true);
    assert_eq!(value.uri_bytes(), b"raw\0\xff");
    assert_eq!(value.id().value(), i32::MIN);
    assert!(value.encrypt());
    let mut copy = value.clone();
    value.set_uri(b"changed");
    assert_eq!(copy.uri_bytes(), b"raw\0\xff");
    copy.clone_from(&plain);
    assert_eq!(copy, plain);
    let constants = [
        E::encrypt_header_extensions_uri(),
        E::audio_level_uri(),
        E::timestamp_offset_uri(),
        E::abs_send_time_uri(),
        E::absolute_capture_time_uri(),
        E::video_rotation_uri(),
        E::video_content_type_uri(),
        E::video_timing_uri(),
        E::generic_frame_descriptor_uri_00(),
        E::dependency_descriptor_uri(),
        E::video_layers_allocation_uri(),
        E::transport_sequence_number_uri(),
        E::transport_sequence_number_v2_uri(),
        E::playout_delay_uri(),
        E::color_space_uri(),
        E::mid_uri(),
        E::rid_uri(),
        E::repaired_rid_uri(),
        E::video_frame_tracking_id_uri(),
        E::csrc_audio_levels_uri(),
        E::corruption_detection_uri(),
    ];
    assert!(constants.iter().all(|uri| !uri.is_empty()));
}

#[test]
fn native_header_extension_lookup_loans_and_dedup_are_sdk_operations() {
    use pulsebeam_webrtc_sys::{
        RtpExtension as E, RtpExtensionFilter as F, RtpExtensions, RtpHeaderExtensionId as Id,
    };
    fn send_sync<T: Send + Sync>() {}
    send_sync::<RtpExtensions>();
    let mut values = RtpExtensions::default();
    for (uri, id, encrypted) in [
        (b"z", 7, false),
        (b"z", 1, false),
        (b"z", 9, true),
        (b"a", 3, false),
        (b"a", 4, true),
    ] {
        values.push(&E::new_encrypted(uri, &Id::new(id), encrypted).unwrap());
    }
    assert_eq!(values.len(), 5);
    assert!(values.get(5).is_none());
    assert_eq!(
        values
            .find_by_uri(b"z", F::DiscardEncrypted)
            .unwrap()
            .unwrap()
            .id()
            .value(),
        7
    );
    let loan = values
        .find_by_uri(b"z", F::PreferEncrypted)
        .unwrap()
        .unwrap();
    assert_eq!(loan.uri_bytes(), b"z");
    assert_eq!(loan.id().value(), 9);
    assert!(loan.encrypt());
    assert_eq!(loan.native_string_bytes(), b"[9 z (encrypted)]");
    assert_eq!(loan.to_string_bytes(), b"{uri: z, id: 9, encrypt}");
    assert_eq!(
        values
            .find_by_uri(b"z", F::RequireEncrypted)
            .unwrap()
            .unwrap()
            .to_owned(),
        loan.to_owned()
    );
    assert_eq!(
        values
            .find_by_uri_and_encryption(b"z", false)
            .unwrap()
            .id()
            .value(),
        7
    );
    assert!(
        values
            .find_by_uri(b"missing", F::PreferEncrypted)
            .unwrap()
            .is_none()
    );
    assert!(
        values
            .find_by_uri_and_encryption(b"missing", false)
            .is_none()
    );
    assert!(values.find_by_uri(b"z", F::Other(3)).unwrap().is_none());
    assert!(values.find_by_uri(b"z", F::Other(-1)).is_err());
    assert!(values.deduplicate(F::Other(i32::MAX)).is_err());
    let discard = values.deduplicate(F::DiscardEncrypted).unwrap();
    assert_eq!(discard.len(), 2);
    assert_eq!(discard.get(0).unwrap().uri_bytes(), b"a");
    assert_eq!(discard.get(0).unwrap().id().value(), 3);
    // SDK keeps the first matching entry, not the minimum ID.
    assert_eq!(discard.get(1).unwrap().id().value(), 7);
    let prefer = values.deduplicate(F::PreferEncrypted).unwrap();
    let require = values.deduplicate(F::RequireEncrypted).unwrap();
    let unnamed = values.deduplicate(F::Other(3)).unwrap();
    for index in 0..2 {
        assert_eq!(prefer.get(index), require.get(index));
        assert_eq!(prefer.get(index), unnamed.get(index));
    }
    let owned = std::thread::scope(|scope| scope.spawn(|| loan.to_owned()).join().unwrap());
    assert_eq!(owned.id().value(), 9);
    let mut copied = values.clone();
    copied.clear();
    copied.clone_from(&values);
    assert_eq!(copied.len(), 5);
    drop(loan);
    values.clear();
    assert!(values.is_empty());
    assert_eq!(owned.id().value(), 9);
    assert_eq!(copied.len(), 5);
}

#[test]
fn native_header_extension_typed_streams_match_sdk_stringify() {
    use pulsebeam_webrtc_sys::{
        RtcError, RtcErrorType, RtpExtension, RtpExtensionFilter, RtpExtensions,
        RtpHeaderExtensionId, StringBuilder,
    };
    let id = RtpHeaderExtensionId::new(-9);
    let mut extension = RtpExtension::default();
    extension.set_uri(b"x\0\xff");
    extension.set_id(&id);
    extension.set_encrypt(true);
    let mut values = RtpExtensions::default();
    values.push(&extension);
    let loan = values
        .find_by_uri(b"x\0\xff", RtpExtensionFilter::PreferEncrypted)
        .unwrap()
        .unwrap();
    let mut expected = id.native_string_bytes();
    expected.extend(extension.native_string_bytes());
    expected.extend(loan.native_string_bytes());
    let mut builder = StringBuilder::default();
    builder.append_header_extension_id(&id);
    builder.append_rtp_extension(&extension);
    builder.append_rtp_extension_ref(&loan);
    assert_eq!(builder.as_bytes(), expected);
    // Native RTCError exposes its message as a C string, unlike the builder.
    let message = &expected[..expected.iter().position(|byte| *byte == 0).unwrap()];
    let mut error = RtcError::new(RtcErrorType::InvalidState);
    {
        let mut stream = error.string_builder();
        stream.append_header_extension_id(&id);
        stream.append_rtp_extension(&extension);
        stream.append_rtp_extension_ref(&loan);
    }
    // SDK message() is a const-char* view; the mutable SDK builder retains
    // the full length-aware storage. Do not confuse the two public contracts.
    assert_eq!(error.message_bytes(), b"-9[-9 x");
    assert_eq!(error.string_builder().as_bytes(), expected);
    let mut error = RtcError::new(RtcErrorType::InvalidState)
        .with_appended_header_extension_id(&id)
        .with_appended_rtp_extension(&extension)
        .with_appended_rtp_extension_ref(&loan);
    assert_eq!(error.message_bytes(), b"-9[-9 x");
    assert_eq!(error.string_builder().as_bytes(), expected);
    assert_eq!(error.kind(), RtcErrorType::InvalidState);
}

use pulsebeam_webrtc_sys::{
    FecMechanism, MediaType, RtcError, RtcpFeedback, RtcpFeedbackType, RtpCapabilities, RtpCodec,
    RtpCodecCapability, RtpCodecParameter, RtpHeaderExtensionCapabilityValue as HeaderCapability,
    RtpHeaderExtensionId, RtpTransceiverDirectionValue as Direction, ScalabilityMode,
    StringBuilder,
};

#[test]
fn codec_capability_preserves_complete_base_fields_modes_and_derived_formatter() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<RtpCodecCapability>();
    let mut value = RtpCodecCapability::default();
    assert_eq!(value.base_codec(), RtpCodec::default());
    assert_eq!(value.preferred_payload_type(), None);
    assert!(value.scalability_modes().is_empty());
    // Pinned RtpCodec initializes kind=AUDIO; its formatter uses native
    // absent-rate/channel defaults without filling the optional fields.
    assert_eq!(value.base_codec().kind(), MediaType::Audio);
    assert_eq!(value.native_string_bytes(), b"[audio//0/1]");
    let mut base = RtpCodec::default();
    base.set_name(b"opus\xff\0");
    base.set_kind(MediaType::Audio);
    base.set_clock_rate(Some(-7));
    base.set_num_channels(Some(0));
    base.set_parameters(&[RtpCodecParameter {
        key: b"k\xff".to_vec(),
        value: b"v\0".to_vec(),
    }]);
    base.set_rtcp_feedback(&[RtcpFeedback::for_type(RtcpFeedbackType::Other(-1))]);
    value.set_base_codec(&base);
    value.set_preferred_payload_type(Some(i32::MIN));
    let modes = [
        ScalabilityMode::L2T2_KEY_SHIFT,
        ScalabilityMode::from_raw(255),
        ScalabilityMode::L2T2_KEY_SHIFT,
    ];
    value.set_scalability_modes(&modes);
    assert_eq!(value.base_codec(), base);
    assert_eq!(value.scalability_modes(), modes);
    assert_eq!(value.native_string_bytes(), b"[audio/opus\xff\0/-7/0]");
    let copy = value.clone();
    base.set_kind(MediaType::Other(-9));
    value.set_base_codec(&base);
    assert_eq!(value.preferred_payload_type(), Some(i32::MIN));
    assert_eq!(value.scalability_modes(), modes);
    assert_eq!(value.mime_type_bytes(), None);
    // Capability formatting does NOT use the base codec's fatal MIME helper.
    let text = value.native_string_bytes();
    assert_eq!(text, b"[video/opus\xff\0]");
    let mut output = StringBuilder::default();
    output.append_rtp_codec_capability(&value);
    assert_eq!(output.as_bytes(), text);
    let mut error = RtcError::success();
    error.string_builder().append_rtp_codec_capability(&value);
    assert_eq!(error.string_builder().as_bytes(), text);
    let mut streamed = RtcError::success().with_appended_rtp_codec_capability(&value);
    assert_eq!(streamed.string_builder().as_bytes(), text);
    assert_ne!(copy, value);
    value.clone_from(&copy);
    assert_eq!(copy, value);
    value.set_preferred_payload_type(None);
    value.set_scalability_modes(&[]);
    assert_eq!(copy.preferred_payload_type(), Some(i32::MIN));
    assert_eq!(copy.scalability_modes(), modes);
}

#[test]
fn header_capability_constructors_optional_ids_raw_direction_bytes_and_assignment() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<HeaderCapability>();
    let value = HeaderCapability::default();
    assert_eq!(value.uri_bytes(), b"");
    assert_eq!(value.preferred_id(), None);
    assert!(!value.preferred_encrypt());
    assert_eq!(value.direction_raw(), 0);
    let uri = b"u\xff\0ri";
    assert_eq!(HeaderCapability::new(uri).uri_bytes(), uri);
    let id = RtpHeaderExtensionId::new(i32::MIN);
    let mut value = HeaderCapability::with_preferences(uri, &id, true, Direction::Other(i32::MAX));
    assert_eq!(value.uri_bytes(), uri);
    assert_eq!(value.preferred_id().unwrap(), id);
    assert!(value.preferred_encrypt());
    assert_eq!(value.direction_raw(), i32::MAX);
    assert_eq!(value.direction(), Direction::Other(i32::MAX));
    assert_eq!(
        HeaderCapability::with_id(uri, &id),
        HeaderCapability::with_raw_id(uri, i32::MIN)
    );
    assert_eq!(
        HeaderCapability::with_direction(uri, &id, Direction::Stopped),
        HeaderCapability::with_raw_id_and_direction(uri, i32::MIN, Direction::Stopped)
    );
    assert_eq!(
        value,
        HeaderCapability::with_raw_preferences(uri, i32::MIN, true, Direction::Other(i32::MAX))
    );
    let copy = value.clone();
    let detached = value.preferred_id().unwrap();
    value.set_uri("");
    value.set_preferred_id(None);
    value.set_preferred_encrypt(false);
    value.set_direction(Direction::Other(-1));
    assert_ne!(copy, value);
    value.clone_from(&copy);
    assert_eq!(copy, value);
    value.set_preferred_id(Some(&RtpHeaderExtensionId::not_set()));
    assert_eq!(value.preferred_id().unwrap().value(), 0);
    drop(value);
    assert_eq!(detached.value(), i32::MIN);
}

#[test]
fn complete_direction_metadata_uses_the_sdk_formatter_including_empty_unknowns() {
    assert_eq!(Direction::Other(0), Direction::SendReceive);
    let known = [
        (Direction::SendReceive, b"sendrecv".as_slice()),
        (Direction::SendOnly, b"send".as_slice()),
        (Direction::ReceiveOnly, b"recv".as_slice()),
        (Direction::Inactive, b"inactive".as_slice()),
        (Direction::Stopped, b"stopped".as_slice()),
    ];
    for (direction, expected) in known {
        assert_eq!(Direction::from_raw(direction.raw()), direction);
        assert_eq!(direction.native_string_bytes(), expected);
        let mut output = StringBuilder::default();
        output.append_rtp_transceiver_direction(direction);
        assert_eq!(output.as_bytes(), expected);
        let mut error = RtcError::success();
        error
            .string_builder()
            .append_rtp_transceiver_direction(direction);
        assert_eq!(error.string_builder().as_bytes(), expected);
        let mut streamed = RtcError::success().with_appended_rtp_transceiver_direction(direction);
        assert_eq!(streamed.string_builder().as_bytes(), expected);
    }
    let unknown = Direction::Other(i32::MIN);
    assert_eq!(unknown.raw(), i32::MIN);
    assert!(unknown.native_string_bytes().is_empty());
    let mut output = StringBuilder::new(b"unchanged");
    output.append_rtp_transceiver_direction(unknown);
    assert_eq!(output.as_bytes(), b"unchanged");
}

#[test]
fn capabilities_children_and_fec_are_independent_ordered_sdk_copies() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<RtpCapabilities>();
    let mut value = RtpCapabilities::default();
    assert!(value.codecs().is_empty());
    assert!(value.header_extensions().is_empty());
    assert!(value.fec().is_empty());
    let mut codec = RtpCodecCapability::default();
    codec.set_preferred_payload_type(Some(i32::MAX));
    let header = HeaderCapability::with_raw_preferences(b"e\xff\0", -1, true, Direction::Other(-2));
    value.set_codecs(&[codec.clone(), codec.clone()]);
    value.set_header_extensions(&[header.clone()]);
    let fec = [
        FecMechanism::Flexfec,
        FecMechanism::Other(i32::MIN),
        FecMechanism::Red,
        FecMechanism::RedAndUlpfec,
        FecMechanism::Flexfec,
    ];
    value.set_fec(&fec);
    let copy = value.clone();
    let codecs = value.codecs();
    let headers = value.header_extensions();
    value.set_codecs(&[]);
    value.set_header_extensions(&[]);
    value.set_fec(&[]);
    assert_ne!(value, copy);
    value.clone_from(&copy);
    assert_eq!(value, copy);
    assert_eq!(value.codecs(), [codec.clone(), codec]);
    assert_eq!(value.header_extensions(), [header]);
    assert_eq!(value.fec(), fec);
    drop(value);
    drop(copy);
    assert_eq!(codecs[0].preferred_payload_type(), Some(i32::MAX));
    assert_eq!(headers[0].uri_bytes(), b"e\xff\0");
}

#[test]
fn scalability_uint8_metadata_and_optional_sdk_name_helper_guard_only_consumption() {
    assert_eq!(FecMechanism::Other(0), FecMechanism::Red);
    assert_eq!(FecMechanism::Other(1), FecMechanism::RedAndUlpfec);
    let modes = ScalabilityMode::all();
    assert_eq!(modes.len(), 34);
    assert_eq!(modes[0], ScalabilityMode::L1T1);
    assert_eq!(modes[33], ScalabilityMode::S3T3H);
    assert_eq!(
        ScalabilityMode::L2T2_KEY_SHIFT.native_name_bytes().unwrap(),
        b"L2T2_KEY_SHIFT"
    );
    assert_eq!(
        ScalabilityMode::optional_native_name_bytes(None).unwrap(),
        b"nullopt"
    );
    for mode in modes {
        assert!(mode.native_name_bytes().is_some());
    }
    let unknown = ScalabilityMode::from_raw(255);
    assert_eq!(unknown.raw(), 255);
    assert_eq!(unknown.native_name_bytes(), None);
}

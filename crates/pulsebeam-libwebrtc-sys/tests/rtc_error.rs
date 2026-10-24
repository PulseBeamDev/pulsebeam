use pulsebeam_webrtc_sys::{RtcError, RtcErrorDetailType, RtcErrorType};

#[test]
fn native_rtc_error_factories_fields_and_assignment() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RtcError>();
    assert!(RtcError::default().is_ok());
    assert!(RtcError::success().is_ok());
    assert_eq!(RtcError::new(RtcErrorType::SyntaxError).message(), "");
    let factories: [(fn(&str) -> RtcError, RtcErrorType); 11] = [
        (
            |message| RtcError::invalid_parameter(message),
            RtcErrorType::InvalidParameter,
        ),
        (
            |message| RtcError::invalid_state(message),
            RtcErrorType::InvalidState,
        ),
        (
            |message| RtcError::invalid_modification(message),
            RtcErrorType::InvalidModification,
        ),
        (
            |message| RtcError::unsupported_operation(message),
            RtcErrorType::UnsupportedOperation,
        ),
        (
            |message| RtcError::unsupported_parameter(message),
            RtcErrorType::UnsupportedParameter,
        ),
        (
            |message| RtcError::invalid_range(message),
            RtcErrorType::InvalidRange,
        ),
        (
            |message| RtcError::syntax_error(message),
            RtcErrorType::SyntaxError,
        ),
        (
            |message| RtcError::network_error(message),
            RtcErrorType::NetworkError,
        ),
        (
            |message| RtcError::resource_exhausted(message),
            RtcErrorType::ResourceExhausted,
        ),
        (
            |message| RtcError::internal_error(message),
            RtcErrorType::InternalError,
        ),
        (
            |message| RtcError::operation_error_with_data(message),
            RtcErrorType::OperationErrorWithData,
        ),
    ];
    for (factory, kind) in factories {
        let error = factory("exact message");
        assert_eq!(error.kind(), kind);
        assert_eq!(error.message(), "exact message");
        assert!(!error.is_ok());
        assert_eq!(error.error_detail(), RtcErrorDetailType::None);
        assert_eq!(error.sctp_cause_code(), None);
        assert_eq!(
            error.native_string(),
            Some(format!(
                "{} with message: \"exact message\"",
                kind.native_name().unwrap()
            ))
        );
    }
    let mut error = RtcError::with_message(RtcErrorType::OperationErrorWithData, "before");
    error.set_error_detail(RtcErrorDetailType::SctpFailure);
    error.set_sctp_cause_code(0);
    error.set_message("after");
    error.append_text(" append");
    assert_eq!(error.message(), "after append");
    assert_eq!(error.error_detail(), RtcErrorDetailType::SctpFailure);
    assert_eq!(error.sctp_cause_code(), Some(0));
    let clone = error.clone();
    error.set_kind(RtcErrorType::None);
    assert!(error.is_ok());
    assert_eq!(clone.kind(), RtcErrorType::OperationErrorWithData);
    error.clone_from(&clone);
    assert_eq!(error.sctp_cause_code(), Some(0));
    let moved = clone.with_appended_text(" rvalue");
    error.move_assign(moved);
    assert_eq!(error.message(), "after append rvalue");
    assert_eq!(error.error_detail(), RtcErrorDetailType::SctpFailure);
    assert_eq!(error.sctp_cause_code(), Some(0));
    assert_eq!(
        RtcErrorDetailType::SctpFailure.native_name(),
        Some("SCTP_FAILURE".into())
    );
}

#[test]
fn unknown_error_discriminants_preserved_without_unsafe_string_conversion() {
    let mut error = RtcError::with_message(RtcErrorType::Other(-1), "kept");
    error.set_error_detail(RtcErrorDetailType::Other(i32::MAX));
    assert_eq!(error.kind(), RtcErrorType::Other(-1));
    assert_eq!(error.error_detail(), RtcErrorDetailType::Other(i32::MAX));
    assert_eq!(error.message(), "kept");
    assert_eq!(error.native_string(), None);
    assert_eq!(error.kind().native_name(), None);
    assert_eq!(error.error_detail().native_name(), None);
    assert_eq!(RtcErrorType::Other(i32::MAX).native_name(), None);
    assert_eq!(RtcErrorDetailType::Other(-1).native_name(), None);
    error.set_kind(RtcErrorType::NetworkError);
    // Native RTCError AbslStringify consumes type only, not error_detail. An
    // unknown detail need not reject this safe native formatting operation.
    assert_eq!(
        error.native_string(),
        Some("NETWORK_ERROR with message: \"kept\"".into())
    );
    assert_eq!(error.error_detail(), RtcErrorDetailType::Other(i32::MAX));
}

#[test]
fn native_borrowed_message_builder_and_non_utf8_bytes() {
    let mut error = RtcError::network_error("old");
    {
        let mut builder = error.string_builder();
        assert_eq!(builder.as_bytes(), b"old");
        builder.clear();
        assert!(builder.is_empty());
        builder.append_text("native ");
        builder.append_i32(-7);
        builder.append_byte(b' ');
        builder.append_u32(8);
        builder.append_byte(b' ');
        builder.append_i64(i64::MIN);
        builder.append_byte(b' ');
        builder.append_u64(u64::MAX);
        builder.append_byte(b' ');
        builder.append_native_long(-9).unwrap();
        builder.append_byte(b' ');
        builder.append_native_unsigned_long(10).unwrap();
        builder.append_byte(b' ');
        builder.append_f32(1.5);
        builder.append_byte(b' ');
        builder.append_f64(2.5);
        assert_eq!(
            builder.as_bytes(),
            b"native -7 8 -9223372036854775808 18446744073709551615 -9 10 1.5 2.5"
        );
        let released = builder.release();
        assert_eq!(
            released,
            b"native -7 8 -9223372036854775808 18446744073709551615 -9 10 1.5 2.5"
        );
        // Do not impose reset semantics on native moved-from storage.
        assert_eq!(builder.len(), builder.as_bytes().len());
        builder.clear();
        builder.append_byte(0xff);
        builder.append_byte(0);
        builder.append_text("after NUL");
        assert_eq!(builder.as_bytes(), b"\xff\0after NUL");
        if cfg!(target_os = "windows") {
            let before = builder.as_bytes().to_vec();
            assert!(builder.append_native_long(i64::MAX).is_err());
            assert!(builder.append_native_unsigned_long(u64::MAX).is_err());
            assert_eq!(builder.as_bytes(), before);
        }
    }
    assert_eq!(error.message_bytes(), vec![0xff]);
    assert_eq!(error.message(), "\u{fffd}");
    assert_eq!(
        error.native_string_bytes(),
        Some(b"NETWORK_ERROR with message: \"\xff\0after NUL\"".to_vec())
    );
    assert_eq!(error.clone().message_bytes(), vec![0xff]);
    let owned = error.string_builder().to_owned();
    error.set_message("replaced");
    assert_eq!(owned.as_bytes(), b"\xff\0after NUL");
    error.string_builder().assign_from(&owned);
    assert_eq!(error.message_bytes(), vec![0xff]);
}

#[test]
fn native_rvalue_scalar_streams_retain_error_metadata() {
    let mut error = RtcError::operation_error_with_data(b"start");
    error.set_error_detail(RtcErrorDetailType::Other(-9));
    error.set_sctp_cause_code(7);
    let mut error = error
        .with_appended_i32(-1)
        .with_appended_byte(b' ')
        .with_appended_u32(2)
        .with_appended_byte(b' ')
        .with_appended_i64(-3)
        .with_appended_byte(b' ')
        .with_appended_u64(4)
        .with_appended_byte(b' ')
        .with_appended_native_long(-5)
        .unwrap()
        .with_appended_byte(b' ')
        .with_appended_native_unsigned_long(6)
        .unwrap()
        .with_appended_byte(b' ')
        .with_appended_f32(1.5)
        .with_appended_byte(b' ')
        .with_appended_f64(2.5)
        .with_appended_byte(b' ')
        .with_appended_bool(true)
        .with_appended_byte(0xff);
    assert_eq!(error.kind(), RtcErrorType::OperationErrorWithData);
    assert_eq!(error.error_detail(), RtcErrorDetailType::Other(-9));
    assert_eq!(error.sctp_cause_code(), Some(7));
    assert_eq!(
        error.string_builder().as_bytes(),
        b"start-1 2 -3 4 -5 6 1.5 2.5 1\xff"
    );
}

#[test]
fn native_owned_string_builder_construction_and_assignment() {
    use pulsebeam_webrtc_sys::StringBuilder;
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<StringBuilder>();
    let mut value = StringBuilder::new(b"\xff\0body");
    let copy = value.clone();
    value.clear();
    assert!(value.is_empty());
    assert_eq!(copy.as_bytes(), b"\xff\0body");
    value.clone_from(&copy);
    assert_eq!(value.as_bytes(), b"\xff\0body");
    assert_eq!(value.release(), b"\xff\0body");
    assert_eq!(value.len(), value.as_bytes().len());
    value.clear();
    value.append_text("actual ");
    value.append_i32(-1);
    value.append_byte(b' ');
    value.append_u32(2);
    value.append_byte(b' ');
    value.append_i64(-3);
    value.append_byte(b' ');
    value.append_u64(4);
    value.append_byte(b' ');
    value.append_native_long(-5).unwrap();
    value.append_byte(b' ');
    value.append_native_unsigned_long(6).unwrap();
    value.append_byte(b' ');
    value.append_f32(1.5);
    value.append_byte(b' ');
    value.append_f64(2.5);
    assert_eq!(value.as_bytes(), b"actual -1 2 -3 4 -5 6 1.5 2.5");
    value.append_bytes(b"\xff\0end");
    assert!(value.as_bytes().ends_with(b"\xff\0end"));
    assert!(StringBuilder::default().is_empty());
    let raw = RtcError::invalid_state(b"\xfe\0extra");
    assert_eq!(raw.message_bytes(), vec![0xfe]);
    let streamed = raw.with_appended_bytes(b"\xff");
    assert!(
        streamed
            .native_string_bytes()
            .unwrap()
            .ends_with(b"\xfe\0extra\xff\"")
    );
}

#[test]
fn native_c_string_message_accessor_does_not_invent_full_length_semantics() {
    let error = RtcError::with_message(RtcErrorType::NetworkError, "left\0right");
    assert_eq!(error.message(), "left");
    assert_eq!(error.clone().message(), "left");
    assert_eq!(
        error.native_string(),
        Some("NETWORK_ERROR with message: \"left\0right\"".into())
    );
}

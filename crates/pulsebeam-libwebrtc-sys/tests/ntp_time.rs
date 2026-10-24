use pulsebeam_webrtc_sys::{NtpTime, Q32x32, Uq32x32};

#[test]
fn owned_ntp_bits_validity_and_native_rounding() {
    let mut value = NtpTime::default();
    assert!(!value.is_valid());
    assert_eq!(value.millis(), 0);
    assert_eq!(value.utc_microseconds(), None);
    assert!(value.utc_timestamp().is_minus_infinity());
    value.set(1, 0x8000_0000);
    assert_eq!(value.raw(), 0x0000_0001_8000_0000);
    assert_eq!(value.seconds(), 1);
    assert_eq!(value.fractions(), 0x8000_0000);
    assert_eq!(value.millis(), 1_500);
    let retained = value;
    value.reset();
    assert!(!value.is_valid());
    assert!(retained.is_valid());
    assert_eq!(NtpTime::from_parts(1, u32::MAX).millis(), 2_000);
    let maximum = NtpTime::from_raw(u64::MAX);
    assert_eq!(maximum.seconds(), u32::MAX);
    assert_eq!(maximum.fractions(), u32::MAX);
    assert_eq!(maximum.millis(), 4_294_967_296_000);
    assert!(maximum.utc_microseconds().is_some());
}

#[test]
fn utc_preserves_pinned_unsigned_epoch_subtraction() {
    assert_eq!(
        NtpTime::from_parts(2_208_988_800, 0).utc_microseconds(),
        Some(0)
    );
    assert_eq!(
        NtpTime::from_parts(2_208_988_800, 0x8000_0000).utc_microseconds(),
        Some(500_000)
    );
    // These are the pinned native wrap semantics, not a corrected era-aware
    // interpretation of a pre-1970 timestamp.
    assert_eq!(
        NtpTime::from_parts(0, 0x8000_0000).utc_microseconds(),
        Some(2_085_978_496_500_000)
    );
}

#[test]
fn all_fixed_point_conversion_helpers_preserve_sign_rounding_and_clamps() {
    let positive = Q32x32::from_millis(500);
    assert_eq!(positive.raw(), 0x8000_0000);
    assert_eq!(positive.millis(), 500);
    assert_eq!(positive.microseconds(), 500_000);
    let negative = Q32x32::from_millis(-500);
    assert_eq!(negative.raw(), -0x8000_0000);
    assert_eq!(negative.millis(), -500);
    assert_eq!(negative.microseconds(), -500_000);
    assert_eq!(Q32x32::from_millis(i64::MIN).raw(), i64::MIN);
    assert_eq!(Q32x32::from_millis(i64::MAX).raw(), i64::MAX);
    assert_eq!(Q32x32::from_raw(-1).microseconds(), 0);
    assert_eq!(Uq32x32::from_millis(-500).raw(), 0);
    assert_eq!(Uq32x32::from_millis(i64::MAX).raw(), u64::MAX);
    assert_eq!(Uq32x32::from_millis(500).microseconds(), 500_000);
    assert_eq!(Uq32x32::from_raw(u64::MAX).millis(), 4_294_967_296_000);
    assert_eq!(
        Uq32x32::from_raw(u64::MAX).microseconds(),
        4_294_967_296_000_000
    );
}

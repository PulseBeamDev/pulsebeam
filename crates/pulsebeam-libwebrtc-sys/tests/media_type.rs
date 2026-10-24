use pulsebeam_webrtc_sys::MediaType;

#[test]
fn native_media_type_names_preserve_converter_domain() {
    assert_eq!(MediaType::Audio.native_name().as_deref(), Some("audio"));
    assert_eq!(MediaType::Video.native_name().as_deref(), Some("video"));
    assert_eq!(MediaType::Data.native_name().as_deref(), Some("data"));
    assert_eq!(MediaType::Unsupported.native_name(), None);
    assert_eq!(MediaType::Any.native_name(), None);
    assert_eq!(MediaType::Other(-9).native_name(), None);
    for (kind, expected) in [
        (MediaType::Audio, Some("audio")),
        (MediaType::Video, Some("video")),
        (MediaType::Data, Some("data")),
        (MediaType::Unsupported, None),
        (MediaType::Any, None),
        (MediaType::Other(-9), None),
    ] {
        assert_eq!(kind.native_string().as_deref(), expected);
    }
}

use pulsebeam_webrtc_sys::{
    DtlsTransportInformation, DtlsTransportState, SctpTransportInformation, SctpTransportState,
};

#[test]
fn native_information_defaults_and_state_only_constructors() {
    let dtls = DtlsTransportInformation::default();
    assert_eq!(dtls.state(), DtlsTransportState::New);
    assert_eq!(dtls.role(), None);
    assert_eq!(dtls.tls_version(), None);
    assert_eq!(dtls.ssl_cipher_suite(), None);
    assert_eq!(dtls.srtp_cipher_suite(), None);
    assert_eq!(dtls.ssl_group_id(), None);
    assert!(dtls.remote_ssl_certificates().is_none());
    assert_eq!(dtls.remote_certificate_data(), None);
    let dtls = DtlsTransportInformation::new(DtlsTransportState::Failed);
    assert_eq!(dtls.clone().state(), DtlsTransportState::Failed);
    assert!(dtls.remote_ssl_certificates().is_none());
    assert_eq!(dtls.remote_certificate_data(), None);

    let sctp = SctpTransportInformation::default();
    assert_eq!(sctp.state(), SctpTransportState::New);
    assert!(sctp.dtls_transport().is_none());
    assert_eq!(sctp.max_message_size(), None);
    assert_eq!(sctp.max_channels(), None);
    let sctp = SctpTransportInformation::new(SctpTransportState::Closed);
    assert_eq!(sctp.clone().state(), SctpTransportState::Closed);
    assert!(sctp.dtls_transport().is_none());
    assert_eq!(sctp.max_message_size(), None);
    assert_eq!(sctp.max_channels(), None);
}

#[test]
fn native_information_full_constructors_assignment_and_optional_presence() {
    use pulsebeam_webrtc_sys::{DtlsTransportRole, TlsCertificateChain};
    let value = DtlsTransportInformation::from_parts(
        DtlsTransportState::Connected,
        Some(DtlsTransportRole::Server),
        Some(0),
        Some(0),
        Some(0),
        Some(TlsCertificateChain::default()),
        Some(0),
    );
    assert_eq!(value.role(), Some(DtlsTransportRole::Server));
    assert_eq!(value.tls_version(), Some(0));
    assert_eq!(value.ssl_cipher_suite(), Some(0));
    assert_eq!(value.srtp_cipher_suite(), Some(0));
    assert_eq!(value.ssl_group_id(), Some(0));
    assert!(value.remote_ssl_certificates().unwrap().is_empty());
    assert_eq!(value.remote_certificate_data(), Some(Vec::new()));
    let mut assigned = DtlsTransportInformation::default();
    assigned.clone_from(&value);
    assert_eq!(assigned.state(), DtlsTransportState::Connected);
    assert!(assigned.remote_ssl_certificates().unwrap().is_empty());
    assigned.clone_from(&DtlsTransportInformation::default());
    assert!(assigned.remote_ssl_certificates().is_none());
    assert!(value.remote_ssl_certificates().unwrap().is_empty());
    assigned.move_assign(value);
    assert_eq!(assigned.role(), Some(DtlsTransportRole::Server));
    assert!(assigned.remote_ssl_certificates().unwrap().is_empty());

    let value = SctpTransportInformation::from_parts(
        SctpTransportState::Connected,
        None,
        Some(0.0),
        Some(0),
    );
    assert!(value.dtls_transport().is_none());
    assert_eq!(value.max_message_size(), Some(0.0));
    assert_eq!(value.max_channels(), Some(0));
    let mut assigned = SctpTransportInformation::default();
    assigned.clone_from(&value);
    assert_eq!(assigned.max_message_size(), Some(0.0));
    assert_eq!(assigned.max_channels(), Some(0));
    assert_eq!(SctpTransportState::Connected.native_string(), "Connected");
    assert_eq!(
        SctpTransportState::Other(-1).native_string(),
        "illegal state -1"
    );
    assert_eq!(assigned.native_string(), value.native_string());
}

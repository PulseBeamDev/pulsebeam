use pulsebeam_webrtc_sys::*;

#[test]
fn crypto_profile_queries_preserve_native_preferences_and_reject_abort_case() {
    use DtlsSrtpCipherSuite as Suite;
    assert_eq!(
        CryptoOptions::default()
            .supported_dtls_srtp_cipher_suites()
            .unwrap(),
        [
            Suite::AES128_CM_SHA1_80,
            Suite::AEAD_AES256_GCM,
            Suite::AEAD_AES128_GCM
        ]
    );
    assert_eq!(
        CryptoOptions::no_gcm()
            .supported_dtls_srtp_cipher_suites()
            .unwrap(),
        [Suite::AES128_CM_SHA1_80]
    );
    assert_eq!(
        CryptoOptions::prefer_gcm()
            .supported_dtls_srtp_cipher_suites()
            .unwrap(),
        [
            Suite::AEAD_AES256_GCM,
            Suite::AEAD_AES128_GCM,
            Suite::AES128_CM_SHA1_80
        ]
    );
    let mut options = CryptoOptions::no_gcm();
    options.srtp.enable_aes128_sha1_80_crypto_cipher = false;
    assert_eq!(
        options
            .supported_dtls_srtp_cipher_suites()
            .unwrap_err()
            .kind,
        PeerErrorKind::InvalidParameter
    );
    options.srtp.enable_aes128_sha1_32_crypto_cipher = true;
    assert_eq!(
        options.supported_dtls_srtp_cipher_suites().unwrap(),
        [Suite::AES128_CM_SHA1_32]
    );
}

#[test]
fn crypto_group_queries_use_the_native_binary_and_values_remain_open() {
    let supported = KeyExchangeGroup::supported();
    assert!(!supported.is_empty());
    assert!(supported.contains(&KeyExchangeGroup::X25519));
    for group in supported {
        assert!(group.name().is_some(), "{group:?}");
    }
    let unknown = KeyExchangeGroup::from_id(u16::MAX);
    assert_eq!(unknown.id(), u16::MAX);
    assert_eq!(unknown.name(), None);
    let mut groups = KeyExchangeGroups::new(vec![
        KeyExchangeGroup::SECP384R1,
        KeyExchangeGroup::X25519,
        KeyExchangeGroup::SECP384R1,
        KeyExchangeGroup::X25519,
        unknown,
    ]);
    groups.add_first(KeyExchangeGroup::X25519);
    assert_eq!(
        groups.enabled(),
        [
            KeyExchangeGroup::X25519,
            KeyExchangeGroup::SECP384R1,
            KeyExchangeGroup::SECP384R1,
            unknown,
        ]
    );
    groups.set_enabled(Vec::new());
    assert!(groups.enabled().is_empty());
}

#[test]
fn crypto_group_update_applies_native_environment_and_disabled_selection() {
    let environment = Environment::builder().build().unwrap();
    let unknown = KeyExchangeGroup::from_id(u16::MAX);
    let mut groups = KeyExchangeGroups::new(vec![
        unknown,
        KeyExchangeGroup::X25519,
        KeyExchangeGroup::X25519,
    ]);
    // Native Update removes supported disabled groups from both its defaults
    // and the existing selection, then deduplicates retained values. It does
    // not silently impose a binding allowlist on unknown identifiers.
    groups.update(&environment, &KeyExchangeGroup::supported());
    assert_eq!(groups.enabled(), [unknown]);
    groups.update(&environment, &[]);
    assert!(groups.enabled().contains(&unknown));
    assert!(groups.enabled().contains(&KeyExchangeGroup::X25519));
    groups.update(&environment, &[unknown]);
    assert!(!groups.enabled().contains(&unknown));
}

#[test]
fn crypto_values_can_be_prepared_before_actor_migration() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CryptoOptions>();
    assert_send_sync::<KeyExchangeGroups>();
    assert_send_sync::<SrtpCryptoOptions>();
}

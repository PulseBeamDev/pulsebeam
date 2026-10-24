use pulsebeam_webrtc_sys::*;
use std::num::NonZeroUsize;

#[test]
fn owned_tls_certificate_chain_digest_and_stats_preserve_native_values() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<TlsCertificate>();
    send_sync::<TlsCertificateChain>();
    send_sync::<TlsCertificateStats>();
    assert!(TlsCertificate::from_pem("invalid certificate").is_none());
    let identity = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    let certificate = TlsCertificate::from_pem(&identity.to_pem().certificate).unwrap();
    let native = identity.chain().remove(0);
    assert_eq!(certificate.to_der(), native.der);
    assert_eq!(
        certificate.signature_digest_algorithm(),
        native.signature_digest_algorithm
    );
    assert_eq!(
        certificate.certificate_expiration_time(),
        native.expiration_unix_seconds.unwrap_or(-1)
    );
    for algorithm in [
        "md5", "sha-1", "sha-224", "sha-256", "sha-384", "sha-512", "unknown", "", "SHA-256",
    ] {
        let expected = identity
            .fingerprint(algorithm)
            .map(|fingerprint| fingerprint.digest);
        assert_eq!(certificate.digest(algorithm), expected, "{algorithm}");
        assert!(
            certificate
                .compute_digest(algorithm, NonZeroUsize::new(1).unwrap())
                .is_none()
        );
    }
    assert_eq!(
        certificate.compute_digest("sha-256", NonZeroUsize::new(usize::MAX).unwrap()),
        certificate.digest("sha-256")
    );
    let leaf_stats = certificate.stats().unwrap();
    assert!(leaf_stats.issuer.is_none());
    assert_eq!(
        leaf_stats.fingerprint,
        identity
            .fingerprint(&leaf_stats.fingerprint_algorithm)
            .unwrap()
            .value
    );
    let other = TlsCertificate::from_pem(
        &Certificate::generate(CertificateKeyParams::default(), None)
            .unwrap()
            .to_pem()
            .certificate,
    )
    .unwrap();
    let other_der = other.to_der();
    let chain = TlsCertificateChain::new(vec![certificate.clone(), other]);
    assert_eq!(chain.len(), 2);
    assert_eq!(chain.get(0).unwrap().to_der(), native.der);
    assert_eq!(chain.get(1).unwrap().to_der(), other_der);
    assert!(chain.get(2).is_none());
    assert!(chain.get(usize::MAX).is_none());
    let mut stats = chain.stats().unwrap();
    assert_eq!(stats.fingerprint, leaf_stats.fingerprint);
    assert!(stats.issuer.is_some());
    assert!(stats.issuer.as_ref().unwrap().issuer.is_none());
    let copy = stats.copy();
    stats.issuer.as_mut().unwrap().fingerprint.clear();
    assert_ne!(stats, copy);
    let retained = chain.clone();
    drop(chain);
    drop(identity);
    drop(certificate);
    std::thread::spawn(move || {
        assert_eq!(retained.get(0).unwrap().to_der(), native.der);
        assert_eq!(retained.stats(), Some(copy));
        let empty = TlsCertificateChain::default();
        assert!(empty.is_empty());
        assert!(empty.get(0).is_none());
        assert!(empty.clone().stats().is_none());
    })
    .join()
    .unwrap();
}

#[test]
fn legacy_verification_default_uses_leaf_and_rejects_empty_chain() {
    struct Legacy(Vec<u8>);
    impl TlsCertificateVerifier for Legacy {
        fn verify(&self, certificate: &TlsCertificate) -> bool {
            certificate.to_der() == self.0
        }
    }
    let certificate = TlsCertificate::from_pem(
        &Certificate::generate(CertificateKeyParams::default(), None)
            .unwrap()
            .to_pem()
            .certificate,
    )
    .unwrap();
    let verifier = Legacy(certificate.to_der());
    assert!(verifier.verify_chain(&TlsCertificateChain::from_certificate(certificate)));
    assert!(!verifier.verify_chain(&TlsCertificateChain::default()));
}

#[test]
fn selected_and_custom_tls_verifiers_cannot_silently_replace_one_another() {
    struct Reject;
    impl TlsCertificateVerifier for Reject {}
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .build()
        .unwrap();
    let result = session.create_peer_with_dependencies(
        RtcConfiguration {
            turn_tls_ca_pem: Some("not a valid CA".into()),
            ..RtcConfiguration::default()
        },
        PeerConnectionDependencies::default().tls_certificate_verifier(Reject),
    );
    assert_eq!(result.unwrap_err().kind, PeerErrorKind::InvalidParameter);
    session.shutdown().unwrap();
}

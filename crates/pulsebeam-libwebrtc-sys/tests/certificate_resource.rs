use pulsebeam_webrtc_sys::*;
use std::time::Duration;

#[test]
fn certificate_key_parameter_queries_follow_native_validation() {
    assert_eq!(
        CertificateKeyParams::default(),
        CertificateKeyParams::Ecdsa {
            curve: CertificateCurve::P256,
        }
    );
    assert_eq!(
        CertificateKeyParams::rsa_default(),
        CertificateKeyParams::Rsa {
            modulus_bits: 2048,
            public_exponent: 65537,
        }
    );
    assert!(CertificateKeyParams::default().is_valid());
    assert!(CertificateKeyParams::rsa_default().is_valid());
    // Do not replace the native unsigned exponent domain with a binding cap.
    assert!(
        CertificateKeyParams::Rsa {
            modulus_bits: 1024,
            public_exponent: u32::MAX,
        }
        .is_valid()
    );
    let invalid = CertificateKeyParams::Rsa {
        modulus_bits: 2048,
        public_exponent: 3,
    };
    assert!(!invalid.is_valid());
    assert_eq!(
        Certificate::generate(invalid, None).unwrap_err().kind,
        PeerErrorKind::InvalidParameter
    );
}

#[test]
fn certificate_resource_preserves_native_identity_value_and_chain() {
    let certificate = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    assert!(certificate.same_resource(&certificate.clone()));
    let pem = certificate.to_pem();
    assert!(!pem.private_key.is_empty());
    assert!(!format!("{pem:?}").contains("PRIVATE KEY"));
    let imported = Certificate::from_pem(&pem).unwrap();
    assert_eq!(certificate, imported);
    assert!(!certificate.same_resource(&imported));
    let expires = certificate.expires_at_unix_millis();
    assert!(!certificate.has_expired_at_unix_millis(expires - 1));
    assert!(certificate.has_expired_at_unix_millis(expires));
    let fingerprint = certificate.fingerprint("sha-256").unwrap();
    assert_eq!(fingerprint.algorithm, "sha-256");
    assert_eq!(fingerprint.digest.len(), 32);
    assert_eq!(
        fingerprint.value,
        fingerprint
            .digest
            .iter()
            .map(|value| format!("{value:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    );
    assert!(certificate.fingerprint("not-a-native-algorithm").is_none());
    assert!(!certificate.chain()[0].der.is_empty());
    assert!(certificate.chain()[0].expiration_unix_seconds.is_some());
    let chain_pem = CertificatePem {
        private_key: pem.private_key,
        certificate: format!("{}{}", pem.certificate, pem.certificate),
    };
    let chain = Certificate::from_pem_chain(&chain_pem).unwrap();
    assert_eq!(chain.chain().len(), 2);
    assert_eq!(chain.to_pem().certificate, pem.certificate);
    drop(certificate);
    assert!(imported.fingerprint("sha-256").is_some());
    assert_eq!(
        Certificate::from_pem(&CertificatePem {
            private_key: "invalid".into(),
            certificate: "invalid".into(),
        })
        .unwrap_err()
        .kind,
        PeerErrorKind::InvalidParameter
    );
}

#[test]
fn certificate_lifetime_uses_native_cap_without_binding_rejection() {
    let default = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    let capped =
        Certificate::generate(CertificateKeyParams::default(), Some(Duration::MAX)).unwrap();
    assert!(
        capped.expires_at_unix_millis() - default.expires_at_unix_millis()
            > Duration::from_secs(300 * 24 * 60 * 60).as_millis() as u64
    );
}

#[test]
fn certificate_immutable_queries_and_final_release_can_cross_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Certificate>();
    let certificate = Certificate::generate(CertificateKeyParams::rsa_default(), None).unwrap();
    let fingerprint = certificate.fingerprint("sha-256").unwrap();
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let certificate = certificate.clone();
            let fingerprint = fingerprint.clone();
            std::thread::spawn(move || {
                for _ in 0..16 {
                    assert_eq!(certificate.fingerprint("sha-256").unwrap(), fingerprint);
                    assert!(!certificate.to_pem().private_key.is_empty());
                    assert_eq!(certificate.chain().len(), 1);
                    assert_eq!(certificate, certificate.clone());
                }
            })
        })
        .collect();
    // At least one worker releases the final native wrapper reference.
    drop(certificate);
    for worker in workers {
        worker.join().unwrap();
    }
}

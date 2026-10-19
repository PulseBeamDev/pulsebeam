//! Real DTLS with fixed test identity and entropy routed through sim_rand.
//! AWS-LC's hardware entropy bypasses the process libc shim. Only this isolated
//! bounded case selects RustCrypto inside dimpl; production providers are intact.
use std::{sync::OnceLock, time::Instant};
use str0m::crypto::dtls::{DtlsCert, DtlsInstance, DtlsProvider, DtlsVersion};
use str0m::crypto::{CryptoError, CryptoProvider};

#[derive(Debug)]
struct FixedIdentity(&'static dyn DtlsProvider);

impl DtlsProvider for FixedIdentity {
    fn generate_certificate(&self) -> Option<DtlsCert> {
        Some(DtlsCert {
            certificate: include_bytes!("fixtures/cert.der").to_vec(),
            private_key: include_bytes!("fixtures/key.der").to_vec(),
        })
    }

    fn new_dtls(
        &self,
        cert: &DtlsCert,
        now: Instant,
        version: DtlsVersion,
        mtu: Option<usize>,
    ) -> Result<Box<dyn DtlsInstance>, CryptoError> {
        self.0.new_dtls(cert, now, version, mtu)
    }
}

#[test]
fn dtls_entropy_is_seeded_in_the_simulator_configuration() {
    let provider = dimpl::crypto::rust_crypto::default_provider();
    let draw = |seed| {
        crate::sim_rand::set_thread_rng(seed);
        let mut bytes = [0; 32];
        provider.secure_random.fill(&mut bytes).unwrap();
        bytes
    };
    assert_eq!(draw(7), draw(7));
    assert_ne!(draw(7), draw(8));
    crate::sim_rand::clear_thread_rng();
}

pub(super) fn install() {
    static IDENTITY: OnceLock<FixedIdentity> = OnceLock::new();
    dimpl::crypto::CryptoProvider::install_default(dimpl::crypto::rust_crypto::default_provider());
    let mut provider = str0m::crypto::from_feature_flags();
    provider.dtls_provider = IDENTITY.get_or_init(|| FixedIdentity(provider.dtls_provider));
    CryptoProvider::install_default(provider);
}

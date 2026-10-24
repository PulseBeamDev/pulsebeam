use crate::{ffi, provider_panic::catch_provider};
use std::{
    fmt,
    num::NonZeroUsize,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Deeply owned native certificate statistics, including the issuer chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TlsCertificateStats {
    pub fingerprint: String,
    pub fingerprint_algorithm: String,
    pub base64_certificate: String,
    pub issuer: Option<Box<TlsCertificateStats>>,
}
impl TlsCertificateStats {
    pub fn copy(&self) -> Self {
        self.clone()
    }
    fn from_native(values: Vec<ffi::FfiTlsCertificateStats>) -> Option<Self> {
        let mut issuer = None;
        for value in values.into_iter().rev() {
            issuer = Some(Box::new(Self {
                fingerprint: value.fingerprint,
                fingerprint_algorithm: value.fingerprint_algorithm,
                base64_certificate: value.base64_certificate,
                issuer,
            }));
        }
        issuer.map(|value| *value)
    }
}

/// Owning immutable certificate, independent of a verifier callback's lifetime.
/// Native Clone retains the same immutable certificate bytes, not an identity
/// or mutable SSL session. This leaf is Send + Sync only for the compile-time
/// qualified pinned BoringSSL backend; no backend mutation is exported.
pub struct TlsCertificate {
    native: cxx::UniquePtr<ffi::NativeTlsCertificate>,
}
// SAFETY: native constructors and SDK verifier inputs produce BoringSSLCertificate
// owners. Clone retains immutable, concurrently reference-counted CRYPTO_BUFFER
// bytes. Every exposed operation reads those bytes or creates a new owner. The
// adapter rejects other backends at compile time; raw pointers are not exported.
unsafe impl Send for TlsCertificate {}
unsafe impl Sync for TlsCertificate {}
impl Clone for TlsCertificate {
    fn clone(&self) -> Self {
        Self::from_native(ffi::tls_certificate_clone(self.native()))
            .expect("native certificate clone")
    }
}
impl fmt::Debug for TlsCertificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsCertificate")
            .field("pem", &self.to_pem_string())
            .finish()
    }
}
impl TlsCertificate {
    pub fn from_pem(pem: &str) -> Option<Self> {
        Self::from_native(ffi::tls_certificate_from_pem(pem))
    }
    pub fn to_pem_string(&self) -> String {
        ffi::tls_certificate_pem(self.native())
    }
    pub fn to_der(&self) -> Vec<u8> {
        ffi::tls_certificate_der(self.native())
    }
    pub fn signature_digest_algorithm(&self) -> Option<String> {
        let mut valid = false;
        let value = ffi::tls_certificate_signature_digest(self.native(), &mut valid);
        valid.then_some(value)
    }
    /// Preserves native failure for unsupported algorithms and insufficient
    /// capacity. Zero capacity is a native DCHECK violation and is unrepresentable.
    pub fn compute_digest(&self, algorithm: &str, capacity: NonZeroUsize) -> Option<Vec<u8>> {
        let mut valid = false;
        let value =
            ffi::tls_certificate_digest(self.native(), algorithm, capacity.get(), &mut valid);
        valid.then_some(value)
    }
    pub fn digest(&self, algorithm: &str) -> Option<Vec<u8>> {
        self.compute_digest(
            algorithm,
            NonZeroUsize::new(64).expect("maximum native digest capacity"),
        )
    }
    /// Native Unix seconds, including exactly -1 for unavailable expiration.
    pub fn certificate_expiration_time(&self) -> i64 {
        ffi::tls_certificate_expiration(self.native())
    }
    pub fn stats(&self) -> Option<TlsCertificateStats> {
        TlsCertificateStats::from_native(ffi::tls_certificate_stats(self.native()))
    }
    pub(crate) fn native(&self) -> &ffi::NativeTlsCertificate {
        self.native.as_ref().expect("owned certificate")
    }
    fn from_native(native: cxx::UniquePtr<ffi::NativeTlsCertificate>) -> Option<Self> {
        (!native.is_null()).then_some(Self { native })
    }
}

/// Owns a native leaf-first chain. Empty chains are supported; out-of-range
/// access returns None before entering native's unchecked Get accessor.
pub struct TlsCertificateChain {
    native: cxx::UniquePtr<ffi::NativeTlsCertificateChain>,
}
// SAFETY: immutable owning chain of the qualified certificate leaves described
// above. Appends occur only during exclusive construction, never after publication.
unsafe impl Send for TlsCertificateChain {}
unsafe impl Sync for TlsCertificateChain {}
impl Clone for TlsCertificateChain {
    fn clone(&self) -> Self {
        Self {
            native: ffi::tls_chain_clone(self.native()),
        }
    }
}
impl Default for TlsCertificateChain {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}
impl fmt::Debug for TlsCertificateChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsCertificateChain")
            .field("len", &self.len())
            .finish()
    }
}
impl TlsCertificateChain {
    pub fn new(certificates: Vec<TlsCertificate>) -> Self {
        let mut native = ffi::tls_chain_new();
        for certificate in certificates {
            ffi::tls_chain_append(native.pin_mut(), certificate.native);
        }
        Self { native }
    }
    pub fn from_certificate(certificate: TlsCertificate) -> Self {
        Self::new(vec![certificate])
    }
    pub fn len(&self) -> usize {
        ffi::tls_chain_size(self.native())
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn get(&self, index: usize) -> Option<TlsCertificate> {
        TlsCertificate::from_native(ffi::tls_chain_get(self.native(), index))
    }
    pub fn stats(&self) -> Option<TlsCertificateStats> {
        TlsCertificateStats::from_native(ffi::tls_chain_stats(self.native()))
    }
    pub(crate) fn native(&self) -> &ffi::NativeTlsCertificateChain {
        self.native.as_ref().expect("owned certificate chain")
    }
    pub(crate) fn into_native(self) -> cxx::UniquePtr<ffi::NativeTlsCertificateChain> {
        self.native
    }
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeTlsCertificateChain>,
    ) -> Option<Self> {
        (!native.is_null()).then_some(Self { native })
    }
}

/// An owning reference to the exact verifier supplied to a native peer. Socket
/// creation options retain this owner, not a pointer borrowed from the factory
/// or peer. It remains usable after socket/peer retirement. Verification is
/// synchronous, shares the original failure latch and contains provider panics.
#[derive(Clone)]
pub struct SocketTlsVerifier(cxx::SharedPtr<ffi::NativeTlsVerifier>);
// The backing contains either a Send + Sync Rust verifier/atomic failure latch,
// or the binding's internal TURN-CA verifier with serialized native-only calls.
// Both own their inputs/dependencies; no SSL session, role or socket is retained.
// Foreign C++ verifier implementations cannot enter this private origin path.
unsafe impl Send for SocketTlsVerifier {}
unsafe impl Sync for SocketTlsVerifier {}
impl SocketTlsVerifier {
    pub(crate) fn from_native(value: cxx::SharedPtr<ffi::NativeTlsVerifier>) -> Option<Self> {
        (!value.is_null()).then_some(Self(value))
    }
    pub fn verify(&self, certificate: &TlsCertificate) -> bool {
        ffi::tls_verifier_verify(self.0.as_ref().unwrap(), certificate.native())
    }
    pub fn verify_chain(&self, chain: &TlsCertificateChain) -> bool {
        ffi::tls_verifier_verify_chain(self.0.as_ref().unwrap(), chain.native())
    }
    pub fn has_failed(&self) -> bool {
        ffi::tls_verifier_failed(self.0.as_ref().unwrap())
    }
}

/// Synchronous native TLS verification. Callbacks normally run on the network
/// sequence and must not wait for/reenter the owning application actor. Values
/// are retained owners, not callback-scoped references; clones can leave the
/// callback or cross application threads. A panic permanently rejects future
/// verification. Destructors must not panic.
///
/// Override either legacy verify or full verify_chain. Native's default chain
/// behavior falls back to legacy verification of the first leaf. The safe default
/// legacy implementation rejects rather than entering native CHECK_NOTREACHED.
pub trait TlsCertificateVerifier: Send + Sync + 'static {
    fn verify(&self, _certificate: &TlsCertificate) -> bool {
        false
    }
    fn verify_chain(&self, chain: &TlsCertificateChain) -> bool {
        chain
            .get(0)
            .is_some_and(|certificate| self.verify(&certificate))
    }
}
impl<T: TlsCertificateVerifier + ?Sized> TlsCertificateVerifier for Arc<T> {
    fn verify(&self, certificate: &TlsCertificate) -> bool {
        (**self).verify(certificate)
    }
    fn verify_chain(&self, chain: &TlsCertificateChain) -> bool {
        (**self).verify_chain(chain)
    }
}
pub(crate) struct RustTlsCertificateVerifier {
    provider: Option<Arc<dyn TlsCertificateVerifier>>,
    failed: AtomicBool,
}
impl Drop for RustTlsCertificateVerifier {
    fn drop(&mut self) {
        let provider = self.provider.take();
        let _ = catch_provider(|| drop(provider));
    }
}
impl RustTlsCertificateVerifier {
    pub(crate) fn new(provider: Option<Arc<dyn TlsCertificateVerifier>>) -> Box<Self> {
        Box::new(Self {
            provider,
            failed: AtomicBool::new(false),
        })
    }
    fn invoke(&self, call: impl FnOnce(&dyn TlsCertificateVerifier) -> bool) -> bool {
        if self.failed.load(Ordering::Acquire) {
            return false;
        }
        let result = catch_provider(AssertUnwindSafe(|| {
            self.provider
                .as_ref()
                .is_some_and(|provider| call(provider.as_ref()))
        }));
        match result {
            Ok(value) => value,
            Err(_) => {
                self.failed.store(true, Ordering::Release);
                false
            }
        }
    }
}
pub(crate) fn rust_tls_certificate_verifier_failed(verifier: &RustTlsCertificateVerifier) -> bool {
    verifier.failed.load(Ordering::Acquire)
}
pub(crate) fn tls_certificate_verifier_present(verifier: &RustTlsCertificateVerifier) -> bool {
    verifier.provider.is_some()
}
pub(crate) fn rust_tls_certificate_verify(
    verifier: &RustTlsCertificateVerifier,
    certificate: cxx::UniquePtr<ffi::NativeTlsCertificate>,
) -> bool {
    TlsCertificate::from_native(certificate)
        .is_some_and(|certificate| verifier.invoke(|provider| provider.verify(&certificate)))
}
pub(crate) fn rust_tls_certificate_verify_chain(
    verifier: &RustTlsCertificateVerifier,
    chain: cxx::UniquePtr<ffi::NativeTlsCertificateChain>,
) -> bool {
    if chain.is_null() {
        return false;
    }
    let chain = TlsCertificateChain { native: chain };
    verifier.invoke(|provider| provider.verify_chain(&chain))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verifier_panic_deactivates_both_native_callback_entries() {
        struct Panic(Arc<std::sync::atomic::AtomicUsize>);
        impl TlsCertificateVerifier for Panic {
            fn verify(&self, _certificate: &TlsCertificate) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("intentional legacy verifier panic")
            }
        }
        let certificate = TlsCertificate::from_pem(
            &crate::Certificate::generate(crate::CertificateKeyParams::default(), None)
                .unwrap()
                .to_pem()
                .certificate,
        )
        .unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let verifier = RustTlsCertificateVerifier::new(Some(Arc::new(Panic(calls.clone()))));
        assert!(!rust_tls_certificate_verify(
            &verifier,
            ffi::tls_certificate_clone(certificate.native())
        ));
        assert!(!rust_tls_certificate_verify(
            &verifier,
            ffi::tls_certificate_clone(certificate.native())
        ));
        let chain = TlsCertificateChain::from_certificate(certificate);
        assert!(!rust_tls_certificate_verify_chain(
            &verifier,
            ffi::tls_chain_clone(chain.native())
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

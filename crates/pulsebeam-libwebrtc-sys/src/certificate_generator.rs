use crate::provider_panic::catch_provider as catch_unwind;
use crate::{Certificate, CertificateKeyParams, ffi};
use std::{
    fmt,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Owned executable native certificate generator. Requests enter on signaling;
/// generation may be delegated to a Rust worker. Do not block or reenter the
/// calling peer/session. Native completion always posts back to signaling.
pub trait CertificateGeneratorProvider: Send + 'static {
    fn generate_certificate_async(
        &mut self,
        params: CertificateKeyParams,
        expires_ms: Option<u64>,
        completion: CertificateCompletion,
    );
}

/// Shareable registration retaining a mutable provider through native peer
/// ownership. Clones serialize requests and share panic diagnostics.
#[derive(Clone)]
pub struct CertificateGenerator(Arc<GeneratorInner>);
struct GeneratorInner {
    provider: Mutex<Box<dyn CertificateGeneratorProvider>>,
    failed: AtomicBool,
}
impl CertificateGenerator {
    pub fn from_provider(provider: impl CertificateGeneratorProvider) -> Self {
        Self(Arc::new(GeneratorInner {
            provider: Mutex::new(Box::new(provider)),
            failed: AtomicBool::new(false),
        }))
    }
    pub fn has_failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
}

/// Move-only native callback ticket, independent of its generator's lifetime.
/// It may cross Rust threads and completes with an owned retained certificate,
/// or None for native generation failure. Delivery is never inline. Dropping an
/// unfinished ticket discards its callback on signaling without invoking it.
/// The factory invalidates remaining tickets before releasing signaling.
pub struct CertificateCompletion(Arc<CompletionInner>);
struct CompletionInner {
    native: cxx::UniquePtr<ffi::NativeCertificateCompletion>,
}
// SAFETY: the opaque native state synchronizes completion/posting/retirement;
// no native callback executes or loses captures on this Rust thread.
unsafe impl Send for CompletionInner {}
unsafe impl Sync for CompletionInner {}
impl CertificateCompletion {
    pub fn complete(
        self,
        certificate: Option<&Certificate>,
    ) -> Result<(), CertificateCompletionError> {
        match self.0.complete(certificate) {
            0 => Ok(()),
            1 => Err(CertificateCompletionError::Retired),
            _ => Err(CertificateCompletionError::AlreadyCompleted),
        }
    }
}
impl CompletionInner {
    fn complete(&self, certificate: Option<&Certificate>) -> u8 {
        let native = self.native.as_ref().expect("owned completion");
        match certificate {
            Some(certificate) => ffi::certificate_complete_success(native, certificate.native()),
            None => ffi::certificate_complete_failure(native),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CertificateCompletionError {
    Retired,
    /// A provider panic already submitted the guarded native failure result.
    AlreadyCompleted,
}
impl fmt::Display for CertificateCompletionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Retired => "the requesting certificate factory has retired",
            Self::AlreadyCompleted => "certificate generation has already completed",
        })
    }
}
impl std::error::Error for CertificateCompletionError {}

pub(crate) struct RustCertificateGenerator(pub(crate) Option<CertificateGenerator>);
impl Drop for RustCertificateGenerator {
    fn drop(&mut self) {
        let handle = self.0.take();
        let _ = catch_unwind(AssertUnwindSafe(|| drop(handle)));
    }
}
pub(crate) fn certificate_generator_present(source: &RustCertificateGenerator) -> bool {
    source.0.is_some()
}
pub(crate) fn certificate_generator_generate(
    source: &RustCertificateGenerator,
    params: ffi::FfiCertificateKeyParams,
    has_expires: bool,
    expires_ms: u64,
    native: cxx::UniquePtr<ffi::NativeCertificateCompletion>,
) {
    let completion = Arc::new(CompletionInner { native });
    let Some(handle) = &source.0 else {
        completion.complete(None);
        return;
    };
    if handle.has_failed() {
        completion.complete(None);
        return;
    }
    let Ok(mut provider) = handle.0.provider.lock() else {
        handle.0.failed.store(true, Ordering::Release);
        completion.complete(None);
        return;
    };
    // This internal shared owner survives provider panic or ticket drop. It is
    // not a public cloneable completion ticket. A panic completes with failure
    // unless the provider already submitted a native result.
    if catch_unwind(AssertUnwindSafe(|| {
        provider.generate_certificate_async(
            CertificateKeyParams::from_native(params),
            has_expires.then_some(expires_ms),
            CertificateCompletion(completion.clone()),
        )
    }))
    .is_err()
    {
        handle.0.failed.store(true, Ordering::Release);
        completion.complete(None);
    }
}

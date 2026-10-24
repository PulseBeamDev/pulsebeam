//! Owning SDK frame crypto providers, separate from SFrame key management.
//! With panic=unwind, callback and provider-destructor panics are contained.
//! Standard panic payloads are disposed; unknown payloads avoid untrusted Drop.
use crate::peer::PeerInner;
use crate::{MediaType, ffi};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameDecryptionStatus {
    Ok,
    Recoverable,
    FailedToDecrypt,
    Unknown,
    Other(i32),
}
impl FrameDecryptionStatus {
    pub fn raw(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::Recoverable => 1,
            Self::FailedToDecrypt => 2,
            Self::Unknown => 3,
            Self::Other(value) => value,
        }
    }
    pub fn from_raw(value: i32) -> Self {
        Self::from_native(value)
    }
    fn native(self) -> i32 {
        self.raw()
    }
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::Ok,
            1 => Self::Recoverable,
            2 => Self::FailedToDecrypt,
            3 => Self::Unknown,
            value => Self::Other(value),
        }
    }
}
/// An owned SDK Result. Construction preserves status and the complete size_t
/// domain; byte-count validation happens when a provider result is consumed.
pub struct FrameDecryptionResult {
    native: cxx::UniquePtr<ffi::NativeFrameDecryptionResult>,
}
impl FrameDecryptionResult {
    pub fn new(status: FrameDecryptionStatus, bytes_written: usize) -> Self {
        Self {
            native: ffi::frame_decryption_result_new(status.native(), bytes_written),
        }
    }
    fn native(&self) -> &ffi::NativeFrameDecryptionResult {
        self.native.as_ref().expect("owned decryption result")
    }
    pub fn status(&self) -> FrameDecryptionStatus {
        FrameDecryptionStatus::from_native(
            ffi::frame_decryption_result_fields(self.native()).status,
        )
    }
    pub fn bytes_written(&self) -> usize {
        ffi::frame_decryption_result_fields(self.native()).written
    }
    pub fn is_ok(&self) -> bool {
        ffi::frame_decryption_result_is_ok(self.native())
    }
    pub fn assign(&mut self, source: &Self) {
        ffi::frame_decryption_result_assign(self.native.pin_mut(), source.native());
    }
    pub fn set_status(&mut self, status: FrameDecryptionStatus) {
        ffi::frame_decryption_result_set_status(self.native.pin_mut(), status.raw());
    }
    pub fn set_bytes_written(&mut self, written: usize) {
        ffi::frame_decryption_result_set_written(self.native.pin_mut(), written);
    }
}
impl Clone for FrameDecryptionResult {
    fn clone(&self) -> Self {
        Self {
            native: ffi::frame_decryption_result_clone(self.native()),
        }
    }
}
impl fmt::Debug for FrameDecryptionResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameDecryptionResult")
            .field("status", &self.status())
            .field("bytes_written", &self.bytes_written())
            .finish()
    }
}
// SAFETY: independently owned SDK Result contains only enum/size_t fields.
// Shared access reads; field mutation and assignment require exclusive loans.
unsafe impl Send for FrameDecryptionResult {}
unsafe impl Sync for FrameDecryptionResult {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameEncryptionResult {
    pub status: i32,
    pub bytes_written: usize,
}
#[derive(Debug)]
pub struct EncryptedFrame {
    pub result: FrameEncryptionResult,
    pub frame: Vec<u8>,
}
#[derive(Debug)]
pub struct DecryptedFrame {
    pub result: FrameDecryptionResult,
    pub frame: Vec<u8>,
}

/// Provider callbacks execute synchronously on SDK media paths, not necessarily
/// on the calling Rust thread. Output is separate, zero-filled scratch. Return
/// the actual written prefix; nonzero encryption status is preserved verbatim.
/// Callbacks must not synchronously wait for or reenter the owning peer/actor.
/// Provider destruction can occur on any SDK or application thread.
pub trait FrameEncryptorProvider: Send + Sync + 'static {
    fn max_ciphertext_byte_size(&self, media: MediaType, frame_size: usize) -> usize;
    fn encrypt(
        &self,
        media: MediaType,
        ssrc: u32,
        additional_data: &[u8],
        frame: &[u8],
        output: &mut [u8],
    ) -> FrameEncryptionResult;
}
/// SDK callbacks can run concurrently with application use. A native decryptor
/// is bound to one receiver for its entire lifetime. Video plaintext capacity
/// must not exceed the encrypted frame size: the SDK CHECKs this invariant.
/// SDK documentation reserves Unknown for internal status, not provider output.
/// Scratch, panic containment and callback reentrancy/destruction restrictions
/// match FrameEncryptorProvider.
pub trait FrameDecryptorProvider: Send + Sync + 'static {
    fn max_plaintext_byte_size(&self, media: MediaType, encrypted_frame_size: usize) -> usize;
    fn decrypt(
        &self,
        media: MediaType,
        csrcs: &[u32],
        additional_data: &[u8],
        encrypted_frame: &[u8],
        output: &mut [u8],
    ) -> FrameDecryptionResult;
}

/// A binding-detected unsafe provider result or panic. The provider is poisoned
/// after this failure; future SDK operations fail rather than report success.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameCryptoFailure {
    ProviderPanicked,
    VideoPlaintextCapacity {
        frame_size: usize,
        capacity: usize,
    },
    BytesWritten {
        bytes_written: usize,
        capacity: usize,
    },
}
#[derive(Default)]
struct FailureState(Mutex<Option<FrameCryptoFailure>>);
impl FailureState {
    fn get(&self) -> Option<FrameCryptoFailure> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    fn set(&self, failure: FrameCryptoFailure) {
        let mut slot = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(failure);
        }
    }
    fn run<T>(&self, body: impl FnOnce() -> T) -> Option<T> {
        let _callback = crate::provider_panic::ProviderCallbackScope::enter();
        if self.get().is_some() {
            return None;
        }
        match catch_unwind(AssertUnwindSafe(body)) {
            Ok(value) => Some(value),
            Err(payload) => {
                self.set(FrameCryptoFailure::ProviderPanicked);
                crate::panic_payload::discard(payload);
                None
            }
        }
    }
}
pub(crate) struct RustFrameEncryptorProvider {
    provider: Option<Arc<dyn FrameEncryptorProvider>>,
    failure: FailureState,
}
pub(crate) struct RustFrameDecryptorProvider {
    provider: Option<Arc<dyn FrameDecryptorProvider>>,
    failure: FailureState,
}
fn contained_drop<T>(value: T) {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
        crate::panic_payload::discard(payload);
    }
}
impl Drop for RustFrameEncryptorProvider {
    fn drop(&mut self) {
        contained_drop(self.provider.take());
    }
}
impl Drop for RustFrameDecryptorProvider {
    fn drop(&mut self) {
        contained_drop(self.provider.take());
    }
}
fn failure_fields(state: &FailureState) -> ffi::FfiFrameCryptoFailure {
    match state.get() {
        None => ffi::FfiFrameCryptoFailure {
            kind: 0,
            first: 0,
            second: 0,
        },
        Some(FrameCryptoFailure::ProviderPanicked) => ffi::FfiFrameCryptoFailure {
            kind: 1,
            first: 0,
            second: 0,
        },
        Some(FrameCryptoFailure::VideoPlaintextCapacity {
            frame_size,
            capacity,
        }) => ffi::FfiFrameCryptoFailure {
            kind: 2,
            first: frame_size,
            second: capacity,
        },
        Some(FrameCryptoFailure::BytesWritten {
            bytes_written,
            capacity,
        }) => ffi::FfiFrameCryptoFailure {
            kind: 3,
            first: bytes_written,
            second: capacity,
        },
    }
}
fn failure_value(fields: ffi::FfiFrameCryptoFailure) -> Option<FrameCryptoFailure> {
    match fields.kind {
        1 => Some(FrameCryptoFailure::ProviderPanicked),
        2 => Some(FrameCryptoFailure::VideoPlaintextCapacity {
            frame_size: fields.first,
            capacity: fields.second,
        }),
        3 => Some(FrameCryptoFailure::BytesWritten {
            bytes_written: fields.first,
            capacity: fields.second,
        }),
        _ => None,
    }
}
pub(crate) fn frame_encryptor_provider_failure(
    provider: &RustFrameEncryptorProvider,
) -> ffi::FfiFrameCryptoFailure {
    failure_fields(&provider.failure)
}
pub(crate) fn frame_decryptor_provider_failure(
    provider: &RustFrameDecryptorProvider,
) -> ffi::FfiFrameCryptoFailure {
    failure_fields(&provider.failure)
}
pub(crate) fn frame_encryptor_provider_max_size(
    provider: &RustFrameEncryptorProvider,
    media: i32,
    size: usize,
) -> usize {
    provider
        .failure
        .run(|| {
            provider
                .provider
                .as_ref()
                .expect("retained provider")
                .max_ciphertext_byte_size(MediaType::from_native(media), size)
        })
        .unwrap_or(0)
}
pub(crate) fn frame_decryptor_provider_max_size(
    provider: &RustFrameDecryptorProvider,
    media: i32,
    size: usize,
) -> usize {
    let media = MediaType::from_native(media);
    let Some(capacity) = provider.failure.run(|| {
        provider
            .provider
            .as_ref()
            .expect("retained provider")
            .max_plaintext_byte_size(media, size)
    }) else {
        return 0;
    };
    if media == MediaType::Video && capacity > size {
        provider
            .failure
            .set(FrameCryptoFailure::VideoPlaintextCapacity {
                frame_size: size,
                capacity,
            });
        // Avoid BufferedFrameDecryptor's CHECK, then Decrypt returns failure.
        return 0;
    }
    capacity
}
fn failed(status: i32) -> ffi::FfiFrameCryptoOutput {
    ffi::FfiFrameCryptoOutput {
        status,
        written: 0,
        bytes: Vec::new(),
    }
}
pub(crate) fn frame_encryptor_provider_encrypt(
    provider: &RustFrameEncryptorProvider,
    media: i32,
    ssrc: u32,
    additional: &[u8],
    input: &[u8],
    capacity: usize,
) -> ffi::FfiFrameCryptoOutput {
    provider
        .failure
        .run(|| {
            let mut bytes = vec![0; capacity];
            let result = provider
                .provider
                .as_ref()
                .expect("retained provider")
                .encrypt(
                    MediaType::from_native(media),
                    ssrc,
                    additional,
                    input,
                    &mut bytes,
                );
            if result.bytes_written > capacity {
                provider.failure.set(FrameCryptoFailure::BytesWritten {
                    bytes_written: result.bytes_written,
                    capacity,
                });
                return failed(-1);
            }
            bytes.truncate(result.bytes_written);
            ffi::FfiFrameCryptoOutput {
                status: result.status,
                written: result.bytes_written,
                bytes,
            }
        })
        .unwrap_or_else(|| failed(-1))
}
pub(crate) fn frame_decryptor_provider_decrypt(
    provider: &RustFrameDecryptorProvider,
    media: i32,
    csrcs: &[u32],
    additional: &[u8],
    input: &[u8],
    capacity: usize,
) -> ffi::FfiFrameCryptoOutput {
    provider
        .failure
        .run(|| {
            let mut bytes = vec![0; capacity];
            let result = provider
                .provider
                .as_ref()
                .expect("retained provider")
                .decrypt(
                    MediaType::from_native(media),
                    csrcs,
                    additional,
                    input,
                    &mut bytes,
                );
            let written = result.bytes_written();
            if written > capacity {
                provider.failure.set(FrameCryptoFailure::BytesWritten {
                    bytes_written: written,
                    capacity,
                });
                return failed(2);
            }
            bytes.truncate(written);
            ffi::FfiFrameCryptoOutput {
                status: result.status().native(),
                written,
                bytes,
            }
        })
        .unwrap_or_else(|| failed(2))
}

/// Refcounted SDK encryptor retaining an owned, thread-safe Rust provider.
pub struct FrameEncryptor {
    native: cxx::UniquePtr<ffi::NativeFrameEncryptor>,
}
impl FrameEncryptor {
    pub fn from_provider(provider: impl FrameEncryptorProvider) -> Self {
        Self {
            native: ffi::frame_encryptor_new(Box::new(RustFrameEncryptorProvider {
                provider: Some(Arc::new(provider)),
                failure: FailureState::default(),
            })),
        }
    }
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeFrameEncryptor>) -> Option<Self> {
        (!native.is_null()).then_some(Self { native })
    }
    pub(crate) fn native(&self) -> &ffi::NativeFrameEncryptor {
        self.native.as_ref().expect("owned frame encryptor")
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::frame_encryptor_same(self.native(), other.native())
    }
    pub fn failure(&self) -> Option<FrameCryptoFailure> {
        failure_value(ffi::frame_encryptor_failure(self.native()))
    }
    pub fn max_ciphertext_byte_size(&self, media: MediaType, frame_size: usize) -> usize {
        ffi::frame_encryptor_max_size(self.native(), media.to_native(), frame_size)
    }
    pub fn encrypt(
        &self,
        media: MediaType,
        ssrc: u32,
        additional_data: &[u8],
        frame: &[u8],
        output_capacity: usize,
    ) -> Result<EncryptedFrame, FrameCryptoOperationError> {
        check_output_capacity(output_capacity)?;
        let value = ffi::frame_encryptor_encrypt(
            self.native(),
            media.to_native(),
            ssrc,
            additional_data,
            frame,
            output_capacity,
        );
        Ok(EncryptedFrame {
            result: FrameEncryptionResult {
                status: value.status,
                bytes_written: value.written,
            },
            frame: value.bytes,
        })
    }
}
impl Clone for FrameEncryptor {
    fn clone(&self) -> Self {
        Self {
            native: ffi::frame_encryptor_clone(self.native()),
        }
    }
}
impl fmt::Debug for FrameEncryptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameEncryptor")
            .field("failure", &self.failure())
            .finish()
    }
}
// SAFETY: this closed producer graph contains only our refcounted Rust adapter
// with owned Send+Sync provider state. No peer, SDK sequence, or borrowed owner
// is retained. Getters alias the same adapter; SDK has no automatic producer.
unsafe impl Send for FrameEncryptor {}
unsafe impl Sync for FrameEncryptor {}

/// A sequence-bound SDK decryptor permanently associated with one receiver.
///
/// ```compile_fail
/// fn send<T: Send>() {} send::<pulsebeam_webrtc_sys::FrameDecryptor>();
/// ```
/// ```compile_fail
/// fn sync<T: Sync>() {} sync::<pulsebeam_webrtc_sys::FrameDecryptor>();
/// ```
pub struct FrameDecryptor {
    native: cxx::UniquePtr<ffi::NativeFrameDecryptor>,
    _peer: Rc<PeerInner>,
}
impl FrameDecryptor {
    pub(crate) fn new(
        receiver: &ffi::NativeRtpReceiver,
        peer: Rc<PeerInner>,
        provider: impl FrameDecryptorProvider,
    ) -> Self {
        Self {
            native: ffi::frame_decryptor_new(
                receiver,
                Box::new(RustFrameDecryptorProvider {
                    provider: Some(Arc::new(provider)),
                    failure: FailureState::default(),
                }),
            ),
            _peer: peer,
        }
    }
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeFrameDecryptor>,
        peer: Rc<PeerInner>,
    ) -> Option<Self> {
        (!native.is_null()).then_some(Self {
            native,
            _peer: peer,
        })
    }
    pub(crate) fn native(&self) -> &ffi::NativeFrameDecryptor {
        self.native.as_ref().expect("owned frame decryptor")
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::frame_decryptor_same(self.native(), other.native())
    }
    pub fn failure(&self) -> Option<FrameCryptoFailure> {
        failure_value(ffi::frame_decryptor_failure(self.native()))
    }
    pub fn max_plaintext_byte_size(&self, media: MediaType, frame_size: usize) -> usize {
        ffi::frame_decryptor_max_size(self.native(), media.to_native(), frame_size)
    }
    pub fn decrypt(
        &self,
        media: MediaType,
        csrcs: &[u32],
        additional_data: &[u8],
        frame: &[u8],
        output_capacity: usize,
    ) -> Result<DecryptedFrame, FrameCryptoOperationError> {
        check_output_capacity(output_capacity)?;
        let value = ffi::frame_decryptor_decrypt(
            self.native(),
            media.to_native(),
            csrcs,
            additional_data,
            frame,
            output_capacity,
        );
        Ok(DecryptedFrame {
            result: FrameDecryptionResult::new(
                FrameDecryptionStatus::from_native(value.status),
                value.written,
            ),
            frame: value.bytes,
        })
    }
}
impl Clone for FrameDecryptor {
    fn clone(&self) -> Self {
        Self {
            native: ffi::frame_decryptor_clone(self.native()),
            _peer: self._peer.clone(),
        }
    }
}
impl fmt::Debug for FrameDecryptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameDecryptor")
            .field("failure", &self.failure())
            .finish()
    }
}
fn check_output_capacity(requested: usize) -> Result<(), FrameCryptoOperationError> {
    // The SDK interface accepts a span; the standalone bridge owns its storage.
    // Reject std::vector's length_error before entering a noexcept allocation.
    let maximum = ffi::frame_crypto_max_output_capacity();
    if requested > maximum {
        Err(FrameCryptoOperationError::OutputCapacityTooLarge { requested, maximum })
    } else {
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameCryptoOperationError {
    Binding(crate::PeerError),
    OutputCapacityTooLarge { requested: usize, maximum: usize },
}
impl From<crate::PeerError> for FrameCryptoOperationError {
    fn from(error: crate::PeerError) -> Self {
        Self::Binding(error)
    }
}
impl fmt::Display for FrameCryptoOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binding(error) => error.fmt(f),
            Self::OutputCapacityTooLarge { requested, maximum } => write!(
                f,
                "frame crypto output capacity {requested} exceeds native vector maximum {maximum}"
            ),
        }
    }
}
impl std::error::Error for FrameCryptoOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Binding(error) => Some(error),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameDecryptorInstallError {
    Binding(crate::PeerError),
    WrongReceiver,
}
impl fmt::Display for FrameDecryptorInstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binding(error) => error.fmt(f),
            Self::WrongReceiver => {
                f.write_str("frame decryptor belongs to a different RTP receiver")
            }
        }
    }
}
impl std::error::Error for FrameDecryptorInstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Binding(error) => Some(error),
            Self::WrongReceiver => None,
        }
    }
}

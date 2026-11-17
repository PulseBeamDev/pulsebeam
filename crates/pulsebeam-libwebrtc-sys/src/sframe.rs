//! Native SFrame values and key-management handles for the upstream SFrame SDK.
use crate::peer::PeerInner;
use crate::{RtcError, ffi};
use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    rc::Rc,
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll, Waker},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SframeMode {
    PerFrame,
    PerPacket,
    Other(i32),
}
impl SframeMode {
    pub const fn raw(self) -> i32 {
        match self {
            Self::PerFrame => 0,
            Self::PerPacket => 1,
            Self::Other(value) => value,
        }
    }
    pub const fn from_raw(value: i32) -> Self {
        match value {
            0 => Self::PerFrame,
            1 => Self::PerPacket,
            value => Self::Other(value),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SframeCipherSuite {
    Aes128CtrHmacSha256_80,
    Aes128CtrHmacSha256_64,
    Aes128CtrHmacSha256_32,
    Aes128GcmSha256_128,
    Aes256GcmSha512_128,
    Aes256CtrHmacSha512_80,
    Aes256CtrHmacSha512_64,
    Aes256CtrHmacSha512_32,
    Other(i32),
}
impl SframeCipherSuite {
    pub const fn raw(self) -> i32 {
        match self {
            Self::Aes128CtrHmacSha256_80 => 0,
            Self::Aes128CtrHmacSha256_64 => 1,
            Self::Aes128CtrHmacSha256_32 => 2,
            Self::Aes128GcmSha256_128 => 3,
            Self::Aes256GcmSha512_128 => 4,
            Self::Aes256CtrHmacSha512_80 => 5,
            Self::Aes256CtrHmacSha512_64 => 6,
            Self::Aes256CtrHmacSha512_32 => 7,
            Self::Other(value) => value,
        }
    }
    pub const fn from_raw(value: i32) -> Self {
        match value {
            0 => Self::Aes128CtrHmacSha256_80,
            1 => Self::Aes128CtrHmacSha256_64,
            2 => Self::Aes128CtrHmacSha256_32,
            3 => Self::Aes128GcmSha256_128,
            4 => Self::Aes256GcmSha512_128,
            5 => Self::Aes256CtrHmacSha512_80,
            6 => Self::Aes256CtrHmacSha512_64,
            7 => Self::Aes256CtrHmacSha512_32,
            value => Self::Other(value),
        }
    }
}
/// Complete owning SDK aggregate. Both fields are explicit; native default
/// initialization without braces leaves them indeterminate, so there is no
/// invented Default. Unknown enum metadata is preserved, not normalized.
pub struct SframeEncrypterInit {
    native: cxx::UniquePtr<ffi::NativeSframeEncrypterInit>,
}
impl SframeEncrypterInit {
    pub fn new(mode: SframeMode, suite: SframeCipherSuite) -> Self {
        Self {
            native: ffi::sframe_init_new(mode.raw(), suite.raw()),
        }
    }
    pub(crate) fn native(&self) -> &ffi::NativeSframeEncrypterInit {
        self.native.as_ref().expect("owned SFrame init")
    }
    pub fn mode(&self) -> SframeMode {
        SframeMode::from_raw(ffi::sframe_init_fields(self.native()).mode)
    }
    pub fn cipher_suite(&self) -> SframeCipherSuite {
        SframeCipherSuite::from_raw(ffi::sframe_init_fields(self.native()).suite)
    }
    pub fn set_mode(&mut self, value: SframeMode) {
        ffi::sframe_init_set_mode(self.native.pin_mut(), value.raw());
    }
    pub fn set_cipher_suite(&mut self, value: SframeCipherSuite) {
        ffi::sframe_init_set_suite(self.native.pin_mut(), value.raw());
    }
    pub fn assign(&mut self, source: &Self) {
        ffi::sframe_init_assign(self.native.pin_mut(), source.native());
    }
}
impl Clone for SframeEncrypterInit {
    fn clone(&self) -> Self {
        Self {
            native: ffi::sframe_init_clone(self.native()),
        }
    }
    fn clone_from(&mut self, source: &Self) {
        self.assign(source);
    }
}
/// Upstream-spelled alias for the SDK's `SframeEncryptorInit` aggregate.
pub type SframeEncryptorInit = SframeEncrypterInit;
impl fmt::Debug for SframeEncrypterInit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SframeEncrypterInit")
            .field("mode", &self.mode())
            .field("cipher_suite", &self.cipher_suite())
            .finish()
    }
}
// SAFETY: independently owned two-enum SDK value; shared access only reads and
// all mutation requires an exclusive loan. No retained peer/callback/origin.
unsafe impl Send for SframeEncrypterInit {}
unsafe impl Sync for SframeEncrypterInit {}

#[derive(Debug)]
pub enum SframeCreateError {
    Binding(crate::PeerError),
    Native(RtcError),
    DetachedOwner,
    MissingNativeProduct,
}
impl From<crate::PeerError> for SframeCreateError {
    fn from(value: crate::PeerError) -> Self {
        Self::Binding(value)
    }
}
impl fmt::Display for SframeCreateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binding(value) => value.fmt(f),
            Self::Native(value) => value.fmt(f),
            Self::DetachedOwner => {
                f.write_str("cannot establish live SDK transceiver owner for SFrame creation")
            }
            Self::MissingNativeProduct => {
                f.write_str("SDK SFrame creation returned success without a key-management handle")
            }
        }
    }
}
impl std::error::Error for SframeCreateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Binding(value) => Some(value),
            Self::Native(value) => Some(value),
            Self::DetachedOwner | Self::MissingNativeProduct => None,
        }
    }
}
/// Owning native encryption key-management handle. Local handles retain their
/// peer and are not Send/Sync.
///
/// ```compile_fail
/// fn send<T: Send>() {} send::<pulsebeam_webrtc_sys::SframeEncrypter>();
/// ```
pub struct SframeEncrypter {
    native: cxx::UniquePtr<ffi::NativeSframeEncrypter>,
    peer: Rc<PeerInner>,
}
impl SframeEncrypter {
    pub(crate) fn from_creation(
        value: ffi::FfiSframeEncrypterCreation,
        peer: Rc<PeerInner>,
    ) -> Result<Self, SframeCreateError> {
        if value.detached_owner {
            return Err(SframeCreateError::DetachedOwner);
        }
        if !value.error.is_null() {
            return Err(SframeCreateError::Native(RtcError::from_native(
                value.error,
            )));
        }
        if value.product.is_null() {
            return Err(SframeCreateError::MissingNativeProduct);
        }
        Ok(Self {
            native: value.product,
            peer,
        })
    }
    fn native(&self) -> &ffi::NativeSframeEncrypter {
        self.native.as_ref().expect("owned SFrame encrypter")
    }
    pub fn set_encryption_key(&self, id: u64, key_material: &[u8]) -> RtcError {
        RtcError::from_native(ffi::sframe_set_encryption_key(
            self.native(),
            id,
            key_material,
        ))
    }
}
impl Clone for SframeEncrypter {
    fn clone(&self) -> Self {
        Self {
            native: ffi::sframe_encrypter_clone(self.native()),
            peer: self.peer.clone(),
        }
    }
}
impl fmt::Debug for SframeEncrypter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SframeEncrypter").finish_non_exhaustive()
    }
}
/// One-shot result of an asynchronous native decryption-key update.
#[derive(Debug)]
pub enum SframeKeyUpdateError {
    Native(RtcError),
    Cancelled,
}
impl fmt::Display for SframeKeyUpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Native(error) => error.fmt(f),
            Self::Cancelled => {
                f.write_str("SFrame key update was cancelled when its handle was released")
            }
        }
    }
}
impl std::error::Error for SframeKeyUpdateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Native(error) => Some(error),
            Self::Cancelled => None,
        }
    }
}
struct KeyUpdateState {
    result: Option<Result<(), SframeKeyUpdateError>>,
    waker: Option<Waker>,
}
/// Future and pollable completion for an asynchronous SFrame key update.
///
/// Dropping the decrypter before the SDK reports a result completes this
/// operation with [`SframeKeyUpdateError::Cancelled`].
pub struct SframeKeyUpdateOperation(Arc<Mutex<KeyUpdateState>>);
impl SframeKeyUpdateOperation {
    fn new() -> (Self, Box<RustSframeKeyCompletion>) {
        let state = Arc::new(Mutex::new(KeyUpdateState {
            result: None,
            waker: None,
        }));
        (
            Self(state.clone()),
            Box::new(RustSframeKeyCompletion {
                state,
                completed: std::sync::atomic::AtomicBool::new(false),
            }),
        )
    }
    pub fn try_take(&mut self) -> Option<Result<(), SframeKeyUpdateError>> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .result
            .take()
    }
}
impl Future for SframeKeyUpdateOperation {
    type Output = Result<(), SframeKeyUpdateError>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(result) = state.result.take() {
            return Poll::Ready(result);
        }
        let old = state.waker.replace(cx.waker().clone());
        drop(state);
        if let Some(old) = old {
            let _ = catch_unwind(AssertUnwindSafe(|| drop(old)));
        }
        Poll::Pending
    }
}
pub(crate) struct RustSframeKeyCompletion {
    state: Arc<Mutex<KeyUpdateState>>,
    completed: std::sync::atomic::AtomicBool,
}
impl RustSframeKeyCompletion {
    fn complete(&self, result: Result<(), SframeKeyUpdateError>) {
        use std::sync::atomic::Ordering;
        if self.completed.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.result = Some(result);
        let waker = state.waker.take();
        drop(state);
        if let Some(waker) = waker {
            let _ = catch_unwind(AssertUnwindSafe(|| waker.wake()));
        }
    }
}
impl Drop for RustSframeKeyCompletion {
    fn drop(&mut self) {
        self.complete(Err(SframeKeyUpdateError::Cancelled));
    }
}
pub(crate) fn sframe_key_update_complete(
    source: &RustSframeKeyCompletion,
    error: cxx::UniquePtr<ffi::NativeRtcError>,
) {
    let error = RtcError::from_native(error);
    source.complete(if error.is_ok() {
        Ok(())
    } else {
        Err(SframeKeyUpdateError::Native(error))
    });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SframeDecryptErrorType {
    Authentication,
    KeyId,
    Syntax,
    Other(i32),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SframeEncryptedFrame {
    pub payload_type: u8,
    pub ssrc: u32,
    pub mime_type: String,
    pub encrypted_data: Vec<u8>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SframeDecryptError {
    pub error_type: SframeDecryptErrorType,
    pub key_id: Option<u64>,
    pub frame: Option<SframeEncryptedFrame>,
    /// True when the SDK frame's ciphertext exceeded the retention byte limit.
    /// The error remains diagnostic evidence, not a successful decryption.
    pub frame_data_dropped: bool,
}

const MAX_SFRAME_DECRYPT_ERROR_EVENTS: usize = 64;
const MAX_SFRAME_DECRYPT_ERROR_BYTES: usize = 1024 * 1024;

/// Saturating count of diagnostic information omitted from the bounded SFrame
/// decrypt-error queue since the previous take. This is independent of
/// cryptographic success.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SframeDecryptErrorDropCounts {
    /// Error records evicted to satisfy the retained-event limit.
    pub events: u64,
    /// Ciphertext bytes omitted from oversized frames or evicted records.
    pub encrypted_frame_bytes: u64,
}
#[derive(Default)]
pub(crate) struct SframeDecryptErrorQueue {
    events: VecDeque<SframeDecryptError>,
    retained_bytes: usize,
    dropped: SframeDecryptErrorDropCounts,
}
impl SframeDecryptErrorQueue {
    fn drop_event(&mut self, event: SframeDecryptError) {
        self.dropped.events = self.dropped.events.saturating_add(1);
        if let Some(frame) = event.frame {
            self.dropped.encrypted_frame_bytes = self
                .dropped
                .encrypted_frame_bytes
                .saturating_add(u64::try_from(frame.encrypted_data.len()).unwrap_or(u64::MAX));
        }
    }
    fn push(&mut self, mut error: SframeDecryptError) {
        let input_bytes = error
            .frame
            .as_ref()
            .map_or(0, |frame| frame.encrypted_data.len());
        if input_bytes > MAX_SFRAME_DECRYPT_ERROR_BYTES {
            self.dropped.events = self.dropped.events.saturating_add(1);
            self.dropped.encrypted_frame_bytes = self
                .dropped
                .encrypted_frame_bytes
                .saturating_add(u64::try_from(input_bytes).unwrap_or(u64::MAX));
            if let Some(frame) = error.frame.as_mut() {
                frame.encrypted_data = Vec::new();
            }
            error.frame_data_dropped = true;
        }
        let retained = if error.frame_data_dropped {
            0
        } else {
            input_bytes
        };
        while self.events.len() >= MAX_SFRAME_DECRYPT_ERROR_EVENTS
            || self.retained_bytes > MAX_SFRAME_DECRYPT_ERROR_BYTES - retained
        {
            let Some(oldest) = self.events.pop_front() else {
                break;
            };
            self.retained_bytes -= oldest
                .frame
                .as_ref()
                .map_or(0, |frame| frame.encrypted_data.len());
            self.drop_event(oldest);
        }
        self.retained_bytes += retained;
        self.events.push_back(error);
    }
    fn take_dropped(&mut self) -> SframeDecryptErrorDropCounts {
        std::mem::take(&mut self.dropped)
    }
}
pub(crate) fn sframe_decrypt_error_observer() -> (
    Box<RustSframeDecryptErrorObserver>,
    Arc<Mutex<SframeDecryptErrorQueue>>,
) {
    let queue = Arc::new(Mutex::new(SframeDecryptErrorQueue::default()));
    (
        Box::new(RustSframeDecryptErrorObserver(Arc::downgrade(&queue))),
        queue,
    )
}
pub(crate) struct RustSframeDecryptErrorObserver(Weak<Mutex<SframeDecryptErrorQueue>>);
pub(crate) fn sframe_decrypt_error_observed(
    source: &RustSframeDecryptErrorObserver,
    value: ffi::FfiSframeDecryptError,
) {
    let error_type = match value.error_type {
        0 => SframeDecryptErrorType::Authentication,
        1 => SframeDecryptErrorType::KeyId,
        2 => SframeDecryptErrorType::Syntax,
        value => SframeDecryptErrorType::Other(value),
    };
    let frame = value.has_frame.then_some(SframeEncryptedFrame {
        payload_type: value.payload_type,
        ssrc: value.ssrc,
        mime_type: value.mime_type,
        encrypted_data: value.frame_data,
    });
    if let Some(queue) = source.0.upgrade() {
        queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(SframeDecryptError {
                error_type,
                key_id: value.has_key_id.then_some(value.key_id),
                frame,
                frame_data_dropped: false,
            });
    }
}

/// Owning native decryption key-management handle, distinct from the legacy
/// FrameDecryptor. Decrypt failures are surfaced by `try_take_decrypt_error`.
///
/// ```compile_fail
/// fn send<T: Send>() {} send::<pulsebeam_webrtc_sys::SframeDecrypter>();
/// ```
pub struct SframeDecrypter {
    native: cxx::UniquePtr<ffi::NativeSframeDecrypter>,
    peer: Rc<PeerInner>,
    decrypt_errors: Arc<Mutex<SframeDecryptErrorQueue>>,
}
impl SframeDecrypter {
    pub(crate) fn from_creation(
        value: ffi::FfiSframeDecrypterCreation,
        peer: Rc<PeerInner>,
        decrypt_errors: Arc<Mutex<SframeDecryptErrorQueue>>,
    ) -> Result<Self, SframeCreateError> {
        if value.detached_owner {
            return Err(SframeCreateError::DetachedOwner);
        }
        if !value.error.is_null() {
            return Err(SframeCreateError::Native(RtcError::from_native(
                value.error,
            )));
        }
        if value.product.is_null() {
            return Err(SframeCreateError::MissingNativeProduct);
        }
        Ok(Self {
            native: value.product,
            peer,
            decrypt_errors,
        })
    }
    fn native(&self) -> &ffi::NativeSframeDecrypter {
        self.native.as_ref().expect("owned SFrame decrypter")
    }
    pub fn add_decryption_key(&self, id: u64, key_material: &[u8]) -> SframeKeyUpdateOperation {
        let (operation, completion) = SframeKeyUpdateOperation::new();
        ffi::sframe_add_decryption_key(self.native(), id, key_material, completion);
        operation
    }
    pub fn remove_decryption_key(&self, id: u64) -> SframeKeyUpdateOperation {
        let (operation, completion) = SframeKeyUpdateOperation::new();
        ffi::sframe_remove_decryption_key(self.native(), id, completion);
        operation
    }
    /// Takes the oldest SDK decrypt failure, including its encrypted frame.
    pub fn try_take_decrypt_error(&self) -> Option<SframeDecryptError> {
        let mut queue = self
            .decrypt_errors
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let error = queue.events.pop_front()?;
        queue.retained_bytes -= error
            .frame
            .as_ref()
            .map_or(0, |frame| frame.encrypted_data.len());
        Some(error)
    }
    /// Takes saturating counts for diagnostics omitted by queue bounds. These
    /// drops do not indicate that any frame decrypted successfully.
    pub fn take_decrypt_error_drops(&self) -> SframeDecryptErrorDropCounts {
        self.decrypt_errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take_dropped()
    }
}
impl Clone for SframeDecrypter {
    fn clone(&self) -> Self {
        Self {
            native: ffi::sframe_decrypter_clone(self.native()),
            peer: self.peer.clone(),
            decrypt_errors: self.decrypt_errors.clone(),
        }
    }
}
impl fmt::Debug for SframeDecrypter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SframeDecrypter").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_update_completion_drop_is_observable_cancellation() {
        let (mut operation, completion) = SframeKeyUpdateOperation::new();
        assert!(operation.try_take().is_none());
        drop(completion);
        assert!(matches!(
            operation.try_take(),
            Some(Err(SframeKeyUpdateError::Cancelled))
        ));
    }

    fn decrypt_error(frame_bytes: usize) -> SframeDecryptError {
        SframeDecryptError {
            error_type: SframeDecryptErrorType::KeyId,
            key_id: Some(7),
            frame: Some(SframeEncryptedFrame {
                payload_type: 96,
                ssrc: 42,
                mime_type: "video/VP8".to_owned(),
                encrypted_data: vec![0xa5; frame_bytes],
            }),
            frame_data_dropped: false,
        }
    }

    #[test]
    fn decrypt_error_queue_bounds_event_count_and_accounts_eviction() {
        let mut queue = SframeDecryptErrorQueue::default();
        for _ in 0..=MAX_SFRAME_DECRYPT_ERROR_EVENTS {
            queue.push(decrypt_error(1));
        }
        assert_eq!(queue.events.len(), MAX_SFRAME_DECRYPT_ERROR_EVENTS);
        assert_eq!(queue.retained_bytes, MAX_SFRAME_DECRYPT_ERROR_EVENTS);
        assert_eq!(
            queue.take_dropped(),
            SframeDecryptErrorDropCounts {
                events: 1,
                encrypted_frame_bytes: 1,
            }
        );
    }

    #[test]
    fn decrypt_error_queue_bounds_ciphertext_bytes_and_accounts_omissions() {
        let mut queue = SframeDecryptErrorQueue::default();
        let frame_bytes = MAX_SFRAME_DECRYPT_ERROR_BYTES / 2 + 1;
        queue.push(decrypt_error(frame_bytes));
        queue.push(decrypt_error(frame_bytes));
        assert_eq!(queue.retained_bytes, frame_bytes);
        assert_eq!(queue.events.len(), 1);
        assert_eq!(
            queue.take_dropped(),
            SframeDecryptErrorDropCounts {
                events: 1,
                encrypted_frame_bytes: frame_bytes as u64,
            }
        );

        queue.push(decrypt_error(MAX_SFRAME_DECRYPT_ERROR_BYTES + 1));
        assert!(queue.retained_bytes <= MAX_SFRAME_DECRYPT_ERROR_BYTES);
        let oversized = queue.events.back().unwrap();
        assert!(oversized.frame_data_dropped);
        let ciphertext = &oversized.frame.as_ref().unwrap().encrypted_data;
        assert!(ciphertext.is_empty());
        assert_eq!(ciphertext.capacity(), 0);
        assert_eq!(
            queue.take_dropped(),
            SframeDecryptErrorDropCounts {
                events: 1,
                encrypted_frame_bytes: (MAX_SFRAME_DECRYPT_ERROR_BYTES + 1) as u64,
            }
        );
    }

    #[test]
    fn decrypt_error_queue_is_shared_by_cloned_handles() {
        let (observer, queue) = sframe_decrypt_error_observer();
        let cloned_queue = queue.clone();
        sframe_decrypt_error_observed(
            &observer,
            ffi::FfiSframeDecryptError {
                error_type: 1,
                has_key_id: true,
                key_id: 7,
                has_frame: true,
                payload_type: 96,
                ssrc: 42,
                mime_type: "video/VP8".to_owned(),
                frame_data: vec![0xde, 0xad],
            },
        );
        assert_eq!(
            cloned_queue.lock().unwrap().events[0]
                .frame
                .as_ref()
                .unwrap()
                .encrypted_data,
            [0xde, 0xad]
        );
        assert!(queue.lock().unwrap().events.pop_front().is_some());
        assert!(cloned_queue.lock().unwrap().events.is_empty());
    }

    #[test]
    fn decrypt_error_drop_accounting_saturates() {
        let mut queue = SframeDecryptErrorQueue::default();
        queue.dropped = SframeDecryptErrorDropCounts {
            events: u64::MAX,
            encrypted_frame_bytes: u64::MAX,
        };
        queue.push(decrypt_error(MAX_SFRAME_DECRYPT_ERROR_BYTES + 1));
        assert_eq!(
            queue.dropped,
            SframeDecryptErrorDropCounts {
                events: u64::MAX,
                encrypted_frame_bytes: u64::MAX,
            }
        );
    }

    #[test]
    fn decrypt_error_callback_preserves_key_id_and_ciphertext_frame() {
        let (observer, _queue) = sframe_decrypt_error_observer();
        sframe_decrypt_error_observed(
            &observer,
            ffi::FfiSframeDecryptError {
                error_type: 1,
                has_key_id: true,
                key_id: 0x1234,
                has_frame: true,
                payload_type: 96,
                ssrc: 0x5566,
                mime_type: "video/VP8".to_owned(),
                frame_data: vec![0xde, 0xad],
            },
        );
        let queue = observer.0.upgrade().unwrap();
        let error = queue.lock().unwrap().events.pop_front().unwrap();
        assert_eq!(error.error_type, SframeDecryptErrorType::KeyId);
        assert_eq!(error.key_id, Some(0x1234));
        assert_eq!(error.frame.unwrap().encrypted_data, [0xde, 0xad]);
    }
}

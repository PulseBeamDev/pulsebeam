//! Owning callback implementations for actual selected native video products.
use crate::provider_panic::catch_provider as catch_unwind;
use crate::{RawVideoFrame, ffi};
use std::{
    fmt,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

/// Callback methods can run on SDK roles. Do not synchronously wait for or
/// reenter the owning decoder/actor. Frames are owning SDK value copies and
/// mutations are copied back to the actual native callback argument.
pub trait DecodedImageCallbackProvider: Send + Sync + 'static {
    fn decoded(&self, frame: &mut RawVideoFrame) -> i32;
    /// Native base default ignores this argument and calls the basic overload.
    fn decoded_with_time(&self, frame: &mut RawVideoFrame, _decode_time_ms: i64) -> i32 {
        self.decoded(frame)
    }
    /// Native base default forwards absent time as -1 to the int64 overload;
    /// this overload is void and does not invent a result for the SDK caller.
    fn decoded_with_metadata(
        &self,
        frame: &mut RawVideoFrame,
        decode_time_ms: Option<i32>,
        _qp: Option<u8>,
    ) {
        let _ = self.decoded_with_time(frame, decode_time_ms.map_or(-1, i64::from));
    }
}
/// Counter-only evidence, including bridge-owned final userdata destruction.
/// The observer never keeps callback userdata or a native decoder alive.
#[derive(Clone, Default)]
pub struct VideoCodecCallbackFailureObserver(Arc<AtomicU64>);
impl VideoCodecCallbackFailureObserver {
    pub fn callback_panics(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
    fn record(&self) {
        let _ = self
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_add(1))
            });
    }
}
impl fmt::Debug for VideoCodecCallbackFailureObserver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VideoCodecCallbackFailureObserver")
            .field("callback_panics", &self.callback_panics())
            .finish()
    }
}
/// A real native DecodedImageCallback implementation with owning Rust userdata.
/// Installed products retain it independently of the caller's outer handle.
pub struct DecodedImageCallbackSink {
    native: cxx::UniquePtr<ffi::NativeDecodedImageSink>,
    failures: VideoCodecCallbackFailureObserver,
}
// SAFETY: shared immutable native implementation holds a Send+Sync provider.
// Every invocation owns its frame copy; no codec pointer or mutable frame alias
// is stored in this callback resource. Final userdata destruction is contained.
unsafe impl Send for DecodedImageCallbackSink {}
unsafe impl Sync for DecodedImageCallbackSink {}
impl Clone for DecodedImageCallbackSink {
    fn clone(&self) -> Self {
        Self {
            native: ffi::decoded_sink_clone(self.native()),
            failures: self.failures.clone(),
        }
    }
}
impl fmt::Debug for DecodedImageCallbackSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecodedImageCallbackSink")
            .field("failures", &self.failures)
            .finish_non_exhaustive()
    }
}
impl DecodedImageCallbackSink {
    pub fn new(provider: impl DecodedImageCallbackProvider) -> Self {
        Self::from_shared(Arc::new(provider))
    }
    pub fn from_shared(provider: Arc<dyn DecodedImageCallbackProvider>) -> Self {
        let failures = VideoCodecCallbackFailureObserver::default();
        Self {
            native: ffi::decoded_sink_new(Box::new(RustDecodedImageSink {
                provider: Some(provider),
                failures: failures.clone(),
                terminal: AtomicBool::new(false),
            })),
            failures,
        }
    }
    pub(crate) fn native(&self) -> &ffi::NativeDecodedImageSink {
        self.native.as_ref().expect("owning decoded callback sink")
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::decoded_sink_same(self.native(), other.native())
    }
    pub fn failure_observer(&self) -> VideoCodecCallbackFailureObserver {
        self.failures.clone()
    }
    pub fn decoded(&self, frame: &mut RawVideoFrame) -> i32 {
        ffi::decoded_sink_frame(
            self.native(),
            frame.native.pin_mut(),
            0,
            0,
            false,
            0,
            false,
            0,
        )
    }
    pub fn decoded_with_time(&self, frame: &mut RawVideoFrame, decode_time_ms: i64) -> i32 {
        ffi::decoded_sink_frame(
            self.native(),
            frame.native.pin_mut(),
            1,
            decode_time_ms,
            false,
            0,
            false,
            0,
        )
    }
    pub fn decoded_with_metadata(
        &self,
        frame: &mut RawVideoFrame,
        decode_time_ms: Option<i32>,
        qp: Option<u8>,
    ) {
        let _ = ffi::decoded_sink_frame(
            self.native(),
            frame.native.pin_mut(),
            2,
            0,
            decode_time_ms.is_some(),
            decode_time_ms.unwrap_or(0),
            qp.is_some(),
            qp.unwrap_or(0),
        );
    }
}
/// Owning FEC decision callback. Calls may arrive on SDK roles. Do not wait for
/// or reenter the owning encoder/session from this callback.
pub trait FecControllerOverrideProvider: Send + Sync + 'static {
    fn set_fec_allowed(&self, allowed: bool);
}
/// Registration-specific capability. Borrowed SDK capabilities retire before
/// replacement/release; aliases never switch to the replacement target.
#[derive(Clone)]
pub struct FecControllerOverride {
    pub(crate) native: cxx::SharedPtr<ffi::NativeFecOverride>,
    failures: Option<VideoCodecCallbackFailureObserver>,
}
// SAFETY: native admission/retirement barrier protects the borrowed target;
// owning providers are Send+Sync. No lock spans user or downstream SDK code.
unsafe impl Send for FecControllerOverride {}
unsafe impl Sync for FecControllerOverride {}
impl fmt::Debug for FecControllerOverride {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FecControllerOverride")
            .field("retired", &self.is_retired())
            .finish_non_exhaustive()
    }
}
impl FecControllerOverride {
    pub fn new(provider: impl FecControllerOverrideProvider) -> Self {
        Self::from_shared(Arc::new(provider))
    }
    pub fn from_shared(provider: Arc<dyn FecControllerOverrideProvider>) -> Self {
        let failures = VideoCodecCallbackFailureObserver::default();
        Self {
            native: ffi::fec_override_new(Box::new(RustFecOverride {
                provider: Some(provider),
                failures: failures.clone(),
                terminal: AtomicBool::new(false),
            })),
            failures: Some(failures),
        }
    }
    pub(crate) fn borrowed(native: cxx::SharedPtr<ffi::NativeFecOverride>) -> Self {
        Self {
            native,
            failures: None,
        }
    }
    pub fn is_retired(&self) -> bool {
        self.native.as_ref().is_none_or(ffi::fec_override_retired)
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        match (self.native.as_ref(), other.native.as_ref()) {
            (Some(a), Some(b)) => ffi::fec_override_same(a, b),
            _ => false,
        }
    }
    pub fn set_fec_allowed(&self, allowed: bool) -> Result<(), crate::CodecError> {
        if self
            .native
            .as_ref()
            .is_some_and(|v| ffi::fec_override_allowed(v, allowed))
        {
            Ok(())
        } else {
            Err(crate::CodecError::CallbackUnavailable)
        }
    }
    pub fn failure_observer(&self) -> Option<VideoCodecCallbackFailureObserver> {
        self.failures.clone()
    }
}
pub(crate) struct RustFecOverride {
    provider: Option<Arc<dyn FecControllerOverrideProvider>>,
    failures: VideoCodecCallbackFailureObserver,
    terminal: AtomicBool,
}
impl Drop for RustFecOverride {
    fn drop(&mut self) {
        if catch_unwind(AssertUnwindSafe(|| drop(self.provider.take()))).is_err() {
            self.failures.record();
        }
    }
}
pub(crate) fn fec_override_provider_allowed(provider: &RustFecOverride, allowed: bool) {
    if provider.terminal.load(Ordering::Acquire) {
        return;
    }
    if catch_unwind(AssertUnwindSafe(|| {
        provider
            .provider
            .as_deref()
            .expect("live FEC provider")
            .set_fec_allowed(allowed)
    }))
    .is_err()
    {
        provider.terminal.store(true, Ordering::Release);
        provider.failures.record();
    }
}
/// Full encoder output, including nullable codec-specific metadata. Encoded
/// byte storage is independently owned before entering this callback.
pub trait EncodedImageCallbackProvider: Send + Sync + 'static {
    fn encoded(
        &self,
        image: crate::EncodedImage,
        info: Option<crate::OwnedCodecSpecificInfo>,
    ) -> crate::EncodedImageCallbackResult;
    fn frame_dropped(&self, rtp_timestamp: u32, spatial_id: i32, is_end_of_temporal_unit: bool);
}
pub struct EncodedImageCallbackSink {
    native: cxx::UniquePtr<ffi::NativeEncodedImageSink>,
    failures: VideoCodecCallbackFailureObserver,
}
// SAFETY: shared immutable native implementation holds a Send+Sync provider.
unsafe impl Send for EncodedImageCallbackSink {}
unsafe impl Sync for EncodedImageCallbackSink {}
impl Clone for EncodedImageCallbackSink {
    fn clone(&self) -> Self {
        Self {
            native: ffi::encoded_sink_clone(self.native()),
            failures: self.failures.clone(),
        }
    }
}
impl fmt::Debug for EncodedImageCallbackSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedImageCallbackSink")
            .field("failures", &self.failures)
            .finish_non_exhaustive()
    }
}
impl EncodedImageCallbackSink {
    pub fn new(provider: impl EncodedImageCallbackProvider) -> Self {
        Self::from_shared(Arc::new(provider))
    }
    pub fn from_shared(provider: Arc<dyn EncodedImageCallbackProvider>) -> Self {
        let failures = VideoCodecCallbackFailureObserver::default();
        Self {
            native: ffi::encoded_sink_new(Box::new(RustEncodedImageSink {
                provider: Some(provider),
                failures: failures.clone(),
                terminal: AtomicBool::new(false),
            })),
            failures,
        }
    }
    pub(crate) fn native(&self) -> &ffi::NativeEncodedImageSink {
        self.native.as_ref().expect("owning encoded callback")
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::encoded_sink_same(self.native(), other.native())
    }
    pub fn failure_observer(&self) -> VideoCodecCallbackFailureObserver {
        self.failures.clone()
    }
    pub fn encoded(
        &self,
        image: &crate::EncodedImage,
        info: Option<&crate::OwnedCodecSpecificInfo>,
    ) -> crate::EncodedImageCallbackResult {
        let pointer = info.map_or(std::ptr::null(), |v| v.native() as *const _);
        // SAFETY: metadata pointer is borrowed only for this synchronous call.
        crate::EncodedImageCallbackResult::from_native(unsafe {
            ffi::encoded_sink_frame(self.native(), image.native(), pointer)
        })
    }
    pub fn frame_dropped(&self, rtp_timestamp: u32, spatial_id: i32, end: bool) {
        ffi::encoded_sink_dropped(self.native(), rtp_timestamp, spatial_id, end);
    }
}
pub(crate) struct RustEncodedImageSink {
    provider: Option<Arc<dyn EncodedImageCallbackProvider>>,
    failures: VideoCodecCallbackFailureObserver,
    terminal: AtomicBool,
}
impl Drop for RustEncodedImageSink {
    fn drop(&mut self) {
        if catch_unwind(AssertUnwindSafe(|| drop(self.provider.take()))).is_err() {
            self.failures.record();
        }
    }
}
pub(crate) fn encoded_sink_provider_frame(
    provider: &RustEncodedImageSink,
    native: cxx::UniquePtr<ffi::NativeEncodedImage>,
    info: cxx::UniquePtr<ffi::NativeCodecSpecificInfo>,
) -> ffi::FfiEncodedCallbackResult {
    let failed = ffi::FfiEncodedCallbackResult {
        invoked: true,
        error: 1,
        frame_id: 0,
        drop_next_frame: false,
    };
    if native.is_null() || provider.terminal.load(Ordering::Acquire) {
        return failed;
    }
    match catch_unwind(AssertUnwindSafe(|| {
        provider
            .provider
            .as_deref()
            .expect("live encoded callback")
            .encoded(
                crate::EncodedImage::from_native(native),
                (!info.is_null()).then(|| crate::OwnedCodecSpecificInfo::from_native(info)),
            )
    })) {
        Ok(result) => result.native(),
        Err(()) => {
            provider.terminal.store(true, Ordering::Release);
            provider.failures.record();
            failed
        }
    }
}
pub(crate) fn encoded_sink_provider_dropped(
    provider: &RustEncodedImageSink,
    rtp_timestamp: u32,
    spatial_id: i32,
    end: bool,
) {
    if provider.terminal.load(Ordering::Acquire) {
        return;
    }
    if catch_unwind(AssertUnwindSafe(|| {
        provider
            .provider
            .as_deref()
            .expect("live encoded callback")
            .frame_dropped(rtp_timestamp, spatial_id, end)
    }))
    .is_err()
    {
        provider.terminal.store(true, Ordering::Release);
        provider.failures.record();
    }
}
pub(crate) struct RustDecodedImageSink {
    provider: Option<Arc<dyn DecodedImageCallbackProvider>>,
    failures: VideoCodecCallbackFailureObserver,
    terminal: AtomicBool,
}
impl Drop for RustDecodedImageSink {
    fn drop(&mut self) {
        if catch_unwind(AssertUnwindSafe(|| drop(self.provider.take()))).is_err() {
            self.failures.record();
        }
    }
}
#[allow(
    clippy::too_many_arguments,
    reason = "Preserve the existing native boundary parameter contract."
)]
pub(crate) fn decoded_sink_provider_invoke(
    provider: &RustDecodedImageSink,
    native: cxx::UniquePtr<ffi::NativeRawVideoFrame>,
    overload: u8,
    time_ms: i64,
    has_time: bool,
    optional_time: i32,
    has_qp: bool,
    qp: u8,
    status: &mut i32,
) -> cxx::UniquePtr<ffi::NativeRawVideoFrame> {
    *status = -1; // WEBRTC_VIDEO_CODEC_ERROR, never cached caller success.
    if native.is_null() {
        return native;
    }
    let mut frame = RawVideoFrame::from_decoded_native(native);
    if !provider.terminal.load(Ordering::Acquire) {
        match catch_unwind(AssertUnwindSafe(|| {
            let callback = provider
                .provider
                .as_deref()
                .expect("live decoded sink provider");
            match overload {
                0 => callback.decoded(&mut frame),
                1 => callback.decoded_with_time(&mut frame, time_ms),
                2 => {
                    callback.decoded_with_metadata(
                        &mut frame,
                        has_time.then_some(optional_time),
                        has_qp.then_some(qp),
                    );
                    0
                }
                _ => -4, // WEBRTC_VIDEO_CODEC_ERR_PARAMETER; closed native overload.
            }
        })) {
            Ok(value) => *status = value,
            Err(()) => {
                provider.terminal.store(true, Ordering::Release);
                provider.failures.record();
            }
        }
    }
    frame.into_native()
}

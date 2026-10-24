use crate::{TransformableFrameDirection, TransformableMediaFrame, ffi};
use std::{
    cell::Cell,
    fmt,
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

/// Provider methods can run concurrently on SDK and installation roles. Do not
/// synchronously wait for or reenter the owning engine/actor from these callbacks. Complete inline,
/// or move the owning frame and completion to an asynchronous caller queue.
/// Panics drop the request and are counted, never converted into passthrough.
pub trait FrameTransformerProvider: Send + Sync + 'static {
    fn transform(&self, frame: TransformableMediaFrame, completion: FrameTransformCompletion);
    /// A native callback binding became available, including when this provider
    /// selects an already installed host. Retain its owning handle if needed.
    /// Concurrent replacement/unregister can retire it before notification.
    fn callback_registered(&self, _callback: FrameTransformCallback) {}
    /// Native unregister or caller replacement retired this exact capability.
    /// Notifications can interleave with concurrent registrations; compare
    /// handles, not SSRC/media fields. Global resource close marks all handles
    /// retired without synthesizing per-host native unregister notifications.
    fn callback_unregistered(&self, _callback: FrameTransformCallback) {}
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameTransformBinding {
    pub audio: bool,
    pub direction: TransformableFrameDirection,
    /// None is the SDK singleton callback, not an inferred frame SSRC.
    pub callback_ssrc: Option<u32>,
}
impl FrameTransformBinding {
    fn from_native(value: ffi::FfiFrameTransformBinding) -> Self {
        Self {
            audio: value.audio,
            direction: if value.sender {
                TransformableFrameDirection::Sender
            } else {
                TransformableFrameDirection::Receiver
            },
            callback_ssrc: value.has_ssrc.then_some(value.ssrc),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameTransformCompletionOutcome {
    /// The installed native stage was invoked. Not a packet/wire acknowledgement.
    DeliveredToNative,
    /// Provider generation or SDK callback retired; owned frame was discarded.
    Retired,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameTransformCompletionError {
    WrongProvenanceOrRole,
    AlreadyCompleted,
    UnsafeNativeConsumption,
}
impl fmt::Display for FrameTransformCompletionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "frame transform completion failed: {self:?}")
    }
}
impl std::error::Error for FrameTransformCompletionError {}

fn completion_result(
    status: u8,
) -> Result<FrameTransformCompletionOutcome, FrameTransformCompletionError> {
    match status {
        0 => Ok(FrameTransformCompletionOutcome::DeliveredToNative),
        1 => Ok(FrameTransformCompletionOutcome::Retired),
        2 => Err(FrameTransformCompletionError::WrongProvenanceOrRole),
        3 => Err(FrameTransformCompletionError::AlreadyCompleted),
        4 => Err(FrameTransformCompletionError::UnsafeNativeConsumption),
        _ => unreachable!("closed completion bridge status"),
    }
}
/// Owning native callback capability for one host registration and caller epoch.
/// It can be retained before the first frame, cloned and used from caller queues.
/// Retiring the registration/provider makes future operations return retired;
/// already admitted calls may finish. No callback userdata is retained here.
pub struct FrameTransformCallback {
    native: cxx::UniquePtr<ffi::NativeFrameTransformCallback>,
}
// SAFETY: immutable binding/epoch data and synchronized unregister-barrier slots.
// SDK unregister retains final SDK-reference destruction on its owning role.
unsafe impl Send for FrameTransformCallback {}
unsafe impl Sync for FrameTransformCallback {}
impl Clone for FrameTransformCallback {
    fn clone(&self) -> Self {
        Self {
            native: ffi::frame_transform_callback_copy(self.native()),
        }
    }
}
impl FrameTransformCallback {
    fn native(&self) -> &ffi::NativeFrameTransformCallback {
        self.native
            .as_ref()
            .expect("owning native transform callback")
    }
    pub fn binding(&self) -> FrameTransformBinding {
        FrameTransformBinding::from_native(ffi::frame_transform_callback_binding(self.native()))
    }
    /// Identity is the actual retained SDK slot plus caller epoch, not fields
    /// inferred from a frame's SSRC or source ID.
    pub fn same_registration(&self, other: &Self) -> bool {
        ffi::frame_transform_callback_same(self.native(), other.native())
    }
    pub fn is_retired(&self) -> bool {
        ffi::frame_transform_callback_retired(self.native())
    }
    /// Actual native request, available before receiving any frame. A true
    /// result means invocation, not proof that a default SDK no-op took effect.
    /// SDK bypass persists until stream recreation, not provider replacement.
    pub fn request_short_circuiting(&self) -> bool {
        ffi::frame_transform_callback_short_circuit(self.native())
    }
    /// Invoke OnTransformedFrame with a real SDK-delivered frame/stock clone.
    /// This preserves its producer/role. Repeated clone forwarding is supported
    /// by the native callback, unlike a one-shot completion. The original input
    /// request, if still pending, becomes completed on the first delivery.
    pub fn on_transformed_frame(
        &self,
        frame: TransformableMediaFrame,
    ) -> Result<FrameTransformCompletionOutcome, FrameTransformCompletionError> {
        completion_result(ffi::frame_transform_callback_forward(
            self.native(),
            frame.into_native(),
        ))
    }
}
impl fmt::Debug for FrameTransformCallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameTransformCallback")
            .field("binding", &self.binding())
            .field("retired", &self.is_retired())
            .finish_non_exhaustive()
    }
}

/// Exclusive, one-shot completion for one actual SDK request. Drop abandons it.
/// Complete with an actual SDK-delivered frame or its stock clone. Video
/// consumers support both native stock directions, including forwarding from
/// another producer and receiver-to-sender cloning; audio's native consumers
/// likewise support forwarding stock frames across roles. Actual provenance is
/// kept, never retagged as this request's input.
/// Source-only prototypes without live SDK provenance cannot be completed.
pub struct FrameTransformCompletion {
    native: cxx::UniquePtr<ffi::NativeFrameTransformCompletion>,
    _exclusive: PhantomData<Cell<()>>,
}
// SAFETY: only synchronized native generation/slot references cross threads.
// Retirement releases the SDK callback on its unregister role. The token
// does not own, operate on or destroy the SDK callback's borrowed queue.
unsafe impl Send for FrameTransformCompletion {}
impl FrameTransformCompletion {
    /// Retain the actual registration callback independently of this one-shot
    /// request. Its identity matches the provider registration notification.
    pub fn callback(&self) -> FrameTransformCallback {
        FrameTransformCallback {
            native: ffi::frame_transform_completion_callback(
                self.native.as_ref().expect("owning transform completion"),
            ),
        }
    }
    pub fn binding(&self) -> FrameTransformBinding {
        FrameTransformBinding::from_native(ffi::frame_transform_completion_binding(
            self.native.as_ref().expect("owning transform completion"),
        ))
    }
    /// Invoke the SDK callback's actual short-circuit request without consuming
    /// this frame. A retired callback returns false. Native support may be a no-op.
    /// Supported SDK delegates bypass future transforms until stream recreation;
    /// selecting another caller provider does not undo that native decision.
    pub fn request_short_circuiting(&self) -> bool {
        ffi::frame_transform_short_circuit(
            self.native.as_ref().expect("owning transform completion"),
        )
    }
    pub fn complete(
        self,
        frame: TransformableMediaFrame,
    ) -> Result<FrameTransformCompletionOutcome, FrameTransformCompletionError> {
        completion_result(ffi::frame_transform_complete(
            self.native,
            frame.into_native(),
        ))
    }
}
impl fmt::Debug for FrameTransformCompletion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameTransformCompletion")
            .field("binding", &self.binding())
            .finish_non_exhaustive()
    }
}

/// Panic evidence that survives native-host and provider userdata retirement.
/// This observer retains only a counter, never callback userdata.
#[derive(Clone, Default)]
pub struct FrameTransformerFailureObserver(Arc<AtomicU64>);
impl FrameTransformerFailureObserver {
    /// Provider invocation and bridge-owned final userdata destruction panics.
    pub fn callback_panics(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
    fn record(&self) {
        let _ = self
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(1))
            });
    }
}
impl fmt::Debug for FrameTransformerFailureObserver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameTransformerFailureObserver")
            .field("callback_panics", &self.callback_panics())
            .finish()
    }
}

/// Independently synchronized counters, not a transactionally consistent sum.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrameTransformerStatistics {
    pub in_flight: usize,
    pub delivered: u64,
    pub completed: u64,
    pub abandoned: u64,
    pub late_completions: u64,
    pub capacity_drops: u64,
    pub wrong_frame: u64,
    pub callback_panics: u64,
    /// Successful reusable native-callback invocations, not input requests.
    pub callback_forwards: u64,
    pub late_callback_forwards: u64,
}
/// Owning thread-safe provider resource. Outer owner/ID release does not retire
/// installed SDK ownership. Explicit close retires new caller admissions;
/// already admitted callbacks may finish, but outstanding completions are late.
/// New frames resume the native path.
/// Native host adapters remain installed to preserve SDK registration lifetime
/// and required encoded observation/metadata composition.
pub struct FrameTransformer {
    native: cxx::UniquePtr<ffi::NativeFrameTransformer>,
    failures: FrameTransformerFailureObserver,
}
// SAFETY: shared native state contains atomic counters/flags, immutable capacity
// and an owning Send+Sync Rust provider. Clones share exactly that resource.
unsafe impl Send for FrameTransformer {}
unsafe impl Sync for FrameTransformer {}
impl Clone for FrameTransformer {
    fn clone(&self) -> Self {
        Self {
            native: ffi::frame_transformer_copy(self.native()),
            failures: self.failures.clone(),
        }
    }
}
impl FrameTransformer {
    /// Capacity is shared by all hosts selecting this resource. At capacity,
    /// frames are dropped, not forwarded around the caller transformation.
    /// Zero deliberately admits no caller requests.
    pub fn new(provider: impl FrameTransformerProvider, maximum_in_flight: usize) -> Self {
        Self::from_shared(Arc::new(provider), maximum_in_flight)
    }
    pub fn from_shared(
        provider: Arc<dyn FrameTransformerProvider>,
        maximum_in_flight: usize,
    ) -> Self {
        let failures = FrameTransformerFailureObserver::default();
        Self {
            native: ffi::frame_transformer_new(
                Box::new(RustFrameTransformerProvider {
                    provider: Some(provider),
                    failures: failures.clone(),
                }),
                maximum_in_flight,
            ),
            failures,
        }
    }
    pub(crate) fn native(&self) -> &ffi::NativeFrameTransformer {
        self.native.as_ref().expect("owning caller transformer")
    }
    pub fn failure_observer(&self) -> FrameTransformerFailureObserver {
        self.failures.clone()
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::frame_transformer_same(self.native(), other.native())
    }
    /// Retire all caller admissions/capabilities. Native registrations remain
    /// installed; no synthetic per-binding unregister notifications are issued.
    pub fn close(&self) {
        ffi::frame_transformer_close(self.native());
    }
    pub fn is_closed(&self) -> bool {
        ffi::frame_transformer_closed(self.native())
    }
    pub fn statistics(&self) -> FrameTransformerStatistics {
        let value = ffi::frame_transformer_statistics(self.native());
        FrameTransformerStatistics {
            in_flight: value.in_flight,
            delivered: value.delivered,
            completed: value.completed,
            abandoned: value.abandoned,
            late_completions: value.late_completions,
            capacity_drops: value.capacity_drops,
            wrong_frame: value.wrong_frame,
            callback_panics: value.callback_panics,
            callback_forwards: value.callback_forwards,
            late_callback_forwards: value.late_callback_forwards,
        }
    }
}
impl fmt::Debug for FrameTransformer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameTransformer")
            .field("closed", &self.is_closed())
            .field("statistics", &self.statistics())
            .finish()
    }
}
pub(crate) struct RustFrameTransformerProvider {
    provider: Option<Arc<dyn FrameTransformerProvider>>,
    failures: FrameTransformerFailureObserver,
}
use crate::panic_payload::discard as discard_panic;
impl Drop for RustFrameTransformerProvider {
    fn drop(&mut self) {
        let _callback = crate::provider_panic::ProviderCallbackScope::enter();
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(self.provider.take()))) {
            self.failures.record();
            discard_panic(payload);
        }
    }
}
fn invoke_provider(
    provider: &RustFrameTransformerProvider,
    operation: impl FnOnce(&dyn FrameTransformerProvider),
) {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
        operation(
            provider
                .provider
                .as_deref()
                .expect("live transformer provider"),
        )
    })) {
        provider.failures.record();
        discard_panic(payload);
    }
}
pub(crate) fn frame_transformer_provider_transform(
    provider: &RustFrameTransformerProvider,
    frame: cxx::UniquePtr<ffi::NativeTransformableFrame>,
    completion: cxx::UniquePtr<ffi::NativeFrameTransformCompletion>,
) {
    invoke_provider(provider, |provider| {
        provider.transform(
            TransformableMediaFrame::from_native(frame),
            FrameTransformCompletion {
                native: completion,
                _exclusive: PhantomData,
            },
        )
    });
}
pub(crate) fn frame_transformer_provider_binding(
    provider: &RustFrameTransformerProvider,
    callback: cxx::UniquePtr<ffi::NativeFrameTransformCallback>,
    registered: bool,
) {
    invoke_provider(provider, |provider| {
        let callback = FrameTransformCallback { native: callback };
        if registered {
            provider.callback_registered(callback);
        } else {
            provider.callback_unregistered(callback);
        }
    });
}
pub(crate) fn frame_transformer_provider_panics(provider: &RustFrameTransformerProvider) -> u64 {
    provider.failures.callback_panics()
}

use crate::provider_panic::catch_provider as catch_unwind;
use crate::{FieldTrialsSnapshot, Timestamp, ffi};
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Complete declared native FEC packet-mask domain. Unlike scoped SDK enums,
/// this unscoped enum cannot safely carry arbitrary signed integer values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FecMaskType {
    #[default]
    Random,
    Bursty,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FecProtectionParams {
    pub fec_rate: i32,
    pub max_fec_frames: i32,
    pub fec_mask_type: FecMaskType,
}
impl FecProtectionParams {
    fn native(self) -> ffi::FfiFecProtectionParams {
        ffi::FfiFecProtectionParams {
            fec_rate: self.fec_rate,
            max_fec_frames: self.max_fec_frames,
            mask_type: match self.fec_mask_type {
                FecMaskType::Random => 0,
                FecMaskType::Bursty => 1,
            },
        }
    }
}
/// Bitwise native retransmission policy. Unknown bits and signed values are
/// retained: the protection callback takes int, not a closed enum parameter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FecRetransmissionMode(i32);
impl FecRetransmissionMode {
    pub const OFF: Self = Self(0);
    pub const BASE_LAYER: Self = Self(2);
    pub const HIGHER_LAYERS: Self = Self(4);
    pub const ALL_LAYERS: Self = Self(6);
    pub const fn from_bits_retain(bits: i32) -> Self {
        Self(bits)
    }
    pub const fn bits(self) -> i32 {
        self.0
    }
}
impl std::ops::BitOr for FecRetransmissionMode {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}
/// Full scoped native frame-type domain. The existing encoded-input
/// VideoFrameType describes only key/delta access units; this controller value
/// also preserves empty frames and unknown signed SDK frame types.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FecEncodedFrameType {
    Empty,
    Key,
    Delta,
    Unknown(i32),
}
impl From<i32> for FecEncodedFrameType {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::Empty,
            3 => Self::Key,
            4 => Self::Delta,
            value => Self::Unknown(value),
        }
    }
}
impl From<crate::VideoFrameType> for FecEncodedFrameType {
    fn from(value: crate::VideoFrameType) -> Self {
        match value {
            crate::VideoFrameType::Key => Self::Key,
            crate::VideoFrameType::Delta => Self::Delta,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FecEncodingData {
    pub width: usize,
    pub height: usize,
    pub num_temporal_layers: usize,
    pub max_payload_size: usize,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FecRateUpdate {
    pub estimated_bitrate_bps: u32,
    pub actual_framerate: i32,
    pub fraction_lost: u8,
    pub loss_mask: Vec<bool>,
    pub round_trip_time_ms: i64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FecProtectionResponse {
    pub native_result: i32,
    pub sent_video_rate_bps: u32,
    pub sent_nack_rate_bps: u32,
    pub sent_fec_rate_bps: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FecProtectionCallbackError {
    Retired,
    WrongSequence,
    OutsideInvocation,
    InvalidParameters,
    ReentrantNativeLock,
    Cancelled,
}
impl std::fmt::Display for FecProtectionCallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Retired => "FEC protection callback is retired",
            Self::WrongSequence => "FEC protection callback requires its native sequence",
            Self::OutsideInvocation => "FEC callback requires a live native invocation borrow",
            Self::InvalidParameters => "invalid native FEC protection parameters",
            Self::ReentrantNativeLock => "FEC callback would reenter the native sender lock",
            Self::Cancelled => "FEC callback task was cancelled before execution",
        })
    }
}
impl std::error::Error for FecProtectionCallbackError {}
/// Retainable, expiring capability for the SDK's borrowed protection callback.
/// Synchronous calls require a live native controller invocation borrow;
/// protection requests additionally require its captured worker sequence.
/// Retransmission mutation rejects UpdateFecRates' nonrecursive sender lock.
/// Async variants dispatch on that existing worker and establish a new borrow
/// only if the sender is still alive when the task runs. Retained handles never
/// extend sender lifetime, and cannot access a partially torn-down sender.
#[derive(Clone)]
pub struct FecProtectionCallback {
    native: cxx::SharedPtr<ffi::NativeFecProtectionCallback>,
}
// Native state locks retirement against posting, checks invocation borrows and
// worker affinity before calls, and never exposes its borrowed sender pointer.
unsafe impl Send for ffi::NativeFecProtectionCallback {}
unsafe impl Sync for ffi::NativeFecProtectionCallback {}
impl FecProtectionCallback {
    fn result(status: u8) -> Result<(), FecProtectionCallbackError> {
        match status {
            0 => Ok(()),
            1 => Err(FecProtectionCallbackError::Retired),
            2 => Err(FecProtectionCallbackError::WrongSequence),
            3 => Err(FecProtectionCallbackError::OutsideInvocation),
            4 => Err(FecProtectionCallbackError::InvalidParameters),
            5 => Err(FecProtectionCallbackError::ReentrantNativeLock),
            _ => Err(FecProtectionCallbackError::Cancelled),
        }
    }
    pub fn request(
        &self,
        delta: FecProtectionParams,
        key: FecProtectionParams,
    ) -> Result<FecProtectionResponse, FecProtectionCallbackError> {
        let value = ffi::fec_protection_request(
            self.native.as_ref().expect("retained FEC callback state"),
            delta.native(),
            key.native(),
        );
        Self::result(value.status)?;
        Ok(FecProtectionResponse {
            native_result: value.native_result,
            sent_video_rate_bps: value.video_bps,
            sent_nack_rate_bps: value.nack_bps,
            sent_fec_rate_bps: value.fec_bps,
        })
    }
    pub fn set_retransmission_mode(
        &self,
        mode: FecRetransmissionMode,
    ) -> Result<(), FecProtectionCallbackError> {
        Self::result(ffi::fec_set_retransmission_mode(
            self.native.as_ref().expect("retained FEC callback state"),
            mode.bits(),
        ))
    }
    /// Dispatch on the existing native worker queue. No thread or runtime is
    /// created. Controlled execution progresses it through the caller's world.
    pub fn request_async(
        &self,
        delta: FecProtectionParams,
        key: FecProtectionParams,
    ) -> FecCallbackOperation<FecProtectionResponse> {
        let (operation, completion) = FecCallbackOperation::new(|value| value);
        ffi::fec_protection_request_async(
            self.native.clone(),
            delta.native(),
            key.native(),
            completion,
        );
        operation
    }
    /// Safe alternative when the current controller call holds the native
    /// sender lock (notably UpdateFecRates). Completion follows the actual
    /// native call. Do not wait for it inside a synchronous provider callback.
    pub fn set_retransmission_mode_async(
        &self,
        mode: FecRetransmissionMode,
    ) -> FecCallbackOperation<()> {
        let (operation, completion) = FecCallbackOperation::new(|value| value.map(|_| ()));
        ffi::fec_set_retransmission_mode_async(self.native.clone(), mode.bits(), completion);
        operation
    }
}
struct CompletionState {
    value: Option<Result<FecProtectionResponse, FecProtectionCallbackError>>,
    waker: Option<std::task::Waker>,
}
impl Drop for CompletionState {
    fn drop(&mut self) {
        let waker = self.waker.take();
        let _ = catch_unwind(AssertUnwindSafe(|| drop(waker)));
    }
}
/// Move-only runtime-neutral native callback completion. Dropping the observer
/// abandons observation, not an already posted native side effect. Queue
/// cancellation and callback retirement yield inspectable terminal errors.
pub struct FecCallbackOperation<T> {
    state: Arc<Mutex<CompletionState>>,
    map: fn(
        Result<FecProtectionResponse, FecProtectionCallbackError>,
    ) -> Result<T, FecProtectionCallbackError>,
    marker: std::marker::PhantomData<std::cell::Cell<()>>,
}
impl<T> FecCallbackOperation<T> {
    fn new(
        map: fn(
            Result<FecProtectionResponse, FecProtectionCallbackError>,
        ) -> Result<T, FecProtectionCallbackError>,
    ) -> (Self, Box<RustFecCompletion>) {
        let state = Arc::new(Mutex::new(CompletionState {
            value: None,
            waker: None,
        }));
        (
            Self {
                state: state.clone(),
                map,
                marker: std::marker::PhantomData,
            },
            Box::new(RustFecCompletion {
                state,
                completed: AtomicBool::new(false),
            }),
        )
    }
    pub fn try_take(&mut self) -> Option<Result<T, FecProtectionCallbackError>> {
        let value = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .value
            .take();
        value.map(self.map)
    }
}
impl<T> std::future::Future for FecCallbackOperation<T> {
    type Output = Result<T, FecProtectionCallbackError>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        let mut state = this.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(value) = state.value.take() {
            return std::task::Poll::Ready((this.map)(value));
        }
        let old = state.waker.replace(cx.waker().clone());
        drop(state);
        let _ = catch_unwind(AssertUnwindSafe(|| drop(old)));
        std::task::Poll::Pending
    }
}
pub(crate) struct RustFecCompletion {
    state: Arc<Mutex<CompletionState>>,
    completed: AtomicBool,
}
impl RustFecCompletion {
    fn complete(&self, result: Result<FecProtectionResponse, FecProtectionCallbackError>) {
        if self.completed.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.value = Some(result);
        let waker = state.waker.take();
        drop(state);
        if let Some(waker) = waker {
            let _ = catch_unwind(AssertUnwindSafe(|| waker.wake()));
        }
    }
}
impl Drop for RustFecCompletion {
    fn drop(&mut self) {
        self.complete(Err(FecProtectionCallbackError::Cancelled));
    }
}
pub(crate) fn fec_async_complete(source: &RustFecCompletion, result: ffi::FfiFecProtectionResult) {
    source.complete(
        FecProtectionCallback::result(result.status).map(|_| FecProtectionResponse {
            native_result: result.native_result,
            sent_video_rate_bps: result.video_bps,
            sent_nack_rate_bps: result.nack_bps,
            sent_fec_rate_bps: result.fec_bps,
        }),
    );
}
/// Retained native creation environment, independent of the factory/peer actor.
pub struct FecControllerEnvironment(crate::EventLogEnvironment);
impl FecControllerEnvironment {
    pub fn time_us(&self) -> i64 {
        self.0.time_us()
    }
    pub fn timestamp(&self) -> Timestamp {
        self.0.timestamp()
    }
    pub fn field_trials(&self) -> FieldTrialsSnapshot {
        self.0.field_trials()
    }
    pub fn environment(&self) -> &crate::Environment {
        self.0.environment()
    }
}
/// Full callable FEC controller product. Native can call methods concurrently
/// from arbitrary threads. Sync and shared receivers preserve that contract;
/// implementors control state locking and must release their own locks before
/// invoking callbacks that can take the native sender lock. No bridge mutex
/// spans provider work. Do not block or reenter the calling session. Controlled
/// providers must perform only caller-controlled synchronous work.
pub trait FecController: Send + Sync + 'static {
    fn set_protection_callback(&self, callback: Option<FecProtectionCallback>);
    fn set_protection_method(&self, enable_fec: bool, enable_nack: bool);
    fn set_encoding_data(&self, data: FecEncodingData);
    fn update_fec_rates(&self, update: FecRateUpdate) -> u32;
    fn update_with_encoded_data(&self, length: usize, frame_type: FecEncodedFrameType);
    fn use_loss_vector_mask(&self) -> bool;
}
pub enum FecControllerCreation {
    Builtin,
    Provider(Box<dyn FecController>),
}
/// Factory creation occurs on the native worker sequence. The video-send
/// caller requires a non-null product, so a provider must return a product or
/// explicitly select the native built-in controller, not nullable success.
pub trait FecControllerFactoryProvider: Send + 'static {
    fn create_fec_controller(
        &mut self,
        environment: FecControllerEnvironment,
    ) -> FecControllerCreation;
}
struct FactoryState {
    provider: Mutex<Option<Box<dyn FecControllerFactoryProvider>>>,
    builtin: bool,
    failed: AtomicBool,
}
impl Drop for FactoryState {
    fn drop(&mut self) {
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let _ = catch_unwind(AssertUnwindSafe(|| drop(provider)));
    }
}
#[derive(Clone)]
pub struct FecControllerFactory(Arc<FactoryState>);
impl FecControllerFactory {
    pub fn from_provider(provider: impl FecControllerFactoryProvider) -> Self {
        Self(Arc::new(FactoryState {
            provider: Mutex::new(Some(Box::new(provider))),
            builtin: false,
            failed: AtomicBool::new(false),
        }))
    }
    pub fn builtin() -> Self {
        Self(Arc::new(FactoryState {
            provider: Mutex::new(None),
            builtin: true,
            failed: AtomicBool::new(false),
        }))
    }
    pub fn has_failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
}
pub(crate) struct RustFecControllerFactory(
    pub Option<FecControllerFactory>,
    pub Option<crate::Environment>,
);
pub(crate) struct RustFecController {
    provider: Option<Box<dyn FecController>>,
    builtin: bool,
    failed: AtomicBool,
    _environment: Option<crate::EventLogEnvironment>,
}
impl Drop for RustFecController {
    fn drop(&mut self) {
        let provider = self.provider.take();
        let _ = catch_unwind(AssertUnwindSafe(|| drop(provider)));
    }
}
impl RustFecController {
    fn with<T>(&self, fallback: T, call: impl FnOnce(&dyn FecController) -> T) -> T {
        if self.failed.load(Ordering::Acquire) {
            return fallback;
        }
        let Some(provider) = self.provider.as_deref() else {
            return fallback;
        };
        match catch_unwind(AssertUnwindSafe(|| call(provider))) {
            Ok(value) => {
                if self.failed.load(Ordering::Acquire) {
                    fallback
                } else {
                    value
                }
            }
            Err(_) => {
                self.failed.store(true, Ordering::Release);
                fallback
            }
        }
    }
}
pub(crate) fn fec_factory_present(source: &RustFecControllerFactory) -> bool {
    source.0.is_some()
}
pub(crate) fn fec_controller_create(
    source: &RustFecControllerFactory,
    environment: cxx::UniquePtr<ffi::NativeEnvironment>,
) -> Box<RustFecController> {
    let environment = crate::EventLogEnvironment::from_native(environment, source.1.as_ref());
    let mut product = None;
    let mut builtin = false;
    let mut failed = false;
    if let Some(factory) = &source.0 {
        let mut provider = factory
            .0
            .provider
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if factory.has_failed() {
            failed = true;
        } else if factory.0.builtin {
            builtin = true;
        } else if let Some(provider) = provider.as_deref_mut() {
            match catch_unwind(AssertUnwindSafe(|| {
                provider.create_fec_controller(FecControllerEnvironment(environment.clone()))
            })) {
                Ok(FecControllerCreation::Builtin) => builtin = true,
                Ok(FecControllerCreation::Provider(value)) => product = Some(value),
                Err(_) => {
                    factory.0.failed.store(true, Ordering::Release);
                    failed = true;
                }
            }
        }
    }
    Box::new(RustFecController {
        provider: product,
        builtin,
        failed: AtomicBool::new(failed),
        _environment: Some(environment),
    })
}
pub(crate) fn fec_controller_builtin(source: &RustFecController) -> bool {
    source.builtin
}
pub(crate) fn fec_controller_set_callback(
    source: &RustFecController,
    native: cxx::SharedPtr<ffi::NativeFecProtectionCallback>,
) {
    let callback = (!native.is_null()).then_some(FecProtectionCallback { native });
    source.with((), |provider| provider.set_protection_callback(callback));
}
pub(crate) fn fec_controller_set_method(source: &RustFecController, fec: bool, nack: bool) {
    source.with((), |provider| provider.set_protection_method(fec, nack));
}
pub(crate) fn fec_controller_set_encoding(
    source: &RustFecController,
    width: usize,
    height: usize,
    temporal_layers: usize,
    max_payload_size: usize,
) {
    source.with((), |provider| {
        provider.set_encoding_data(FecEncodingData {
            width,
            height,
            num_temporal_layers: temporal_layers,
            max_payload_size,
        })
    });
}
pub(crate) fn fec_controller_update_rates(
    source: &RustFecController,
    bitrate: u32,
    framerate: i32,
    lost: u8,
    mask: &[u8],
    rtt_ms: i64,
) -> u32 {
    source.with(0, |provider| {
        provider.update_fec_rates(FecRateUpdate {
            estimated_bitrate_bps: bitrate,
            actual_framerate: framerate,
            fraction_lost: lost,
            loss_mask: mask.iter().map(|&value| value != 0).collect(),
            round_trip_time_ms: rtt_ms,
        })
    })
}
pub(crate) fn fec_controller_update_encoded(
    source: &RustFecController,
    length: usize,
    frame_type: i32,
) {
    source.with((), |provider| {
        provider.update_with_encoded_data(length, frame_type.into())
    });
}
pub(crate) fn fec_controller_use_mask(source: &RustFecController) -> bool {
    source.with(false, |provider| provider.use_loss_vector_mask())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, atomic::AtomicUsize};
    #[derive(Debug, PartialEq)]
    enum Call {
        Callback(bool),
        Method(bool, bool),
        Encoding(FecEncodingData),
        Encoded(usize, FecEncodedFrameType),
        Rates(FecRateUpdate),
        Mask,
    }
    #[derive(Default)]
    struct Observations {
        calls: Mutex<Vec<Call>>,
        callback: Mutex<Option<FecProtectionCallback>>,
        environment: Mutex<Option<FecControllerEnvironment>>,
        response: Mutex<Option<FecProtectionResponse>>,
        retired_in_drop: AtomicBool,
        factory_drops: AtomicUsize,
        product_drops: AtomicUsize,
    }
    struct Factory(Arc<Observations>, std::cell::Cell<usize>);
    impl FecControllerFactoryProvider for Factory {
        fn create_fec_controller(
            &mut self,
            environment: FecControllerEnvironment,
        ) -> FecControllerCreation {
            self.1.set(self.1.get() + 1);
            *self.0.environment.lock().unwrap() = Some(environment);
            FecControllerCreation::Provider(Box::new(Observe(self.0.clone())))
        }
    }
    impl Drop for Factory {
        fn drop(&mut self) {
            self.0.factory_drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Observe(Arc<Observations>);
    fn params(rate: i32, frames: i32, mask: FecMaskType) -> FecProtectionParams {
        FecProtectionParams {
            fec_rate: rate,
            max_fec_frames: frames,
            fec_mask_type: mask,
        }
    }
    impl FecController for Observe {
        fn set_protection_callback(&self, callback: Option<FecProtectionCallback>) {
            self.0
                .calls
                .lock()
                .unwrap()
                .push(Call::Callback(callback.is_some()));
            if let Some(callback) = &callback {
                callback
                    .set_retransmission_mode(FecRetransmissionMode::from_bits_retain(-17))
                    .unwrap();
                *self.0.response.lock().unwrap() = Some(
                    callback
                        .request(
                            params(200, 31, FecMaskType::Random),
                            params(7, -3, FecMaskType::Bursty),
                        )
                        .unwrap(),
                );
                assert_eq!(
                    callback.request(
                        params(256, 1, FecMaskType::Random),
                        FecProtectionParams::default()
                    ),
                    Err(FecProtectionCallbackError::InvalidParameters)
                );
                assert_eq!(
                    callback
                        .request_async(
                            FecProtectionParams::default(),
                            FecProtectionParams::default()
                        )
                        .try_take(),
                    Some(Err(FecProtectionCallbackError::WrongSequence))
                );
            }
            *self.0.callback.lock().unwrap() = callback;
        }
        fn set_protection_method(&self, fec: bool, nack: bool) {
            self.0.calls.lock().unwrap().push(Call::Method(fec, nack));
        }
        fn set_encoding_data(&self, data: FecEncodingData) {
            self.0.calls.lock().unwrap().push(Call::Encoding(data));
        }
        fn update_fec_rates(&self, update: FecRateUpdate) -> u32 {
            assert_eq!(self.0.factory_drops.load(Ordering::SeqCst), 1);
            let callback = self.0.callback.lock().unwrap().clone().unwrap();
            assert_eq!(
                callback.set_retransmission_mode(FecRetransmissionMode::ALL_LAYERS),
                Err(FecProtectionCallbackError::ReentrantNativeLock)
            );
            self.0.calls.lock().unwrap().push(Call::Rates(update));
            73_212
        }
        fn update_with_encoded_data(&self, length: usize, frame_type: FecEncodedFrameType) {
            self.0
                .calls
                .lock()
                .unwrap()
                .push(Call::Encoded(length, frame_type));
        }
        fn use_loss_vector_mask(&self) -> bool {
            self.0.calls.lock().unwrap().push(Call::Mask);
            true
        }
    }
    impl Drop for Observe {
        fn drop(&mut self) {
            let callback = self.0.callback.lock().unwrap().clone();
            if let Some(callback) = callback {
                self.0.retired_in_drop.store(
                    callback.request(
                        FecProtectionParams::default(),
                        FecProtectionParams::default(),
                    ) == Err(FecProtectionCallbackError::Retired),
                    Ordering::SeqCst,
                );
            }
            self.0.product_drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[test]
    fn native_product_outlives_factory_and_preserves_all_calls_and_callback_outputs() {
        let environment = crate::Environment::builder().build().unwrap();
        let observations = Arc::new(Observations::default());
        let registration = FecControllerFactory::from_provider(Factory(
            observations.clone(),
            std::cell::Cell::new(0),
        ));
        assert_eq!(
            ffi::fec_native_probe(
                environment.native(),
                Box::new(RustFecControllerFactory(Some(registration), None))
            ),
            73_212
        );
        assert_eq!(
            *observations.response.lock().unwrap(),
            Some(FecProtectionResponse {
                native_result: -17,
                sent_video_rate_bps: 200,
                sent_nack_rate_bps: (-3i32) as u32,
                sent_fec_rate_bps: 1
            })
        );
        assert_eq!(
            *observations.calls.lock().unwrap(),
            vec![
                Call::Callback(true),
                Call::Method(true, false),
                Call::Encoding(FecEncodingData {
                    width: 640,
                    height: 360,
                    num_temporal_layers: 3,
                    max_payload_size: 1234
                }),
                Call::Encoded(71, FecEncodedFrameType::Key),
                Call::Encoded(0, FecEncodedFrameType::Empty),
                Call::Encoded(89, FecEncodedFrameType::Unknown(-19)),
                Call::Mask,
                Call::Rates(FecRateUpdate {
                    estimated_bitrate_bps: 123456,
                    actual_framerate: -7,
                    fraction_lost: 255,
                    loss_mask: vec![true, false, true],
                    round_trip_time_ms: i64::MIN
                }),
            ]
        );
        assert_eq!(observations.factory_drops.load(Ordering::SeqCst), 1);
        assert_eq!(observations.product_drops.load(Ordering::SeqCst), 1);
        assert!(observations.retired_in_drop.load(Ordering::SeqCst));
        let retained = observations.callback.lock().unwrap().take().unwrap();
        drop(environment);
        assert_eq!(
            retained.request(
                FecProtectionParams::default(),
                FecProtectionParams::default()
            ),
            Err(FecProtectionCallbackError::Retired)
        );
        assert_eq!(
            retained.set_retransmission_mode(FecRetransmissionMode::OFF),
            Err(FecProtectionCallbackError::Retired)
        );
        assert_eq!(
            retained
                .set_retransmission_mode_async(FecRetransmissionMode::OFF)
                .try_take(),
            Some(Err(FecProtectionCallbackError::Retired))
        );
        let environment = observations.environment.lock().unwrap().take().unwrap();
        let _ = environment.timestamp();
        let _ = environment.field_trials();
        std::thread::spawn(move || {
            let _ = environment.time_us();
        })
        .join()
        .unwrap();
    }
    struct Concurrent {
        entered: Arc<Barrier>,
        released: Arc<Barrier>,
    }
    impl FecController for Concurrent {
        fn set_protection_callback(&self, _: Option<FecProtectionCallback>) {}
        fn set_protection_method(&self, _: bool, _: bool) {}
        fn set_encoding_data(&self, _: FecEncodingData) {}
        fn update_fec_rates(&self, _: FecRateUpdate) -> u32 {
            self.entered.wait();
            self.released.wait();
            99
        }
        fn update_with_encoded_data(&self, _: usize, _: FecEncodedFrameType) {}
        fn use_loss_vector_mask(&self) -> bool {
            panic!("product failure")
        }
    }
    #[test]
    fn concurrent_failure_discards_late_product_results_without_serializing_callbacks() {
        let entered = Arc::new(Barrier::new(2));
        let released = Arc::new(Barrier::new(2));
        let source = Arc::new(RustFecController {
            provider: Some(Box::new(Concurrent {
                entered: entered.clone(),
                released: released.clone(),
            })),
            builtin: false,
            failed: AtomicBool::new(false),
            _environment: None,
        });
        let worker = {
            let source = source.clone();
            std::thread::spawn(move || {
                fec_controller_update_rates(&source, u32::MAX, i32::MIN, u8::MAX, &[0, 1], i64::MIN)
            })
        };
        entered.wait();
        assert!(!fec_controller_use_mask(&source));
        released.wait();
        assert_eq!(worker.join().unwrap(), 0);
        assert!(source.failed.load(Ordering::Acquire));
        assert!(!fec_controller_use_mask(&source));
    }
    struct PanicFactory(Arc<AtomicUsize>);
    impl FecControllerFactoryProvider for PanicFactory {
        fn create_fec_controller(&mut self, _: FecControllerEnvironment) -> FecControllerCreation {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("factory failure")
        }
    }
    #[test]
    fn factory_failure_latches_and_never_selects_builtin_success() {
        let calls = Arc::new(AtomicUsize::new(0));
        let factory = FecControllerFactory::from_provider(PanicFactory(calls.clone()));
        let environment = crate::Environment::builder().build().unwrap();
        for _ in 0..2 {
            assert_eq!(
                ffi::fec_native_probe(
                    environment.native(),
                    Box::new(RustFecControllerFactory(Some(factory.clone()), None))
                ),
                0
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(factory.has_failed());
    }
    #[test]
    fn registration_and_pending_outcomes_are_actor_safe_and_exactly_once() {
        fn shared<T: Send + Sync>() {}
        fn send<T: Send>() {}
        shared::<FecControllerFactory>();
        shared::<FecControllerEnvironment>();
        shared::<FecProtectionCallback>();
        send::<FecCallbackOperation<FecProtectionResponse>>();
        send::<FecCallbackOperation<()>>();
        assert_eq!(
            (FecRetransmissionMode::BASE_LAYER | FecRetransmissionMode::HIGHER_LAYERS).bits(),
            FecRetransmissionMode::ALL_LAYERS.bits()
        );
        assert_eq!(FecEncodedFrameType::from(4), FecEncodedFrameType::Delta);
        assert!(!fec_factory_present(&RustFecControllerFactory(None, None)));
        let (mut operation, completion) = FecCallbackOperation::new(|value| value.map(|_| ()));
        assert_eq!(operation.try_take(), None);
        drop(completion);
        assert_eq!(
            operation.try_take(),
            Some(Err(FecProtectionCallbackError::Cancelled))
        );
        let (mut operation, completion) = FecCallbackOperation::new(|value| value);
        fec_async_complete(
            &completion,
            ffi::FfiFecProtectionResult {
                status: 0,
                native_result: i32::MIN,
                video_bps: u32::MAX,
                nack_bps: 0,
                fec_bps: 9,
            },
        );
        drop(completion);
        assert_eq!(
            operation.try_take(),
            Some(Ok(FecProtectionResponse {
                native_result: i32::MIN,
                sent_video_rate_bps: u32::MAX,
                sent_nack_rate_bps: 0,
                sent_fec_rate_bps: 9
            }))
        );
        assert_eq!(operation.try_take(), None);
        let environment = crate::Environment::builder().build().unwrap();
        let builtin = fec_controller_create(
            &RustFecControllerFactory(Some(FecControllerFactory::builtin()), None),
            ffi::clone_environment(environment.native()),
        );
        assert!(fec_controller_builtin(&builtin));
    }
}

//! Owning native video encoder selectors. Native callbacks run on the SDK
//! encoder queue; creation and final destruction can run on another thread.
use crate::provider_panic::catch_provider as catch_unwind;
use crate::{DataRate, RenderResolution, SdpVideoFormat, ffi};
use std::{
    cell::{Cell, RefCell},
    fmt,
    marker::PhantomData,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
};

/// Stateful selector. Calls are serialized, including standalone application
/// use and native use. Do not wait for or reenter the owning peer/actor from a
/// callback. Inputs and returned formats are independent owning SDK values.
pub trait VideoEncoderSelectorProvider: Send + 'static {
    fn on_current_encoder(&mut self, format: SdpVideoFormat);
    fn on_available_bitrate(&mut self, bitrate: DataRate) -> Option<SdpVideoFormat>;
    fn on_resolution_change(&mut self, _resolution: RenderResolution) -> Option<SdpVideoFormat> {
        None
    }
    fn on_encoder_broken(&mut self) -> Option<SdpVideoFormat>;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoEncoderSelectorFailure {
    ProviderPanicked,
    ReentrantCall,
    DestructorPanicked,
}
impl VideoEncoderSelectorFailure {
    fn raw(self) -> u8 {
        match self {
            Self::ProviderPanicked => 1,
            Self::ReentrantCall => 2,
            Self::DestructorPanicked => 3,
        }
    }
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::ProviderPanicked),
            2 => Some(Self::ReentrantCall),
            3 => Some(Self::DestructorPanicked),
            _ => None,
        }
    }
}
impl fmt::Display for VideoEncoderSelectorFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ProviderPanicked => "video encoder selector provider panicked",
            Self::ReentrantCall => "recursive use of the same video encoder selector",
            Self::DestructorPanicked => "video encoder selector provider destructor panicked",
        })
    }
}
impl std::error::Error for VideoEncoderSelectorFailure {}
/// Independent first-failure observer. It retains no selector, peer or queue,
/// and can observe a destructor failure after the last native owner is dropped.
#[derive(Clone, Debug)]
pub struct VideoEncoderSelectorFailureObserver(Arc<AtomicU8>);
impl VideoEncoderSelectorFailureObserver {
    pub fn failure(&self) -> Option<VideoEncoderSelectorFailure> {
        VideoEncoderSelectorFailure::from_raw(self.0.load(Ordering::Acquire))
    }
    fn set(&self, failure: VideoEncoderSelectorFailure) {
        let _ = self
            .0
            .compare_exchange(0, failure.raw(), Ordering::AcqRel, Ordering::Acquire);
    }
}
thread_local! { static ACTIVE_SELECTORS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) }; }
struct ActiveSelector(usize);
impl Drop for ActiveSelector {
    fn drop(&mut self) {
        ACTIVE_SELECTORS.with(|active| {
            let mut active = active.borrow_mut();
            let index = active
                .iter()
                .rposition(|id| *id == self.0)
                .expect("active selector lease");
            active.remove(index);
        });
    }
}
struct SelectorState {
    provider: Mutex<Option<Box<dyn VideoEncoderSelectorProvider>>>,
    failure: VideoEncoderSelectorFailureObserver,
}
impl SelectorState {
    fn run<T>(&self, body: impl FnOnce(&mut dyn VideoEncoderSelectorProvider) -> T) -> Option<T> {
        if self.failure.failure().is_some() {
            return None;
        }
        let identity = self as *const Self as usize;
        let entered = ACTIVE_SELECTORS.with(|active| {
            let mut active = active.borrow_mut();
            if active.contains(&identity) {
                false
            } else {
                active.push(identity);
                true
            }
        });
        if !entered {
            self.failure.set(VideoEncoderSelectorFailure::ReentrantCall);
            return None;
        }
        let _active = ActiveSelector(identity);
        let mut provider = self
            .provider
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // A preceding concurrent call may have failed while this one waited.
        if self.failure.failure().is_some() {
            return None;
        }
        // Keep serialization held through failure publication. Catching around
        // the lock itself would unlock during unwind before latching poison,
        // allowing a waiter to enter the partially failed provider.
        match catch_unwind(AssertUnwindSafe(|| {
            body(provider.as_deref_mut().expect("live selector provider"))
        })) {
            Ok(result) => {
                if self.failure.failure().is_some() {
                    None
                } else {
                    Some(result)
                }
            }
            Err(()) => {
                self.failure
                    .set(VideoEncoderSelectorFailure::ProviderPanicked);
                None
            }
        }
    }
}
impl Drop for SelectorState {
    fn drop(&mut self) {
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if catch_unwind(AssertUnwindSafe(|| drop(provider))).is_err() {
            self.failure
                .set(VideoEncoderSelectorFailure::DestructorPanicked);
        }
    }
}
pub(crate) struct RustVideoEncoderSelectorObserver(pub VideoEncoderSelectorFailureObserver);
pub(crate) fn video_selector_provider_observer(
    provider: &RustVideoEncoderSelector,
) -> Box<RustVideoEncoderSelectorObserver> {
    Box::new(RustVideoEncoderSelectorObserver(
        provider.state.failure.clone(),
    ))
}
pub(crate) struct RustVideoEncoderSelector {
    state: Arc<SelectorState>,
}
fn registration(
    provider: impl VideoEncoderSelectorProvider,
) -> (
    Box<RustVideoEncoderSelector>,
    VideoEncoderSelectorFailureObserver,
) {
    let failure = VideoEncoderSelectorFailureObserver(Arc::new(AtomicU8::new(0)));
    let state = Arc::new(SelectorState {
        provider: Mutex::new(Some(Box::new(provider))),
        failure: failure.clone(),
    });
    (Box::new(RustVideoEncoderSelector { state }), failure)
}
pub(crate) fn video_selector_provider_current(
    provider: &RustVideoEncoderSelector,
    format: cxx::UniquePtr<ffi::NativeSdpVideoFormat>,
) {
    provider
        .state
        .run(|provider| provider.on_current_encoder(SdpVideoFormat::from_native(format)));
}
pub(crate) fn video_selector_provider_bitrate(
    provider: &RustVideoEncoderSelector,
    bits: i64,
    kind: i8,
) -> cxx::UniquePtr<ffi::NativeSdpVideoFormat> {
    provider
        .state
        .run(|provider| provider.on_available_bitrate(DataRate::from_native(bits, kind)))
        .flatten()
        .map_or_else(cxx::UniquePtr::null, |format| format.native)
}
pub(crate) fn video_selector_provider_resolution(
    provider: &RustVideoEncoderSelector,
    resolution: cxx::UniquePtr<ffi::NativeRenderResolution>,
) -> cxx::UniquePtr<ffi::NativeSdpVideoFormat> {
    provider
        .state
        .run(|provider| provider.on_resolution_change(RenderResolution::from_native(resolution)))
        .flatten()
        .map_or_else(cxx::UniquePtr::null, |format| format.native)
}
pub(crate) fn video_selector_provider_broken(
    provider: &RustVideoEncoderSelector,
) -> cxx::UniquePtr<ffi::NativeSdpVideoFormat> {
    provider
        .state
        .run(|provider| provider.on_encoder_broken())
        .flatten()
        .map_or_else(cxx::UniquePtr::null, |format| format.native)
}

/// Move-only SDK unique product (refcount zero before installation/conversion).
/// It is not a unique_ptr disguise for an already-refcounted shared selector.
///
/// ```compile_fail
/// fn sync<T: Sync>() {} sync::<pulsebeam_webrtc_sys::OwnedVideoEncoderSelector>();
/// ```
pub struct OwnedVideoEncoderSelector {
    pub(crate) native: cxx::UniquePtr<ffi::NativeOwnedVideoEncoderSelector>,
    failure: VideoEncoderSelectorFailureObserver,
    _not_sync: PhantomData<Cell<()>>,
}
// SAFETY: only our owning adapter with Send provider and serialized state; no
// endpoint, native sequence, borrowed factory, or task queue is retained.
unsafe impl Send for OwnedVideoEncoderSelector {}
impl OwnedVideoEncoderSelector {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeOwnedVideoEncoderSelector>,
    ) -> Option<Self> {
        let value = native.as_ref()?;
        let failure = ffi::video_selector_owned_observer(value).0.clone();
        Some(Self {
            native,
            failure,
            _not_sync: PhantomData,
        })
    }
    pub(crate) fn into_native(self) -> cxx::UniquePtr<ffi::NativeOwnedVideoEncoderSelector> {
        self.native
    }
    pub fn from_provider(provider: impl VideoEncoderSelectorProvider) -> Self {
        let (provider, failure) = registration(provider);
        Self {
            native: ffi::video_selector_owned_new(provider),
            failure,
            _not_sync: PhantomData,
        }
    }
    pub fn failure_observer(&self) -> VideoEncoderSelectorFailureObserver {
        self.failure.clone()
    }
    fn native(&self) -> &ffi::NativeOwnedVideoEncoderSelector {
        self.native.as_ref().expect("owning unique SDK selector")
    }
    pub fn failure(&self) -> Option<VideoEncoderSelectorFailure> {
        self.failure.failure()
    }
    pub fn on_current_encoder(&mut self, format: &SdpVideoFormat) {
        ffi::video_selector_owned_current(self.native(), format.native());
    }
    pub fn on_available_bitrate(
        &mut self,
        rate: DataRate,
    ) -> Result<Option<SdpVideoFormat>, VideoEncoderSelectorInputError> {
        let (bits, kind) = rate_parts(rate);
        let mut status = 0;
        let result = ffi::video_selector_owned_bitrate(self.native(), bits, kind, &mut status);
        rate_result(result, status)
    }
    pub fn on_resolution_change(
        &mut self,
        resolution: &RenderResolution,
    ) -> Option<SdpVideoFormat> {
        let result = ffi::video_selector_owned_resolution(self.native(), resolution.native());
        (!result.is_null()).then(|| SdpVideoFormat::from_native(result))
    }
    pub fn on_encoder_broken(&mut self) -> Option<SdpVideoFormat> {
        let result = ffi::video_selector_owned_broken(self.native());
        (!result.is_null()).then(|| SdpVideoFormat::from_native(result))
    }
    pub fn into_shared(self) -> VideoEncoderSelector {
        VideoEncoderSelector {
            native: ffi::video_selector_into_shared(self.native),
            failure: self.failure,
        }
    }
}
impl fmt::Debug for OwnedVideoEncoderSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedVideoEncoderSelector")
            .field("failure", &self.failure.failure())
            .finish()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoEncoderSelectorInputError {
    FiniteRateSentinel,
    NativeNegativeRateAssertion,
}
impl fmt::Display for VideoEncoderSelectorInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::FiniteRateSentinel => "finite native data rate cannot use an infinity sentinel",
            Self::NativeNegativeRateAssertion => {
                "the compiled native DataRate constructor asserts on a negative rate"
            }
        })
    }
}
impl std::error::Error for VideoEncoderSelectorInputError {}
fn rate_parts(rate: DataRate) -> (i64, i8) {
    match rate {
        DataRate::BitsPerSecond(bits) => (bits, 0),
        DataRate::PositiveInfinity => (0, 1),
        DataRate::NegativeInfinity => (0, -1),
    }
}
fn rate_result(
    result: cxx::UniquePtr<ffi::NativeSdpVideoFormat>,
    status: i32,
) -> Result<Option<SdpVideoFormat>, VideoEncoderSelectorInputError> {
    match status {
        0 => Ok((!result.is_null()).then(|| SdpVideoFormat::from_native(result))),
        1 => Err(VideoEncoderSelectorInputError::FiniteRateSentinel),
        2 => Err(VideoEncoderSelectorInputError::NativeNegativeRateAssertion),
        _ => unreachable!("closed selector input status"),
    }
}
/// Refcounted native selector. Aliases share exactly one stateful provider.
/// Standalone calls invoke its real SDK interface synchronously on the caller;
/// when installed, WebRTC invokes that interface on its encoder queue. Native
/// no-switch and binding failure are distinguished by the independent observer,
/// not by invented native return codes or atomically rolled-back provider state.
pub struct VideoEncoderSelector {
    pub(crate) native: cxx::UniquePtr<ffi::NativeVideoEncoderSelector>,
    failure: VideoEncoderSelectorFailureObserver,
}
// SAFETY: closed Rust-adapter producer graph. SDK refcounting is atomic;
// provider calls are mutex-serialized, same-registration recursion is rejected,
// and final provider destruction is contained and allowed on any thread.
unsafe impl Send for VideoEncoderSelector {}
unsafe impl Sync for VideoEncoderSelector {}
impl VideoEncoderSelector {
    pub fn from_provider(provider: impl VideoEncoderSelectorProvider) -> Self {
        OwnedVideoEncoderSelector::from_provider(provider).into_shared()
    }
    pub(crate) fn native(&self) -> &ffi::NativeVideoEncoderSelector {
        self.native.as_ref().expect("owning SDK selector")
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::video_selector_same(self.native(), other.native())
    }
    pub fn failure_observer(&self) -> VideoEncoderSelectorFailureObserver {
        self.failure.clone()
    }
    pub fn failure(&self) -> Option<VideoEncoderSelectorFailure> {
        self.failure.failure()
    }
    pub fn on_current_encoder(&self, format: &SdpVideoFormat) {
        ffi::video_selector_current(self.native(), format.native());
    }
    pub fn on_available_bitrate(
        &self,
        rate: DataRate,
    ) -> Result<Option<SdpVideoFormat>, VideoEncoderSelectorInputError> {
        let (bits, kind) = rate_parts(rate);
        let mut status = 0;
        let result = ffi::video_selector_bitrate(self.native(), bits, kind, &mut status);
        rate_result(result, status)
    }
    pub fn on_resolution_change(&self, resolution: &RenderResolution) -> Option<SdpVideoFormat> {
        let result = ffi::video_selector_resolution(self.native(), resolution.native());
        (!result.is_null()).then(|| SdpVideoFormat::from_native(result))
    }
    pub fn on_encoder_broken(&self) -> Option<SdpVideoFormat> {
        let result = ffi::video_selector_broken(self.native());
        (!result.is_null()).then(|| SdpVideoFormat::from_native(result))
    }
}
impl Clone for VideoEncoderSelector {
    fn clone(&self) -> Self {
        Self {
            native: ffi::video_selector_clone(self.native()),
            failure: self.failure.clone(),
        }
    }
}
impl fmt::Debug for VideoEncoderSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VideoEncoderSelector")
            .field("failure", &self.failure())
            .finish()
    }
}

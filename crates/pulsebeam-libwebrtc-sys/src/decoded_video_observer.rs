//! Synchronous observations of actual decoded track callbacks, before the
//! independent best-effort polling queue. This is not UI presentation feedback.
use crate::{RawVideoError, RawVideoFrame, VideoFrame, provider_panic::catch_provider};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// Owned CPU pixels and metadata copied from an actual native decoded frame.
/// Arrival is sampled synchronously at callback entry, before pixel conversion.
pub struct DecodedVideoFrameObservation {
    pub observed_at: std::time::Instant,
    /// SDK global monotonic clock sampled at the native callback. In an
    /// exclusive ControlledWorld this is its controlled clock, independent of
    /// host runtime/thread clock shims. Not a capture or presentation time.
    pub native_observed_at_us: i64,
    /// Conversion/geometry failures remain explicit observations, never frames.
    pub frame: Result<VideoFrame, RawVideoError>,
}

/// Runs synchronously on the native callback sequence. Do not block, await, or
/// synchronously control the SDK here. Retaining owned CPU values is permitted.
/// Panics retire this observer and are reported by its sink's failure status.
pub trait DecodedVideoFrameObserver: Send + 'static {
    fn on_frame(&mut self, observation: DecodedVideoFrameObservation);
}

impl<F> DecodedVideoFrameObserver for F
where
    F: FnMut(DecodedVideoFrameObservation) + Send + 'static,
{
    fn on_frame(&mut self, observation: DecodedVideoFrameObservation) {
        self(observation);
    }
}

#[derive(Clone, Default)]
pub(crate) struct ObserverStatus(Arc<AtomicBool>);
impl ObserverStatus {
    pub(crate) fn failed(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub(crate) struct RustDecodedVideoObserver {
    callback: Mutex<Option<Box<dyn DecodedVideoFrameObserver>>>,
    status: ObserverStatus,
}
impl RustDecodedVideoObserver {
    pub(crate) fn new(observer: Box<dyn DecodedVideoFrameObserver>) -> (Box<Self>, ObserverStatus) {
        let status = ObserverStatus::default();
        (
            Box::new(Self {
                callback: Mutex::new(Some(observer)),
                status: status.clone(),
            }),
            status,
        )
    }
}
impl Drop for RustDecodedVideoObserver {
    fn drop(&mut self) {
        let observer = self
            .callback
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let _ = catch_provider(|| drop(observer));
    }
}

pub(crate) fn decoded_video_frame_observed(
    source: &RustDecodedVideoObserver,
    frame: cxx::UniquePtr<crate::ffi::NativeRawVideoFrame>,
    native_observed_at_us: i64,
) {
    let observed_at = std::time::Instant::now();
    if source.status.failed() {
        return;
    }
    let mut callback = source.callback.lock().unwrap_or_else(|e| e.into_inner());
    let Some(observer) = callback.as_mut() else {
        return;
    };
    let result = catch_provider(|| {
        let snapshot = if frame.is_null() {
            Err(RawVideoError::Uninitialized)
        } else {
            let frame = RawVideoFrame::from_decoded_native(frame);
            // Bound callback conversion independently; do not change the
            // existing polling queue or allocate unbounded native images.
            let width = i64::from(frame.width());
            let height = i64::from(frame.height());
            let bytes = (width > 0 && height > 0)
                .then(|| {
                    width.checked_mul(height)?.checked_add(
                        ((width + 1) / 2)
                            .checked_mul((height + 1) / 2)?
                            .checked_mul(2)?,
                    )
                })
                .flatten();
            if bytes.is_some_and(|bytes| bytes <= 16 * 1024 * 1024) {
                frame.to_legacy()
            } else {
                Err(RawVideoError::InvalidGeometry)
            }
        };
        observer.on_frame(DecodedVideoFrameObservation {
            observed_at,
            native_observed_at_us,
            frame: snapshot,
        });
    });
    if result.is_err() {
        source.status.0.store(true, Ordering::Release);
        let retired = callback.take();
        drop(callback);
        let _ = catch_provider(|| drop(retired));
    }
}

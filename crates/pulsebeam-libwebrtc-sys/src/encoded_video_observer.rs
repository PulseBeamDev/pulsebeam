//! Native ordered encoded observations before independent polling retention.
use crate::{EncodedReceivedVideoFrame, provider_panic::catch_provider};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// Actual native recordable-frame output, or loss of the metadata needed to
/// identify that output. Polling queue eviction is deliberately not a loss here.
#[derive(Clone, Debug)]
pub enum EncodedVideoFrameObservation {
    Frame(EncodedReceivedVideoFrame),
    Dropped { skipped: u64 },
}

/// Runs synchronously on a native callback sequence, independently of the
/// best-effort polling queue. The configured decoder remains active. Its I420
/// callback may precede or follow this callback. Do not block, await, or
/// synchronously control the SDK here. Retaining owned values is permitted.
/// Panics retire this observer and are reported by its sink's failure status.
pub trait EncodedVideoFrameObserver: Send + 'static {
    fn on_frame(&mut self, observation: EncodedVideoFrameObservation);
}
impl<F: FnMut(EncodedVideoFrameObservation) + Send + 'static> EncodedVideoFrameObserver for F {
    fn on_frame(&mut self, observation: EncodedVideoFrameObservation) {
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

pub(crate) struct RustEncodedVideoObserver {
    callback: Mutex<Option<Box<dyn EncodedVideoFrameObserver>>>,
    status: ObserverStatus,
}
impl RustEncodedVideoObserver {
    pub(crate) fn new(observer: Box<dyn EncodedVideoFrameObserver>) -> (Box<Self>, ObserverStatus) {
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
impl Drop for RustEncodedVideoObserver {
    fn drop(&mut self) {
        let observer = self
            .callback
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let _ = catch_provider(|| drop(observer));
    }
}

pub(crate) fn encoded_video_observer_closed(source: &RustEncodedVideoObserver) {
    let retired = source
        .callback
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let _ = catch_provider(|| drop(retired));
}

pub(crate) fn encoded_video_frame_observed(
    source: &RustEncodedVideoObserver,
    frame: crate::ffi::FfiEncodedVideoFrame,
    dropped: u64,
) {
    if source.status.failed() {
        return;
    }
    let mut callback = source.callback.lock().unwrap_or_else(|e| e.into_inner());
    let Some(observer) = callback.as_mut() else {
        return;
    };
    let result = catch_provider(|| {
        if dropped != 0 {
            observer.on_frame(EncodedVideoFrameObservation::Dropped { skipped: dropped });
        }
        if let Some(frame) = EncodedReceivedVideoFrame::from_native(frame) {
            observer.on_frame(EncodedVideoFrameObservation::Frame(frame));
        }
    });
    if result.is_err() {
        source.status.0.store(true, Ordering::Release);
        let retired = callback.take();
        drop(callback);
        let _ = catch_provider(|| drop(retired));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Guard(Arc<AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn owned_frame_loss_order_and_close_retirement() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Guard(dropped.clone());
        let (observer, status) = RustEncodedVideoObserver::new(Box::new(move |event| {
            let _ = &guard;
            captured.lock().unwrap().push(event);
        }));
        encoded_video_frame_observed(
            &observer,
            crate::ffi::FfiEncodedVideoFrame {
                available: true,
                data: vec![1, 2, 3],
                rtp_timestamp: 42,
                ssrc: 7,
                mime_type: "video/H264".into(),
                ..Default::default()
            },
            2,
        );
        let observed = events.lock().unwrap();
        assert!(matches!(
            observed[0],
            EncodedVideoFrameObservation::Dropped { skipped: 2 }
        ));
        let EncodedVideoFrameObservation::Frame(frame) = &observed[1] else {
            panic!("actual encoded output required")
        };
        assert_eq!(
            (frame.rtp_timestamp, frame.ssrc, frame.data.as_slice()),
            (42, 7, &[1, 2, 3][..])
        );
        drop(observed);
        encoded_video_observer_closed(&observer);
        assert!(dropped.load(Ordering::Acquire));
        encoded_video_frame_observed(&observer, Default::default(), 1);
        assert_eq!(events.lock().unwrap().len(), 2);
        assert!(!status.failed());
    }

    #[test]
    fn panic_retires_observer_and_reports_sticky_failure() {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Guard(dropped.clone());
        let (observer, status) = RustEncodedVideoObserver::new(Box::new(move |_| {
            let _ = &guard;
            panic!("intentional native encoded observer panic");
        }));
        encoded_video_frame_observed(&observer, Default::default(), 1);
        assert!(status.failed());
        assert!(dropped.load(Ordering::Acquire));
        encoded_video_frame_observed(&observer, Default::default(), 1);
        encoded_video_observer_closed(&observer);
        assert!(status.failed());
    }
}

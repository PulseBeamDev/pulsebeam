#![cfg(all(feature = "native", target_os = "linux"))]
//! Explicit physical-camera gate. Never discovers and opens an arbitrary camera.
//! An operator must name the authorized V4L2 ID via PULSEBEAM_TEST_CAMERA_ID.
use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[derive(Default)]
struct Observed {
    calls: Mutex<usize>,
    ready: Condvar,
    drops: AtomicUsize,
}
impl Observed {
    fn wait_for(&self, minimum: usize) {
        let (calls, timeout) = self
            .ready
            .wait_timeout_while(
                self.calls.lock().unwrap(),
                Duration::from_secs(5),
                |calls| *calls < minimum,
            )
            .unwrap();
        assert!(
            !timeout.timed_out() && *calls >= minimum,
            "authorized camera did not deliver real SDK raw frames"
        );
    }
}
struct Sink {
    observed: Arc<Observed>,
    panic: bool,
}
impl RawCameraSink for Sink {
    fn on_raw_frame(&mut self, frame: RawCameraFrame<'_>) -> i32 {
        assert!(!frame.bytes.is_empty());
        assert!(frame.settings.width > 0 && frame.settings.height > 0);
        // Do not retain, print, hash or persist physical-camera image pixels.
        *self.observed.calls.lock().unwrap() += 1;
        self.observed.ready.notify_all();
        assert!(!self.panic, "selected raw camera sink failure");
        -17 // Exact native return is ignored by this SDK, not delivery failure.
    }
}
impl Drop for Sink {
    fn drop(&mut self) {
        self.observed.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Initialized(Arc<AtomicUsize>);
impl CameraInitializationObserver for Initialized {
    fn on_initialized(&mut self, status: CameraInitializationStatus) {
        assert_eq!(status, CameraInitializationStatus::Success);
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
#[ignore = "requires explicit authorization and PULSEBEAM_TEST_CAMERA_ID; opens that physical V4L2 camera"]
fn authorized_v4l2_real_callbacks_replace_quarantine_and_fence_final_teardown() {
    let id = std::env::var("PULSEBEAM_TEST_CAMERA_ID")
        .expect("explicit authorized V4L2 camera ID is required");
    assert!(!id.is_empty());
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut options = factory.create_camera_options().unwrap();
    let mut flags = options.settings().unwrap();
    flags.allow_v4l2 = true;
    flags.allow_pipewire = flags.allow_pipewire.map(|_| false);
    options.set_settings(flags).unwrap();
    let initialized = Arc::new(AtomicUsize::new(0));
    let observer = CameraInitializationObserverHandle::new(Initialized(initialized.clone()));
    assert_eq!(
        options.initialize(Some(&observer)).unwrap(),
        CameraInitializationStatus::Success
    );
    assert_eq!(
        observer.last_notification(),
        Some(CameraInitializationStatus::Success)
    );
    assert_eq!(initialized.load(Ordering::SeqCst), 1);
    assert!(!observer.has_failed());
    let mut info = factory
        .create_camera_device_info_with_options(&options)
        .unwrap()
        .expect("authorized V4L2 camera must supply DeviceInfo");
    assert!(info.number_of_capabilities(&id).unwrap() > 0);
    let settings = info.capability(&id, 0).unwrap();
    assert!(settings.width > 0 && settings.height > 0 && settings.max_fps > 0);
    let mut capture = factory.create_camera_with_options(&options, &id).unwrap();
    assert!(!capture.capture_started().unwrap());
    let observed = Arc::new(Observed::default());
    let sink = RawCameraSinkHandle::new(Sink {
        observed: observed.clone(),
        panic: false,
    });
    capture.set_raw_sink(Some(sink.clone())).unwrap();
    capture.start_device(&settings).unwrap();
    observed.wait_for(3);
    assert!(capture.capture_started().unwrap());
    assert!(!sink.has_failed());
    assert_eq!(capture.current_device_name().unwrap(), id);
    let actual = capture.settings().unwrap();
    assert!(actual.width > 0 && actual.height > 0);
    drop((options, factory));
    // Both the actual capture and its DeviceInfo keep their consuming options
    // and factory alive after all originating public options/factory handles go.
    assert!(info.number_of_capabilities(&id).unwrap() > 0);
    capture.set_raw_sink(None).unwrap(); // SDK deregistration fences active calls.
    let count = *observed.calls.lock().unwrap();
    drop(sink);
    assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
    let failed_observed = Arc::new(Observed::default());
    let failed = RawCameraSinkHandle::new(Sink {
        observed: failed_observed.clone(),
        panic: true,
    });
    capture.set_raw_sink(Some(failed.clone())).unwrap();
    failed_observed.wait_for(1);
    capture.set_raw_sink(None).unwrap(); // Fence quarantine before inspecting it.
    assert!(failed.has_failed());
    assert_eq!(*failed_observed.calls.lock().unwrap(), 1);
    assert_eq!(failed_observed.drops.load(Ordering::SeqCst), 1);
    assert!(matches!(
        capture.set_raw_sink(Some(failed.clone())),
        Err(CameraControlError::SinkFailed)
    ));
    capture.stop_device().unwrap();
    assert!(!capture.capture_started().unwrap());
    assert_eq!(*observed.calls.lock().unwrap(), count);
    capture.stop().unwrap();
    assert_eq!(
        capture.status(Duration::from_secs(1)),
        CameraCaptureStatus::Closed
    );
    assert!(matches!(
        capture.settings(),
        Err(CameraControlError::Closed)
    ));
    drop((capture, info, observer, failed));
    assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
    assert_eq!(failed_observed.drops.load(Ordering::SeqCst), 1);
}

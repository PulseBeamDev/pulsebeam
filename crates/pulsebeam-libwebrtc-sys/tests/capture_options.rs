#![cfg(all(feature = "native", target_os = "linux"))]
//! Native option ownership and explicit limitations, without opening hardware.
use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI64, AtomicUsize, Ordering},
};

static TEST_LOCK: Mutex<()> = Mutex::new(());
struct ClockSource {
    time: Arc<AtomicI64>,
    drops: Arc<AtomicUsize>,
}
impl ClockProvider for ClockSource {
    fn current_time(&self) -> FiniteTimestamp {
        FiniteTimestamp::from_microseconds(self.time.load(Ordering::SeqCst)).unwrap()
    }
    fn convert_timestamp_to_ntp_time(&self, _: Timestamp) -> NtpTime {
        NtpTime::from_parts(123, 456)
    }
}
impl Drop for ClockSource {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct NeverAllocate(Arc<AtomicUsize>);
impl DesktopMemoryFactory for NeverAllocate {
    fn create_shared_memory(&mut self, _: usize) -> Option<DesktopMemory> {
        self.0.fetch_add(1, Ordering::SeqCst);
        None
    }
}

#[test]
fn desktop_options_keep_actual_selected_clock_and_reject_unconsumed_memory_factory() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let time = Arc::new(AtomicI64::new(42_000));
    let drops = Arc::new(AtomicUsize::new(0));
    let clock = Clock::from_provider(ClockSource {
        time: time.clone(),
        drops: drops.clone(),
    });
    let factory = PeerConnectionFactory::builder()
        .environment(Environment::builder().clock_handle(&clock).build().unwrap())
        .build()
        .unwrap();
    let other_factory = PeerConnectionFactory::builder().build().unwrap();
    let mut options = factory.desktop_capture_options(false, true).unwrap();
    let mut settings = options.settings().unwrap();
    settings.use_update_notifications = !settings.use_update_notifications;
    settings.disable_effects = !settings.disable_effects;
    settings.detect_updated_region = !settings.detect_updated_region;
    settings.prefer_cursor_embedded = !settings.prefer_cursor_embedded;
    if let Some(pipewire) = settings.pipewire.as_mut() {
        pipewire.allow = false;
        pipewire.width = 640;
        pipewire.height = 480;
        pipewire.use_damage_region = false;
    }
    options.set_settings(&settings).unwrap();
    assert_eq!(options.settings().unwrap(), settings);
    let mut copy = options.try_clone().unwrap();
    assert_eq!(copy.settings().unwrap(), settings);
    let incompatible = other_factory.desktop_capture_options(false, true).unwrap();
    assert_eq!(
        copy.assign_from(&incompatible).err().unwrap().kind,
        PeerErrorKind::InvalidParameter
    );
    assert_eq!(copy.settings().unwrap(), settings);
    let requests = Arc::new(AtomicUsize::new(0));
    let memory = DesktopMemoryFactoryHandle::new(NeverAllocate(requests.clone()));
    for kind in [
        DesktopCaptureKind::Screen,
        DesktopCaptureKind::Window,
        DesktopCaptureKind::Generic,
    ] {
        let error = factory
            .create_desktop_capture_with_providers(&options, kind, None, None, None, Some(&memory))
            .err()
            .unwrap();
        assert_eq!(error.kind, PeerErrorKind::UnsupportedOperation);
        assert!(error.message.contains("do not consume SharedMemoryFactory"));
    }
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert!(!memory.has_failed());
    assert_eq!(options.clock_time_us().unwrap(), 42_000);
    let utility = options.clock().unwrap();
    drop((factory, clock, options));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    time.store(43_000, Ordering::SeqCst);
    assert_eq!(copy.clock_time_us().unwrap(), 43_000);
    assert_eq!(utility.current_time().microseconds().unwrap(), 43_000);
    drop(copy);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(utility);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

struct Observer(Arc<AtomicUsize>);
impl CameraInitializationObserver for Observer {
    fn on_initialized(&mut self, _: CameraInitializationStatus) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn camera_option_copies_preserve_selection_without_unsafe_pipewire_init_or_fallback() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let other = PeerConnectionFactory::builder().build().unwrap();
    let mut options = factory.create_camera_options().unwrap();
    let original = options.settings().unwrap();
    let selected = CameraOptionSettings {
        allow_v4l2: false,
        allow_pipewire: original.allow_pipewire.map(|_| true),
    };
    options.set_settings(selected).unwrap();
    let copy = options.try_clone().unwrap();
    assert_eq!(copy.settings().unwrap(), selected);
    assert_eq!(
        other
            .create_camera_device_info_with_options(&copy)
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::InvalidState
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let observer = CameraInitializationObserverHandle::new(Observer(calls.clone()));
    if selected.allow_pipewire == Some(true) {
        let error = options.initialize(Some(&observer)).err().unwrap();
        assert_eq!(error.kind, PeerErrorKind::UnsupportedOperation);
        assert!(error.message.contains("safe shutdown ordering"));
        assert_eq!(
            factory
                .create_camera_device_info_with_options(&copy)
                .err()
                .unwrap()
                .kind,
            PeerErrorKind::UnsupportedOperation
        );
        assert_eq!(
            factory
                .create_camera_with_options(&copy, "not-a-real-camera")
                .err()
                .unwrap()
                .kind,
            PeerErrorKind::UnsupportedOperation
        );
    } else {
        assert_eq!(
            options
                .set_settings(CameraOptionSettings {
                    allow_v4l2: false,
                    allow_pipewire: Some(true)
                })
                .err()
                .unwrap()
                .kind,
            PeerErrorKind::UnsupportedOperation
        );
    }
    assert_eq!(
        observer.last_notification(),
        None,
        "unsupported initialization must not synthesize SDK success"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!observer.has_failed());
    assert_eq!(options.settings().unwrap(), selected);
    drop(factory);
    assert_eq!(copy.settings().unwrap(), selected);
}

#[test]
fn actor_option_ids_preserve_values_across_migration_and_reject_cross_session_ids() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut actor = ProductionSession::new(Default::default()).unwrap();
    let desktop = actor.create_desktop_options(false, true).unwrap();
    let desktop_settings = actor.desktop_options_settings(desktop).unwrap();
    let desktop_copy = actor.clone_desktop_options(desktop).unwrap();
    let camera = actor.create_camera_options().unwrap();
    let mut camera_settings = actor.camera_options_settings(camera).unwrap();
    camera_settings.allow_v4l2 = false;
    camera_settings.allow_pipewire = camera_settings.allow_pipewire.map(|_| false);
    actor
        .set_camera_options_settings(camera, camera_settings)
        .unwrap();
    let camera_copy = actor.clone_camera_options(camera).unwrap();
    let other = ProductionSession::new(Default::default()).unwrap();
    assert!(other.desktop_options_settings(desktop).is_err());
    assert!(other.camera_options_settings(camera).is_err());
    let mut actor = std::thread::spawn(move || {
        assert_eq!(
            actor.desktop_options_settings(desktop_copy).unwrap(),
            desktop_settings
        );
        assert_eq!(
            actor.camera_options_settings(camera_copy).unwrap(),
            camera_settings
        );
        actor.close_desktop_options(desktop).unwrap();
        actor.close_camera_options(camera).unwrap();
        assert!(actor.desktop_options_settings(desktop).is_err());
        assert!(actor.camera_options_settings(camera).is_err());
        actor
    })
    .join()
    .unwrap();
    assert!(actor.desktop_options_settings(desktop_copy).is_ok());
    assert_eq!(
        actor.camera_options_settings(camera_copy).unwrap(),
        camera_settings
    );
    actor.shutdown().unwrap();
    assert_eq!(
        actor
            .desktop_options_settings(desktop_copy)
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::Closed
    );
    assert_eq!(
        actor
            .camera_options_settings(camera_copy)
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::Closed
    );
}

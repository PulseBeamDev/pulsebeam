#![cfg(all(feature = "native", target_os = "linux"))]
//! Explicit optional live SDK callback gate on a fresh private Xvfb display.
//! Never opens the user's DISPLAY/Wayland session or a physical capture device.
use pulsebeam_webrtc_sys::*;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

struct XServer(Child);
impl Drop for XServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn private_display() -> (XServer, String) {
    let child = Command::new("Xvfb")
        .args([
            "-displayfd",
            "1",
            "-screen",
            "0",
            "64x64x24",
            "-nolisten",
            "tcp",
            "-noreset",
            "-ac",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("this explicit live-callback gate requires Xvfb on PATH");
    let mut server = XServer(child);
    let mut number = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut number)
        .unwrap();
    let number: u16 = number
        .trim()
        .parse()
        .expect("Xvfb did not report its private display");
    (server, format!(":{number}"))
}
#[derive(Default)]
struct Observed {
    starts: AtomicUsize,
    results: AtomicUsize,
    drops: AtomicUsize,
    snapshots: Mutex<Vec<DesktopFrameSnapshot>>,
}
struct Receiver {
    observed: Arc<Observed>,
    panic: bool,
}
impl DesktopCaptureReceiver for Receiver {
    fn on_frame_capture_start(&mut self) {
        self.observed.starts.fetch_add(1, Ordering::SeqCst);
    }
    fn on_capture_result(
        &mut self,
        result: DesktopCaptureResult,
        frame: Option<CapturedDesktopFrame>,
    ) {
        self.observed.results.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            result,
            DesktopCaptureResult::Success,
            "private X11 capture must produce a real successful frame"
        );
        let frame = frame.expect("SDK success without a native frame");
        let info = frame.info();
        assert_eq!((info.width, info.height), (64, 64));
        assert!(info.has_cpu_data && !info.has_texture);
        let snapshot = frame.snapshot().unwrap();
        assert_eq!(snapshot.packed_pixels.as_ref().unwrap().len(), 64 * 64 * 4);
        // Exercise a real captured SDK frame copy while the borrowed producer
        // callback is live, without pretending its local handle is Send.
        let copy = frame.try_clone().unwrap();
        assert_eq!(
            copy.snapshot().unwrap().packed_pixels,
            snapshot.packed_pixels
        );
        self.observed.snapshots.lock().unwrap().push(snapshot);
        assert!(!self.panic, "selected desktop receiver failure");
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.observed.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
#[ignore = "explicit native X11 callback gate, requires Xvfb; never uses an existing display"]
fn private_x11_sdk_frame_callbacks_replace_quarantine_and_release_actual_capture() {
    let (_server, display_name) = private_display();
    let factory = PeerConnectionFactory::builder().build().unwrap();
    // Empty SDK options avoid CreateDefault opening ambient platform resources.
    let mut options = factory.desktop_capture_options(false, true).unwrap();
    assert!(
        options.supports_x11().unwrap(),
        "the native X11 artifact is required"
    );
    let mut settings = options.settings().unwrap();
    if let Some(pipewire) = settings.pipewire.as_mut() {
        pipewire.allow = false;
    }
    settings.detect_updated_region = false;
    options.set_settings(&settings).unwrap();
    let display = options
        .create_x_display(&display_name)
        .unwrap()
        .expect("private X display did not open");
    options.set_x_display(Some(&display)).unwrap();
    let first_observed = Arc::new(Observed::default());
    let first = DesktopCaptureReceiverHandle::new(Receiver {
        observed: first_observed.clone(),
        panic: false,
    });
    let mut capture = factory
        .create_desktop_capture_with_callbacks(
            &options,
            DesktopCaptureKind::Screen,
            None,
            Some(&first),
            None,
        )
        .unwrap();
    let screens = capture.sources().unwrap();
    assert!(
        !screens.is_empty(),
        "fresh Xvfb must expose an actual SDK screen source"
    );
    assert!(capture.select_source(screens[0].id).unwrap());
    capture.request_frame().unwrap();
    assert_eq!(first_observed.results.load(Ordering::SeqCst), 1);
    assert!(!first.has_failed());
    // X11 need not invoke OnFrameCaptureStart. Never synthesize that optional
    // upstream event just to make an assertion about callback coverage pass.
    let second_observed = Arc::new(Observed::default());
    let second = DesktopCaptureReceiverHandle::new(Receiver {
        observed: second_observed.clone(),
        panic: false,
    });
    capture.set_receiver(Some(&second)).unwrap();
    drop(first);
    assert_eq!(first_observed.drops.load(Ordering::SeqCst), 1);
    let old_count = first_observed.results.load(Ordering::SeqCst);
    drop((options, display, factory));
    capture.request_frame().unwrap();
    assert_eq!(first_observed.results.load(Ordering::SeqCst), old_count);
    assert_eq!(second_observed.results.load(Ordering::SeqCst), 1);
    assert!(!second.has_failed());
    let failed_observed = Arc::new(Observed::default());
    let failed = DesktopCaptureReceiverHandle::new(Receiver {
        observed: failed_observed.clone(),
        panic: true,
    });
    capture.set_receiver(Some(&failed)).unwrap();
    drop(second);
    assert_eq!(second_observed.drops.load(Ordering::SeqCst), 1);
    capture.request_frame().unwrap();
    assert!(failed.has_failed());
    assert_eq!(failed_observed.results.load(Ordering::SeqCst), 1);
    assert_eq!(failed_observed.drops.load(Ordering::SeqCst), 1);
    capture.request_frame().unwrap();
    assert_eq!(
        failed_observed.results.load(Ordering::SeqCst),
        1,
        "quarantined SDK receiver must not reenter"
    );
    capture.set_receiver(None).unwrap();
    capture.request_frame().unwrap();
    capture.stop().unwrap();
    assert_eq!(capture.status(), ScreenCaptureStatus::Closed);
    assert_eq!(
        capture.request_frame().err().unwrap().kind,
        PeerErrorKind::Closed
    );
    assert_eq!(
        capture.set_receiver(Some(&failed)).err().unwrap().kind,
        PeerErrorKind::Closed
    );
    drop((capture, failed));
    // Owning snapshots are valid after the capturer/options/display/factory
    // graph is gone. They contain only this fresh private X server's pixels.
    for observed in [first_observed, second_observed, failed_observed] {
        assert!(
            observed
                .snapshots
                .lock()
                .unwrap()
                .iter()
                .all(|snapshot| snapshot.packed_pixels.as_ref().unwrap().len() == 64 * 64 * 4)
        );
        assert_eq!(observed.drops.load(Ordering::SeqCst), 1);
    }
}

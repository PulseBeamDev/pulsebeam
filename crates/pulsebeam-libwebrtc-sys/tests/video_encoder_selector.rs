use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct Trace {
    current: Mutex<Vec<SdpVideoFormatFields>>,
    rates: Mutex<Vec<DataRate>>,
    resolutions: Mutex<Vec<(i32, i32)>>,
    dropped: AtomicUsize,
}
struct Selector(Arc<Trace>);
impl VideoEncoderSelectorProvider for Selector {
    fn on_current_encoder(&mut self, format: SdpVideoFormat) {
        self.0.current.lock().unwrap().push(format.fields());
    }
    fn on_available_bitrate(&mut self, rate: DataRate) -> Option<SdpVideoFormat> {
        self.0.rates.lock().unwrap().push(rate);
        None
    }
    fn on_resolution_change(&mut self, resolution: RenderResolution) -> Option<SdpVideoFormat> {
        self.0
            .resolutions
            .lock()
            .unwrap()
            .push((resolution.width(), resolution.height()));
        None
    }
    fn on_encoder_broken(&mut self) -> Option<SdpVideoFormat> {
        let mut format = SdpVideoFormat::new([0xff, 0]);
        let mut fields = format.fields();
        fields.parameters.insert(vec![0xfe, 0], vec![0xfd, 0]);
        fields.scalability_modes = vec![ScalabilityMode::from_raw(255)];
        fields.packetization = Some(vec![]);
        fields.tx_mode = Some(vec![0xfc]);
        format.set_fields(fields);
        Some(format)
    }
}
impl Drop for Selector {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn actual_selector_interface_preserves_owned_values_and_native_rate_states() {
    let trace = Arc::new(Trace::default());
    let owned = OwnedVideoEncoderSelector::from_provider(Selector(trace.clone()));
    let observer = owned.failure_observer();
    let selector = owned.into_shared();
    let alias = selector.clone();
    assert!(selector.same_resource(&alias));
    let mut format = SdpVideoFormat::new([0xff, 0]);
    let mut fields = format.fields();
    fields.packetization = Some(vec![]);
    fields.tx_mode = Some(vec![0xfe]);
    format.set_fields(fields.clone());
    selector.on_current_encoder(&format);
    assert_eq!(*trace.current.lock().unwrap(), vec![fields]);
    assert!(
        selector
            .on_available_bitrate(DataRate::BitsPerSecond(0))
            .unwrap()
            .is_none()
    );
    assert!(
        selector
            .on_available_bitrate(DataRate::PositiveInfinity)
            .unwrap()
            .is_none()
    );
    assert!(
        selector
            .on_available_bitrate(DataRate::NegativeInfinity)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        *trace.rates.lock().unwrap(),
        vec![
            DataRate::BitsPerSecond(0),
            DataRate::PositiveInfinity,
            DataRate::NegativeInfinity
        ]
    );
    assert_eq!(
        selector
            .on_available_bitrate(DataRate::BitsPerSecond(i64::MAX))
            .unwrap_err(),
        VideoEncoderSelectorInputError::FiniteRateSentinel
    );
    assert_eq!(
        selector
            .on_available_bitrate(DataRate::BitsPerSecond(i64::MIN))
            .unwrap_err(),
        VideoEncoderSelectorInputError::FiniteRateSentinel
    );
    assert_eq!(trace.rates.lock().unwrap().len(), 3);
    assert!(
        selector
            .on_resolution_change(&RenderResolution::new(-7, i32::MAX))
            .is_none()
    );
    assert_eq!(*trace.resolutions.lock().unwrap(), vec![(-7, i32::MAX)]);
    let selected = selector.on_encoder_broken().unwrap().fields();
    assert_eq!(selected.name, vec![0xff, 0]);
    assert_eq!(selected.packetization, Some(vec![]));
    assert_eq!(selected.tx_mode, Some(vec![0xfc]));
    assert_eq!(
        selected.scalability_modes,
        vec![ScalabilityMode::from_raw(255)]
    );
    drop(selector);
    assert_eq!(trace.dropped.load(Ordering::SeqCst), 0);
    std::thread::spawn(move || {
        alias.on_current_encoder(&SdpVideoFormat::new("VP8"));
        drop(alias);
    })
    .join()
    .unwrap();
    assert_eq!(trace.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure(), None);
}

#[test]
fn unique_product_invokes_all_native_methods_before_consuming_conversion() {
    let trace = Arc::new(Trace::default());
    let mut owned = OwnedVideoEncoderSelector::from_provider(Selector(trace.clone()));
    owned.on_current_encoder(&SdpVideoFormat::new("VP8"));
    assert!(
        owned
            .on_available_bitrate(DataRate::BitsPerSecond(7))
            .unwrap()
            .is_none()
    );
    assert!(
        owned
            .on_resolution_change(&RenderResolution::new(0, -1))
            .is_none()
    );
    assert_eq!(
        owned.on_encoder_broken().unwrap().fields().packetization,
        Some(vec![])
    );
    assert_eq!(trace.current.lock().unwrap().len(), 1);
    assert_eq!(
        *trace.rates.lock().unwrap(),
        vec![DataRate::BitsPerSecond(7)]
    );
    assert_eq!(*trace.resolutions.lock().unwrap(), vec![(0, -1)]);
    // Release artifacts preserve finite negative native rates. DCHECK-enabled
    // artifacts report only the actual reached one-sided constructor assertion.
    match owned.on_available_bitrate(DataRate::BitsPerSecond(-1)) {
        Ok(None) => assert_eq!(
            trace.rates.lock().unwrap().last(),
            Some(&DataRate::BitsPerSecond(-1))
        ),
        Err(VideoEncoderSelectorInputError::NativeNegativeRateAssertion) => {
            assert_eq!(trace.rates.lock().unwrap().len(), 1)
        }
        outcome => panic!("unexpected finite negative native rate outcome: {outcome:?}"),
    }
    let shared = owned.into_shared();
    shared.on_current_encoder(&SdpVideoFormat::new("VP9"));
    assert_eq!(trace.current.lock().unwrap().len(), 2);
    assert_eq!(shared.failure(), None);
    drop(shared);
    assert_eq!(trace.dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn shared_standalone_and_native_style_calls_serialize_one_stateful_provider() {
    let trace = Arc::new(Trace::default());
    let selector = VideoEncoderSelector::from_provider(Selector(trace.clone()));
    let threads: Vec<_> = (1..=8)
        .map(|rate| {
            let selector = selector.clone();
            std::thread::spawn(move || {
                selector
                    .on_available_bitrate(DataRate::BitsPerSecond(rate))
                    .unwrap()
            })
        })
        .collect();
    for thread in threads {
        assert!(thread.join().unwrap().is_none());
    }
    let mut rates: Vec<_> = trace
        .rates
        .lock()
        .unwrap()
        .iter()
        .map(|rate| match rate {
            DataRate::BitsPerSecond(rate) => *rate,
            _ => panic!("finite rate required"),
        })
        .collect();
    rates.sort_unstable();
    assert_eq!(rates, (1..=8).collect::<Vec<_>>());
    assert_eq!(selector.failure(), None);
}

struct DestructorFailure;
impl VideoEncoderSelectorProvider for DestructorFailure {
    fn on_current_encoder(&mut self, _: SdpVideoFormat) {}
    fn on_available_bitrate(&mut self, _: DataRate) -> Option<SdpVideoFormat> {
        None
    }
    fn on_encoder_broken(&mut self) -> Option<SdpVideoFormat> {
        None
    }
}
impl Drop for DestructorFailure {
    fn drop(&mut self) {
        panic!("selector destructor");
    }
}
#[test]
fn independent_observer_survives_contained_unique_and_shared_destruction() {
    let owned = OwnedVideoEncoderSelector::from_provider(DestructorFailure);
    let failure = owned.failure_observer();
    drop(owned);
    assert_eq!(
        failure.failure(),
        Some(VideoEncoderSelectorFailure::DestructorPanicked)
    );
    let selector = VideoEncoderSelector::from_provider(DestructorFailure);
    let failure = selector.failure_observer();
    let alias = selector.clone();
    drop(selector);
    assert_eq!(failure.failure(), None);
    drop(alias);
    assert_eq!(
        failure.failure(),
        Some(VideoEncoderSelectorFailure::DestructorPanicked)
    );
}

// Unknown payloads are intentionally forgotten by the containment policy.
// Exercise a double-panicking destructor without deliberately allocating and
// leaking a payload-owned counter; this is not an arbitrary-payload leak test.
#[path = "common/panic_payload.rs"]
mod panic_payload;
struct Panicked(Arc<AtomicUsize>);
impl VideoEncoderSelectorProvider for Panicked {
    fn on_current_encoder(&mut self, _: SdpVideoFormat) {
        panic!("poisoned selector must not call current");
    }
    fn on_available_bitrate(&mut self, _: DataRate) -> Option<SdpVideoFormat> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic_payload::raise()
    }
    fn on_encoder_broken(&mut self) -> Option<SdpVideoFormat> {
        panic!("poisoned selector must not be called");
    }
}
#[test]
fn panic_poisoning_is_distinct_from_native_no_switch_and_suppresses_later_calls() {
    let calls = Arc::new(AtomicUsize::new(0));
    let selector = VideoEncoderSelector::from_provider(Panicked(calls.clone()));
    assert!(
        selector
            .on_available_bitrate(DataRate::BitsPerSecond(100))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        selector.failure(),
        Some(VideoEncoderSelectorFailure::ProviderPanicked)
    );
    assert!(selector.on_encoder_broken().is_none());
    selector.on_current_encoder(&SdpVideoFormat::new("VP8"));
    assert!(
        selector
            .on_available_bitrate(DataRate::PositiveInfinity)
            .unwrap()
            .is_none()
    );
    // Only the callback runs. Arbitrary panic payloads are not destructed:
    // their field cleanup could double-panic and abort across the SDK loan.
    // Later callbacks do not touch the failed provider.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

struct Recursive(Arc<Mutex<Option<VideoEncoderSelector>>>);
impl VideoEncoderSelectorProvider for Recursive {
    fn on_current_encoder(&mut self, _: SdpVideoFormat) {
        panic!("recursive provider must not be entered");
    }
    fn on_available_bitrate(&mut self, _: DataRate) -> Option<SdpVideoFormat> {
        None
    }
    fn on_encoder_broken(&mut self) -> Option<SdpVideoFormat> {
        let alias = self.0.lock().unwrap().as_ref().unwrap().clone();
        alias.on_current_encoder(&SdpVideoFormat::new("VP8"));
        Some(SdpVideoFormat::new("VP9"))
    }
}
#[test]
fn same_registration_reentry_fails_without_deadlock_or_fabricated_selection() {
    let slot = Arc::new(Mutex::new(None));
    let selector = VideoEncoderSelector::from_provider(Recursive(slot.clone()));
    *slot.lock().unwrap() = Some(selector.clone());
    assert!(selector.on_encoder_broken().is_none());
    assert_eq!(
        selector.failure(),
        Some(VideoEncoderSelectorFailure::ReentrantCall)
    );
    slot.lock().unwrap().take(); // Explicitly break the application's cycle.
}

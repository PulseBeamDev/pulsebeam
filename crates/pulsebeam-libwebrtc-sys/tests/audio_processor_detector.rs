use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Default)]
struct Evidence {
    render: AtomicUsize,
    capture: AtomicUsize,
    drops: AtomicUsize,
    reject_metrics: AtomicBool,
}
struct Detector(Arc<Evidence>);
impl Drop for Detector {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl AudioProcessingEchoDetectorProvider for Detector {
    fn initialize(&mut self, cr: i32, cc: i32, rr: i32, rc: i32) -> Result<(), String> {
        assert!(cr > 0 && cc > 0 && rr > 0 && rc > 0);
        Ok(())
    }
    fn analyze_render(&mut self, samples: &[f32]) -> Result<(), String> {
        assert!(!samples.is_empty() && samples.iter().all(|x| x.is_finite()));
        self.0.render.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn analyze_capture(&mut self, samples: &[f32]) -> Result<(), String> {
        assert!(!samples.is_empty() && samples.iter().all(|x| x.is_finite()));
        self.0.capture.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn metrics(&mut self) -> Result<AudioProcessingEchoDetectorMetrics, String> {
        if self.0.reject_metrics.swap(false, Ordering::SeqCst) {
            return Err("metrics rejected".into());
        }
        Ok(AudioProcessingEchoDetectorMetrics {
            echo_likelihood: Some(1.25),
            echo_likelihood_recent_max: None,
        })
    }
}
fn build(env: &Environment, detector: AudioProcessingEchoDetector) -> AudioProcessingModule {
    let mut p = BuiltinAudioProcessorBuilder::default()
        .echo_detector(detector)
        .build(env)
        .unwrap();
    let s = AudioProcessingStream {
        sample_rate_hz: 16000,
        channels: 1,
    };
    p.initialize_with_formats(AudioProcessingFormats {
        input: s,
        output: s,
        reverse_input: s,
        reverse_output: s,
    })
    .unwrap();
    p
}

#[test]
fn selected_shared_detector_receives_native_frames_and_preserves_optional_metrics() {
    let env = Environment::builder().build().unwrap();
    let evidence = Arc::new(Evidence::default());
    let detector = AudioProcessingEchoDetector::from_provider(Detector(evidence.clone()));
    let observer = detector.failure_observer();
    let mut first = build(&env, detector.clone());
    let second = build(&env, detector);
    drop(env);
    let stream = AudioProcessingStream {
        sample_rate_hz: 16000,
        channels: 1,
    };
    first
        .process_reverse_interleaved(stream, &mut [1000; 160])
        .unwrap();
    first.process_interleaved(stream, &mut [1000; 160]).unwrap();
    let statistics = first.try_statistics().unwrap();
    assert_eq!(statistics.residual_echo_likelihood, Some(1.25));
    assert_eq!(statistics.residual_echo_likelihood_recent_max, None);
    assert!(evidence.render.load(Ordering::SeqCst) > 0);
    assert!(evidence.capture.load(Ordering::SeqCst) > 0);
    drop(first);
    assert_eq!(evidence.drops.load(Ordering::SeqCst), 0);
    drop(second);
    assert_eq!(evidence.drops.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure(), None);
}

#[test]
fn detector_metrics_failure_is_fallible_and_does_not_poison_later_snapshots() {
    let env = Environment::builder().build().unwrap();
    let evidence = Arc::new(Evidence::default());
    let detector = AudioProcessingEchoDetector::from_provider(Detector(evidence.clone()));
    let observer = detector.failure_observer();
    let mut p = build(&env, detector);
    // The SDK collects metrics during capture, not when reading statistics.
    assert_eq!(p.try_statistics().unwrap().residual_echo_likelihood, None);
    let stream = AudioProcessingStream {
        sample_rate_hz: 16000,
        channels: 1,
    };
    p.process_reverse_interleaved(stream, &mut [1000; 160])
        .unwrap();
    evidence.reject_metrics.store(true, Ordering::SeqCst);
    let mut capture = [1000; 160];
    assert_eq!(
        p.process_interleaved(stream, &mut capture).unwrap_err(),
        AudioProcessorError::CallbackFailed
    );
    assert_eq!(capture, [1000; 160]);
    assert!(matches!(
        p.try_statistics(),
        Err(AudioProcessorError::CallbackFailed)
    ));
    assert_eq!(
        observer.failure().unwrap().operation,
        AudioProcessingCustomOperation::Metrics
    );
    p.process_interleaved(stream, &mut capture).unwrap();
    assert_eq!(
        p.try_statistics().unwrap().residual_echo_likelihood,
        Some(1.25)
    );
    assert_eq!(observer.failure_count(), 1);
}

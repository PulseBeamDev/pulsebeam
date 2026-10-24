use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct Calls {
    adjust: AtomicUsize,
    reset: AtomicUsize,
    estimate: AtomicUsize,
    drops: AtomicUsize,
}
struct Estimator {
    calls: Arc<Calls>,
    reject_adjust: bool,
}
impl Drop for Estimator {
    fn drop(&mut self) {
        self.calls.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl NeuralResidualEchoEstimatorProvider for Estimator {
    fn adjust_config(&mut self, config: Aec3Suppressor) -> Result<Aec3Suppressor, String> {
        self.calls.adjust.fetch_add(1, Ordering::SeqCst);
        if self.reject_adjust {
            Err("adjust rejected".into())
        } else {
            Ok(config)
        }
    }
    fn reset(&mut self) -> Result<(), String> {
        self.calls.reset.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn estimate(
        &mut self,
        input: NeuralEchoEstimateInput<'_>,
    ) -> Result<NeuralEchoEstimate, String> {
        self.calls.estimate.fetch_add(1, Ordering::SeqCst);
        Ok(NeuralEchoEstimate {
            conservative: vec![[0.0; 65]; input.conservative_output_channels],
            unbounded: vec![[0.0; 65]; input.unbounded_output_channels],
        })
    }
}
fn config() -> AudioProcessorConfig {
    let mut config = AudioProcessorConfig::default();
    config.echo_canceller.enabled = true;
    config
}
#[test]
fn stock_aec3_drives_the_selected_caller_neural_estimator() {
    let env = Environment::builder().build().unwrap();
    let calls = Arc::new(Calls::default());
    let estimator = NeuralResidualEchoEstimator::from_provider(Estimator {
        calls: calls.clone(),
        reject_adjust: false,
    });
    let observer = estimator.failure_observer();
    let mut apm = BuiltinAudioProcessorBuilder::default()
        .config(config())
        .neural_residual_echo_estimator(estimator)
        .build(&env)
        .unwrap();
    let mono = AudioProcessingStream {
        sample_rate_hz: 16000,
        channels: 1,
    };
    apm.initialize_with_formats(AudioProcessingFormats {
        input: mono,
        output: mono,
        reverse_input: mono,
        reverse_output: mono,
    })
    .unwrap();
    assert_eq!(apm.set_stream_delay_ms(0), AudioProcessingStatus::OK);
    drop(env);
    for block in 0..40 {
        let mut render = [0_i16; 160];
        let mut capture = [0_i16; 160];
        for i in 0..160 {
            let phase = (block * 160 + i) as f32;
            render[i] = ((phase * 0.08).sin() * 5000.0) as i16;
            capture[i] = ((phase * 0.08 + 0.2).sin() * 2000.0) as i16;
        }
        assert_eq!(
            apm.process_reverse_interleaved(mono, &mut render).unwrap(),
            AudioProcessingStatus::OK
        );
        assert_eq!(
            apm.process_interleaved(mono, &mut capture).unwrap(),
            AudioProcessingStatus::OK
        );
    }
    assert!(calls.adjust.load(Ordering::SeqCst) > 0);
    assert!(calls.reset.load(Ordering::SeqCst) > 0);
    assert!(calls.estimate.load(Ordering::SeqCst) > 0);
    assert_eq!(calls.drops.load(Ordering::SeqCst), 0);
    drop(apm);
    assert_eq!(calls.drops.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure(), None);
}
#[test]
fn rejected_neural_construction_returns_error_and_retires_provider() {
    let env = Environment::builder().build().unwrap();
    let calls = Arc::new(Calls::default());
    let estimator = NeuralResidualEchoEstimator::from_provider(Estimator {
        calls: calls.clone(),
        reject_adjust: true,
    });
    let observer = estimator.failure_observer();
    let result = BuiltinAudioProcessorBuilder::default()
        .config(config())
        .neural_residual_echo_estimator(estimator)
        .build(&env);
    assert!(matches!(result, Err(AudioProcessorError::CallbackFailed)));
    assert_eq!(calls.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        observer.failure().unwrap().operation,
        AudioProcessingCustomOperation::AdjustConfig
    );
}

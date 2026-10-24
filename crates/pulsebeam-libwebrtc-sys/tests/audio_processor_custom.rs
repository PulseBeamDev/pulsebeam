use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn stream() -> AudioProcessingStream {
    AudioProcessingStream {
        sample_rate_hz: 16000,
        channels: 1,
    }
}
fn initialize(p: &mut AudioProcessingModule) {
    let s = stream();
    assert_eq!(
        p.initialize_with_formats(AudioProcessingFormats {
            input: s,
            output: s,
            reverse_input: s,
            reverse_output: s,
        })
        .unwrap(),
        AudioProcessingStatus::OK
    );
}

struct Post {
    reject: Arc<AtomicBool>,
    drops: Arc<AtomicUsize>,
}
impl AudioProcessingCustomProcessorProvider for Post {
    fn initialize(&mut self, rate: i32, channels: i32) -> Result<(), String> {
        assert!(rate > 0 && channels > 0);
        Ok(())
    }
    fn process(&mut self, mut audio: AudioProcessingBufferMut<'_>) -> Result<(), String> {
        assert_eq!(audio.sample_rate_hz(), 16000);
        assert_eq!((audio.channels(), audio.frames()), (1, 160));
        for sample in audio.samples_mut() {
            *sample += 250.0;
        }
        if self.reject.swap(false, Ordering::SeqCst) {
            Err("reject this frame".into())
        } else {
            Ok(())
        }
    }
    fn to_string(&mut self) -> Result<Vec<u8>, String> {
        Ok(b"post\0\xff".to_vec())
    }
}
impl Drop for Post {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn capture_post_changes_native_pcm_after_environment_retirement() {
    let env = Environment::builder().build().unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let processor = AudioProcessingCustomProcessor::from_provider(Post {
        reject: Arc::new(AtomicBool::new(false)),
        drops: drops.clone(),
    });
    let observer = processor.failure_observer();
    let mut apm = BuiltinAudioProcessorBuilder::default()
        .capture_post_processing(processor)
        .build(&env)
        .unwrap();
    initialize(&mut apm);
    drop(env);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    // The standalone owner is deliberately !Send. Installation is the separate
    // consuming SDK handoff; do not widen that contract just to move this test.
    let mut pcm = vec![1000; 167];
    assert_eq!(
        apm.process_interleaved(stream(), &mut pcm).unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(&pcm[..160], &[1250; 160]);
    assert_eq!(&pcm[160..], &[1000; 7]);
    drop(apm);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure(), None);
}

#[test]
fn rejected_capture_and_render_callbacks_preserve_public_pcm_and_allow_later_frames() {
    for render in [false, true] {
        let env = Environment::builder().build().unwrap();
        let processor = AudioProcessingCustomProcessor::from_provider(Post {
            reject: Arc::new(AtomicBool::new(true)),
            drops: Arc::new(AtomicUsize::new(0)),
        });
        let observer = processor.failure_observer();
        let builder = BuiltinAudioProcessorBuilder::default();
        let mut apm = if render {
            builder.render_pre_processing(processor)
        } else {
            builder.capture_post_processing(processor)
        }
        .build(&env)
        .unwrap();
        initialize(&mut apm);
        let mut pcm = vec![1000; 160];
        let result = if render {
            apm.process_reverse_interleaved(stream(), &mut pcm)
        } else {
            apm.process_interleaved(stream(), &mut pcm)
        };
        assert_eq!(result, Err(AudioProcessorError::CallbackFailed));
        assert_eq!(pcm, vec![1000; 160]);
        let failure = observer.failure().unwrap();
        assert_eq!(failure.operation, AudioProcessingCustomOperation::Process);
        assert!(matches!(
            failure.kind,
            AudioProcessingCustomFailureKind::Rejected(_)
        ));
        let result = if render {
            apm.process_reverse_interleaved(stream(), &mut pcm)
        } else {
            apm.process_interleaved(stream(), &mut pcm)
        };
        assert_eq!(result.unwrap(), AudioProcessingStatus::OK);
        if !render {
            assert_eq!(pcm, vec![1250; 160]);
        }
        assert_eq!(observer.failure_count(), 1);
    }
}

struct RejectAnalyzer;
impl AudioProcessingCustomAnalyzerProvider for RejectAnalyzer {
    fn initialize(&mut self, _: i32, _: i32) -> Result<(), String> {
        Ok(())
    }
    fn to_string(&mut self) -> Result<Vec<u8>, String> {
        Ok(b"analyzer".to_vec())
    }
    fn analyze(&mut self, audio: AudioProcessingBuffer<'_>) -> Result<(), String> {
        assert_eq!((audio.channels(), audio.frames()), (1, 160));
        assert!(audio.samples().iter().all(|x| x.is_finite()));
        Err("analysis rejected".into())
    }
}
#[test]
fn analyzer_failure_is_not_successful_unchanged_audio() {
    let env = Environment::builder().build().unwrap();
    let analyzer = AudioProcessingCustomAnalyzer::from_provider(RejectAnalyzer);
    let observer = analyzer.failure_observer();
    let mut apm = BuiltinAudioProcessorBuilder::default()
        .capture_analyzer(analyzer)
        .build(&env)
        .unwrap();
    initialize(&mut apm);
    let mut pcm = vec![1234; 160];
    assert_eq!(
        apm.process_interleaved(stream(), &mut pcm),
        Err(AudioProcessorError::CallbackFailed)
    );
    assert_eq!(pcm, vec![1234; 160]);
    assert_eq!(
        observer.failure().unwrap().operation,
        AudioProcessingCustomOperation::Analyze
    );
}

struct EchoFactory(Arc<AtomicUsize>);
struct Echo(Arc<AtomicUsize>);
impl EchoControlFactoryProvider for EchoFactory {
    fn create<'a>(
        &mut self,
        mut context: EchoControlCreationContext<'a>,
    ) -> Result<EchoControl<'a>, String> {
        assert!(context.sample_rate_hz() >= 16000);
        assert!(context.render_channels() > 0 && context.capture_channels() > 0);
        assert!(context.neural_estimator().is_none());
        context
            .controller(Echo(self.0.clone()))
            .map_err(|e| e.to_string())
    }
}
impl EchoControlProvider for Echo {
    fn analyze_render(
        &mut self,
        _: EchoControlDependencies<'_>,
        audio: EchoControlAudio<'_>,
    ) -> Result<(), String> {
        assert!(audio.channel(0).unwrap().iter().all(|x| x.is_finite()));
        Ok(())
    }
    fn analyze_capture(
        &mut self,
        _: EchoControlDependencies<'_>,
        audio: EchoControlAudio<'_>,
    ) -> Result<(), String> {
        assert!(audio.channel(0).unwrap().iter().all(|x| x.is_finite()));
        Ok(())
    }
    fn process_capture(
        &mut self,
        _: EchoControlDependencies<'_>,
        mut audio: EchoControlAudioMut<'_>,
        _: bool,
    ) -> Result<(), String> {
        assert_eq!(audio.as_read().bands(), 1);
        audio.channel_mut(0).unwrap().fill(123.0);
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn process_capture_with_linear(
        &mut self,
        dependencies: EchoControlDependencies<'_>,
        audio: EchoControlAudioMut<'_>,
        linear: Option<EchoControlLinearOutput<'_>>,
        changed: bool,
    ) -> Result<(), String> {
        assert!(linear.is_none());
        self.process_capture(dependencies, audio, changed)
    }
    fn metrics(&mut self, _: EchoControlDependencies<'_>) -> Result<EchoControlMetrics, String> {
        Ok(EchoControlMetrics {
            echo_return_loss: 20.0,
            echo_return_loss_enhancement: 10.0,
            delay_ms: 0,
        })
    }
    fn set_audio_buffer_delay(
        &mut self,
        _: EchoControlDependencies<'_>,
        _: i32,
    ) -> Result<(), String> {
        Ok(())
    }
    fn active_processing(&mut self, _: EchoControlDependencies<'_>) -> Result<bool, String> {
        Ok(true)
    }
}
#[test]
fn caller_echo_factory_is_selected_even_with_config_echo_disabled() {
    let env = Environment::builder().build().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = EchoControlFactory::from_provider(EchoFactory(calls.clone()));
    let observer = factory.failure_observer().unwrap();
    let mut apm = BuiltinAudioProcessorBuilder::default()
        .echo_control_factory(factory)
        .build(&env)
        .unwrap();
    initialize(&mut apm);
    assert!(!apm.configuration().echo_canceller.enabled);
    let mut render = vec![0; 160];
    apm.process_reverse_interleaved(stream(), &mut render)
        .unwrap();
    let mut capture = vec![1000; 160];
    assert_eq!(
        apm.process_interleaved(stream(), &mut capture).unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(capture, vec![123; 160]);
    assert!(calls.load(Ordering::SeqCst) > 0);
    drop(apm);
    assert_eq!(observer.failure(), None);
}

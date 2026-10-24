use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
};

#[derive(Clone, Debug, PartialEq)]
enum Trace {
    Init(AudioProcessingFormats, i64),
    Audio(
        AudioProcessingDumpOperation,
        u8,
        usize,
        usize,
        Vec<i16>,
        Vec<f32>,
    ),
    State(AudioProcessingDumpState),
    Capture,
    Runtime(AudioProcessingRuntimeSetting),
    Config(AudioProcessingDumpConfig),
}
#[derive(Default)]
struct State {
    trace: Mutex<Vec<Trace>>,
    mode: AtomicU8,
    drops: AtomicUsize,
    panic_drop: AtomicBool,
}
struct Recorder(Arc<State>);
fn audio(op: AudioProcessingDumpOperation, a: AudioProcessingDumpSamples<'_>) -> Trace {
    use AudioProcessingDumpSamples as A;
    match a {
        A::Interleaved {
            channels,
            samples_per_channel,
            samples,
        } => Trace::Audio(
            op,
            0,
            channels,
            samples_per_channel,
            samples.to_vec(),
            vec![],
        ),
        A::PlanarFrame {
            channels,
            samples_per_channel,
            samples,
        } => Trace::Audio(
            op,
            1,
            channels,
            samples_per_channel,
            vec![],
            samples.to_vec(),
        ),
        A::Mono(samples) => Trace::Audio(op, 2, 1, samples.len(), vec![], samples.to_vec()),
        A::PlanarPointers {
            channels,
            samples_per_channel,
            samples,
        } => Trace::Audio(
            op,
            3,
            channels,
            samples_per_channel,
            vec![],
            samples.to_vec(),
        ),
    }
}
struct BadPanicPayload;
impl Drop for BadPanicPayload {
    fn drop(&mut self) {
        panic!("payload drop panic");
    }
}
impl AudioProcessingDumpProvider for Recorder {
    fn record(&self, event: AudioProcessingDumpEvent<'_>) -> Result<(), String> {
        use AudioProcessingDumpEvent as E;
        use AudioProcessingDumpOperation as O;
        let op = event.operation();
        let trace = match event {
            E::Initialize {
                formats,
                time_now_ms,
            } => Trace::Init(formats, time_now_ms),
            E::CaptureInput(a) => audio(op, a),
            E::CaptureOutput(a) => audio(op, a),
            E::RenderStream(a) => audio(op, a),
            E::ProcessingState(s) => Trace::State(s),
            E::CaptureStream => Trace::Capture,
            E::RuntimeSetting(s) => Trace::Runtime(s),
            E::Config(c) => Trace::Config(c.clone()),
        };
        self.0.trace.lock().unwrap().push(trace);
        let mode = self.0.mode.load(Ordering::Acquire);
        if ((mode == 1 || mode == 2 || mode == 6) && op == O::CaptureInput)
            || (mode == 3 && op == O::Initialize)
            || (mode == 4 && op == O::RenderStream)
        {
            self.0.mode.store(0, Ordering::Release);
            if mode == 2 {
                panic!("dump callback panic");
            }
            if mode == 6 {
                std::panic::panic_any(BadPanicPayload);
            }
            return Err("selected callback refusal".into());
        }
        Ok(())
    }
}
impl Drop for Recorder {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::Release);
        if self.0.panic_drop.load(Ordering::Acquire) {
            panic!("dump provider drop panic");
        }
    }
}
fn dump() -> (
    AudioProcessingDump,
    Arc<State>,
    AudioProcessingDumpFailureObserver,
) {
    let state = Arc::new(State::default());
    let dump = AudioProcessingDump::from_provider(Recorder(state.clone()));
    let observer = dump.failure_observer();
    (dump, state, observer)
}
fn processor(gain: f32) -> AudioProcessingModule {
    let mut c = AudioProcessorConfig::default();
    c.pre_amplifier.enabled = gain != 1.0;
    c.pre_amplifier.fixed_gain_factor = gain;
    AudioProcessingModule::builtin_with_config(&Environment::builder().build().unwrap(), &c)
        .unwrap()
}
fn stream(channels: usize) -> AudioProcessingStream {
    AudioProcessingStream {
        sample_rate_hz: 16000,
        channels,
    }
}

#[test]
fn unique_dump_observes_actual_capture_config_optional_state_and_runtime_values() {
    use AudioProcessingDumpOperation as O;
    let mut p = processor(2.0);
    let (d, state, observer) = dump();
    p.attach_aec_dump(d).unwrap();
    assert_eq!(state.drops.load(Ordering::Acquire), 0);
    let mut pcm = vec![1000; 160];
    pcm.extend([777; 7]);
    assert_eq!(
        p.process_interleaved(stream(1), &mut pcm).unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(&pcm[..160], &[2000; 160]);
    assert_eq!(&pcm[160..], &[777; 7]);
    let trace = state.trace.lock().unwrap().clone();
    assert!(trace.iter().any(|e| matches!(e, Trace::Config(c) if c.pre_amplifier_enabled && c.pre_amplifier_fixed_gain_factor == 2.0 && c.api_config_string == p.configuration().to_native_string().as_bytes())));
    assert!(
        trace
            .iter()
            .any(|e| matches!(e, Trace::Init(f, t) if f.input == stream(1) && *t > 0))
    );
    assert!(trace.contains(&Trace::Audio(
        O::CaptureInput,
        0,
        1,
        160,
        vec![1000; 160],
        vec![]
    )));
    assert!(trace.contains(&Trace::Audio(
        O::CaptureOutput,
        0,
        1,
        160,
        vec![2000; 160],
        vec![]
    )));
    assert!(trace.contains(&Trace::State(AudioProcessingDumpState {
        delay: 0,
        drift: 0,
        applied_input_volume: None,
        keypress: false
    })));
    assert_eq!(trace.last(), Some(&Trace::Capture));
    p.set_stream_analog_level(17).unwrap();
    p.set_stream_key_pressed(true);
    assert_eq!(p.set_stream_delay_ms(203), AudioProcessingStatus::OK);
    assert!(
        p.post_runtime_setting(AudioProcessingRuntimeSetting::CompressionGainDb(19))
            .unwrap()
    );
    assert!(
        p.post_runtime_setting(AudioProcessingRuntimeSetting::CaptureOutputUsed(true))
            .unwrap()
    );
    p.process_interleaved(stream(1), &mut pcm).unwrap();
    let trace = state.trace.lock().unwrap().clone();
    assert!(trace.contains(&Trace::Runtime(
        AudioProcessingRuntimeSetting::CompressionGainDb(19)
    )));
    assert!(trace.contains(&Trace::Runtime(
        AudioProcessingRuntimeSetting::CaptureOutputUsed(true)
    )));
    assert!(trace.contains(&Trace::State(AudioProcessingDumpState {
        delay: 203,
        drift: 0,
        applied_input_volume: Some(17),
        keypress: true
    })));
    drop(p);
    assert_eq!(state.drops.load(Ordering::Acquire), 1);
    assert_eq!(observer.failure_count(), 0);
    assert_eq!(observer.failure(), None);
}

#[test]
fn float_mono_and_render_pointer_overloads_preserve_channel_rows_and_native_dimensions() {
    use AudioProcessingDumpOperation as O;
    let mut p = processor(1.0);
    let (d, state, _) = dump();
    p.attach_aec_dump(d).unwrap();
    let mut input = vec![0.125; 160];
    input.extend([0.25; 160]);
    let mut output = vec![9.0; 327];
    assert_eq!(
        p.process_planar(stream(2), &input, stream(2), &mut output)
            .unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(&output[320..], &[9.0; 7]);
    let trace = state.trace.lock().unwrap().clone();
    // The pin emits one MonoView callback per capture channel, not a
    // combined AudioFrameView. Preserve the actual overload and row order.
    let inputs: Vec<_> = trace
        .iter()
        .filter(|e| matches!(e, Trace::Audio(O::CaptureInput, ..)))
        .cloned()
        .collect();
    let outputs: Vec<_> = trace
        .iter()
        .filter(|e| matches!(e, Trace::Audio(O::CaptureOutput, ..)))
        .cloned()
        .collect();
    assert_eq!(
        inputs,
        vec![
            Trace::Audio(O::CaptureInput, 2, 1, 160, vec![], input[..160].to_vec()),
            Trace::Audio(O::CaptureInput, 2, 1, 160, vec![], input[160..].to_vec()),
        ]
    );
    assert_eq!(
        outputs,
        vec![
            Trace::Audio(O::CaptureOutput, 2, 1, 160, vec![], output[..160].to_vec()),
            Trace::Audio(
                O::CaptureOutput,
                2,
                1,
                160,
                vec![],
                output[160..320].to_vec()
            ),
        ]
    );
    assert_eq!(
        p.process_reverse_planar(stream(2), &input, stream(2), &mut output)
            .unwrap(),
        AudioProcessingStatus::OK
    );
    assert!(state.trace.lock().unwrap().contains(&Trace::Audio(
        O::RenderStream,
        3,
        2,
        160,
        vec![],
        input
    )));
    let mut integers = vec![1; 160];
    integers.extend([2; 160]);
    p.process_reverse_interleaved(stream(2), &mut integers)
        .unwrap();
    // This pin actually swaps these two arguments for int16 render callbacks.
    // Preserve the native callback values, never infer/repair stream metadata.
    assert!(state.trace.lock().unwrap().contains(&Trace::Audio(
        O::RenderStream,
        0,
        160,
        2,
        integers,
        vec![]
    )));
}

#[test]
fn callback_refusal_and_panics_are_local_errors_without_pcm_publication_or_stale_poison() {
    use AudioProcessingDumpOperation as O;
    for mode in [1, 2, 6] {
        let mut p = processor(2.0);
        let (d, state, observer) = dump();
        p.attach_aec_dump(d).unwrap();
        state.mode.store(mode, Ordering::Release);
        let original = vec![1000; 167];
        let mut pcm = original.clone();
        assert_eq!(
            p.process_interleaved(stream(1), &mut pcm),
            Err(AudioProcessorError::CallbackFailed)
        );
        assert_eq!(pcm, original);
        // SDK void callbacks cannot abort DSP. It really produced 2000, but
        // the wrapper withheld that result instead of claiming successful PCM.
        assert!(state.trace.lock().unwrap().contains(&Trace::Audio(
            O::CaptureOutput,
            0,
            1,
            160,
            vec![2000; 160],
            vec![]
        )));
        let failure = observer.failure().unwrap();
        assert_eq!(failure.operation, O::CaptureInput);
        assert_eq!(
            failure.kind,
            if mode == 1 {
                AudioProcessingDumpFailureKind::Rejected("selected callback refusal".into())
            } else {
                AudioProcessingDumpFailureKind::Panicked
            }
        );
        assert_eq!(observer.failure_count(), 1);
        assert_eq!(
            p.process_interleaved(stream(1), &mut pcm).unwrap(),
            AudioProcessingStatus::OK
        );
        assert_eq!(&pcm[..160], &[2000; 160]);
        assert_eq!(&pcm[160..], &[1000; 7]);
        assert_eq!(observer.failure_count(), 1);
    }
}

#[test]
fn planar_and_analyze_failures_do_not_publish_successful_audio_or_status() {
    let mut p = processor(2.0);
    let (d, state, observer) = dump();
    p.attach_aec_dump(d).unwrap();
    let input = vec![0.125; 160];
    let original = vec![7.25; 169];
    let mut output = original.clone();
    state.mode.store(1, Ordering::Release);
    assert_eq!(
        p.process_planar(stream(1), &input, stream(1), &mut output),
        Err(AudioProcessorError::CallbackFailed)
    );
    assert_eq!(output, original);
    state.mode.store(4, Ordering::Release);
    assert_eq!(
        p.process_reverse_planar(stream(1), &input, stream(1), &mut output),
        Err(AudioProcessorError::CallbackFailed)
    );
    assert_eq!(output, original);
    state.mode.store(4, Ordering::Release);
    assert_eq!(
        p.analyze_reverse_planar(stream(1), &input),
        Err(AudioProcessorError::CallbackFailed)
    );
    assert_eq!(observer.failure_count(), 3);
    assert_eq!(
        p.process_planar(stream(1), &input, stream(1), &mut output)
            .unwrap(),
        AudioProcessingStatus::OK
    );
    assert_eq!(&output[..160], &[0.25; 160]);
    assert_eq!(&output[160..], &[7.25; 9]);
}

#[test]
fn attach_failure_does_not_roll_back_ownership_and_drop_panics_are_observable() {
    let mut p = processor(1.0);
    let (d, first, observer) = dump();
    first.mode.store(3, Ordering::Release);
    assert_eq!(
        p.attach_aec_dump(d),
        Err(AudioProcessorError::CallbackFailed)
    );
    assert_eq!(
        observer.failure().unwrap().operation,
        AudioProcessingDumpOperation::Initialize
    );
    let mut pcm = vec![1000; 160];
    p.process_interleaved(stream(1), &mut pcm).unwrap();
    assert!(first.trace.lock().unwrap().contains(&Trace::Capture));
    first.panic_drop.store(true, Ordering::Release);
    let (second_dump, second, second_observer) = dump();
    assert_eq!(
        p.attach_aec_dump(second_dump),
        Err(AudioProcessorError::CallbackFailed)
    );
    assert_eq!(first.drops.load(Ordering::Acquire), 1);
    assert_eq!(observer.failure_count(), 2);
    p.process_interleaved(stream(1), &mut pcm).unwrap();
    assert!(second.trace.lock().unwrap().contains(&Trace::Capture));
    second.panic_drop.store(true, Ordering::Release);
    assert_eq!(
        p.detach_aec_dump(),
        Err(AudioProcessorError::CallbackFailed)
    );
    assert_eq!(
        second_observer.failure().unwrap().operation,
        AudioProcessingDumpOperation::Destroy
    );
    p.detach_aec_dump().unwrap();
    assert_eq!(second.drops.load(Ordering::Acquire), 1);
    let (last_dump, last, last_observer) = dump();
    last.panic_drop.store(true, Ordering::Release);
    p.attach_aec_dump(last_dump).unwrap();
    drop(p);
    assert_eq!(last.drops.load(Ordering::Acquire), 1);
    assert_eq!(
        last_observer.failure().unwrap().operation,
        AudioProcessingDumpOperation::Destroy
    );
}

#[test]
fn unattached_owner_drop_is_contained_and_native_config_values_preserve_bytes_and_nan() {
    let (d, state, observer) = dump();
    state.panic_drop.store(true, Ordering::Release);
    drop(d);
    assert_eq!(state.drops.load(Ordering::Acquire), 1);
    assert_eq!(observer.failure_count(), 1);
    let mut a = AudioProcessingDumpConfig::default();
    assert!(!a.aec_enabled);
    assert_eq!(a.pre_amplifier_fixed_gain_factor, 1.0);
    assert!(a.api_config_string.is_empty());
    a.api_config_string = vec![0, 255, 17];
    a.experiments_description = vec![255, 0, 29];
    assert_eq!(a, a.clone());
    let mut b = a.clone();
    b.api_config_string.push(13);
    assert_ne!(a, b);
    a.pre_amplifier_fixed_gain_factor = f32::NAN;
    assert_ne!(a, a.clone());
}

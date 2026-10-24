use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Source {
    value: i16,
    rate: i32,
    kind: usize,
    calls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}
impl Drop for Source {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl AudioMixerSourceProvider for Source {
    fn preferred_sample_rate(&mut self) -> Result<i32, AudioMixerError> {
        Ok(self.rate)
    }
    fn ssrc(&mut self) -> Result<i32, AudioMixerError> {
        Ok(-42)
    }
    fn audio_frame(&mut self, rate: i32) -> Result<MixerAudioFrame, AudioMixerError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.kind == 3 {
            panic!("source panic");
        }
        if self.kind == 2 {
            return Ok(MixerAudioFrame::Error);
        }
        let mut frame = AudioFrame::new();
        let samples = vec![self.value; rate as usize / 100];
        frame
            .update_frame(
                123,
                if self.kind == 1 { None } else { Some(&samples) },
                samples.len(),
                if self.kind == 4 { rate + 1 } else { rate },
                AudioFrameSpeechType::NORMAL_SPEECH,
                AudioFrameVadActivity::ACTIVE,
                1,
            )
            .unwrap();
        Ok(if self.kind == 1 {
            MixerAudioFrame::Muted(frame)
        } else {
            MixerAudioFrame::Normal(frame)
        })
    }
}
fn source(
    value: i16,
    rate: i32,
    kind: usize,
) -> (AudioMixerSource, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    (
        AudioMixerSource::from_provider(Source {
            value,
            rate,
            kind,
            calls: calls.clone(),
            drops: drops.clone(),
        }),
        calls,
        drops,
    )
}

#[test]
fn native_mixer_retains_sources_and_mixes_real_frames() {
    let mut mixer = AudioMixer::stock().unwrap();
    let empty = mixer.mix(1).unwrap();
    assert!(empty.muted());
    let (a, calls, drops) = source(1000, 16000, 0);
    let alias = a.clone();
    assert_eq!(a.ssrc(), Ok(-42));
    assert_eq!(a.preferred_sample_rate(), Ok(16000));
    assert!(mixer.add_source(&a).unwrap());
    assert!(!mixer.add_source(&alias).unwrap());
    drop(a);
    let frame = mixer.mix(1).unwrap();
    assert_eq!(frame.fields().sample_rate_hz, 16000);
    assert_eq!(frame.fields().timestamp, 123);
    assert_eq!(frame.data_view().unwrap(), &[1000; 160]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let (b, _, b_drops) = source(2000, 16000, 0);
    assert!(mixer.add_source(&b).unwrap());
    drop(b);
    let mixed = mixer.mix(2).unwrap();
    assert_eq!(mixed.fields().num_channels, 2);
    assert_eq!(mixed.data_view().unwrap().len(), 320);
    assert!(mixed.data_view().unwrap().iter().all(|&v| v > 1000));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(mixer.remove_source(&alias).unwrap());
    assert!(!mixer.remove_source(&alias).unwrap());
    drop(alias);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(mixer);
    assert_eq!(b_drops.load(Ordering::SeqCst), 1);
    assert!(mixed.data_view().unwrap().iter().any(|&v| v != 0));
}

#[test]
fn normal_muted_native_error_and_callback_failure_are_distinct() {
    for kind in [1, 2, 3, 4] {
        let mut mixer = AudioMixer::stock().unwrap();
        let (source, _, _) = source(1000, 8000, kind);
        let failure = source.failure_observer();
        mixer.add_source(&source).unwrap();
        let result = mixer.mix(1);
        match kind {
            1 | 2 => {
                assert!(result.unwrap().muted());
                assert_eq!(failure.failure(), None);
            }
            3 => {
                assert!(matches!(result, Err(AudioMixerError::CallbackRejected)));
                assert_eq!(failure.failure(), Some(AudioMixerError::CallbackPanicked));
            }
            _ => {
                assert!(matches!(result, Err(AudioMixerError::InvalidFrame)));
                assert_eq!(failure.failure(), Some(AudioMixerError::InvalidFrame));
            }
        }
        drop(source);
        drop(mixer);
        if kind >= 3 {
            assert!(failure.failure().is_some());
        }
    }
}

#[test]
fn current_preferred_rates_are_not_cached_and_invalid_maximum_refuses_before_frames() {
    struct Changing {
        query: usize,
        calls: Arc<AtomicUsize>,
    }
    impl AudioMixerSourceProvider for Changing {
        fn ssrc(&mut self) -> Result<i32, AudioMixerError> {
            Ok(1)
        }
        fn preferred_sample_rate(&mut self) -> Result<i32, AudioMixerError> {
            self.query += 1;
            Ok(if self.query == 1 { 16001 } else { i32::MAX })
        }
        fn audio_frame(&mut self, _: i32) -> Result<MixerAudioFrame, AudioMixerError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(MixerAudioFrame::Error)
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let source = AudioMixerSource::from_provider(Changing {
        query: 0,
        calls: calls.clone(),
    });
    let mut mixer = AudioMixer::stock().unwrap();
    mixer.add_source(&source).unwrap();
    assert_eq!(mixer.mix(1).unwrap().fields().sample_rate_hz, 32000);
    assert!(matches!(
        mixer.mix(1),
        Err(AudioMixerError::InvalidPreferredRate)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        mixer.mix(0),
        Err(AudioMixerError::InvalidChannels)
    ));
}

#[test]
fn recursive_source_metadata_is_observable_without_mutex_deadlock() {
    struct Recursive(Arc<Mutex<Option<AudioMixerSource>>>);
    impl AudioMixerSourceProvider for Recursive {
        fn ssrc(&mut self) -> Result<i32, AudioMixerError> {
            Ok(1)
        }
        fn preferred_sample_rate(&mut self) -> Result<i32, AudioMixerError> {
            self.0.lock().unwrap().as_ref().unwrap().ssrc()?;
            Ok(16000)
        }
        fn audio_frame(&mut self, _: i32) -> Result<MixerAudioFrame, AudioMixerError> {
            unreachable!()
        }
    }
    let slot = Arc::new(Mutex::new(None));
    let source = AudioMixerSource::from_provider(Recursive(slot.clone()));
    let failure = source.failure_observer();
    *slot.lock().unwrap() = Some(source.clone());
    let mut mixer = AudioMixer::stock().unwrap();
    mixer.add_source(&source).unwrap();
    assert!(matches!(
        mixer.mix(1),
        Err(AudioMixerError::CallbackRejected)
    ));
    assert_eq!(failure.failure(), Some(AudioMixerError::ReentrantCall));
    slot.lock().unwrap().take();
    drop(source);
    drop(mixer);
    assert_eq!(failure.failure(), Some(AudioMixerError::ReentrantCall));
}

#[test]
fn source_readers_serialize_and_native_rate_admission_preserves_safe_current_answers() {
    struct Reader(usize);
    impl AudioMixerSourceProvider for Reader {
        fn ssrc(&mut self) -> Result<i32, AudioMixerError> {
            self.0 += 1;
            Ok(self.0 as i32)
        }
        fn preferred_sample_rate(&mut self) -> Result<i32, AudioMixerError> {
            Ok(0)
        }
        fn audio_frame(&mut self, _: i32) -> Result<MixerAudioFrame, AudioMixerError> {
            Ok(MixerAudioFrame::Error)
        }
    }
    let a = AudioMixerSource::from_provider(Reader(0));
    let threads = (0..16)
        .map(|_| {
            let a = a.clone();
            std::thread::spawn(move || a.ssrc().unwrap())
        })
        .collect::<Vec<_>>();
    let mut values = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect::<Vec<_>>();
    values.sort_unstable();
    assert_eq!(values, (1..=16).collect::<Vec<_>>());
    let (b, _, _) = source(1000, 16000, 0);
    let mut mixer = AudioMixer::stock().unwrap();
    mixer.add_source(&a).unwrap();
    mixer.add_source(&b).unwrap();
    // The native calculator's reached bound is on the maximum preference, not
    // every source. The zero preference is safely consumed at 16 kHz; do not
    // replace it with a made-up rate or invent an unrelated whitelist.
    assert_eq!(mixer.mix(1).unwrap().fields().sample_rate_hz, 16000);
    assert_eq!(a.preferred_sample_rate(), Ok(0));
    assert_eq!(a.failure_observer().failure(), None);
}

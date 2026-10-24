use pulsebeam_webrtc_sys::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Source(i16, Arc<AtomicUsize>);
impl Drop for Source {
    fn drop(&mut self) {
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}
impl AudioMixerSourceProvider for Source {
    fn ssrc(&mut self) -> Result<i32, AudioMixerError> {
        Ok(-42)
    }
    fn preferred_sample_rate(&mut self) -> Result<i32, AudioMixerError> {
        Ok(16000)
    }
    fn audio_frame(&mut self, rate: i32) -> Result<MixerAudioFrame, AudioMixerError> {
        let mut frame = AudioFrame::new();
        let samples = vec![self.0; rate as usize / 100];
        frame
            .update_frame(
                123,
                Some(&samples),
                samples.len(),
                rate,
                AudioFrameSpeechType::NORMAL_SPEECH,
                AudioFrameVadActivity::ACTIVE,
                1,
            )
            .unwrap();
        Ok(MixerAudioFrame::Normal(frame))
    }
}
struct Mixer {
    sources: Vec<AudioMixerSourceToken>,
    retired: Option<AudioMixerSourceToken>,
    drops: Arc<AtomicUsize>,
}
impl Drop for Mixer {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl AudioMixerProvider for Mixer {
    fn add_source(
        &mut self,
        source: AudioMixerSourceToken,
        info: &mut AudioMixerSourceInfo<'_>,
    ) -> Result<bool, AudioMixerError> {
        assert_eq!(info.ssrc(&source)?, -42);
        assert_eq!(info.preferred_sample_rate(&source)?, 16000);
        self.sources.push(source);
        Ok(true)
    }
    fn remove_source(
        &mut self,
        source: AudioMixerSourceToken,
        info: &mut AudioMixerSourceInfo<'_>,
    ) -> Result<(), AudioMixerError> {
        assert_eq!(info.ssrc(&source)?, -42);
        self.sources.retain(|token| token != &source);
        self.retired = Some(source);
        Ok(())
    }
    fn mix(
        &mut self,
        channels: usize,
        inputs: &mut AudioMixerInputs<'_>,
    ) -> Result<AudioFrame, AudioMixerError> {
        assert_eq!(channels, 1);
        if let Some(retired) = &self.retired {
            assert_eq!(inputs.ssrc(retired), Err(AudioMixerError::InvalidSource));
        }
        let source = &self.sources[0];
        assert_eq!(inputs.ssrc(source)?, -42);
        let rate = inputs.preferred_sample_rate(source)?;
        match inputs.audio_frame(source, rate)? {
            MixerAudioFrame::Normal(frame) | MixerAudioFrame::Muted(frame) => Ok(frame),
            MixerAudioFrame::Error => Err(AudioMixerError::CallbackRejected),
        }
    }
}

#[test]
fn selected_caller_mixer_drives_native_sources_and_retires_scoped_tokens() {
    let source_drops = Arc::new(AtomicUsize::new(0));
    let mixer_drops = Arc::new(AtomicUsize::new(0));
    let source = AudioMixerSource::from_provider(Source(1000, source_drops.clone()));
    let mut mixer = AudioMixer::from_provider(Mixer {
        sources: Vec::new(),
        retired: None,
        drops: mixer_drops.clone(),
    })
    .unwrap();
    let observer = mixer.failure_observer();
    assert!(mixer.add_source(&source).unwrap());
    assert!(!mixer.add_source(&source).unwrap());
    let frame = mixer.mix(1).unwrap();
    assert_eq!(frame.fields().timestamp, 123);
    assert_eq!(frame.data_view().unwrap(), &[1000; 160]);
    assert!(mixer.remove_source(&source).unwrap());
    drop(source);
    assert_eq!(source_drops.load(Ordering::SeqCst), 1);
    let second = AudioMixerSource::from_provider(Source(2000, source_drops.clone()));
    assert!(mixer.add_source(&second).unwrap());
    drop(second);
    assert_eq!(source_drops.load(Ordering::SeqCst), 1);
    assert_eq!(mixer.mix(1).unwrap().data_view().unwrap(), &[2000; 160]);
    drop(mixer);
    assert_eq!(source_drops.load(Ordering::SeqCst), 2);
    assert_eq!(mixer_drops.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure(), None);
}

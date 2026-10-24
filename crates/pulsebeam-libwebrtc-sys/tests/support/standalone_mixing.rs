//! Real stock multi-source mixing, with no environment constructor in the SDK.
use pulsebeam_webrtc_sys::*;
use serde::Serialize;
use std::sync::{Arc, Mutex};

#[derive(Debug, Eq, PartialEq, Serialize)]
struct Frame {
    timestamp: u32,
    elapsed_time_ms: i64,
    ntp_time_ms: i64,
    samples_per_channel: usize,
    sample_rate_hz: i32,
    num_channels: usize,
    speech_type: i32,
    vad_activity: i32,
    packet_infos_len: usize,
    channel_layout: i32,
    absolute_capture_timestamp_ms: Option<i64>,
    muted: bool,
    samples: Vec<i16>,
}
impl Frame {
    fn take(frame: &AudioFrame) -> Self {
        let fields = frame.fields();
        // These local sources supply no RTP metadata. A nonempty native output
        // would fail this corpus, rather than silently discarding packet infos.
        assert!(fields.packet_infos.is_empty());
        Self {
            timestamp: fields.timestamp,
            elapsed_time_ms: fields.elapsed_time_ms,
            ntp_time_ms: fields.ntp_time_ms,
            samples_per_channel: fields.samples_per_channel,
            sample_rate_hz: fields.sample_rate_hz,
            num_channels: fields.num_channels,
            speech_type: fields.speech_type.0,
            vad_activity: fields.vad_activity.0,
            packet_infos_len: fields.packet_infos.len(),
            channel_layout: frame.channel_layout().0,
            absolute_capture_timestamp_ms: frame.absolute_capture_timestamp_ms(),
            muted: frame.muted(),
            samples: frame.data_view().unwrap().to_vec(),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Serialize)]
enum SourceEvent {
    PreferredRate {
        id: i32,
        rate: i32,
    },
    Ssrc {
        id: i32,
    },
    Audio {
        id: i32,
        requested_rate: i32,
        frame: Frame,
    },
    Drop {
        id: i32,
    },
}

type Audit = Arc<Mutex<Vec<SourceEvent>>>;
struct Source {
    id: i32,
    preferred_rate: i32,
    values: Vec<i16>,
    block: u32,
    audit: Audit,
}
impl Drop for Source {
    fn drop(&mut self) {
        self.audit
            .lock()
            .unwrap()
            .push(SourceEvent::Drop { id: self.id });
    }
}
impl AudioMixerSourceProvider for Source {
    fn preferred_sample_rate(&mut self) -> Result<i32, AudioMixerError> {
        self.audit.lock().unwrap().push(SourceEvent::PreferredRate {
            id: self.id,
            rate: self.preferred_rate,
        });
        Ok(self.preferred_rate)
    }
    fn ssrc(&mut self) -> Result<i32, AudioMixerError> {
        self.audit
            .lock()
            .unwrap()
            .push(SourceEvent::Ssrc { id: self.id });
        Ok(self.id)
    }
    fn audio_frame(&mut self, rate: i32) -> Result<MixerAudioFrame, AudioMixerError> {
        // Source providers render the format requested by the native mixer.
        // This generates caller input; no resampler or mixer is substituted.
        let samples = (0..rate / 100)
            .flat_map(|_| self.values.iter().copied())
            .collect::<Vec<_>>();
        let mut frame = AudioFrame::new();
        frame
            .update_frame(
                self.block * (rate as u32 / 100),
                Some(&samples),
                rate as usize / 100,
                rate,
                AudioFrameSpeechType::NORMAL_SPEECH,
                AudioFrameVadActivity::ACTIVE,
                self.values.len(),
            )
            .unwrap();
        let mut fields = frame.fields();
        fields.elapsed_time_ms = i64::from(self.block) * 10;
        fields.ntp_time_ms = 7000 + fields.elapsed_time_ms;
        frame.set_fields(&fields).unwrap();
        frame.set_absolute_capture_timestamp_ms(7100 + fields.elapsed_time_ms);
        self.audit.lock().unwrap().push(SourceEvent::Audio {
            id: self.id,
            requested_rate: rate,
            frame: Frame::take(&frame),
        });
        self.block += 1;
        Ok(MixerAudioFrame::Normal(frame))
    }
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct Output {
    after_removal: bool,
    requested_channels: usize,
    frame: Frame,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct Observation {
    outputs: Vec<Output>,
    events: Vec<SourceEvent>,
}

pub fn run() -> Observation {
    let audit = Audit::default();
    let source = |id, preferred_rate, values| {
        AudioMixerSource::from_provider(Source {
            id,
            preferred_rate,
            values,
            block: 0,
            audit: audit.clone(),
        })
    };
    let a = source(-42, 8000, vec![1000]);
    let b = source(17, 16000, vec![2000, -1000]);
    let mut b_alias = Some(b.clone());
    let mut mixer = AudioMixer::stock().unwrap();
    assert!(mixer.add_source(&a).unwrap());
    assert!(mixer.add_source(&b).unwrap());
    drop(a);
    drop(b);
    let mut outputs = Vec::new();
    for after_removal in [false, true] {
        if after_removal {
            let alias = b_alias.take().unwrap();
            assert!(mixer.remove_source(&alias).unwrap());
            assert!(!mixer.remove_source(&alias).unwrap());
            // Only the caller alias still retains the removed provider.
            assert!(
                !audit
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e, SourceEvent::Drop { .. }))
            );
            drop(alias);
            assert!(
                audit
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e, SourceEvent::Drop { id: 17 }))
            );
        }
        for index in 0..20 {
            let channels = 1 + index % 2;
            let frame = Frame::take(&mixer.mix(channels).unwrap());
            assert_eq!(
                frame.sample_rate_hz,
                if after_removal { 8000 } else { 16000 }
            );
            assert_eq!(frame.num_channels, channels);
            assert_eq!(frame.samples.len(), frame.samples_per_channel * channels);
            outputs.push(Output {
                after_removal,
                requested_channels: channels,
                frame,
            });
        }
    }
    assert!(b_alias.is_none());
    assert!(
        audit
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, SourceEvent::Drop { id: 17 }))
    );
    assert!(
        !audit
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, SourceEvent::Drop { id: -42 }))
    );
    drop(mixer);
    let events = std::mem::take(&mut *audit.lock().unwrap());
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, SourceEvent::Drop { .. }))
            .count(),
        2
    );
    let b_last = events
        .iter()
        .rposition(|e| matches!(e, SourceEvent::Audio { id: 17, .. }))
        .unwrap();
    let a_last = events
        .iter()
        .rposition(|e| matches!(e, SourceEvent::Audio { id: -42, .. }))
        .unwrap();
    assert!(
        a_last > b_last,
        "remaining source must continue supplying native input"
    );
    let stereo = &outputs[19].frame.samples;
    assert!(
        stereo
            .chunks_exact(2)
            .all(|s| s[0] > 1000 && s[1].abs() < 100),
        "both sources must contribute to the native stereo sum"
    );
    let single = &outputs[39].frame.samples;
    assert!(
        single.chunks_exact(2).all(|s| s[0] > 0 && s[0] == s[1]),
        "remaining mono source must survive as native stereo output"
    );
    Observation { outputs, events }
}

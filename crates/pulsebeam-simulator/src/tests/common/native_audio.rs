//! Native PCM playout facts, deliberately without per-block publication identity.
use anyhow::{Context, Result, ensure};
use pulsebeam_agent_native::{OutputFrame, RemoteFrame, RemoteTrack};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NativePcmLog {
    receivers: BTreeMap<(u64, u64), Receiver>,
    invalid: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq)]
struct Receiver {
    track: RemoteTrack,
    nontrivial_samples: u64,
    blocks: u64,
    last_at: Instant,
    last_signal_at: Option<Instant>,
    max_gap: Duration,
}

impl NativePcmLog {
    pub fn begin_window(&mut self) {
        self.receivers.clear();
    }

    pub fn observe(&mut self, frame: &RemoteFrame, now: Instant) {
        let OutputFrame::Pcm {
            sample_rate_hz,
            channels,
            samples,
        } = &frame.frame
        else {
            panic!("native PCM oracle requires actual decoded PCM");
        };
        self.receivers
            .retain(|(generation, _), _| *generation == frame.track.generation);
        let key = (frame.track.generation, frame.track.receiver);
        if !self.receivers.contains_key(&key) && self.receivers.len() >= 16 {
            self.invalid = Some("native PCM observation receiver budget exhausted");
            return;
        }
        let per_channel = sample_rate_hz / 100;
        if *sample_rate_hz == 0
            || sample_rate_hz % 100 != 0
            || *channels == 0
            || frame.metadata.samples_per_channel != Some(per_channel)
            || samples.len() != usize::try_from(per_channel).unwrap() * usize::from(*channels)
        {
            self.invalid = Some("native PCM is not a complete declared 10 ms block");
            return;
        }
        let receiver = self.receivers.entry(key).or_insert_with(|| Receiver {
            track: frame.track.clone(),
            nontrivial_samples: 0,
            blocks: 0,
            last_at: now,
            last_signal_at: None,
            max_gap: Duration::ZERO,
        });
        receiver.max_gap = receiver.max_gap.max(now.duration_since(receiver.last_at));
        receiver.last_at = now;
        receiver.blocks += 1;
        if samples.iter().any(|sample| sample.unsigned_abs() > 32) {
            receiver.nontrivial_samples += u64::try_from(samples.len()).unwrap();
            receiver.last_signal_at = Some(now);
        }
    }

    pub fn require(
        &self,
        generation: u64,
        min_samples: u64,
        min_receivers: usize,
        max_silence: Option<Duration>,
        now: Instant,
    ) -> Result<Vec<RemoteTrack>> {
        ensure!(
            min_samples > 0 && min_receivers > 0,
            "native PCM proof requires nontrivial samples and receivers"
        );
        ensure!(
            self.invalid.is_none(),
            "invalid native PCM observation: {:?}",
            self.invalid
        );
        let mut tracks = Vec::new();
        for ((observed_generation, _), receiver) in &self.receivers {
            if *observed_generation != generation || receiver.nontrivial_samples < min_samples {
                continue;
            }
            Self::require_fresh(receiver, now)?;
            if let Some(limit) = max_silence {
                ensure!(
                    receiver
                        .last_signal_at
                        .is_some_and(|last| now.duration_since(last) <= limit),
                    "native PCM has no recent nontrivial signal after recovery"
                );
            }
            tracing::info!(track = ?receiver.track, receiver.blocks, receiver.nontrivial_samples, "native PCM window progress without publication attribution");
            tracks.push(receiver.track.clone());
        }
        ensure!(
            tracks.len() >= min_receivers,
            "generation {generation}: expected {min_receivers} native PCM receivers with {min_samples} nontrivial samples, got {}: {:?}",
            tracks.len(),
            self.receivers
        );
        Ok(tracks)
    }

    fn require_fresh(receiver: &Receiver, now: Instant) -> Result<()> {
        ensure!(
            now.duration_since(receiver.last_at) <= Duration::from_millis(50),
            "native PCM playout is stale"
        );
        ensure!(
            receiver.max_gap <= Duration::from_millis(50),
            "native PCM playout gap {:?}",
            receiver.max_gap
        );
        Ok(())
    }

    pub fn require_quiet(
        &self,
        generation: u64,
        min_blocks: u64,
        min_receivers: usize,
        now: Instant,
    ) -> Result<Vec<RemoteTrack>> {
        self.require_silent(generation)?;
        self.require_progress(generation, min_blocks, min_receivers, now)
    }

    pub fn require_progress(
        &self,
        generation: u64,
        min_blocks: u64,
        min_receivers: usize,
        now: Instant,
    ) -> Result<Vec<RemoteTrack>> {
        ensure!(
            min_blocks > 0 && min_receivers > 0,
            "PCM progress requires actual blocks and receivers"
        );
        ensure!(
            self.invalid.is_none(),
            "invalid native PCM observation: {:?}",
            self.invalid
        );
        let mut tracks = Vec::new();
        for ((observed, _), receiver) in &self.receivers {
            if *observed != generation || receiver.blocks < min_blocks {
                continue;
            }
            Self::require_fresh(receiver, now)?;
            tracks.push(receiver.track.clone());
        }
        ensure!(
            tracks.len() >= min_receivers,
            "generation {generation}: expected {min_receivers} PCM receivers with {min_blocks} actual blocks, got {}: {:?}",
            tracks.len(),
            self.receivers
        );
        Ok(tracks)
    }

    pub fn require_silent(&self, generation: u64) -> Result<()> {
        ensure!(
            self.invalid.is_none(),
            "invalid native PCM observation: {:?}",
            self.invalid
        );
        ensure!(
            self.receivers
                .iter()
                .filter(|((observed, _), _)| *observed == generation)
                .all(|(_, receiver)| receiver.nontrivial_samples == 0),
            "unexpected native PCM signal: {:?}",
            self.receivers
        );
        Ok(())
    }
}

fn entries(report: &serde_json::Value) -> Result<Vec<&serde_json::Value>> {
    match report {
        serde_json::Value::Array(entries) => Ok(entries.iter().collect()),
        serde_json::Value::Object(entries) => Ok(entries.values().collect()),
        _ => anyhow::bail!("native SDK report has no records"),
    }
}

fn decoder_counts(report: &[u8], mid: &str) -> Result<(u64, u64, u64)> {
    let report = serde_json::from_slice(report)?;
    let records = entries(&report)?;
    let inbound = records
        .iter()
        .copied()
        .find(|entry| {
            entry["type"] == "inbound-rtp" && entry["kind"] == "audio" && entry["mid"] == mid
        })
        .with_context(|| {
            let mids = records.iter()
                .filter(|entry| entry["type"] == "inbound-rtp" && entry["kind"] == "audio")
                .map(|entry| &entry["mid"])
                .collect::<Vec<_>>();
            format!("native inbound audio report unavailable for MID {mid:?}; reported audio MIDs={mids:?}")
        })?;
    let received = inbound["totalSamplesReceived"]
        .as_u64()
        .context("native received sample count unavailable")?;
    let concealed = inbound["concealedSamples"]
        .as_u64()
        .context("native concealed sample count unavailable")?;
    let emitted = inbound["jitterBufferEmittedCount"]
        .as_u64()
        .context("native jitter-buffer emitted count unavailable")?;
    Ok((received, concealed, emitted))
}

pub fn require_decoder_report(report: &[u8], mid: &str) -> Result<()> {
    let (received, concealed, emitted) = decoder_counts(report, mid)?;
    ensure!(
        emitted > 0 && received > concealed,
        "native audio has no non-concealed decode progress: received={received}, concealed={concealed}, emitted={emitted}"
    );
    tracing::info!(
        mid,
        received,
        concealed,
        emitted,
        "actual native audio receive/decode readback"
    );
    Ok(())
}

pub fn require_decoder_progress(before: &[u8], after: &[u8], mid: &str) -> Result<()> {
    let (before_received, before_concealed, before_emitted) = decoder_counts(before, mid)?;
    let (received, concealed, emitted) = decoder_counts(after, mid)?;
    ensure!(
        received >= before_received && concealed >= before_concealed && emitted >= before_emitted,
        "native audio counters regressed within the measurement window"
    );
    ensure!(
        received - before_received > concealed - before_concealed && emitted > before_emitted,
        "native audio window has no new non-concealed decode progress: received={before_received}->{received}, concealed={before_concealed}->{concealed}, emitted={before_emitted}->{emitted}"
    );
    Ok(())
}

pub fn require_decoder_receivers(
    before: Option<&[u8]>,
    after: &[u8],
    tracks: &[RemoteTrack],
    required: usize,
) -> Result<()> {
    ensure!(required > 0, "audio decode requires actual receivers");
    let mut decoded = std::collections::BTreeSet::new();
    let mut failures = Vec::new();
    for track in tracks {
        let result = match before {
            Some(before) => require_decoder_progress(before, after, &track.mid),
            None => require_decoder_report(after, &track.mid),
        };
        match result {
            Ok(()) => {
                decoded.insert(&track.mid);
            }
            Err(error) => failures.push(format!("MID {:?}: {error:#}", track.mid)),
        }
    }
    ensure!(
        decoded.len() >= required,
        "expected {required} distinct decoded audio receivers, got {}; candidates={failures:?}",
        decoded.len()
    );
    Ok(())
}

pub fn require_no_audio(report: &[u8]) -> Result<()> {
    let report = serde_json::from_slice(report)?;
    let entries = entries(&report)?;
    ensure!(
        entries.iter().any(|entry| entry["type"] == "transport"),
        "native transport report unavailable"
    );
    for inbound in entries
        .into_iter()
        .filter(|entry| entry["type"] == "inbound-rtp" && entry["kind"] == "audio")
    {
        let received = inbound["packetsReceived"]
            .as_u64()
            .context("native audio packet count unavailable")?;
        ensure!(received == 0, "unexpected native audio ingress: {inbound}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_agent_native::FrameMetadata;

    // Synthetic values exercise oracle failures, never native media acceptance.
    fn pcm(generation: u64, audible: bool) -> RemoteFrame {
        RemoteFrame {
            track: RemoteTrack {
                generation,
                receiver: 3,
                track_id: "unit".into(),
                mid: "2".into(),
                media_index: 2,
            },
            metadata: FrameMetadata {
                samples_per_channel: Some(480),
                ..Default::default()
            },
            frame: OutputFrame::Pcm {
                sample_rate_hz: 48_000,
                channels: 1,
                samples: vec![if audible { 200 } else { 0 }; 480],
            },
        }
    }

    #[test]
    fn native_pcm_requires_audible_current_generation_complete_continuous_blocks() {
        let now = Instant::now();
        let mut log = NativePcmLog::default();
        log.observe(&pcm(1, false), now);
        assert!(log.require(1, 480, 1, None, now).is_err());
        log.observe(&pcm(1, true), now + Duration::from_millis(10));
        assert!(
            log.require(1, 480, 1, None, now + Duration::from_millis(10))
                .is_ok()
        );
        assert!(
            log.require(1, 480, 2, None, now + Duration::from_millis(10))
                .is_err()
        );
        assert!(log.require_silent(1).is_err());
        log.observe(&pcm(1, false), now + Duration::from_millis(20));
        assert!(
            log.require(1, 480, 1, None, now + Duration::from_millis(20))
                .is_ok()
        );
        assert!(
            log.require(
                1,
                480,
                1,
                Some(Duration::from_millis(5)),
                now + Duration::from_millis(20)
            )
            .is_err()
        );
        assert!(
            log.require(2, 480, 1, None, now + Duration::from_millis(20))
                .is_err()
        );
        assert!(
            log.require(1, 480, 1, None, now + Duration::from_secs(1))
                .is_err()
        );
        log.observe(&pcm(1, true), now + Duration::from_secs(1));
        assert!(
            log.require(1, 480, 1, None, now + Duration::from_secs(1))
                .is_err()
        );
        log.begin_window();
        assert!(
            log.require(1, 480, 1, None, now + Duration::from_secs(1))
                .is_err()
        );
        let mut invalid = pcm(2, true);
        invalid.metadata.samples_per_channel = None;
        log.observe(&invalid, now + Duration::from_secs(1));
        assert!(
            log.require(2, 480, 1, None, now + Duration::from_secs(1))
                .is_err()
        );
    }

    #[test]
    fn native_audio_stats_reject_missing_data_and_concealment_only() {
        let report = serde_json::json!([{"type":"inbound-rtp","kind":"audio","mid":"2","totalSamplesReceived":9600,"concealedSamples":480,"jitterBufferEmittedCount":9600}]);
        assert!(require_decoder_report(&serde_json::to_vec(&report).unwrap(), "2").is_ok());
        assert!(require_decoder_report(&serde_json::to_vec(&report).unwrap(), "1").is_err());
        for field in [
            "totalSamplesReceived",
            "concealedSamples",
            "jitterBufferEmittedCount",
        ] {
            let mut missing = report.clone();
            missing[0].as_object_mut().unwrap().remove(field);
            assert!(require_decoder_report(&serde_json::to_vec(&missing).unwrap(), "2").is_err());
        }
        let mut concealed = report;
        concealed[0]["concealedSamples"] = 9600.into();
        assert!(require_decoder_report(&serde_json::to_vec(&concealed).unwrap(), "2").is_err());
    }

    #[test]
    fn quiet_pcm_requires_actual_fresh_complete_blocks_in_the_current_generation() {
        let now = Instant::now();
        let mut log = NativePcmLog::default();
        assert!(log.require_quiet(1, 2, 1, now).is_err());
        log.observe(&pcm(1, false), now);
        assert!(log.require_quiet(1, 2, 1, now).is_err());
        log.observe(&pcm(1, false), now + Duration::from_millis(10));
        assert!(
            log.require_quiet(1, 2, 1, now + Duration::from_millis(10))
                .is_ok()
        );
        assert!(
            log.require_quiet(2, 2, 1, now + Duration::from_millis(10))
                .is_err()
        );
        assert!(
            log.require_quiet(1, 2, 2, now + Duration::from_millis(10))
                .is_err()
        );
        assert!(
            log.require_quiet(1, 2, 1, now + Duration::from_secs(1))
                .is_err()
        );
        log.observe(&pcm(1, false), now + Duration::from_secs(1));
        assert!(
            log.require_quiet(1, 2, 1, now + Duration::from_secs(1))
                .is_err()
        );
        log.begin_window();
        assert!(
            log.require_quiet(1, 1, 1, now + Duration::from_secs(1))
                .is_err()
        );
        log.observe(&pcm(1, true), now + Duration::from_secs(1));
        assert!(
            log.require_quiet(1, 1, 1, now + Duration::from_secs(1))
                .is_err()
        );
        assert!(
            log.require_progress(1, 1, 1, now + Duration::from_secs(1))
                .is_ok()
        );
        log.begin_window();
        let mut invalid = pcm(1, false);
        invalid.metadata.samples_per_channel = None;
        log.observe(&invalid, now + Duration::from_secs(1));
        assert!(
            log.require_quiet(1, 1, 1, now + Duration::from_secs(1))
                .is_err()
        );
    }

    #[test]
    fn decoder_progress_rejects_old_success_and_concealment_only_windows() {
        let before = serde_json::json!([{"type":"inbound-rtp","kind":"audio","mid":"2","totalSamplesReceived":9600,"concealedSamples":480,"jitterBufferEmittedCount":9600}]);
        let bytes = |report: &serde_json::Value| serde_json::to_vec(report).unwrap();
        assert!(require_decoder_progress(&bytes(&before), &bytes(&before), "2").is_err());
        let mut after = before.clone();
        after[0]["totalSamplesReceived"] = 10080.into();
        after[0]["jitterBufferEmittedCount"] = 10080.into();
        assert!(require_decoder_progress(&bytes(&before), &bytes(&after), "2").is_ok());
        after[0]["concealedSamples"] = 960.into();
        assert!(require_decoder_progress(&bytes(&before), &bytes(&after), "2").is_err());
        after[0]["totalSamplesReceived"] = 9000.into();
        assert!(require_decoder_progress(&bytes(&before), &bytes(&after), "2").is_err());
        assert!(require_decoder_progress(&bytes(&before), &bytes(&before), "missing").is_err());
    }

    #[test]
    fn receiver_progress_counts_distinct_active_mids_not_idle_surplus_slots() {
        let before = serde_json::json!([
            {"type":"inbound-rtp","kind":"audio","mid":"2","totalSamplesReceived":9600,"concealedSamples":480,"jitterBufferEmittedCount":9600},
            {"type":"inbound-rtp","kind":"audio","mid":"3","totalSamplesReceived":9600,"concealedSamples":480,"jitterBufferEmittedCount":9600}
        ]);
        let mut after = before.clone();
        after[0]["totalSamplesReceived"] = 10080.into();
        after[0]["jitterBufferEmittedCount"] = 10080.into();
        let before = serde_json::to_vec(&before).unwrap();
        let after = serde_json::to_vec(&after).unwrap();
        let active = pcm(1, true).track;
        let mut idle = active.clone();
        idle.mid = "3".into();
        idle.receiver += 1;
        let tracks = [active.clone(), idle.clone()];
        assert!(require_decoder_receivers(Some(&before), &after, &tracks, 1).is_ok());
        assert!(require_decoder_receivers(Some(&before), &after, &tracks, 2).is_err());
        assert!(require_decoder_receivers(Some(&before), &after, &[idle], 1).is_err());
        assert!(
            require_decoder_receivers(Some(&before), &after, &[active.clone(), active], 2).is_err()
        );
        assert!(require_decoder_receivers(Some(&before), &before, &tracks, 1).is_err());
        assert!(require_decoder_receivers(None, &before, &tracks, 2).is_ok());
    }

    #[test]
    fn no_audio_requires_actual_native_report_and_rejects_any_received_packet() {
        assert!(require_no_audio(b"[]").is_err());
        let mut report = serde_json::json!([{"type":"transport"}, {"type":"inbound-rtp","kind":"audio","packetsReceived":0}]);
        assert!(require_no_audio(&serde_json::to_vec(&report).unwrap()).is_ok());
        report[1]["packetsReceived"] = 1.into();
        assert!(require_no_audio(&serde_json::to_vec(&report).unwrap()).is_err());
        report[1].as_object_mut().unwrap().remove("packetsReceived");
        assert!(require_no_audio(&serde_json::to_vec(&report).unwrap()).is_err());
    }
}

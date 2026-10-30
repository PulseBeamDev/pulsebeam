//! Native decoded-output facts, separate from passive encoded observations.
//! Only actual I420 callbacks qualify; independent decoder results never do.
use anyhow::{Context, Result, ensure};
use pulsebeam_agent_native::{OutputFrame, RemoteFrame, RemoteTrack};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

const MAX_OBSERVED_FORMATS: usize = 16;

#[derive(Clone, Copy, Debug)]
pub enum Playback {
    /// Continuous changing video, including interior freeze detection.
    Camera,
    /// Captured desktop changes, without imposing a camera frame rate.
    DesktopUpdates,
}

#[derive(Clone, Copy, Debug)]
pub struct PlaybackExpectation {
    pub content: Playback,
    pub min_frames: u64,
    pub min_resolution: (u32, u32),
}

#[derive(Clone, Debug, Default)]
pub struct NativeDecodedVideoLog {
    receivers: BTreeMap<(u64, u64), Receiver>,
    drops: u64,
    callback_failures: Arc<AtomicU64>,
    /// Loss from independent best-effort polling, not decoded callback loss.
    polling_drops: u64,
}

#[derive(Clone, Debug, Default)]
struct Receiver {
    track: Option<RemoteTrack>,
    nontrivial: BTreeMap<(u32, u32), u64>,
    last_dimensions: Option<(u32, u32)>,
    last_received: Option<Instant>,
    last_rtp: Option<u32>,
    invalid: u64,
    format_budget_exhausted: bool,
    regressions: u64,
    window_max_gap: Duration,
    window_fingerprints: BTreeMap<(u32, u32), BTreeSet<u64>>,
}

impl NativeDecodedVideoLog {
    /// Return whether this actual output has valid, nontrivial image data.
    pub fn observe(&mut self, frame: &RemoteFrame, now: Instant) -> bool {
        let OutputFrame::I420 { frame: pixels } = &frame.frame else {
            panic!("native decoded-video oracle requires actual I420 output");
        };
        self.receivers
            .retain(|(generation, _), _| *generation == frame.track.generation);
        let receiver = self
            .receivers
            .entry((frame.track.generation, frame.track.receiver))
            .or_default();
        receiver.track = Some(frame.track.clone());
        let dimensions = (pixels.width, pixels.height);
        let w = usize::try_from(pixels.width).unwrap();
        let h = usize::try_from(pixels.height).unwrap();
        let expected = w.checked_mul(h).and_then(|y| {
            w.div_ceil(2)
                .checked_mul(h.div_ceil(2))
                .and_then(|uv| uv.checked_mul(2)?.checked_add(y))
        });
        if w == 0 || h == 0 || Some(pixels.data.len()) != expected {
            receiver.invalid += 1;
            return false;
        }
        if !receiver.nontrivial.contains_key(&dimensions)
            && receiver.nontrivial.len() >= MAX_OBSERVED_FORMATS
        {
            receiver.format_budget_exhausted = true;
            return false;
        }
        // Contrast belongs to the old animated-source witness. Source-event
        // proof instead checks uniform native pixels against supplied content.
        let luma = &pixels.data[..w * h];
        let mut minimum = u8::MAX;
        let mut maximum = u8::MIN;
        let nontrivial = luma.iter().copied().any(|value| {
            minimum = minimum.min(value);
            maximum = maximum.max(value);
            maximum.saturating_sub(minimum) > 8
        });
        if nontrivial {
            *receiver.nontrivial.entry(dimensions).or_default() += 1;
            let fingerprints = receiver.window_fingerprints.entry(dimensions).or_default();
            // Only two distinct observations are needed, never a pixel history.
            if fingerprints.len() < 2 {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                luma.hash(&mut hasher);
                fingerprints.insert(hasher.finish());
            }
        }
        if let Some(previous) = receiver.last_received {
            if let Some(gap) = now.checked_duration_since(previous) {
                receiver.window_max_gap = receiver.window_max_gap.max(gap);
            } else {
                receiver.invalid += 1;
            }
        }
        match frame.metadata.rtp_timestamp {
            Some(timestamp) => {
                if receiver.last_rtp.is_some_and(|previous| {
                    timestamp == previous || timestamp.wrapping_sub(previous) >= 0x8000_0000
                }) {
                    receiver.regressions += 1;
                }
                receiver.last_rtp = Some(timestamp);
            }
            None => receiver.invalid += 1,
        }
        receiver.last_received = Some(now);
        receiver.last_dimensions = Some(dimensions);
        nontrivial
    }

    /// Cumulative validity for source-correlated progress, without inventing a
    /// motion/frame-rate requirement for genuine still or 1fps source content.
    pub fn require_valid_receiver(&self, generation: u64, receiver: u64) -> Result<()> {
        self.require_callback_collector()?;
        ensure!(
            self.drops == 0,
            "native decoded observer lost {} frames",
            self.drops
        );
        let receiver = self
            .receivers
            .get(&(generation, receiver))
            .context("native decoded receiver unavailable")?;
        ensure!(
            !receiver.format_budget_exhausted,
            "native decoded format budget exhausted"
        );
        ensure!(
            receiver.invalid == 0,
            "native decoded malformed/missing-metadata frames: {}",
            receiver.invalid
        );
        ensure!(
            receiver.regressions == 0,
            "native decoded RTP regressions: {}",
            receiver.regressions
        );
        Ok(())
    }

    /// Receiver-specific playback after an explicit window boundary. Source
    /// identity is established separately from actual paired encoded/I420 data.
    pub fn require_playback(
        &self,
        baseline: &Self,
        track: &RemoteTrack,
        expected: PlaybackExpectation,
        now: Instant,
    ) -> Result<()> {
        ensure!(
            expected.min_frames > 0,
            "playback requires positive progress"
        );
        self.require_valid_receiver(track.generation, track.receiver)?;
        let key = (track.generation, track.receiver);
        let receiver = self
            .receivers
            .get(&key)
            .context("playback receiver missing")?;
        ensure!(
            receiver
                .track
                .as_ref()
                .is_some_and(|actual| actual.track_id == track.track_id
                    && actual.mid == track.mid
                    && actual.media_index == track.media_index),
            "native playback track identity changed"
        );
        let dimensions = receiver.last_dimensions.context("no decoded dimensions")?;
        ensure!(
            dimensions.0 >= expected.min_resolution.0 && dimensions.1 >= expected.min_resolution.1,
            "native playback dimensions {dimensions:?}, need {:?}",
            expected.min_resolution
        );
        let count = receiver.nontrivial.get(&dimensions).copied().unwrap_or(0);
        let previous = baseline
            .receivers
            .get(&key)
            .and_then(|prior| prior.nontrivial.get(&dimensions))
            .copied()
            .unwrap_or(0);
        let fresh = count
            .checked_sub(previous)
            .context("native playback count reset")?;
        ensure!(
            fresh >= expected.min_frames,
            "only {fresh} new native pictures, need {}",
            expected.min_frames
        );
        ensure!(
            receiver
                .window_fingerprints
                .get(&dimensions)
                .is_some_and(|seen| seen.len() >= 2),
            "native playback pixels did not change"
        );
        let max_age = match expected.content {
            Playback::Camera => Duration::from_millis(250),
            // Reuse the existing visible-output deadline for sparse desktop
            // updates rather than treating camera cadence as a screen contract.
            Playback::DesktopUpdates => super::harness::MAX_TIME_TO_FIRST_FRAME,
        };
        let last = receiver.last_received.context("no decoded arrival")?;
        ensure!(
            now.checked_duration_since(last)
                .is_some_and(|age| age <= max_age),
            "native playback is stale or in the future"
        );
        if matches!(expected.content, Playback::Camera) {
            // Unlike an implicit initial baseline, an explicit playback window
            // must reject interior freezes even on a newly created receiver.
            ensure!(
                receiver.window_max_gap <= Duration::from_millis(500),
                "native camera continuity gap {:?}",
                receiver.window_max_gap
            );
        }
        // A still desktop need not repaint at camera cadence. New genuine
        // pictures and source-authenticated updates are still mandatory.
        Ok(())
    }

    pub fn require_recent(&self, track: &RemoteTrack, now: Instant) -> Result<()> {
        self.require_valid_receiver(track.generation, track.receiver)?;
        let receiver = self
            .receivers
            .get(&(track.generation, track.receiver))
            .context("native receiver missing")?;
        ensure!(
            receiver
                .last_received
                .and_then(|last| now.checked_duration_since(last))
                .is_some_and(|age| age <= Duration::from_millis(250)),
            "native output is stale or in the future"
        );
        Ok(())
    }

    pub fn callback_failure_counter(&self) -> Arc<AtomicU64> {
        self.callback_failures.clone()
    }

    pub(super) fn require_callback_collector(&self) -> Result<()> {
        let failures = self.callback_failures.load(Ordering::Acquire);
        ensure!(
            failures == 0,
            "native callback collector lost/failed {failures} observations"
        );
        Ok(())
    }

    pub fn has_observations(&self) -> bool {
        !self.receivers.is_empty()
    }

    pub fn dropped(&mut self, skipped: u64) {
        self.drops += skipped;
    }

    pub fn polling_dropped(&mut self, skipped: u64) {
        self.polling_drops += skipped;
    }

    pub fn malformed_callback(&mut self, track: &RemoteTrack) {
        self.receivers
            .retain(|(generation, _), _| *generation == track.generation);
        let receiver = self
            .receivers
            .entry((track.generation, track.receiver))
            .or_default();
        receiver.track = Some(track.clone());
        receiver.invalid += 1;
    }

    pub fn require(
        &self,
        baseline: &Self,
        generation: u64,
        dimensions: (u32, u32),
        min_frames: u64,
        now: Instant,
    ) -> Result<RemoteTrack> {
        ensure!(min_frames > 0, "native decoded proof requires fresh frames");
        self.require_callback_collector()?;
        ensure!(
            self.drops == 0,
            "native decoded observation lost {} frames",
            self.drops
        );
        let mut failures = Vec::new();
        for (key, receiver) in &self.receivers {
            if key.0 != generation {
                continue;
            }
            let count = receiver.nontrivial.get(&dimensions).copied().unwrap_or(0);
            // These are local observed-callback counters, not missing SDK stats.
            let previous = baseline
                .receivers
                .get(key)
                .and_then(|r| r.nontrivial.get(&dimensions))
                .copied()
                .unwrap_or(0);
            let result = (|| -> Result<()> {
                ensure!(
                    !receiver.format_budget_exhausted,
                    "native decoded oracle format budget exhausted"
                );
                let fresh = count
                    .checked_sub(previous)
                    .context("native decoded count reset")?;
                ensure!(
                    fresh >= min_frames,
                    "only {fresh} fresh nontrivial {dimensions:?} frames, need {min_frames}"
                );
                ensure!(
                    receiver
                        .window_fingerprints
                        .get(&dimensions)
                        .is_some_and(|seen| seen.len() >= 2),
                    "native pixels did not change at the requested rung"
                );
                ensure!(
                    receiver.last_dimensions == Some(dimensions),
                    "last native dimensions {:?}, expected {dimensions:?}",
                    receiver.last_dimensions
                );
                // Malformed buffers/missing native metadata are contract failures,
                // not playout warmup. A checkpoint must never erase them.
                ensure!(
                    receiver.invalid == 0,
                    "{} malformed/native-metadata-missing frames",
                    receiver.invalid
                );
                ensure!(
                    receiver.regressions == 0,
                    "{} native decoded timestamp regressions",
                    receiver.regressions
                );
                let last = receiver
                    .last_received
                    .context("no native decoded arrival")?;
                ensure!(
                    now.checked_duration_since(last)
                        .is_some_and(|age| age <= Duration::from_millis(250)),
                    "native decoded output is stale"
                );
                if baseline.receivers.contains_key(key) {
                    ensure!(
                        receiver.window_max_gap <= Duration::from_millis(500),
                        "native decoded continuity gap {:?}",
                        receiver.window_max_gap
                    );
                }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    return receiver
                        .track
                        .clone()
                        .context("native decoded track unavailable");
                }
                Err(error) => failures.push(format!("receiver {key:?}: {error}")),
            }
        }
        anyhow::bail!(
            "no current-generation native decoded witness: {}",
            failures.join("; ")
        )
    }

    /// Recovery starts after an intentionally absent publication. Keep counts
    /// and cumulative validity, but measure cadence between resumed callbacks.
    /// Continuous rung switching must use `checkpoint` instead.
    pub fn checkpoint_after_pause(&mut self) -> Self {
        let baseline = self.checkpoint();
        for receiver in self.receivers.values_mut() {
            receiver.last_received = None;
        }
        baseline
    }

    pub fn checkpoint(&mut self) -> Self {
        for receiver in self.receivers.values_mut() {
            receiver.window_max_gap = Duration::ZERO;
            receiver.window_fingerprints.clear();
        }
        self.clone()
    }
}

pub fn require_decoder_report(
    report: &[u8],
    mid: &str,
    dimensions: (u32, u32),
    min_frames: u64,
) -> Result<u64> {
    let report: serde_json::Value = serde_json::from_slice(report)?;
    let entries: Vec<_> = match &report {
        serde_json::Value::Array(entries) => entries.iter().collect(),
        serde_json::Value::Object(entries) => entries.values().collect(),
        _ => anyhow::bail!("native SDK report has no records"),
    };
    let inbound = entries
        .into_iter()
        .find(|entry| {
            entry["type"] == "inbound-rtp" && entry["kind"] == "video" && entry["mid"] == mid
        })
        .context("native inbound video decoder report unavailable")?;
    let decoded = inbound["framesDecoded"]
        .as_u64()
        .context("native framesDecoded unavailable")?;
    ensure!(
        decoded >= min_frames,
        "native SDK decoded only {decoded} frames"
    );
    let width = inbound["frameWidth"]
        .as_u64()
        .context("native decoded width unavailable")?;
    let height = inbound["frameHeight"]
        .as_u64()
        .context("native decoded height unavailable")?;
    ensure!(
        (width, height) == (u64::from(dimensions.0), u64::from(dimensions.1)),
        "native SDK decoded dimensions {width}x{height}, expected {dimensions:?}"
    );
    let implementation = inbound["decoderImplementation"]
        .as_str()
        .filter(|value| !value.is_empty())
        .context("native decoder implementation unavailable")?;
    tracing::info!(
        mid,
        decoded,
        width,
        height,
        implementation,
        "actual native video decoder readback"
    );
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_agent_native::{FrameMetadata, I420Frame};

    // Synthetic owned buffers test the oracle, never an RTC media path.
    fn observation(generation: u64, width: u32, height: u32, timestamp: u32) -> RemoteFrame {
        RemoteFrame {
            track: RemoteTrack {
                generation,
                receiver: 7,
                track_id: "unit".into(),
                mid: "1".into(),
                media_index: 1,
            },
            metadata: FrameMetadata {
                rtp_timestamp: Some(timestamp),
                ..Default::default()
            },
            frame: OutputFrame::I420 {
                frame: I420Frame {
                    width,
                    height,
                    timestamp_us: 0,
                    data: (0..usize::try_from(width).unwrap()
                        * usize::try_from(height).unwrap()
                        * 3
                        / 2)
                        .map(|i| {
                            if i % 2 == 0 {
                                16 + (timestamp % 100) as u8
                            } else {
                                200
                            }
                        })
                        .collect(),
                    rotation_degrees: 0,
                },
            },
        }
    }

    #[test]
    fn playback_is_receiver_scoped_and_rejects_stale_or_frozen_camera_output() {
        let now = Instant::now();
        let mut log = NativeDecodedVideoLog::default();
        let first = observation(1, 4, 4, 1);
        log.observe(&first, now);
        let baseline = log.checkpoint();
        log.observe(&observation(1, 4, 4, 2), now + Duration::from_millis(100));
        log.observe(&observation(1, 4, 4, 3), now + Duration::from_millis(200));
        let camera = PlaybackExpectation {
            content: Playback::Camera,
            min_frames: 2,
            min_resolution: (4, 4),
        };
        let desktop = PlaybackExpectation {
            content: Playback::DesktopUpdates,
            ..camera
        };
        let track = &first.track;
        assert!(
            log.require_playback(&baseline, track, camera, now + Duration::from_millis(200))
                .is_ok()
        );
        assert!(
            log.require_playback(&baseline, track, camera, now + Duration::from_millis(800))
                .is_err()
        );
        assert!(
            log.require_playback(&baseline, track, desktop, now + Duration::from_millis(800))
                .is_ok()
        );
        assert!(
            log.require_playback(&baseline, track, desktop, now + Duration::from_secs(6))
                .is_err()
        );
        let mut wrong = track.clone();
        wrong.receiver += 1;
        assert!(
            log.require_playback(&baseline, &wrong, camera, now)
                .is_err()
        );
        wrong = track.clone();
        wrong.generation += 1;
        assert!(
            log.require_playback(&baseline, &wrong, camera, now)
                .is_err()
        );
        wrong = track.clone();
        wrong.mid = "wrong".into();
        assert!(
            log.require_playback(&baseline, &wrong, camera, now)
                .is_err()
        );
        log.observe(&observation(1, 4, 4, 4), now + Duration::from_millis(1820));
        log.observe(&observation(1, 4, 4, 5), now + Duration::from_millis(1840));
        assert!(
            log.require_playback(&baseline, track, camera, now + Duration::from_millis(1840))
                .is_err(),
            "a real 1.62s freeze cannot be reset away by fresh output"
        );
        let resumed = log.checkpoint_after_pause();
        log.observe(&observation(1, 4, 4, 6), now + Duration::from_secs(5));
        log.observe(&observation(1, 4, 4, 7), now + Duration::from_millis(5100));
        assert!(
            log.require_playback(&resumed, track, camera, now + Duration::from_millis(5100))
                .is_ok()
        );
        assert!(
            log.require_playback(&log, track, camera, now + Duration::from_millis(5100))
                .is_err(),
            "old cumulative counts are not interval progress"
        );
        log.dropped(1);
        let resumed = log.checkpoint_after_pause();
        assert!(
            log.require_playback(&resumed, track, camera, now + Duration::from_millis(5100))
                .is_err(),
            "a pause checkpoint cannot erase collector loss"
        );
    }

    #[test]
    fn decoded_witness_rejects_stale_rung_generation_and_placeholder_pixels() {
        let now = Instant::now();
        let mut log = NativeDecodedVideoLog::default();
        let empty = log.clone();
        log.observe(&observation(1, 4, 4, 10), now);
        log.observe(&observation(1, 4, 4, 11), now);
        assert!(log.require(&empty, 1, (4, 4), 1, now).is_ok());
        assert!(log.require(&empty, 2, (4, 4), 1, now).is_err());
        assert!(log.require(&empty, 1, (8, 8), 1, now).is_err());
        assert!(
            log.require(&empty, 1, (4, 4), 1, now + Duration::from_secs(1))
                .is_err()
        );
        let checkpoint = log.checkpoint();
        assert!(log.require(&checkpoint, 1, (4, 4), 1, now).is_err());
        let mut constant = observation(1, 4, 4, 20);
        if let OutputFrame::I420 { frame } = &mut constant.frame {
            frame.data.fill(0);
        }
        log.observe(&constant, now);
        assert!(log.require(&checkpoint, 1, (4, 4), 1, now).is_err());
        log.observe(&observation(1, 8, 8, 30), now);
        log.observe(&observation(1, 8, 8, 31), now);
        assert!(log.require(&checkpoint, 1, (4, 4), 1, now).is_err());
        assert!(log.require(&checkpoint, 1, (8, 8), 1, now).is_ok());
        log.dropped(1);
        assert!(log.require(&checkpoint, 1, (8, 8), 1, now).is_err());
    }

    #[test]
    fn decoded_witness_rejects_regressions_and_interior_freezes() {
        let now = Instant::now();
        let mut log = NativeDecodedVideoLog::default();
        log.observe(&observation(1, 4, 4, 100), now);
        let baseline = log.checkpoint();
        log.observe(&observation(1, 4, 4, 200), now + Duration::from_secs(1));
        log.observe(&observation(1, 4, 4, 201), now + Duration::from_secs(1));
        assert!(
            log.require(&baseline, 1, (4, 4), 1, now + Duration::from_secs(1))
                .is_err()
        );
        let baseline = log.checkpoint();
        log.observe(&observation(1, 4, 4, 150), now + Duration::from_secs(1));
        log.observe(&observation(1, 4, 4, 151), now + Duration::from_secs(1));
        assert!(
            log.require(&baseline, 1, (4, 4), 1, now + Duration::from_secs(1))
                .is_err()
        );
    }

    #[test]
    fn paused_lifecycle_window_keeps_freshness_cadence_and_cumulative_failures() {
        let start = Instant::now();
        let mut log = NativeDecodedVideoLog::default();
        log.observe(&observation(1, 4, 4, 10), start);
        let baseline = log.checkpoint_after_pause();
        let resumed = start + Duration::from_secs(3);
        assert!(log.require(&baseline, 1, (4, 4), 2, resumed).is_err());
        log.observe(&observation(1, 4, 4, 20), resumed);
        assert!(log.require(&baseline, 1, (4, 4), 2, resumed).is_err());
        let next = resumed + Duration::from_millis(20);
        log.observe(&observation(1, 4, 4, 21), next);
        assert!(log.require(&baseline, 1, (4, 4), 2, next).is_ok());
        assert!(
            log.require(&baseline, 1, (4, 4), 2, next + Duration::from_millis(251))
                .is_err()
        );
        let frozen = next + Duration::from_millis(501);
        log.observe(&observation(1, 4, 4, 22), frozen);
        assert!(
            log.require(&baseline, 1, (4, 4), 2, frozen)
                .unwrap_err()
                .to_string()
                .contains("continuity gap")
        );
        // A pause cannot erase a native timestamp regression or observer loss.
        log.observe(&observation(1, 4, 4, 19), frozen);
        let baseline = log.checkpoint_after_pause();
        log.observe(&observation(1, 4, 4, 30), frozen);
        log.observe(&observation(1, 4, 4, 31), frozen);
        assert!(
            log.require(&baseline, 1, (4, 4), 2, frozen)
                .unwrap_err()
                .to_string()
                .contains("timestamp regressions")
        );
        log.dropped(1);
        let baseline = log.checkpoint_after_pause();
        assert!(
            log.require(&baseline, 1, (4, 4), 2, frozen)
                .unwrap_err()
                .to_string()
                .contains("observation lost")
        );
    }

    #[test]
    fn decoded_oracle_bounds_formats_and_never_erases_malformed_observations() {
        let now = Instant::now();
        let mut log = NativeDecodedVideoLog::default();
        for index in 0..=MAX_OBSERVED_FORMATS {
            let index = u32::try_from(index).unwrap();
            log.observe(&observation(1, 4 + 2 * index, 4, index), now);
        }
        let receiver = &log.receivers[&(1, 7)];
        assert_eq!(receiver.nontrivial.len(), MAX_OBSERVED_FORMATS);
        assert_eq!(receiver.window_fingerprints.len(), MAX_OBSERVED_FORMATS);
        assert!(receiver.format_budget_exhausted);

        let mut log = NativeDecodedVideoLog::default();
        let mut malformed = observation(1, 4, 4, 1);
        if let OutputFrame::I420 { frame } = &mut malformed.frame {
            frame.data.truncate(1);
        }
        log.observe(&malformed, now);
        let baseline = log.checkpoint();
        log.observe(&observation(1, 4, 4, 20), now);
        log.observe(&observation(1, 4, 4, 21), now);
        assert!(
            log.require(&baseline, 1, (4, 4), 1, now)
                .unwrap_err()
                .to_string()
                .contains("malformed/native-metadata-missing")
        );
    }

    #[test]
    fn complete_callback_validity_is_separate_from_polling_retention_loss() {
        let frame = observation(1, 16, 16, 1);
        let mut log = NativeDecodedVideoLog::default();
        assert!(log.observe(&frame, Instant::now()));
        log.polling_dropped(17);
        assert_eq!(log.polling_drops, 17);
        assert!(log.require_valid_receiver(1, 7).is_ok());
        log.dropped(1);
        assert!(log.require_valid_receiver(1, 7).is_err());
    }

    #[test]
    fn failed_callback_collection_and_backwards_clock_cannot_qualify() {
        let frame = observation(1, 16, 16, 1);
        let now = Instant::now();
        let mut log = NativeDecodedVideoLog::default();
        assert!(log.observe(&frame, now));
        let failures = log.callback_failure_counter();
        let _baseline = log.checkpoint();
        failures.fetch_add(1, Ordering::Release);
        assert!(log.require_valid_receiver(1, 7).is_err());

        let mut backwards = NativeDecodedVideoLog::default();
        assert!(backwards.observe(&frame, now + Duration::from_millis(1)));
        assert!(backwards.observe(&observation(1, 16, 16, 2), now));
        assert!(backwards.require_valid_receiver(1, 7).is_err());
    }

    #[test]
    fn failed_callback_conversion_cannot_be_erased_by_a_checkpoint() {
        let frame = observation(1, 16, 16, 1);
        let mut log = NativeDecodedVideoLog::default();
        assert!(log.observe(&frame, Instant::now()));
        log.malformed_callback(&frame.track);
        log.checkpoint();
        assert!(log.require_valid_receiver(1, 7).is_err());
    }

    #[test]
    fn native_decoder_readback_requires_actual_available_decode_fields() {
        let report = serde_json::json!([{"type":"inbound-rtp","kind":"video","mid":"1","framesDecoded":31,"frameWidth":1280,"frameHeight":720,"decoderImplementation":"OpenH264"}]);
        assert!(
            require_decoder_report(&serde_json::to_vec(&report).unwrap(), "1", (1280, 720), 30)
                .is_ok()
        );
        for field in [
            "framesDecoded",
            "frameWidth",
            "frameHeight",
            "decoderImplementation",
        ] {
            let mut missing = report.clone();
            missing[0].as_object_mut().unwrap().remove(field);
            assert!(
                require_decoder_report(
                    &serde_json::to_vec(&missing).unwrap(),
                    "1",
                    (1280, 720),
                    30
                )
                .is_err()
            );
        }
        assert!(
            require_decoder_report(&serde_json::to_vec(&report).unwrap(), "2", (1280, 720), 30)
                .is_err()
        );
        assert!(
            require_decoder_report(&serde_json::to_vec(&report).unwrap(), "1", (320, 180), 30)
                .is_err()
        );
    }
}

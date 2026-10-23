//! Native decoded-output oracle. Encoded interception is a different boundary:
//! it consumes frames before libwebrtc's native decoder and playout buffering.
use anyhow::{Context, Result, ensure};
use pulsebeam_agent_native::{OutputFrame, RemoteFrame, RemoteTrack};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

const MAX_OBSERVED_FORMATS: usize = 16;

#[derive(Clone, Debug, Default)]
pub struct NativeDecodedVideoLog {
    receivers: BTreeMap<(u64, u64), Receiver>,
    drops: u64,
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
    pub fn observe(&mut self, frame: &RemoteFrame, now: Instant) {
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
            return;
        }
        if !receiver.nontrivial.contains_key(&dimensions)
            && receiver.nontrivial.len() >= MAX_OBSERVED_FORMATS
        {
            receiver.format_budget_exhausted = true;
            return;
        }
        // All-zero/constant placeholder buffers cannot establish media delivery.
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
            receiver.window_max_gap = receiver.window_max_gap.max(now.duration_since(previous));
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
    }

    pub fn has_observations(&self) -> bool {
        !self.receivers.is_empty()
    }

    pub fn dropped(&mut self, skipped: u64) {
        self.drops += skipped;
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
                    now.duration_since(last) <= Duration::from_millis(250),
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

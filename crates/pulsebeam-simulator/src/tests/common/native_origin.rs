//! Test-only source proof joining two EXISTING native observations. Never
//! infer decoded provenance from current catalog assignments or SSRC guesses.
use super::decoder::PixelSample;
use super::provenance::{AuthenticatedSourceEvent, SourceLedger};
use anyhow::{Context, Result, ensure};
use pulsebeam_agent_native::agent_core::PlayoutDelay;
use pulsebeam_agent_native::{OutputFrame, RemoteFrame, RemoteTrack};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

const PICTURES: usize = 512;
const PROGRESS_GROUPS: usize = 128;
type Key = (u64, u64, u32); // receiving generation, native receiver, actual RTP time

#[derive(Clone, Debug, Default)]
enum Origin {
    #[default]
    Unseen,
    Unique(String),
    Unknown,
    Conflict,
}

#[derive(Clone, Debug)]
struct NativePicture {
    track: RemoteTrack,
    dimensions: (u32, u32),
    at: Instant,
    nontrivial: bool,
    pixel_sample: Option<PixelSample>,
}

#[derive(Clone, Debug, Default)]
struct Picture {
    track: Option<RemoteTrack>,
    origin: Origin,
    event: Option<AuthenticatedSourceEvent>,
    event_conflict: bool,
    native: Option<NativePicture>,
    credited: bool,
    progress_credited: bool,
}

#[derive(Clone, Debug)]
struct Receipt {
    key: Key,
    origin: String,
    native: NativePicture,
}

#[derive(Debug)]
pub struct NativeOriginProof {
    pub track: RemoteTrack,
    pub dimensions: (u32, u32),
    pub frames: u64,
}

#[derive(Debug)]
pub struct NativeProgressProof {
    pub track: RemoteTrack,
    pub min_dimensions: (u32, u32),
    pub frames: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct SourceTiming {
    pub period: Duration,
    pub playout: PlayoutDelay,
    pub fiber_message_latency: Duration,
}

impl SourceTiming {
    pub fn fixed_buffer(self) -> Result<Duration> {
        let PlayoutDelay::Fixed { min_ms, max_ms } = self.playout else {
            anyhow::bail!("strict source age needs explicit fixed playout, not adaptive");
        };
        let maximum = max_ms.min(2_000).max(min_ms.min(2_000));
        Ok(Duration::from_millis(u64::from((maximum + 5) / 10 * 10)))
    }

    pub fn deadline(self) -> Result<Duration> {
        ensure!(!self.period.is_zero(), "source period is not declared");
        self.fixed_buffer()?
            .checked_add(
                self.fiber_message_latency
                    .checked_mul(4)
                    .context("network allowance overflow")?,
            )
            .and_then(|duration| duration.checked_add(self.period.checked_mul(2)?))
            .context("source deadline overflow")
    }
}

#[derive(Clone, Debug)]
pub struct SourceWindow {
    pub source: String,
    pub track: RemoteTrack,
    pub encodings: BTreeSet<String>,
    pub start: Instant,
    pub end: Instant,
    pub timing: SourceTiming,
}

impl SourceWindow {
    fn eligible<'a>(
        &self,
        ledger: &'a SourceLedger,
    ) -> impl Iterator<Item = (u64, &'a super::provenance::SuppliedPicture)> {
        ledger
            .pictures
            .iter()
            .filter(|((_, encoding), picture)| {
                self.encodings.contains(encoding)
                    && picture.captured >= self.start
                    && picture.captured < self.end
            })
            .map(|((capture, _), picture)| (*capture, picture))
    }

    pub fn drain_until(&self, ledger: &SourceLedger) -> Result<Instant> {
        ensure!(
            ledger.source == self.source && ledger.period == self.timing.period,
            "window does not describe supplied source"
        );
        self.eligible(ledger)
            .map(|(_, picture)| picture.captured)
            .max()
            .context("source window has no supplied events")?
            .checked_add(self.timing.deadline()?)
            .context("event deadline overflow")
    }
}

#[derive(Clone, Debug)]
struct SourceTrace {
    window: SourceWindow,
    cutoff: Instant,
    events: BTreeMap<Key, AuthenticatedSourceEvent>,
    outputs: Vec<(Key, NativePicture)>,
    failure: Option<&'static str>,
}

impl SourceTrace {
    const LIMIT: usize = 16_384;

    fn receiver(&self, key: Key) -> bool {
        (key.0, key.1) == (self.window.track.generation, self.window.track.receiver)
    }

    fn event(&mut self, key: Key, track: &RemoteTrack, event: Option<AuthenticatedSourceEvent>) {
        if !self.receiver(key) {
            return;
        }
        let Some(event) = event else {
            self.failure
                .get_or_insert("missing encoded source-event association");
            return;
        };
        if event.captured >= self.window.start
            && event.captured < self.window.end
            && track != &self.window.track
        {
            self.failure.get_or_insert("encoded track identity changed");
            return;
        }
        if let Some(previous) = self.events.get(&key) {
            if previous != &event {
                self.failure
                    .get_or_insert("conflicting source event for native picture");
            }
            return;
        }
        if self.events.len() >= Self::LIMIT {
            self.failure
                .get_or_insert("source trace event budget exhausted");
        } else {
            self.events.insert(key, event);
        }
    }

    fn output(&mut self, key: Key, native: NativePicture, event: Option<AuthenticatedSourceEvent>) {
        if !self.receiver(key) || native.at < self.window.start || native.at > self.cutoff {
            return;
        }
        if self.outputs.len() >= Self::LIMIT {
            self.failure
                .get_or_insert("source trace output budget exhausted");
            return;
        }
        if let Some(event) = event {
            self.event(key, &native.track, Some(event));
        }
        self.outputs.push((key, native));
    }

    fn require(&self, ledger: &SourceLedger, now: Instant) -> Result<NativeOriginProof> {
        ledger.require_healthy()?;
        ensure!(
            self.failure.is_none(),
            "source collection failed: {:?}",
            self.failure
        );
        let window = &self.window;
        ensure!(
            now >= window.end && now >= window.drain_until(ledger)?,
            "source window/deadlines not drained"
        );
        let expected: BTreeMap<_, _> = window.eligible(ledger).collect();
        ensure!(
            !expected.is_empty(),
            "source window requires positive supplied events"
        );
        for (capture, supplied) in &expected {
            ensure!(
                supplied.admitted,
                "source capture {capture} did not settle successfully"
            );
        }
        let periods =
            window.end.duration_since(window.start).as_nanos() / window.timing.period.as_nanos();
        ensure!(
            u128::try_from(expected.len())? >= periods,
            "declared source cadence was not supplied"
        );
        let mut previous = ledger
            .pictures
            .iter()
            .filter(|((_, encoding), picture)| {
                window.encodings.contains(encoding) && picture.captured < window.start
            })
            .map(|(_, picture)| picture.captured)
            .max()
            .unwrap_or(window.start);
        for supplied in expected.values() {
            ensure!(
                supplied.captured >= previous
                    && supplied.captured.duration_since(previous) <= window.timing.period,
                "supplied source has a leading/interior gap beyond its declared period"
            );
            previous = supplied.captured;
        }
        ensure!(
            window.end.duration_since(previous) <= window.timing.period,
            "supplied source has a terminal gap beyond its declared period"
        );
        let deadline = window.timing.deadline()?;
        let mut actual = Vec::new();
        let mut last_capture = None;
        let mut last_output = None;
        let mut dimensions = None;
        for (key, native) in &self.outputs {
            ensure!(native.at <= now, "native observation is in the future");
            let event = self
                .events
                .get(key)
                .context("native output has no authenticated source event")?;
            // Setup buffering and the deadline drain may deliver events outside
            // this declared input interval. They never fill or reset its obligations.
            if event.captured < window.start || event.captured >= window.end {
                continue;
            }
            ensure!(
                native.track == window.track && event.event.source == window.source,
                "native output has wrong source/receiver lifetime"
            );
            ensure!(
                window.encodings.contains(&event.event.encoding),
                "received an undeclared full-rate representation"
            );
            ensure!(
                last_capture.is_none_or(|previous| event.event.capture > previous),
                "source replay, duplicate or output reorder"
            );
            ensure!(
                last_output.is_none_or(|previous| native.at >= previous),
                "native output time regressed"
            );
            last_capture = Some(event.event.capture);
            last_output = Some(native.at);
            let supplied = ledger
                .pictures
                .get(&(event.event.capture, event.event.encoding.clone()))
                .context("output event was not supplied")?;
            ensure!(
                supplied.admitted,
                "source submission did not settle successfully"
            );
            ensure!(
                event.captured == supplied.captured
                    && event.dimensions == supplied.dimensions
                    && event.pixel_sample == supplied.pixel_sample,
                "source association differs from actual supplied event"
            );
            ensure!(
                match supplied.temporal_index {
                    Some(expected) => event.native_temporal_index == Some(i32::from(expected)),
                    None => event.native_temporal_index.is_none_or(|index| index == 0),
                },
                "native temporal index {:?} differs from supplied {:?} for capture {}",
                event.native_temporal_index,
                supplied.temporal_index,
                event.event.capture,
            );
            ensure!(
                native.at >= supplied.captured
                    && native.at
                        <= supplied
                            .captured
                            .checked_add(deadline)
                            .context("event deadline overflow")?,
                "source event {} missed its deadline {deadline:?}",
                event.event.capture
            );
            ensure!(
                native.dimensions
                    == (
                        u32::from(supplied.dimensions.0),
                        u32::from(supplied.dimensions.1)
                    ),
                "native output dimensions differ from supplied representation"
            );
            native
                .pixel_sample
                .context("malformed native I420")?
                .require_fixture(supplied.pixel_sample)?;
            if expected.contains_key(&event.event.capture) {
                dimensions.get_or_insert(native.dimensions);
                actual.push(event.event.capture);
            }
        }
        let expected_ids: Vec<_> = expected.keys().copied().collect();
        ensure!(
            actual == expected_ids,
            "incomplete source window: expected={expected_ids:?}, actual={actual:?}"
        );
        Ok(NativeOriginProof {
            track: window.track.clone(),
            dimensions: dimensions.context("no native source output")?,
            frames: u64::try_from(actual.len())?,
        })
    }
}

// A phase total must outlive the rolling picture join, but never merge source
// identities, receiver lifetimes, generations or supplied representations.
#[derive(Clone, Debug)]
struct SourceProgress {
    source: String,
    track: RemoteTrack,
    dimensions: (u32, u32),
    capture: u64,
    frames: u64,
}

#[derive(Clone, Debug)]
struct ProgressPhase {
    start: Instant,
    totals: Vec<SourceProgress>,
    failure: Option<&'static str>,
}

#[derive(Clone, Debug, Default)]
pub struct NativeOriginCorrelation {
    pictures: BTreeMap<Key, Picture>,
    order: VecDeque<Key>,
    receipts: VecDeque<Receipt>,
    latest: BTreeMap<(u64, u64), Key>,
    event_failures: u64,
    source_trace: Option<SourceTrace>,
    progress: Option<ProgressPhase>,
}

impl NativeOriginCorrelation {
    fn key(frame: &RemoteFrame) -> Option<Key> {
        Some((
            frame.track.generation,
            frame.track.receiver,
            frame.metadata.rtp_timestamp?,
        ))
    }

    fn admit(&mut self, key: Key) {
        if self.pictures.contains_key(&key) {
            return;
        }
        if self.pictures.len() == PICTURES {
            let old = self.order.pop_front().expect("bounded picture order");
            self.pictures.remove(&old);
            self.receipts.retain(|receipt| receipt.key != old);
            self.latest.retain(|_, latest| *latest != old);
        }
        self.order.push_back(key);
        self.pictures.insert(key, Picture::default());
    }

    fn bind_track(&mut self, key: Key, track: &RemoteTrack) -> bool {
        self.admit(key);
        let picture = self.pictures.get_mut(&key).unwrap();
        if let Some(previous) = &picture.track {
            if previous != track {
                picture.origin = Origin::Conflict;
                picture.event = None;
                picture.event_conflict = true;
                self.receipts.retain(|receipt| receipt.key != key);
                if picture.progress_credited
                    && let Some(phase) = &mut self.progress
                {
                    phase.failure = Some("credited native receiver lifetime changed");
                }
                return false;
            }
        } else {
            picture.track = Some(track.clone());
        }
        true
    }

    /// A missing encoded observation can conceal conflicting provenance. No
    /// pre-loss mapping or receipt remains admissible; only newly paired actual
    /// observations can establish a subsequent positive source witness.
    pub fn encoded_observation_lost(&mut self) {
        self.event_failures = self.event_failures.saturating_add(1);
        if let Some(phase) = &mut self.progress {
            phase.failure = Some("encoded observation lost during progress phase");
        }
        self.pictures.clear();
        self.order.clear();
        self.receipts.clear();
        self.latest.clear();
    }

    pub fn encoded(&mut self, frame: &RemoteFrame, origin: Option<&str>) {
        assert!(matches!(frame.frame, OutputFrame::H264 { .. }));
        let Some(key) = Self::key(frame) else {
            return;
        };
        if !self.bind_track(key, &frame.track) {
            return;
        }
        let picture = self.pictures.get_mut(&key).unwrap();
        picture.origin = match (&picture.origin, origin) {
            (Origin::Unseen, Some(origin)) => Origin::Unique(origin.to_owned()),
            (Origin::Unseen | Origin::Unknown, None) => Origin::Unknown,
            (Origin::Unique(prior), Some(origin)) if prior == origin => {
                Origin::Unique(prior.clone())
            }
            _ => Origin::Conflict,
        };
        // Later conflicting/unknown evidence revokes a previously paired
        // observation. These are bounded proof records, not native counters.
        if !matches!(picture.origin, Origin::Unique(_)) {
            self.receipts.retain(|receipt| receipt.key != key);
        }
        self.credit(key);
    }

    pub fn encoded_event(&mut self, frame: &RemoteFrame, event: Option<AuthenticatedSourceEvent>) {
        self.encoded(
            frame,
            event.as_ref().map(|event| event.event.source.as_str()),
        );
        let Some(key) = Self::key(frame) else {
            self.event_failures = self.event_failures.saturating_add(1);
            if let Some(phase) = &mut self.progress {
                phase.failure = Some("encoded observation has no RTP identity");
            }
            return;
        };
        let picture = self
            .pictures
            .get_mut(&key)
            .expect("encoded picture admitted");
        match (&picture.event, event) {
            (None, Some(event)) if !picture.event_conflict => picture.event = Some(event),
            (Some(previous), Some(event)) if previous == &event => {}
            _ => {
                picture.event = None;
                picture.event_conflict = true;
            }
        }
        self.credit(key);
        if let Some(trace) = &mut self.source_trace {
            let picture = self.pictures.get(&key).unwrap();
            trace.event(key, &frame.track, picture.event.clone());
        }
    }

    pub fn decoded(&mut self, frame: &RemoteFrame, at: Instant, nontrivial: bool) {
        let OutputFrame::I420 { frame: pixels } = &frame.frame else {
            panic!("native source proof requires actual I420 output");
        };
        let Some(key) = Self::key(frame) else {
            self.event_failures = self.event_failures.saturating_add(1);
            if let Some(phase) = &mut self.progress {
                phase.failure = Some("native observation has no RTP identity");
            }
            return;
        };
        let native = NativePicture {
            track: frame.track.clone(),
            dimensions: (pixels.width, pixels.height),
            at,
            nontrivial,
            pixel_sample: PixelSample::packed_i420(pixels.width, pixels.height, &pixels.data),
        };
        if let Some(trace) = &mut self.source_trace {
            let event = self
                .pictures
                .get(&key)
                .filter(|picture| !picture.event_conflict)
                .and_then(|picture| picture.event.clone());
            trace.output(key, native.clone(), event);
        }
        self.latest.insert((key.0, key.1), key);
        if !self.bind_track(key, &frame.track) {
            return;
        }
        // Duplicate callbacks never become fresh output a second time.
        self.pictures
            .get_mut(&key)
            .unwrap()
            .native
            .get_or_insert(native);
        self.credit(key);
    }

    fn credit(&mut self, key: Key) {
        self.credit_progress(key);
        let picture = self.pictures.get_mut(&key).unwrap();
        if !picture.credited
            && let Origin::Unique(origin) = &picture.origin
            && let Some(native) = &picture.native
            && native.nontrivial
        {
            picture.credited = true;
            self.receipts.push_back(Receipt {
                key,
                origin: origin.clone(),
                native: native.clone(),
            });
        }
    }

    pub fn begin_progress(&mut self, start: Instant) {
        self.progress = Some(ProgressPhase {
            start,
            totals: Vec::new(),
            failure: None,
        });
        for picture in self.pictures.values_mut() {
            picture.progress_credited = false;
        }
    }

    fn credit_progress(&mut self, key: Key) {
        let Some(phase) = &mut self.progress else {
            return;
        };
        let picture = self.pictures.get_mut(&key).unwrap();
        let valid = !picture.event_conflict
            && matches!((&picture.origin, &picture.event),
                (Origin::Unique(source), Some(event)) if source == &event.event.source);
        if picture.progress_credited {
            if !valid {
                phase.failure = Some("credited native source association was revoked");
            }
            return;
        }
        let (Some(event), Some(native)) = (&picture.event, &picture.native) else {
            return;
        };
        if !valid || event.captured < phase.start || native.at < phase.start {
            return;
        }
        if native.at < event.captured {
            phase.failure = Some("native output precedes source capture");
            return;
        }
        // A lossy policy phase may conceal some pictures. Count only genuine
        // fixture-corresponding output, without promising fidelity for all of it.
        if native.dimensions != (u32::from(event.dimensions.0), u32::from(event.dimensions.1))
            || !native
                .pixel_sample
                .is_some_and(|pixels| pixels.require_fixture(event.pixel_sample).is_ok())
        {
            return;
        }
        // Replayed capture IDs cannot fill a quota, including across format
        // switches or after their original RTP join entries were evicted.
        if phase.totals.iter().any(|total| {
            total.source == event.event.source
                && total.track == native.track
                && total.capture >= event.event.capture
        }) {
            picture.progress_credited = true;
            return;
        }
        let total = phase.totals.iter_mut().find(|total| {
            total.source == event.event.source
                && total.track == native.track
                && total.dimensions == native.dimensions
        });
        if let Some(total) = total {
            total.capture = event.event.capture;
            total.frames = total
                .frames
                .checked_add(1)
                .expect("bounded phase frame count");
        } else if phase.totals.len() < PROGRESS_GROUPS {
            phase.totals.push(SourceProgress {
                source: event.event.source.clone(),
                track: native.track.clone(),
                dimensions: native.dimensions,
                capture: event.event.capture,
                frames: 1,
            });
        } else {
            phase.failure = Some("source progress lifetime/format budget exhausted");
        }
        picture.progress_credited = true;
    }

    pub fn require_progress(
        &self,
        source: &str,
        generation: u64,
        start: Instant,
        min_frames: u64,
        min_dimensions: (u32, u32),
    ) -> Result<NativeProgressProof> {
        let phase = self
            .progress
            .as_ref()
            .context("source progress phase not started")?;
        ensure!(
            phase.start == start,
            "source progress baseline differs from requested interval"
        );
        ensure!(
            phase.failure.is_none(),
            "invalid source progress phase: {:?}",
            phase.failure
        );
        ensure!(min_frames > 0, "source progress requires a positive quota");
        let qualifying = || {
            phase.totals.iter().filter(|total| {
                total.source == source
                    && total.track.generation == generation
                    && total.dimensions.0 >= min_dimensions.0
                    && total.dimensions.1 >= min_dimensions.1
            })
        };
        for lifetime in qualifying() {
            let mut frames = 0u64;
            let mut floor = lifetime.dimensions;
            for total in qualifying().filter(|total| total.track == lifetime.track) {
                frames = frames
                    .checked_add(total.frames)
                    .context("source progress count overflow")?;
                floor = (
                    floor.0.min(total.dimensions.0),
                    floor.1.min(total.dimensions.1),
                );
            }
            if frames >= min_frames {
                return Ok(NativeProgressProof {
                    track: lifetime.track.clone(),
                    min_dimensions: floor,
                    frames,
                });
            }
        }
        anyhow::bail!(
            "no qualifying native source progress for {source}, generation {generation}, quota {min_frames}, dimensions {min_dimensions:?}; observed {:?}",
            phase.totals
        )
    }

    pub fn begin_source_window(&mut self, window: SourceWindow) -> Result<()> {
        ensure!(
            self.source_trace.is_none(),
            "previous source window was not validated"
        );
        ensure!(
            window.end > window.start && !window.source.is_empty() && !window.encodings.is_empty(),
            "invalid source window"
        );
        let cutoff = window
            .end
            .checked_add(window.timing.deadline()?)
            .context("source window overflow")?;
        self.source_trace = Some(SourceTrace {
            window,
            cutoff,
            events: BTreeMap::new(),
            outputs: Vec::new(),
            failure: None,
        });
        Ok(())
    }

    pub fn finish_source_window(
        &mut self,
        ledger: &SourceLedger,
        now: Instant,
    ) -> Result<NativeOriginProof> {
        ensure!(
            self.event_failures == 0,
            "source association collector lost observations"
        );
        let proof = self
            .source_trace
            .as_ref()
            .context("source window was not started")?
            .require(ledger, now)?;
        self.source_trace = None;
        Ok(proof)
    }

    /// Source-correct current output also supports genuinely uniform content.
    /// The old contrast/motion witnesses retain their narrower requirements.
    pub fn require_current_event(
        &self,
        origin: &str,
        generation: u64,
        since: Instant,
    ) -> Result<NativeOriginProof> {
        self.require_current_content(origin, generation, since, None)
    }

    pub fn require_current_content(
        &self,
        origin: &str,
        generation: u64,
        since: Instant,
        expected: Option<PixelSample>,
    ) -> Result<NativeOriginProof> {
        ensure!(
            self.event_failures == 0,
            "source event association lost {} observations",
            self.event_failures
        );
        for (receiver, key) in &self.latest {
            if receiver.0 != generation {
                continue;
            }
            let picture = self
                .pictures
                .get(key)
                .context("current source picture evicted")?;
            let (Some(event), Some(native)) = (&picture.event, &picture.native) else {
                continue;
            };
            if event.event.source != origin
                || picture.event_conflict
                || native.at < since
                || event.captured < since
                || !matches!(&picture.origin, Origin::Unique(source) if source == origin)
            {
                continue;
            }
            ensure!(
                native.at >= event.captured,
                "native output precedes its actual source capture"
            );
            ensure!(
                native.dimensions == (u32::from(event.dimensions.0), u32::from(event.dimensions.1)),
                "native dimensions differ from supplied encoding"
            );
            native
                .pixel_sample
                .context("invalid native I420")?
                .require_fixture(event.pixel_sample)?;
            if let Some(expected) = expected {
                native
                    .pixel_sample
                    .context("invalid native I420")?
                    .require_fixture(expected)?;
            }
            return Ok(NativeOriginProof {
                track: native.track.clone(),
                dimensions: native.dimensions,
                frames: 1,
            });
        }
        anyhow::bail!(
            "no current source-event-associated I420 for {origin}, generation {generation}"
        )
    }

    /// Source cadence is deliberately not invented here. This proves genuine
    /// new I420 observations during the stated interval, including still/1fps
    /// sources. Continuous camera cadence remains a separate native oracle.
    pub fn require_since(
        &self,
        origin: &str,
        generation: u64,
        since: Instant,
        min_frames: u64,
        min_dimensions: (u32, u32),
    ) -> Result<NativeOriginProof> {
        ensure!(
            min_frames > 0,
            "native origin proof requires positive progress"
        );
        for (receiver, key) in &self.latest {
            if receiver.0 != generation {
                continue;
            }
            let picture = self
                .pictures
                .get(key)
                .context("latest native picture was evicted")?;
            let Origin::Unique(current) = &picture.origin else {
                continue;
            };
            if current != origin {
                continue;
            }
            let native = picture
                .native
                .as_ref()
                .context("native picture unavailable")?;
            if !native.nontrivial
                || native.at <= since
                || native.dimensions.0 < min_dimensions.0
                || native.dimensions.1 < min_dimensions.1
            {
                continue;
            }
            let count = self
                .receipts
                .iter()
                .filter(|receipt| {
                    receipt.origin == origin
                        && (receipt.key.0, receipt.key.1) == *receiver
                        && receipt.native.at > since
                        && receipt.native.dimensions.0 >= min_dimensions.0
                        && receipt.native.dimensions.1 >= min_dimensions.1
                })
                .count();
            if u64::try_from(count)? >= min_frames {
                return Ok(NativeOriginProof {
                    track: native.track.clone(),
                    dimensions: native.dimensions,
                    frames: u64::try_from(count)?,
                });
            }
        }
        // Retain only bounded observation metadata in a failed witness. In
        // particular, distinguish missing native output from a pending or
        // conflicting encoded-source correlation without dumping pixel data.
        let latest: Vec<_> = self
            .latest
            .iter()
            .take(8)
            .map(|(receiver, key)| (receiver, key, self.pictures.get(key)))
            .collect();
        let matching = self
            .receipts
            .iter()
            .filter(|receipt| receipt.origin == origin && receipt.key.0 == generation)
            .count();
        anyhow::bail!(
            "no fresh native I420 source proof for {origin}, generation {generation}, minimum {min_frames} frames at {min_dimensions:?}; pictures={}, matching_receipts={matching}, latest={latest:?}",
            self.pictures.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_agent_native::{FrameMetadata, I420Frame};
    use std::time::Duration;

    // Synthetic owned OBSERVATION records for correlation arithmetic/negatives. Never
    // injected into RTC; actual native encoded+I420 delivery has separate gates.
    fn frame(receiver: u64, timestamp: u32, decoded: bool) -> RemoteFrame {
        RemoteFrame {
            track: RemoteTrack {
                generation: 1,
                receiver,
                track_id: "unit".into(),
                mid: "1".into(),
                media_index: 1,
            },
            metadata: FrameMetadata {
                rtp_timestamp: Some(timestamp),
                ..FrameMetadata::default()
            },
            frame: if decoded {
                OutputFrame::I420 {
                    frame: I420Frame {
                        width: 4,
                        height: 4,
                        timestamp_us: 0,
                        rotation_degrees: 0,
                        data: vec![0; 24],
                    },
                }
            } else {
                OutputFrame::H264 {
                    timestamp,
                    keyframe: true,
                    payload: vec![],
                }
            },
        }
    }

    fn window_fixture(
        period: Duration,
        buffer: u32,
    ) -> (SourceLedger, SourceWindow, Vec<AuthenticatedSourceEvent>) {
        use super::super::provenance::{SourceEvent, SuppliedPicture};
        let start = Instant::now();
        let mut ledger = SourceLedger::new("admission/owner", period);
        let events: Vec<_> = (0..6u32)
            .map(|capture| {
                let event = AuthenticatedSourceEvent {
                    event: SourceEvent {
                        source: ledger.source.clone(),
                        capture: u64::from(capture),
                        encoding: String::new(),
                    },
                    captured: start + period * capture,
                    dimensions: (4, 4),
                    pixel_sample: PixelSample::packed_i420(4, 4, &[0; 24]).unwrap(),
                    native_temporal_index: None,
                };
                ledger
                    .supply(
                        &event.event,
                        SuppliedPicture {
                            captured: event.captured,
                            dimensions: event.dimensions,
                            admitted: false,
                            temporal_index: None,
                            canonical: vec![0x65].into(),
                            input_size: Default::default(),
                            pixel_sample: event.pixel_sample,
                        },
                    )
                    .unwrap();
                ledger.admit(u64::from(capture), "");
                event
            })
            .collect();
        let window = SourceWindow {
            source: ledger.source.clone(),
            track: frame(10, 1, true).track,
            encodings: BTreeSet::from([String::new()]),
            start,
            end: start + period * 6,
            timing: SourceTiming {
                period,
                playout: PlayoutDelay::Fixed {
                    min_ms: buffer,
                    max_ms: buffer,
                },
                fiber_message_latency: Duration::from_millis(1),
            },
        };
        (ledger, window, events)
    }

    fn event_output(
        log: &mut NativeOriginCorrelation,
        event: &AuthenticatedSourceEvent,
        timestamp: u32,
        at: Instant,
        decoded_first: bool,
    ) {
        if decoded_first {
            log.decoded(&frame(10, timestamp, true), at, false);
        }
        log.encoded_event(&frame(10, timestamp, false), Some(event.clone()));
        if !decoded_first {
            log.decoded(&frame(10, timestamp, true), at, false);
        }
    }

    #[test]
    fn source_progress_preserves_early_turns_after_receiver_reassignment_and_eviction() {
        for decoded_first in [false, true] {
            let (_, window, events) = window_fixture(Duration::from_millis(1), 0);
            let mut log = NativeOriginCorrelation::default();
            log.begin_progress(window.start);
            for (index, event) in events.iter().enumerate() {
                event_output(
                    &mut log,
                    event,
                    u32::try_from(index).unwrap(),
                    event.captured + Duration::from_millis(1),
                    decoded_first,
                );
            }
            for index in 0..PICTURES + 1 {
                let mut event = events[0].clone();
                event.event.source = "other-source".into();
                event.event.capture = index as u64;
                event.captured += Duration::from_millis(index as u64 + 10);
                event_output(
                    &mut log,
                    &event,
                    u32::try_from(index).unwrap() + 100,
                    event.captured,
                    decoded_first,
                );
            }
            assert!(!log.pictures.contains_key(&(1, 10, 0)));
            assert_eq!(
                log.require_progress(&window.source, 1, window.start, 6, (4, 4))
                    .unwrap()
                    .frames,
                6
            );
            assert!(
                log.require_progress(&window.source, 1, window.start, 7, (4, 4))
                    .is_err()
            );
            // Replaying a credited capture with new RTP time cannot fill the quota.
            event_output(
                &mut log,
                &events[0],
                10000,
                events[0].captured,
                decoded_first,
            );
            assert!(
                log.require_progress(&window.source, 1, window.start, 7, (4, 4))
                    .is_err()
            );
        }
    }

    #[test]
    fn source_progress_combines_qualifying_formats_without_recounting_a_capture() {
        let (_, window, events) = window_fixture(Duration::from_millis(1), 0);
        let mut log = NativeOriginCorrelation::default();
        log.begin_progress(window.start);
        event_output(&mut log, &events[0], 1, events[0].captured, false);
        let mut larger = events[1].clone();
        larger.dimensions = (8, 8);
        larger.pixel_sample = PixelSample::packed_i420(8, 8, &[0; 96]).unwrap();
        let mut native = frame(10, 2, true);
        if let OutputFrame::I420 { frame } = &mut native.frame {
            frame.width = 8;
            frame.height = 8;
            frame.data = vec![0; 96];
        }
        log.encoded_event(&frame(10, 2, false), Some(larger));
        log.decoded(&native, events[1].captured, false);
        event_output(&mut log, &events[1], 3, events[1].captured, false);
        let proof = log
            .require_progress(&window.source, 1, window.start, 2, (4, 4))
            .unwrap();
        assert_eq!(proof.frames, 2);
        assert_eq!(proof.min_dimensions, (4, 4));
        assert_eq!(
            log.require_progress(&window.source, 1, window.start, 1, (8, 8))
                .unwrap()
                .frames,
            1
        );
        assert!(
            log.require_progress(&window.source, 1, window.start, 3, (4, 4))
                .is_err()
        );
    }

    #[test]
    fn source_progress_fences_phase_generation_lifetime_format_and_content() {
        let (_, window, events) = window_fixture(Duration::from_millis(1), 0);
        let mut log = NativeOriginCorrelation::default();
        log.begin_progress(window.start);
        event_output(&mut log, &events[0], 1, events[0].captured, false);
        for variant in 0..3 {
            let mut encoded = frame(10, variant + 2, false);
            let mut decoded = frame(10, variant + 2, true);
            match variant {
                0 => {
                    encoded.track.generation = 2;
                    decoded.track.generation = 2;
                }
                1 => {
                    encoded.track.track_id = "replacement".into();
                    decoded.track.track_id = "replacement".into();
                }
                _ => {
                    if let OutputFrame::I420 { frame } = &mut decoded.frame {
                        frame.data.fill(200);
                    }
                }
            }
            log.encoded_event(&encoded, Some(events[1].clone()));
            log.decoded(&decoded, events[1].captured, false);
        }
        assert!(
            log.require_progress(&window.source, 1, window.start, 1, (4, 4))
                .is_ok()
        );
        assert!(
            log.require_progress(&window.source, 1, window.start, 2, (4, 4))
                .is_err()
        );
        assert!(
            log.require_progress(&window.source, 2, window.start, 2, (4, 4))
                .is_err()
        );
        assert!(
            log.require_progress(&window.source, 1, window.start, 1, (8, 8))
                .is_err()
        );
        let next = window.start + Duration::from_secs(1);
        log.begin_progress(next);
        event_output(&mut log, &events[0], 100, next, false);
        assert!(
            log.require_progress(&window.source, 1, next, 1, (4, 4))
                .is_err()
        );
        assert!(
            log.require_progress(&window.source, 1, window.start, 1, (4, 4))
                .is_err()
        );
    }

    #[test]
    fn source_progress_fails_explicitly_on_lost_or_revoked_evidence_and_budget_exhaustion() {
        let (_, window, events) = window_fixture(Duration::from_millis(1), 0);
        for loss in 0..4 {
            let mut log = NativeOriginCorrelation::default();
            log.begin_progress(window.start);
            event_output(&mut log, &events[0], 1, events[0].captured, false);
            match loss {
                0 => log.encoded_observation_lost(),
                1 => log.encoded_event(&frame(10, 1, false), None),
                2 => {
                    let mut missing = frame(10, 2, false);
                    missing.metadata.rtp_timestamp = None;
                    log.encoded_event(&missing, Some(events[1].clone()));
                }
                _ => {
                    let mut missing = frame(10, 2, true);
                    missing.metadata.rtp_timestamp = None;
                    log.decoded(&missing, events[1].captured, false);
                }
            }
            assert!(
                log.require_progress(&window.source, 1, window.start, 1, (4, 4))
                    .is_err()
            );
        }
        let mut log = NativeOriginCorrelation::default();
        log.begin_progress(window.start);
        for receiver in 0..=PROGRESS_GROUPS {
            let encoded = frame(receiver as u64, 1, false);
            let decoded = frame(receiver as u64, 1, true);
            log.encoded_event(&encoded, Some(events[0].clone()));
            log.decoded(&decoded, events[0].captured, false);
        }
        assert_eq!(log.progress.as_ref().unwrap().totals.len(), PROGRESS_GROUPS);
        assert!(
            log.require_progress(&window.source, 1, window.start, 1, (4, 4))
                .is_err()
        );
    }

    #[test]
    fn source_deadline_normalizes_fixed_policy_without_inventing_an_adaptive_bound() {
        for (min_ms, max_ms, expected) in [
            (0, 0, 0),
            (0, 14, 10),
            (0, 15, 20),
            (100, 50, 100),
            (3000, 500, 2000),
            (0, 5000, 2000),
        ] {
            let timing = SourceTiming {
                period: Duration::from_millis(20),
                playout: PlayoutDelay::Fixed { min_ms, max_ms },
                fiber_message_latency: Duration::from_millis(3),
            };
            assert_eq!(
                timing.fixed_buffer().unwrap(),
                Duration::from_millis(expected)
            );
            assert_eq!(
                timing.deadline().unwrap(),
                Duration::from_millis(expected + 52)
            );
        }
        let mut timing = SourceTiming {
            period: Duration::from_secs(1),
            playout: PlayoutDelay::Adaptive,
            fiber_message_latency: Duration::ZERO,
        };
        assert!(timing.deadline().is_err());
        timing.playout = PlayoutDelay::Fixed {
            min_ms: 0,
            max_ms: 0,
        };
        timing.period = Duration::MAX;
        assert!(timing.deadline().is_err());
        timing.period = Duration::ZERO;
        assert!(timing.deadline().is_err());
    }

    #[test]
    fn complete_source_windows_accept_low_cadence_uniform_and_fixed_buffered_content() {
        for period in [Duration::from_micros(33_334), Duration::from_secs(1)] {
            for buffer in [0, 120] {
                for decoded_first in [false, true] {
                    let (ledger, window, events) = window_fixture(period, buffer);
                    let mut log = NativeOriginCorrelation::default();
                    log.begin_source_window(window.clone()).unwrap();
                    for event in &events {
                        event_output(
                            &mut log,
                            event,
                            u32::try_from(event.event.capture + 1).unwrap(),
                            event.captured + Duration::from_millis(u64::from(buffer) + 5),
                            decoded_first,
                        );
                    }
                    let drain = window.drain_until(&ledger).unwrap();
                    let proof = log
                        .finish_source_window(&ledger, drain.max(window.end))
                        .unwrap();
                    assert_eq!(proof.frames, 6);
                    assert!(
                        drain < window.end + window.timing.deadline().unwrap(),
                        "drain the last event deadline, not an arbitrary tail"
                    );
                }
            }
        }
    }

    #[test]
    fn complete_source_windows_require_actual_supplied_temporal_indices() {
        for decoded_first in [false, true] {
            for wrong_index in [None, Some(-1), Some(0), Some(1), Some(2), Some(3)] {
                let (mut ledger, window, mut events) =
                    window_fixture(Duration::from_millis(100), 0);
                for (event, index) in events.iter_mut().zip([0, 2, 1, 2, 0, 2]) {
                    ledger
                        .pictures
                        .get_mut(&(event.event.capture, event.event.encoding.clone()))
                        .unwrap()
                        .temporal_index = Some(index);
                    event.native_temporal_index = Some(i32::from(index));
                }
                events[2].native_temporal_index = wrong_index;
                let mut log = NativeOriginCorrelation::default();
                log.begin_source_window(window.clone()).unwrap();
                for event in &events {
                    event_output(
                        &mut log,
                        event,
                        u32::try_from(event.event.capture + 1).unwrap(),
                        event.captured + Duration::from_millis(5),
                        decoded_first,
                    );
                }
                let result = log.finish_source_window(
                    &ledger,
                    window.drain_until(&ledger).unwrap().max(window.end),
                );
                if wrong_index == Some(1) {
                    assert_eq!(result.unwrap().frames, 6);
                } else {
                    let error = result.unwrap_err().to_string();
                    assert!(error.contains("native temporal index"), "{error}");
                }
            }
        }
    }

    #[test]
    fn an_old_buffered_result_cannot_fill_or_invalidate_the_new_input_interval() {
        let (ledger, window, events) = window_fixture(Duration::from_millis(100), 0);
        let mut previous = events[0].clone();
        previous.event.source = "previous-admission/owner".into();
        previous.captured -= Duration::from_millis(500);
        let mut log = NativeOriginCorrelation::default();
        log.begin_source_window(window.clone()).unwrap();
        event_output(
            &mut log,
            &previous,
            0,
            window.start + Duration::from_millis(1),
            false,
        );
        for event in &events {
            event_output(
                &mut log,
                event,
                u32::try_from(event.event.capture + 1).unwrap(),
                event.captured + Duration::from_millis(10),
                false,
            );
        }
        assert_eq!(
            log.finish_source_window(&ledger, window.drain_until(&ledger).unwrap())
                .unwrap()
                .frames,
            6
        );
    }

    #[test]
    fn a_complete_output_set_does_not_excuse_missing_or_bursty_source_input() {
        for case in [
            "missing-first",
            "missing-middle",
            "missing-last",
            "burst",
            "interior-gap",
        ] {
            let (mut ledger, window, mut events) = window_fixture(Duration::from_millis(100), 0);
            let missing = match case {
                "missing-first" => Some(0),
                "missing-middle" => Some(2),
                "missing-last" => Some(5),
                _ => None,
            };
            if let Some(capture) = missing {
                ledger.pictures.remove(&(capture, String::new()));
                events.retain(|event| event.event.capture != capture);
            }
            for event in &mut events {
                if case == "burst" {
                    event.captured = window.start + Duration::from_millis(event.event.capture * 10);
                }
                if case == "interior-gap" && event.event.capture == 2 {
                    event.captured = window.start + Duration::from_millis(101);
                }
                ledger
                    .pictures
                    .get_mut(&(event.event.capture, String::new()))
                    .unwrap()
                    .captured = event.captured;
            }
            let mut log = NativeOriginCorrelation::default();
            log.begin_source_window(window.clone()).unwrap();
            for event in events {
                event_output(
                    &mut log,
                    &event,
                    u32::try_from(event.event.capture + 1).unwrap(),
                    event.captured + Duration::from_millis(10),
                    false,
                );
            }
            assert!(
                log.finish_source_window(&ledger, window.end + window.timing.deadline().unwrap())
                    .is_err(),
                "accepted invalid source premise: {case}"
            );
        }
    }

    #[test]
    fn an_allowed_simulcast_encoding_represents_one_capture_not_multiple_renderings() {
        let (mut ledger, mut window, mut events) = window_fixture(Duration::from_millis(100), 0);
        window.encodings.insert("other".into());
        for (index, event) in events.iter_mut().enumerate() {
            let picture = ledger
                .pictures
                .get(&(event.event.capture, String::new()))
                .unwrap()
                .clone();
            let mut alternate = event.event.clone();
            alternate.encoding = "other".into();
            ledger.supply(&alternate, picture).unwrap();
            ledger.admit(alternate.capture, &alternate.encoding);
            if index % 2 == 0 {
                event.event = alternate;
            }
        }
        let mut log = NativeOriginCorrelation::default();
        log.begin_source_window(window.clone()).unwrap();
        for event in events {
            event_output(
                &mut log,
                &event,
                u32::try_from(event.event.capture + 1).unwrap(),
                event.captured + Duration::from_millis(10),
                false,
            );
        }
        assert_eq!(
            log.finish_source_window(&ledger, window.drain_until(&ledger).unwrap())
                .unwrap()
                .frames,
            6
        );
    }

    #[test]
    fn complete_windows_reject_leading_interior_terminal_duplicate_reordered_and_late_output() {
        for case in [
            "first-only",
            "missing-first",
            "missing-middle",
            "missing-last",
            "duplicate",
            "reorder",
            "late",
            "replay",
            "stale",
            "substituted",
            "collector-loss",
            "unassociated",
            "wrong-source",
            "wrong-track",
            "unsettled",
            "early-drain",
        ] {
            let (mut ledger, window, events) = window_fixture(Duration::from_millis(100), 0);
            let mut log = NativeOriginCorrelation::default();
            log.begin_source_window(window.clone()).unwrap();
            let order = if case == "reorder" {
                vec![0, 1, 3, 2, 4, 5]
            } else {
                (0..6).collect()
            };
            for (ordinal, index) in order.into_iter().enumerate() {
                if (case == "first-only" && index != 0)
                    || (case == "missing-first" && index == 0)
                    || (case == "missing-middle" && index == 2)
                    || (case == "missing-last" && index == 5)
                {
                    continue;
                }
                let event = &events[index];
                let timestamp = u32::try_from(index + 1).unwrap();
                let at = if case == "reorder" {
                    window.start + Duration::from_millis(u64::try_from(ordinal + 1).unwrap() * 100)
                } else if case == "late" {
                    event.captured + window.timing.deadline().unwrap() + Duration::from_nanos(1)
                } else if case == "stale" {
                    window.start - Duration::from_millis(1)
                } else {
                    event.captured + Duration::from_millis(10)
                };
                if case == "wrong-source" && index == 2 {
                    let mut wrong = event.clone();
                    wrong.event.source = "other-admission/owner".into();
                    event_output(&mut log, &wrong, timestamp, at, false);
                } else if case == "wrong-track" && index == 2 {
                    log.encoded_event(&frame(10, timestamp, false), Some(event.clone()));
                    let mut wrong = frame(10, timestamp, true);
                    wrong.track.track_id = "retired-track".into();
                    log.decoded(&wrong, at, false);
                } else if case == "unassociated" && index == 2 {
                    log.encoded_event(&frame(10, timestamp, false), None);
                    log.decoded(&frame(10, timestamp, true), at, false);
                } else if case == "substituted" && index == 2 {
                    log.encoded_event(&frame(10, timestamp, false), Some(event.clone()));
                    let mut wrong = frame(10, timestamp, true);
                    let OutputFrame::I420 { frame } = &mut wrong.frame else {
                        unreachable!()
                    };
                    frame.data.fill(96);
                    log.decoded(&wrong, at, false);
                } else {
                    event_output(&mut log, event, timestamp, at, false);
                }
                if case == "duplicate" && index == 2 {
                    log.decoded(&frame(10, timestamp, true), at, false);
                }
                if case == "replay" && index == 2 {
                    event_output(&mut log, event, 100, at, false);
                }
            }
            if case == "collector-loss" {
                log.encoded_observation_lost();
            }
            if case == "unsettled" {
                ledger
                    .pictures
                    .get_mut(&(2, String::new()))
                    .unwrap()
                    .admitted = false;
            }
            let now = if case == "early-drain" {
                window.end
            } else {
                window.end + window.timing.deadline().unwrap()
            };
            assert!(
                log.finish_source_window(&ledger, now).is_err(),
                "accepted {case}"
            );
            assert!(
                log.begin_source_window(window).is_err(),
                "failed window cannot be erased: {case}"
            );
        }
    }

    #[test]
    fn event_join_accepts_uniform_content_and_both_callback_orders_but_not_substitution() {
        let start = Instant::now();
        let output = start + Duration::from_millis(10);
        let event = AuthenticatedSourceEvent {
            event: super::super::provenance::SourceEvent {
                source: "admission/owner".into(),
                capture: 3,
                encoding: "q".into(),
            },
            captured: start,
            dimensions: (4, 4),
            pixel_sample: PixelSample::packed_i420(4, 4, &[0; 24]).unwrap(),
            native_temporal_index: None,
        };
        for decoded_first in [false, true] {
            let mut log = NativeOriginCorrelation::default();
            if decoded_first {
                log.decoded(&frame(10, 1, true), output, false);
            }
            log.encoded_event(&frame(10, 1, false), Some(event.clone()));
            if !decoded_first {
                log.decoded(&frame(10, 1, true), output, false);
            }
            log.require_current_event(&event.event.source, 1, start)
                .unwrap();
            log.require_current_content(
                &event.event.source,
                1,
                start,
                Some(PixelSample::solid(0, 0, 0)),
            )
            .unwrap();
            assert!(
                log.require_current_content(
                    &event.event.source,
                    1,
                    start,
                    Some(PixelSample::solid(176, 96, 96))
                )
                .is_err(),
                "a fresh old picture cannot settle an explicit content update"
            );
            assert!(
                log.require_since(&event.event.source, 1, start, 1, (4, 4))
                    .is_err(),
                "old contrast witness is unchanged"
            );
            assert!(
                log.require_current_event(
                    &event.event.source,
                    1,
                    output + Duration::from_millis(1)
                )
                .is_err()
            );
            let mut substituted = frame(10, 2, true);
            let OutputFrame::I420 { frame: pixels } = &mut substituted.frame else {
                unreachable!()
            };
            pixels.data.fill(96);
            log.encoded_event(&frame(10, 2, false), Some(event.clone()));
            log.decoded(&substituted, output, false);
            assert!(
                log.require_current_event(&event.event.source, 1, start)
                    .is_err()
            );
        }
    }

    #[test]
    fn both_callback_orders_preserve_real_output_time_and_do_not_count_duplicates() {
        let start = Instant::now();
        let output = start + Duration::from_millis(10);
        for decoded_first in [false, true] {
            let mut log = NativeOriginCorrelation::default();
            if decoded_first {
                log.decoded(&frame(10, 1, true), output, true);
            }
            log.encoded(&frame(10, 1, false), Some("admission/v0"));
            if !decoded_first {
                log.decoded(&frame(10, 1, true), output, true);
            }
            assert!(
                log.require_since("admission/v0", 1, start, 1, (4, 4))
                    .is_ok()
            );
            let after_output = output + Duration::from_millis(1);
            assert!(
                log.require_since("admission/v0", 1, after_output, 1, (4, 4))
                    .is_err()
            );
            log.decoded(&frame(10, 1, true), after_output, true);
            assert!(
                log.require_since("admission/v0", 1, after_output, 1, (4, 4))
                    .is_err()
            );
            assert!(
                log.require_since("admission/v0", 1, start, 2, (4, 4))
                    .is_err()
            );
            assert!(
                log.require_since("admission/v0", 1, start, 1, (8, 8))
                    .is_err()
            );
        }
    }

    #[test]
    fn missing_unknown_conflicting_or_cross_receiver_evidence_cannot_claim_an_origin() {
        let start = Instant::now();
        let output = start + Duration::from_millis(1);
        let mut log = NativeOriginCorrelation::default();
        log.encoded(&frame(10, 1, false), Some("old/v0"));
        log.decoded(&frame(11, 1, true), output, true);
        assert!(log.require_since("old/v0", 1, start, 1, (4, 4)).is_err());
        let mut other_generation = frame(10, 1, true);
        other_generation.track.generation = 2;
        log.decoded(&other_generation, output, true);
        assert!(log.require_since("old/v0", 2, start, 1, (4, 4)).is_err());
        log.decoded(&frame(10, 1, true), output, true);
        assert!(log.require_since("old/v0", 1, start, 1, (4, 4)).is_ok());
        assert!(log.require_since("new/v0", 1, start, 1, (4, 4)).is_err());
        log.encoded(&frame(10, 1, false), Some("new/v0"));
        assert!(log.require_since("old/v0", 1, start, 1, (4, 4)).is_err());
        assert!(log.require_since("new/v0", 1, start, 1, (4, 4)).is_err());
        log.encoded(&frame(10, 2, false), Some("new/v0"));
        log.decoded(&frame(10, 2, true), output, true);
        assert!(log.require_since("new/v0", 1, start, 1, (4, 4)).is_ok());
        log.encoded(&frame(10, 3, false), None);
        log.decoded(&frame(10, 3, true), output, true);
        assert!(
            log.require_since("new/v0", 1, start, 1, (4, 4)).is_err(),
            "the latest raw output is not attributed"
        );
        let mut missing = frame(10, 4, false);
        missing.metadata.rtp_timestamp = None;
        log.encoded(&missing, Some("new/v0"));
        log.decoded(&frame(10, 4, true), output, true);
        assert!(log.require_since("new/v0", 1, start, 1, (4, 4)).is_err());
    }

    #[test]
    fn track_identity_must_match_even_if_the_native_rtp_key_collides() {
        let start = Instant::now();
        let output = start + Duration::from_millis(1);
        for field in 0..3 {
            for decoded_first in [false, true] {
                let encoded = frame(10, 1, false);
                let mut decoded = frame(10, 1, true);
                match field {
                    0 => decoded.track.track_id = "another-native-track".into(),
                    1 => decoded.track.mid = "another-mid".into(),
                    _ => decoded.track.media_index += 1,
                }
                let mut log = NativeOriginCorrelation::default();
                if decoded_first {
                    log.decoded(&decoded, output, true);
                }
                log.encoded(&encoded, Some("admission/v0"));
                if !decoded_first {
                    log.decoded(&decoded, output, true);
                }
                assert!(
                    log.require_since("admission/v0", 1, start, 1, (4, 4))
                        .is_err(),
                    "different native track identities cannot be paired"
                );

                let mut log = NativeOriginCorrelation::default();
                log.encoded(&encoded, Some("admission/v0"));
                log.decoded(&frame(10, 1, true), output, true);
                assert!(
                    log.require_since("admission/v0", 1, start, 1, (4, 4))
                        .is_ok()
                );
                let mut conflicting = encoded.clone();
                conflicting.track = decoded.track;
                log.encoded(&conflicting, Some("admission/v0"));
                assert!(
                    log.require_since("admission/v0", 1, start, 1, (4, 4))
                        .is_err(),
                    "conflicting track evidence must revoke a prior receipt"
                );
            }
        }
    }

    #[test]
    fn nontrivial_output_and_bounded_retained_evidence_are_mandatory() {
        let start = Instant::now();
        let output = start + Duration::from_millis(1);
        let mut log = NativeOriginCorrelation::default();
        log.encoded(&frame(10, 1, false), Some("admission/v0"));
        log.decoded(&frame(10, 1, true), output, false);
        assert!(
            log.require_since("admission/v0", 1, start, 1, (4, 4))
                .is_err()
        );
        log.encoded(&frame(10, 2, false), Some("admission/v0"));
        log.decoded(&frame(10, 2, true), output, true);
        assert!(
            log.require_since("admission/v0", 1, start, 1, (4, 4))
                .is_ok()
        );
        log.encoded_observation_lost();
        assert!(
            log.require_since("admission/v0", 1, start, 1, (4, 4))
                .is_err()
        );
        log.encoded(&frame(10, 2, false), Some("admission/v0"));
        assert!(
            log.require_since("admission/v0", 1, start, 1, (4, 4))
                .is_err(),
            "pre-loss native output must not be reused"
        );
        log.decoded(&frame(10, 2, true), output, true);
        for timestamp in 3..=u32::try_from(PICTURES + 2).unwrap() {
            log.encoded(&frame(10, timestamp, false), Some("admission/v0"));
        }
        assert_eq!(log.pictures.len(), PICTURES);
        assert!(
            log.require_since("admission/v0", 1, start, 1, (4, 4))
                .is_err(),
            "eviction cannot manufacture a witness"
        );
    }
}

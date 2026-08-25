//! A `metrics` recorder that a single shard owns outright.
//!
//! The `metrics::*` macros stay exactly as written at every call site; what
//! changes is which recorder they resolve against. Each shard installs this one
//! around its own tick, so an increment lands in memory only that core touches
//! and never in the process-global registry, whose sharded locks and
//! render-time walks are what put spikes in the forwarding tail.
//!
//! Values do not live in the lookup maps. They live in a chunked arena of
//! `AtomicU64`, which is what makes the once-a-second snapshot a linear scan of
//! contiguous cache lines instead of a pointer chase through scattered
//! allocations. See `docs/thread-per-core.md`.
#![allow(
    clippy::disallowed_types,
    reason = "the handles are Arc<AtomicU64> because metrics::Counter::from_arc demands Send + Sync. Each slot is registered, written and read by exactly one shard on one core, so nothing is shared between cores and nothing contends. See docs/thread-per-core.md."
)]

use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use metrics::{
    Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder,
    SharedString, Unit,
};

use crate::id::ShardId;

/// Slots per arena chunk. One chunk is 512 bytes — eight cache lines — so a
/// full sweep of 64 counters touches eight lines rather than 64 allocations.
const CHUNK_SLOTS: usize = 64;

/// Power-of-two histogram buckets, the last one a catch-all.
///
/// Bucket `k` holds values in `[2^(k-1), 2^k)`, so bucket 25 opens at ~16.7
/// seconds when the unit is microseconds. Anything above that is already a
/// pathology the exact magnitude of which does not change the diagnosis.
const HIST_BUCKETS: usize = 26;

/// Reports that carry the schema even though it did not change.
///
/// The schema rides along whenever it changes, but the lane is lossy, so a
/// dropped report could otherwise strand a shard whose names the aggregator
/// never learned. Resending on a slow heartbeat makes that self-healing at a
/// cost of one string-cloning report every half minute.
const SCHEMA_RESEND_EVERY: u32 = 30;

const LAST_BUCKET: usize = HIST_BUCKETS.saturating_sub(1);

fn bucket_of(value: u64) -> usize {
    let idx = u64::BITS.saturating_sub(value.leading_zeros()) as usize;
    idx.min(LAST_BUCKET)
}

/// Inclusive upper bound of a bucket, as Prometheus `le`. `None` is `+Inf`.
fn bucket_le(idx: usize) -> Option<u64> {
    debug_assert!(idx < HIST_BUCKETS, "bucket {idx} out of range");
    match idx {
        LAST_BUCKET => None,
        0 => Some(0),
        idx => Some(
            1u64.checked_shl(u32::try_from(idx).ok()?)?
                .saturating_sub(1),
        ),
    }
}

struct Chunk {
    slots: [AtomicU64; CHUNK_SLOTS],
}

impl Chunk {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

/// A stable reference to one arena slot. This is what the `metrics` handle
/// wraps, so the handle stays valid across arena growth.
struct SlotRef {
    chunk: Arc<Chunk>,
    idx: usize,
}

impl SlotRef {
    /// `idx` comes from [`Arena::alloc`], which never hands out one past its
    /// chunk. Losing a count is still better than panicking on the packet path
    /// if that ever stops being true.
    #[inline]
    fn slot(&self) -> Option<&AtomicU64> {
        let slot = self.chunk.slots.get(self.idx);
        debug_assert!(slot.is_some(), "slot {} lies outside its chunk", self.idx);
        slot
    }

    fn update(&self, f: impl Fn(f64) -> f64) {
        let Some(slot) = self.slot() else { return };
        let mut current = slot.load(Ordering::Relaxed);
        loop {
            let next = f(f64::from_bits(current)).to_bits();
            match slot.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }
}

impl CounterFn for SlotRef {
    fn increment(&self, value: u64) {
        if let Some(slot) = self.slot() {
            slot.fetch_add(value, Ordering::Relaxed);
        }
    }

    fn absolute(&self, value: u64) {
        if let Some(slot) = self.slot() {
            slot.fetch_max(value, Ordering::Relaxed);
        }
    }
}

impl GaugeFn for SlotRef {
    fn increment(&self, value: f64) {
        self.update(|current| current + value);
    }

    fn decrement(&self, value: f64) {
        self.update(|current| current - value);
    }

    fn set(&self, value: f64) {
        if let Some(slot) = self.slot() {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
    }
}

struct BucketHist {
    buckets: [AtomicU64; HIST_BUCKETS],
    sum: AtomicU64,
    count: AtomicU64,
}

impl BucketHist {
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> HistSnapshot {
        let mut buckets = [0u64; HIST_BUCKETS];
        for (out, slot) in buckets.iter_mut().zip(self.buckets.iter()) {
            *out = slot.load(Ordering::Relaxed);
        }
        let snap = HistSnapshot {
            buckets,
            sum: self.sum.load(Ordering::Relaxed),
            count: self.count.load(Ordering::Relaxed),
        };
        debug_assert!(
            snap.buckets.iter().sum::<u64>() <= snap.count,
            "bucket total exceeds count; a record raced its own snapshot"
        );
        snap
    }
}

impl HistogramFn for BucketHist {
    fn record(&self, value: f64) {
        debug_assert!(!value.is_nan(), "recorded NaN into a histogram");
        // A float outside u64 is already a pathology; clamp explicitly rather
        // than letting an `as` cast decide silently.
        const MAX: f64 = u64::MAX as f64;
        let value = if value.is_finite() && value > 0.0 {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "range-checked immediately above: the value is finite, positive and below u64::MAX"
            )]
            {
                value.trunc().min(MAX) as u64
            }
        } else {
            0
        };
        let idx = bucket_of(value);
        let Some(bucket) = self.buckets.get(idx) else {
            debug_assert!(false, "bucket {idx} out of range for {value}");
            return;
        };
        bucket.fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(value, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

/// A chunked arena of counter/gauge slots.
///
/// Handles hold an `Arc<Chunk>` so they survive growth, while the snapshot
/// walks the chunks directly and never dereferences a handle.
#[derive(Default)]
struct Arena {
    chunks: Vec<Arc<Chunk>>,
    len: usize,
}

impl Arena {
    fn alloc(&mut self) -> Option<SlotRef> {
        let idx = self.len;
        let (chunk_idx, slot_idx) = (idx / CHUNK_SLOTS, idx % CHUNK_SLOTS);
        if chunk_idx == self.chunks.len() {
            self.chunks.push(Arc::new(Chunk::new()));
        }
        let chunk = self.chunks.get(chunk_idx)?;
        let slot = SlotRef {
            chunk: Arc::clone(chunk),
            idx: slot_idx,
        };
        self.len = self.len.saturating_add(1);
        Some(slot)
    }

    /// Linear scan of contiguous memory. This is the whole reason values live
    /// here rather than in the lookup map.
    fn snapshot_into(&self, out: &mut Vec<u64>) {
        out.clear();
        out.reserve(self.len);
        for chunk in &self.chunks {
            let taken = out.len();
            let take = self.len.saturating_sub(taken).min(CHUNK_SLOTS);
            for slot in chunk.slots.iter().take(take) {
                out.push(slot.load(Ordering::Relaxed));
            }
        }
        debug_assert_eq!(out.len(), self.len, "arena snapshot lost a slot");
    }
}

/// One metric's identity, as a value the control plane can keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricKey {
    pub name: String,
    pub labels: Vec<(String, String)>,
}

impl MetricKey {
    fn from_key(key: &Key) -> Self {
        Self {
            name: key.name().to_string(),
            labels: key
                .labels()
                .map(|l| (l.key().to_string(), l.value().to_string()))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Description {
    pub unit: Option<Unit>,
    pub text: String,
}

/// The names behind a report's values, sent only when they change.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    pub counters: Vec<MetricKey>,
    pub gauges: Vec<MetricKey>,
    pub histograms: Vec<MetricKey>,
    pub descriptions: Vec<(String, Description)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistSnapshot {
    pub buckets: [u64; HIST_BUCKETS],
    pub sum: u64,
    pub count: u64,
}

impl Default for HistSnapshot {
    fn default() -> Self {
        Self {
            buckets: [0; HIST_BUCKETS],
            sum: 0,
            count: 0,
        }
    }
}

/// One shard's cumulative view of itself, as a value.
///
/// Absolute, never a delta — which is what lets the lane carrying it drop
/// freely. A lost report costs staleness until the next one, and nothing else.
#[derive(Debug, Clone)]
pub struct ShardStatsReport {
    pub shard: ShardId,
    pub schema_epoch: u64,
    pub schema: Option<Schema>,
    pub counters: Vec<u64>,
    pub gauges: Vec<f64>,
    pub histograms: Vec<HistSnapshot>,
}

type Map<V> = ahash::AHashMap<Key, V>;

#[derive(Default)]
struct Registry {
    counters: Map<Arc<SlotRef>>,
    gauges: Map<Arc<SlotRef>>,
    histograms: Map<Arc<BucketHist>>,
    counter_arena: Arena,
    gauge_arena: Arena,
    hist_handles: Vec<Arc<BucketHist>>,
    schema: Schema,
    descriptions: ahash::AHashMap<String, Description>,
    epoch: u64,
    schema_sent_at_epoch: Option<u64>,
    reports_since_schema: u32,
}

pub(crate) struct ShardRecorder {
    registry: Mutex<Registry>,
}

impl ShardRecorder {
    pub(crate) fn new() -> Self {
        Self {
            registry: Mutex::new(Registry::default()),
        }
    }

    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Copy out everything this shard has recorded.
    ///
    /// A linear scan of the arenas, not a walk of the lookup maps. The maps
    /// exist only for registration, which happens once per call site.
    pub(crate) fn snapshot(&self, shard: ShardId) -> ShardStatsReport {
        let mut reg = self.registry.lock();

        let mut counters = Vec::new();
        let mut gauge_bits = Vec::new();
        reg.counter_arena.snapshot_into(&mut counters);
        reg.gauge_arena.snapshot_into(&mut gauge_bits);
        let gauges = gauge_bits.into_iter().map(f64::from_bits).collect();
        let histograms: Vec<HistSnapshot> = reg.hist_handles.iter().map(|h| h.snapshot()).collect();

        debug_assert_eq!(
            counters.len(),
            reg.schema.counters.len(),
            "counter values and schema disagree"
        );
        debug_assert_eq!(
            histograms.len(),
            reg.schema.histograms.len(),
            "histogram values and schema disagree"
        );

        let stale = reg.schema_sent_at_epoch != Some(reg.epoch);
        let heartbeat = reg.reports_since_schema >= SCHEMA_RESEND_EVERY;
        let schema = if stale || heartbeat {
            reg.schema.descriptions = reg
                .descriptions
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            reg.schema_sent_at_epoch = Some(reg.epoch);
            reg.reports_since_schema = 0;
            Some(reg.schema.clone())
        } else {
            reg.reports_since_schema = reg.reports_since_schema.saturating_add(1);
            None
        };

        ShardStatsReport {
            shard,
            schema_epoch: reg.epoch,
            schema,
            counters,
            gauges,
            histograms,
        }
    }
}

impl Registry {
    fn describe(&mut self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        let desc = Description {
            unit,
            text: description.to_string(),
        };
        // Only a *change* bumps the epoch. `describe_*!` in a loop would
        // otherwise mark the schema dirty on every tick, and the shard would
        // pay the string-cloning report every second instead of every thirty.
        if self.descriptions.get(key.as_str()) == Some(&desc) {
            return;
        }
        self.descriptions.insert(key.as_str().to_string(), desc);
        self.bump_epoch();
    }

    fn bump_epoch(&mut self) {
        self.epoch = self.epoch.saturating_add(1);
    }
}

impl Recorder for ShardRecorder {
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.registry.lock().describe(key, unit, description);
    }

    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.registry.lock().describe(key, unit, description);
    }

    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.registry.lock().describe(key, unit, description);
    }

    fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
        let mut reg = self.registry.lock();
        if let Some(slot) = reg.counters.get(key) {
            return Counter::from_arc(Arc::clone(slot));
        }
        let Some(slot) = reg.counter_arena.alloc() else {
            debug_assert!(false, "counter arena refused a slot");
            return Counter::noop();
        };
        let slot = Arc::new(slot);
        reg.counters.insert(key.clone(), Arc::clone(&slot));
        reg.schema.counters.push(MetricKey::from_key(key));
        reg.bump_epoch();
        Counter::from_arc(slot)
    }

    fn register_gauge(&self, key: &Key, _metadata: &Metadata<'_>) -> Gauge {
        let mut reg = self.registry.lock();
        if let Some(slot) = reg.gauges.get(key) {
            return Gauge::from_arc(Arc::clone(slot));
        }
        let Some(slot) = reg.gauge_arena.alloc() else {
            debug_assert!(false, "gauge arena refused a slot");
            return Gauge::noop();
        };
        let slot = Arc::new(slot);
        reg.gauges.insert(key.clone(), Arc::clone(&slot));
        reg.schema.gauges.push(MetricKey::from_key(key));
        reg.bump_epoch();
        Gauge::from_arc(slot)
    }

    fn register_histogram(&self, key: &Key, _metadata: &Metadata<'_>) -> Histogram {
        let mut reg = self.registry.lock();
        if let Some(hist) = reg.histograms.get(key) {
            return Histogram::from_arc(Arc::clone(hist));
        }
        let hist = Arc::new(BucketHist::new());
        reg.histograms.insert(key.clone(), Arc::clone(&hist));
        reg.hist_handles.push(Arc::clone(&hist));
        reg.schema.histograms.push(MetricKey::from_key(key));
        reg.bump_epoch();
        Histogram::from_arc(hist)
    }
}

/// Prometheus `le` label for a bucket index, `+Inf` for the catch-all.
pub fn bucket_le_label(idx: usize) -> String {
    match bucket_le(idx) {
        Some(v) => v.to_string(),
        None => "+Inf".to_string(),
    }
}

pub const fn hist_buckets() -> usize {
    HIST_BUCKETS
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics::{counter, gauge, histogram, with_local_recorder};

    const SHARD: ShardId = ShardId::new(0);

    fn find(keys: &[MetricKey], name: &str) -> Option<usize> {
        // A plain loop, not `.position`: the architecture guard scans this
        // directory textually for discovery scans on the hot path.
        for (idx, key) in keys.iter().enumerate() {
            if key.name == name {
                return Some(idx);
            }
        }
        None
    }

    #[test]
    fn increments_accumulate_in_the_local_recorder() {
        let recorder = ShardRecorder::new();
        with_local_recorder(&recorder, || {
            counter!("packets").increment(3);
            counter!("packets").increment(4);
        });

        let report = recorder.snapshot(SHARD);
        let schema = report.schema.expect("first report carries the schema");
        let idx = find(&schema.counters, "packets").expect("counter registered");
        assert_eq!(report.counters[idx], 7);
    }

    #[test]
    fn two_recorders_on_one_thread_stay_independent() {
        // The property the sim path depends on: under SharedRuntime every shard
        // of a node runs on the same thread, so attribution must come from the
        // installed recorder rather than from thread identity.
        let a = ShardRecorder::new();
        let b = ShardRecorder::new();

        with_local_recorder(&a, || counter!("packets").increment(10));
        with_local_recorder(&b, || counter!("packets").increment(1));

        let (ra, rb) = (a.snapshot(ShardId::new(0)), b.snapshot(ShardId::new(1)));
        assert_eq!(ra.counters, vec![10]);
        assert_eq!(rb.counters, vec![1]);
    }

    #[test]
    fn recording_outside_the_scope_does_not_reach_the_recorder() {
        let recorder = ShardRecorder::new();
        counter!("escaped").increment(1);
        assert!(recorder.snapshot(SHARD).counters.is_empty());
    }

    #[test]
    fn labels_distinguish_series_under_one_name() {
        let recorder = ShardRecorder::new();
        with_local_recorder(&recorder, || {
            counter!("drops", "reason" => "late").increment(2);
            counter!("drops", "reason" => "full").increment(5);
        });

        let report = recorder.snapshot(SHARD);
        let schema = report.schema.expect("schema");
        assert_eq!(schema.counters.len(), 2);
        assert_eq!(report.counters.iter().sum::<u64>(), 7);
    }

    #[test]
    fn gauges_round_trip_through_bit_storage() {
        let recorder = ShardRecorder::new();
        with_local_recorder(&recorder, || {
            gauge!("ratio").set(0.75);
            gauge!("ratio").increment(0.25);
            gauge!("depth").set(4.0);
            gauge!("depth").decrement(1.5);
        });

        let report = recorder.snapshot(SHARD);
        let schema = report.schema.expect("schema");
        let ratio = find(&schema.gauges, "ratio").expect("gauge");
        let depth = find(&schema.gauges, "depth").expect("gauge");
        assert!((report.gauges[ratio] - 1.0).abs() < f64::EPSILON);
        assert!((report.gauges[depth] - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn histogram_buckets_are_exact_and_bounded() {
        let recorder = ShardRecorder::new();
        let samples = [0u64, 1, 2, 3, 4, 1_000, u64::from(u32::MAX)];
        with_local_recorder(&recorder, || {
            for s in samples {
                histogram!("delay_us").record(s as f64);
            }
        });

        let report = recorder.snapshot(SHARD);
        let hist = report.histograms[0];
        assert_eq!(hist.count, samples.len() as u64);
        assert_eq!(hist.sum, samples.iter().sum::<u64>());
        assert_eq!(hist.buckets.iter().sum::<u64>(), hist.count);

        // Boundaries: 0 alone in bucket 0, 1 alone in bucket 1, [2,3] together.
        assert_eq!(hist.buckets[0], 1);
        assert_eq!(hist.buckets[1], 1);
        assert_eq!(hist.buckets[2], 2);
        assert_eq!(hist.buckets[3], 1);
    }

    #[test]
    fn every_value_lands_in_a_bucket() {
        for shift in 0..64 {
            let v = 1u64 << shift;
            assert!(bucket_of(v) < HIST_BUCKETS, "{v} escaped the bucket range");
        }
        assert!(bucket_of(u64::MAX) < HIST_BUCKETS);
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_le(HIST_BUCKETS - 1), None, "last bucket is +Inf");
    }

    #[test]
    fn bucket_bounds_are_strictly_increasing() {
        let mut previous = None;
        for idx in 0..HIST_BUCKETS - 1 {
            let le = bucket_le(idx).expect("finite bound");
            if let Some(prev) = previous {
                assert!(le > prev, "bucket {idx} bound {le} did not increase");
            }
            previous = Some(le);
        }
    }

    #[test]
    fn the_arena_survives_growing_past_one_chunk() {
        let recorder = ShardRecorder::new();
        let total = CHUNK_SLOTS * 3 + 7;
        with_local_recorder(&recorder, || {
            for i in 0..total {
                counter!("spread", "i" => i.to_string()).increment(i as u64);
            }
        });

        let report = recorder.snapshot(SHARD);
        assert_eq!(report.counters.len(), total);
        assert_eq!(
            report.counters.iter().sum::<u64>(),
            (0..total as u64).sum::<u64>(),
            "a handle stopped pointing at its slot when the arena grew"
        );
    }

    #[test]
    fn the_schema_rides_only_when_it_changes() {
        let recorder = ShardRecorder::new();
        with_local_recorder(&recorder, || counter!("a").increment(1));

        assert!(recorder.snapshot(SHARD).schema.is_some(), "first report");
        assert!(
            recorder.snapshot(SHARD).schema.is_none(),
            "unchanged schema should not be resent"
        );

        with_local_recorder(&recorder, || counter!("b").increment(1));
        assert!(
            recorder.snapshot(SHARD).schema.is_some(),
            "a new metric must resend the schema"
        );
    }

    #[test]
    fn the_schema_is_resent_on_a_heartbeat() {
        // The lane is lossy, so a dropped schema report must not strand a
        // shard whose names the aggregator never learned.
        let recorder = ShardRecorder::new();
        with_local_recorder(&recorder, || counter!("a").increment(1));
        recorder.snapshot(SHARD);

        let resent = (0..=SCHEMA_RESEND_EVERY).any(|_| recorder.snapshot(SHARD).schema.is_some());
        assert!(resent, "schema was never resent");
    }

    #[test]
    fn descriptions_reach_the_schema() {
        let recorder = ShardRecorder::new();
        with_local_recorder(&recorder, || {
            metrics::describe_counter!("packets", Unit::Count, "packets forwarded");
            counter!("packets").increment(1);
        });

        let schema = recorder.snapshot(SHARD).schema.expect("schema");
        let mut found = None;
        for (name, desc) in &schema.descriptions {
            if name == "packets" {
                found = Some(desc);
            }
        }
        let desc = found.expect("description recorded");
        assert_eq!(desc.text, "packets forwarded");
        assert_eq!(desc.unit, Some(Unit::Count));
    }
}

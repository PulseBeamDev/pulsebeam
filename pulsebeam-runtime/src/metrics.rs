use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::sync::{Arc, Mutex};
use dashmap::DashMap;
pub use metrics::{
    Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder,
    SharedString, Unit,
};
use once_cell::sync::Lazy;
use quanta::Clock;

/// Maximum number of unique metrics supported.
const MAX_METRICS: usize = 2048;

struct MetricMetadata {
    unit: Option<Unit>,
    description: SharedString,
}

/// Fixed latency buckets for WebRTC (in nanoseconds):
/// <1ms, 1-5ms, 5-10ms, 10-50ms, 100ms, >100ms
const LATENCY_BUCKETS: [u64; 5] = [
    1_000_000,   // 1ms
    5_000_000,   // 5ms
    10_000_000,  // 10ms
    50_000_000,  // 50ms
    100_000_000, // 100ms
];

/// A handle to a metric in the thread-local registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MetricHandle(usize);

/// A thread-safe counter using Relaxed atomics for zero-synchronization overhead.
struct SyncCell(std::sync::atomic::AtomicU64);

impl SyncCell {
    const fn new(val: u64) -> Self {
        Self(std::sync::atomic::AtomicU64::new(val))
    }

    #[inline(always)]
    fn set(&self, val: u64) {
        self.0.store(val, std::sync::atomic::Ordering::Relaxed);
    }

    #[inline(always)]
    fn get(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Thread-local storage for metrics.
struct ThreadRegistry {
    counters: [SyncCell; MAX_METRICS],
    histograms: [[SyncCell; LATENCY_BUCKETS.len() + 1]; MAX_METRICS],
    gauges: [SyncCell; MAX_METRICS],
}

impl Default for ThreadRegistry {
    fn default() -> Self {
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO_CELL: SyncCell = SyncCell::new(0);
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO_HISTOGRAM: [SyncCell; LATENCY_BUCKETS.len() + 1] =
            [ZERO_CELL; LATENCY_BUCKETS.len() + 1];

        Self {
            counters: [ZERO_CELL; MAX_METRICS],
            histograms: [ZERO_HISTOGRAM; MAX_METRICS],
            gauges: [ZERO_CELL; MAX_METRICS],
        }
    }
}

// Global state to manage metric names and thread-local registries.
static METRIC_MAP: Lazy<DashMap<Key, MetricHandle>> = Lazy::new(DashMap::new);
static METRIC_METADATA: Lazy<DashMap<KeyName, MetricMetadata>> = Lazy::new(DashMap::new);
static NEXT_METRIC_ID: AtomicUsize = AtomicUsize::new(0);
static ALL_REGISTRIES: Lazy<Mutex<Vec<Arc<ThreadRegistry>>>> = Lazy::new(|| Mutex::new(Vec::new()));

thread_local! {
    static LOCAL_REGISTRY: Arc<ThreadRegistry> = {
        let registry = Arc::new(ThreadRegistry::default());
        let mut all = ALL_REGISTRIES.lock();
        all.push(registry.clone());
        registry
    };

    static SAMPLING_COUNTER: Cell<u64> = const { Cell::new(0) };
}

/// Initialize the global TPC-optimized metrics recorder.
pub(crate) fn init() {
    static RECORDER_INSTALLED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    if !RECORDER_INSTALLED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        let recorder = TpcRecorder::new();
        let _ = metrics::set_global_recorder(recorder);
    }
}

/// The TPC-optimized metrics recorder.
pub struct TpcRecorder {
    clock: Clock,
}

impl TpcRecorder {
    pub fn new() -> Self {
        Self {
            clock: Clock::new(),
        }
    }

    /// Helper to get or create a metric handle.
    fn get_or_create_handle(&self, key: &Key) -> MetricHandle {
        if let Some(handle) = METRIC_MAP.get(key) {
            return *handle;
        }

        let id = NEXT_METRIC_ID.fetch_add(1, Ordering::Relaxed);
        if id >= MAX_METRICS {
            panic!("Exceeded MAX_METRICS ({})", MAX_METRICS);
        }

        let handle = MetricHandle(id);
        METRIC_MAP.insert(key.clone(), handle);
        handle
    }

    /// Low-overhead timing helper.
    pub fn now(&self) -> u64 {
        self.clock.raw()
    }

    /// Record latency between two `now()` samples.
    pub fn record_latency(&self, handle: Histogram, start: u64, end: u64) {
        let delta = self.clock.delta(start, end).as_nanos() as f64;
        handle.record(delta);
    }

    /// Sampling helper: returns true once every `n` calls for this thread.
    pub fn should_sample(&self, n: u64) -> bool {
        if n <= 1 {
            return true;
        }
        SAMPLING_COUNTER.with(|c| {
            let val = c.get();
            let res = val % n == 0;
            c.set(val.wrapping_add(1));
            res
        })
    }
}

impl Recorder for TpcRecorder {
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        METRIC_METADATA.insert(key, MetricMetadata { unit, description });
    }

    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        METRIC_METADATA.insert(key, MetricMetadata { unit, description });
    }

    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        METRIC_METADATA.insert(key, MetricMetadata { unit, description });
    }

    fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
        let handle = self.get_or_create_handle(key);
        Counter::from_arc(Arc::new(TpcCounter(handle)))
    }

    fn register_gauge(&self, key: &Key, _metadata: &Metadata<'_>) -> Gauge {
        let handle = self.get_or_create_handle(key);
        Gauge::from_arc(Arc::new(TpcGauge(handle)))
    }

    fn register_histogram(&self, key: &Key, _metadata: &Metadata<'_>) -> Histogram {
        let handle = self.get_or_create_handle(key);
        Histogram::from_arc(Arc::new(TpcHistogram(handle)))
    }
}

struct TpcCounter(MetricHandle);

impl CounterFn for TpcCounter {
    fn increment(&self, value: u64) {
        LOCAL_REGISTRY.with(|r| {
            let cell = &r.counters[self.0.0];
            cell.set(cell.get() + value);
        });
    }

    fn absolute(&self, value: u64) {
        LOCAL_REGISTRY.with(|r| {
            r.counters[self.0.0].set(value);
        });
    }
}

struct TpcGauge(MetricHandle);

impl GaugeFn for TpcGauge {
    fn increment(&self, value: f64) {
        LOCAL_REGISTRY.with(|r| {
            let cell = &r.gauges[self.0.0];
            cell.set(cell.get().wrapping_add(value as u64));
        });
    }

    fn decrement(&self, value: f64) {
        LOCAL_REGISTRY.with(|r| {
            let cell = &r.gauges[self.0.0];
            cell.set(cell.get().wrapping_sub(value as u64));
        });
    }

    fn set(&self, value: f64) {
        LOCAL_REGISTRY.with(|r| {
            r.gauges[self.0.0].set(value as u64);
        });
    }
}

struct TpcHistogram(MetricHandle);

impl HistogramFn for TpcHistogram {
    fn record(&self, value: f64) {
        let val_ns = value as u64;
        LOCAL_REGISTRY.with(|r| {
            let buckets = &r.histograms[self.0.0];

            // Use simple if/else ladder to find the bucket (jitter protection)
            if val_ns < LATENCY_BUCKETS[0] {
                buckets[0].set(buckets[0].get() + 1);
            } else if val_ns < LATENCY_BUCKETS[1] {
                buckets[1].set(buckets[1].get() + 1);
            } else if val_ns < LATENCY_BUCKETS[2] {
                buckets[2].set(buckets[2].get() + 1);
            } else if val_ns < LATENCY_BUCKETS[3] {
                buckets[3].set(buckets[3].get() + 1);
            } else if val_ns < LATENCY_BUCKETS[4] {
                buckets[4].set(buckets[4].get() + 1);
            } else {
                buckets[5].set(buckets[5].get() + 1);
            }
        });
    }
}

/// Slow-path: Scrape all per-thread registries and aggregate values.
pub fn scrape_to_prometheus() -> String {
    let mut output = String::new();
    let registries = ALL_REGISTRIES.lock();

    for entry in METRIC_MAP.iter() {
        let key: &metrics::Key = entry.key();
        let handle: &MetricHandle = entry.value();
        let metric_name = key.name().to_string();

        // Sum across all threads
        let mut total_counter = 0u64;
        let mut total_gauge = 0u64;
        let mut total_buckets = [0u64; LATENCY_BUCKETS.len() + 1];

        for reg in registries.iter() {
            total_counter += reg.counters[handle.0].get();
            total_gauge += reg.gauges[handle.0].get();
            for (i, bucket) in reg.histograms[handle.0].iter().enumerate() {
                let val = bucket.get();
                total_buckets[i] += val;
            }
        }

        // Output Help/Unit if available
        if let Some(metadata) = METRIC_METADATA.get(key.name()) {
            if !metadata.description.is_empty() {
                output.push_str(&format!(
                    "# HELP {} {}\n",
                    metric_name, metadata.description
                ));
            }
            if let Some(unit) = &metadata.unit {
                output.push_str(&format!(
                    "# UNIT {} {}\n",
                    metric_name,
                    unit.as_canonical_label()
                ));
            }
        }

        // Output Counter
        if total_counter > 0 {
            output.push_str(&format!("# TYPE {} counter\n", metric_name));
            output.push_str(&format!("{} {}\n", metric_name, total_counter));
        }

        // Output Gauge
        if total_gauge > 0 {
            output.push_str(&format!("# TYPE {} gauge\n", metric_name));
            output.push_str(&format!("{} {}\n", metric_name, total_gauge));
        }

        // Output Histogram
        if total_buckets.iter().any(|&b| b > 0) {
            output.push_str(&format!("# TYPE {} histogram\n", metric_name));
            let mut cumulative = 0;
            for (i, &limit) in LATENCY_BUCKETS.iter().enumerate() {
                cumulative += total_buckets[i];
                output.push_str(&format!(
                    "{}_bucket{{le=\"{:.3}\"}} {}\n",
                    metric_name,
                    limit as f64 / 1_000_000.0,
                    cumulative
                ));
            }
            cumulative += total_buckets[LATENCY_BUCKETS.len()];
            output.push_str(&format!(
                "{}_bucket{{le=\"+Inf\"}} {}\n",
                metric_name, cumulative
            ));
            output.push_str(&format!("{}_count {}\n", metric_name, cumulative));
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_tpc_recorder_counters() {
        let recorder = TpcRecorder::new();
        let c = recorder.register_counter(
            &metrics::Key::from_name("test_counter"),
            &metrics::Metadata::new("test", metrics::Level::DEBUG, None),
        );

        let c_clone = c.clone();
        let handle = thread::spawn(move || {
            c_clone.increment(10);
            c_clone.increment(5);
        });
        handle.join().unwrap();

        c.increment(7);

        let output = scrape_to_prometheus();
        assert!(output.contains("test_counter 22"));
    }

    #[test]
    fn test_tpc_recorder_histograms() {
        let recorder = TpcRecorder::new();

        let h = recorder.register_histogram(
            &metrics::Key::from_name("test_latency"),
            &metrics::Metadata::new("test", metrics::Level::DEBUG, None),
        );

        h.record(500_000.0); // 0.5ms -> bucket 0
        h.record(2_000_000.0); // 2ms -> bucket 1
        h.record(150_000_000.0); // 150ms -> bucket 5

        let output = scrape_to_prometheus();
        assert!(output.contains("test_latency_bucket{le=\"1.000\"} 1"));
        assert!(output.contains("test_latency_bucket{le=\"5.000\"} 2"));
        assert!(output.contains("test_latency_bucket{le=\"+Inf\"} 3"));
        assert!(output.contains("test_latency_count 3"));
    }

    #[test]
    fn test_sampling() {
        let recorder = TpcRecorder::new();
        let mut sampled_count = 0;
        for _ in 0..100 {
            if recorder.should_sample(10) {
                sampled_count += 1;
            }
        }
        // Should be exactly 10 if started at 0
        assert_eq!(sampled_count, 10);
    }

    #[test]
    fn test_describe_metadata() {
        let recorder = TpcRecorder::new();
        recorder.describe_counter(
            metrics::KeyName::from_const_str("test_meta"),
            Some(metrics::Unit::Bytes),
            metrics::SharedString::from("A test metric"),
        );
        let c = recorder.register_counter(
            &metrics::Key::from_name("test_meta"),
            &metrics::Metadata::new("test", metrics::Level::DEBUG, None),
        );
        c.increment(10);

        let output = scrape_to_prometheus();
        assert!(output.contains("# HELP test_meta A test metric"));
        assert!(output.contains("# UNIT test_meta"));
        assert!(output.contains("test_meta 10"));
    }
}

use dashmap::DashMap;
pub use metrics::{
    Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder,
    SharedString, Unit,
};
use once_cell::sync::Lazy;
use quanta::Clock;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Maximum number of unique metrics supported.
const MAX_METRICS: usize = 4096;

/// Fixed latency buckets for WebRTC (in nanoseconds):
/// <1ms, 1-5ms, 5-10ms, 10-50ms, 100ms, >100ms
const LATENCY_BUCKETS: [u64; 5] = [
    1_000_000,   // 1ms
    5_000_000,   // 5ms
    10_000_000,  // 10ms
    50_000_000,  // 50ms
    100_000_000, // 100ms
];

struct MetricMetadata {
    unit: Option<Unit>,
    description: SharedString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MetricHandle(usize);

/// A thread-safe u64 counter. Since TPC guarantees single-writer, we don't need fetch_add.
struct SyncU64Cell(AtomicU64);

impl SyncU64Cell {
    const fn new(val: u64) -> Self {
        Self(AtomicU64::new(val))
    }

    #[inline(always)]
    fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    #[inline(always)]
    fn set(&self, val: u64) {
        self.0.store(val, Ordering::Relaxed);
    }
}

/// A thread-safe f64 cell. Stores f64 as bits in an AtomicU64
struct SyncF64Cell(AtomicU64);

impl SyncF64Cell {
    const fn new(val: f64) -> Self {
        Self(AtomicU64::new(val.to_bits()))
    }

    #[inline(always)]
    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }

    #[inline(always)]
    fn set(&self, val: f64) {
        self.0.store(val.to_bits(), Ordering::Relaxed);
    }
}

struct HistogramData {
    buckets: [SyncU64Cell; LATENCY_BUCKETS.len() + 1],
    sum: SyncF64Cell,
    count: SyncU64Cell,
}

impl HistogramData {
    const fn new() -> Self {
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO_U64: SyncU64Cell = SyncU64Cell::new(0);
        Self {
            buckets: [ZERO_U64; LATENCY_BUCKETS.len() + 1],
            sum: SyncF64Cell::new(0.0),
            count: SyncU64Cell::new(0),
        }
    }
}

struct ThreadRegistry {
    counters: [SyncU64Cell; MAX_METRICS],
    gauges: [SyncF64Cell; MAX_METRICS],
    histograms: [HistogramData; MAX_METRICS],
}

impl Default for ThreadRegistry {
    fn default() -> Self {
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO_U64: SyncU64Cell = SyncU64Cell::new(0);
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO_F64: SyncF64Cell = SyncF64Cell::new(0.0);
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO_HIST: HistogramData = HistogramData::new();

        Self {
            counters: [ZERO_U64; MAX_METRICS],
            gauges: [ZERO_F64; MAX_METRICS],
            histograms: [ZERO_HIST; MAX_METRICS],
        }
    }
}

static METRIC_MAP: Lazy<DashMap<Key, MetricHandle>> = Lazy::new(DashMap::new);
static METRIC_METADATA: Lazy<DashMap<KeyName, MetricMetadata>> = Lazy::new(DashMap::new);
// Start at 1. Index 0 is strictly reserved for the dummy fallback metric.
static NEXT_METRIC_ID: AtomicUsize = AtomicUsize::new(1);
static ALL_REGISTRIES: Lazy<Mutex<Vec<Arc<ThreadRegistry>>>> = Lazy::new(|| Mutex::new(Vec::new()));

thread_local! {
    static LOCAL_REGISTRY: Arc<ThreadRegistry> = {
        let registry = Arc::new(ThreadRegistry::default());
        if let Ok(mut all) = ALL_REGISTRIES.lock() {
            all.push(registry.clone());
        }
        registry
    };

    static SAMPLING_COUNTER: Cell<u64> = const { Cell::new(0) };
}

pub(crate) fn init() {
    static RECORDER_INSTALLED: AtomicBool = AtomicBool::new(false);
    if !RECORDER_INSTALLED.swap(true, Ordering::SeqCst) {
        let _ = metrics::set_global_recorder(TpcRecorder::new());
    }
}

pub struct TpcRecorder {
    clock: Clock,
}

impl Default for TpcRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl TpcRecorder {
    pub fn new() -> Self {
        Self {
            clock: Clock::new(),
        }
    }

    fn get_or_create_handle(&self, key: &Key) -> MetricHandle {
        if let Some(handle) = METRIC_MAP.get(key) {
            return *handle;
        }

        let id = NEXT_METRIC_ID.fetch_add(1, Ordering::Relaxed);
        if id >= MAX_METRICS {
            // Data plane protection: return a dead-end handle instead of panicking.
            tracing::warn!(
                "CRITICAL: MAX_METRICS ({}) exceeded. Data discarded.",
                MAX_METRICS
            );
            return MetricHandle(0);
        }

        let handle = MetricHandle(id);
        METRIC_MAP.insert(key.clone(), handle);
        handle
    }

    #[inline(always)]
    pub fn now(&self) -> u64 {
        self.clock.raw()
    }

    #[inline(always)]
    pub fn record_latency(&self, handle: &Histogram, start: u64, end: u64) {
        let delta = self.clock.delta(start, end).as_nanos() as f64;
        handle.record(delta);
    }

    #[inline(always)]
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

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        Counter::from_arc(Arc::new(TpcCounter(self.get_or_create_handle(key))))
    }

    fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::from_arc(Arc::new(TpcGauge(self.get_or_create_handle(key))))
    }

    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::from_arc(Arc::new(TpcHistogram(self.get_or_create_handle(key))))
    }
}

struct TpcCounter(MetricHandle);
impl CounterFn for TpcCounter {
    #[inline(always)]
    fn increment(&self, value: u64) {
        LOCAL_REGISTRY.with(|r| {
            let cell = &r.counters[self.0.0];
            cell.set(cell.get().wrapping_add(value));
        });
    }

    #[inline(always)]
    fn absolute(&self, value: u64) {
        LOCAL_REGISTRY.with(|r| r.counters[self.0.0].set(value));
    }
}

struct TpcGauge(MetricHandle);
impl GaugeFn for TpcGauge {
    #[inline(always)]
    fn increment(&self, value: f64) {
        LOCAL_REGISTRY.with(|r| {
            let cell = &r.gauges[self.0.0];
            cell.set(cell.get() + value);
        });
    }

    #[inline(always)]
    fn decrement(&self, value: f64) {
        LOCAL_REGISTRY.with(|r| {
            let cell = &r.gauges[self.0.0];
            cell.set(cell.get() - value);
        });
    }

    #[inline(always)]
    fn set(&self, value: f64) {
        LOCAL_REGISTRY.with(|r| r.gauges[self.0.0].set(value));
    }
}

struct TpcHistogram(MetricHandle);
impl HistogramFn for TpcHistogram {
    #[inline(always)]
    fn record(&self, value: f64) {
        let val_ns = value as u64;
        LOCAL_REGISTRY.with(|r| {
            let h = &r.histograms[self.0.0];

            h.sum.set(h.sum.get() + value);
            h.count.set(h.count.get().wrapping_add(1));

            if val_ns < LATENCY_BUCKETS[0] {
                h.buckets[0].set(h.buckets[0].get() + 1);
            } else if val_ns < LATENCY_BUCKETS[1] {
                h.buckets[1].set(h.buckets[1].get() + 1);
            } else if val_ns < LATENCY_BUCKETS[2] {
                h.buckets[2].set(h.buckets[2].get() + 1);
            } else if val_ns < LATENCY_BUCKETS[3] {
                h.buckets[3].set(h.buckets[3].get() + 1);
            } else if val_ns < LATENCY_BUCKETS[4] {
                h.buckets[4].set(h.buckets[4].get() + 1);
            } else {
                h.buckets[5].set(h.buckets[5].get() + 1);
            }
        });
    }
}

/// Formats a metric key and its labels to the Prometheus text format representation.
fn format_labels(key: &Key) -> String {
    let mut labels = key.labels();
    if let Some(first) = labels.next() {
        let mut s = format!("{{ {}=\"{}\"", first.key(), first.value());
        for label in labels {
            s.push_str(&format!(", {}=\"{}\"", label.key(), label.value()));
        }
        s.push_str(" }");
        s
    } else {
        String::new()
    }
}

/// Slow-path: Scrape all per-thread registries and aggregate values.
pub fn scrape_to_prometheus() -> String {
    let mut output = String::with_capacity(8192);
    let registries = ALL_REGISTRIES.lock().unwrap();

    let mut described_metrics = std::collections::HashSet::new();

    for entry in METRIC_MAP.iter() {
        let key: &Key = entry.key();
        let handle: &MetricHandle = entry.value();
        if handle.0 == 0 {
            continue; // Skip the dummy fallback metric
        }

        let base_name = key.name_shared();
        let labels_str = format_labels(key);

        // Aggregate across threads
        let mut total_counter = 0u64;
        let mut total_gauge = 0f64;
        let mut hist_sum = 0f64;
        let mut hist_count = 0u64;
        let mut total_buckets = [0u64; LATENCY_BUCKETS.len() + 1];

        for reg in registries.iter() {
            total_counter += reg.counters[handle.0].get();
            total_gauge += reg.gauges[handle.0].get();

            let h = &reg.histograms[handle.0];
            hist_sum += h.sum.get();
            hist_count += h.count.get();
            for (i, bucket) in h.buckets.iter().enumerate() {
                total_buckets[i] += bucket.get();
            }
        }

        // Output Help/Unit once per base metric name
        if described_metrics.insert(base_name.clone())
            && let Some(metadata) = METRIC_METADATA.get(&base_name)
        {
            if !metadata.description.is_empty() {
                output.push_str(&format!(
                    "# HELP {} {}\n",
                    base_name.as_str(),
                    metadata.description
                ));
            }
            if let Some(unit) = &metadata.unit {
                output.push_str(&format!(
                    "# UNIT {} {}\n",
                    base_name.as_str(),
                    unit.as_canonical_label()
                ));
            }
        }

        // Determine metric type implicitly by checking if it contains data
        if total_counter > 0 {
            output.push_str(&format!("# TYPE {} counter\n", base_name.as_str()));
            output.push_str(&format!(
                "{}{}{} {}\n",
                base_name.as_str(),
                labels_str,
                if labels_str.is_empty() { "" } else { " " },
                total_counter
            ));
        }

        if total_gauge.abs() > f64::EPSILON {
            output.push_str(&format!("# TYPE {} gauge\n", base_name.as_str()));
            output.push_str(&format!(
                "{}{}{} {}\n",
                base_name.as_str(),
                labels_str,
                if labels_str.is_empty() { "" } else { " " },
                total_gauge
            ));
        }

        if hist_count > 0 {
            output.push_str(&format!("# TYPE {} histogram\n", base_name.as_str()));
            let mut cumulative = 0;

            for (i, &limit) in LATENCY_BUCKETS.iter().enumerate() {
                cumulative += total_buckets[i];
                let limit_ms = limit as f64 / 1_000_000.0;
                let le_str = if labels_str.is_empty() {
                    format!("{{le=\"{:.3}\"}}", limit_ms)
                } else {
                    format!(
                        "{}, le=\"{:.3}\"}}",
                        &labels_str[..labels_str.len() - 1],
                        limit_ms
                    )
                };

                output.push_str(&format!(
                    "{}_bucket{} {}\n",
                    base_name.as_str(),
                    le_str,
                    cumulative
                ));
            }

            cumulative += total_buckets[LATENCY_BUCKETS.len()];
            let inf_str = if labels_str.is_empty() {
                "{le=\"+Inf\"}".to_string()
            } else {
                format!("{}, le=\"+Inf\"}}", &labels_str[..labels_str.len() - 1])
            };

            output.push_str(&format!(
                "{}_bucket{} {}\n",
                base_name.as_str(),
                inf_str,
                cumulative
            ));
            output.push_str(&format!(
                "{}_sum{} {}\n",
                base_name.as_str(),
                labels_str,
                hist_sum
            ));
            output.push_str(&format!(
                "{}_count{} {}\n",
                base_name.as_str(),
                labels_str,
                hist_count
            ));
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
        println!("{}", output);
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

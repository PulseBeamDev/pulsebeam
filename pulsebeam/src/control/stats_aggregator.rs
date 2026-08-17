//! Folds per-shard metric reports into one Prometheus exposition.
//!
//! Runs on the control runtime, separate from the controller loop so a scrape
//! can never add latency to participant handling. It holds only values that
//! shards sent it — it never reaches into shard memory.

use std::collections::BTreeMap;

use pulsebeam_runtime::mailbox;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::id::ShardId;
use crate::shard::recorder::{
    Description, HistSnapshot, MetricKey, Schema, ShardStatsReport, bucket_le_label, hist_buckets,
};

const PREFIX: &str = "pulsebeam_shard_";

/// Metrics that keep a `shard` label instead of being summed.
///
/// Aggregation is the default because a shard dimension on every series
/// multiplies cardinality by core count. These are the exceptions: imbalance
/// between shards is the one question summing destroys, and it is the question
/// a thread-per-core node most needs to answer. Ratios belong here too — an
/// intensive quantity summed across shards is meaningless.
const PER_SHARD: &[&str] = &["busy_us", "idle_us", "tick_us", "participants_live"];

fn is_per_shard(name: &str) -> bool {
    PER_SHARD.contains(&name)
}

#[derive(Default)]
struct ShardEntry {
    schema: Option<Schema>,
    schema_epoch: Option<u64>,
    counters: Vec<u64>,
    gauges: Vec<f64>,
    histograms: Vec<HistSnapshot>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

enum Agg {
    Counter(u64),
    Gauge(f64),
    Histogram(Box<HistSnapshot>),
}

struct Group {
    kind: Kind,
    help: Option<String>,
    series: BTreeMap<Vec<(String, String)>, Agg>,
}

pub(crate) struct StatsAggregator {
    shards: Vec<ShardEntry>,
    /// Last value rendered for each counter series, to catch the one way this
    /// design can corrupt a `rate()`: an exported counter going backwards.
    #[cfg(debug_assertions)]
    last_counters: std::collections::HashMap<String, u64>,
}

impl StatsAggregator {
    pub(crate) fn new(shard_count: usize) -> Self {
        Self {
            shards: (0..shard_count).map(|_| ShardEntry::default()).collect(),
            #[cfg(debug_assertions)]
            last_counters: std::collections::HashMap::new(),
        }
    }

    /// Absorb one shard's report.
    ///
    /// A report **replaces** that shard's contribution and never adds to it —
    /// the values are already cumulative. An entry is never cleared, because a
    /// shard dropping out of the sum would make the exported counter fall, and
    /// Prometheus reads any counter decrease as a reset.
    pub(crate) fn absorb(&mut self, report: ShardStatsReport) {
        let idx = report.shard.index();
        let Some(entry) = self.shards.get_mut(idx) else {
            debug_assert!(
                false,
                "report from shard {idx}, which this node has no slot for"
            );
            return;
        };

        if let Some(schema) = report.schema {
            entry.schema = Some(schema);
            entry.schema_epoch = Some(report.schema_epoch);
        }

        // A schema report can be dropped by the lossy lane, leaving values we
        // cannot name. Keep the previous ones rather than guessing; the shard
        // resends the schema on a heartbeat.
        if entry.schema_epoch != Some(report.schema_epoch) {
            return;
        }

        entry.counters = report.counters;
        entry.gauges = report.gauges;
        entry.histograms = report.histograms;
    }

    pub(crate) fn render(&mut self) -> String {
        let mut groups: BTreeMap<String, Group> = BTreeMap::new();

        for (shard_idx, entry) in self.shards.iter().enumerate() {
            let Some(schema) = &entry.schema else {
                continue;
            };
            let shard = ShardId::new(shard_idx);

            for (key, value) in schema.counters.iter().zip(entry.counters.iter()) {
                merge(
                    &mut groups,
                    schema,
                    key,
                    shard,
                    Kind::Counter,
                    Agg::Counter(*value),
                );
            }
            for (key, value) in schema.gauges.iter().zip(entry.gauges.iter()) {
                merge(
                    &mut groups,
                    schema,
                    key,
                    shard,
                    Kind::Gauge,
                    Agg::Gauge(*value),
                );
            }
            for (key, value) in schema.histograms.iter().zip(entry.histograms.iter()) {
                merge(
                    &mut groups,
                    schema,
                    key,
                    shard,
                    Kind::Histogram,
                    Agg::Histogram(Box::new(*value)),
                );
            }
        }

        let mut out = String::new();
        for (name, group) in &groups {
            if let Some(help) = &group.help {
                out.push_str(&format!("# HELP {name} {}\n", escape_help(help)));
            }
            out.push_str(&format!("# TYPE {name} {}\n", group.kind.as_str()));
            for (labels, agg) in &group.series {
                self.render_series(&mut out, name, labels, agg);
            }
        }
        out
    }

    fn render_series(
        &mut self,
        out: &mut String,
        name: &str,
        labels: &[(String, String)],
        agg: &Agg,
    ) {
        match agg {
            Agg::Counter(v) => {
                #[cfg(debug_assertions)]
                {
                    let series = format!("{name}{}", format_labels(labels, None));
                    if let Some(previous) = self.last_counters.get(&series) {
                        debug_assert!(
                            v >= previous,
                            "counter {series} went backwards ({previous} -> {v}); \
                             a shard's contribution left the sum and Prometheus will read a reset"
                        );
                    }
                    self.last_counters.insert(series, *v);
                }
                out.push_str(&format!("{name}{} {v}\n", format_labels(labels, None)));
            }
            Agg::Gauge(v) => {
                out.push_str(&format!(
                    "{name}{} {}\n",
                    format_labels(labels, None),
                    format_float(*v)
                ));
            }
            Agg::Histogram(h) => {
                let mut cumulative = 0u64;
                for (idx, count) in h.buckets.iter().enumerate().take(hist_buckets()) {
                    cumulative = cumulative.saturating_add(*count);
                    out.push_str(&format!(
                        "{name}_bucket{} {cumulative}\n",
                        format_labels(labels, Some(&bucket_le_label(idx)))
                    ));
                }
                debug_assert_eq!(
                    cumulative, h.count,
                    "histogram {name} buckets and count disagree"
                );
                let l = format_labels(labels, None);
                out.push_str(&format!("{name}_sum{l} {}\n", h.sum));
                out.push_str(&format!("{name}_count{l} {}\n", h.count));
            }
        }
    }
}

fn merge(
    groups: &mut BTreeMap<String, Group>,
    schema: &Schema,
    key: &MetricKey,
    shard: ShardId,
    kind: Kind,
    value: Agg,
) {
    let name = format!("{PREFIX}{}", key.name);
    let mut labels = key.labels.clone();
    if is_per_shard(&key.name) {
        labels.push(("shard".to_string(), shard.index().to_string()));
    }
    labels.sort();

    let group = groups.entry(name).or_insert_with(|| Group {
        kind,
        help: description_of(schema, &key.name).map(help_text),
        series: BTreeMap::new(),
    });
    debug_assert_eq!(
        group.kind as u8, kind as u8,
        "metric {} registered with two different types",
        key.name
    );

    match group.series.entry(labels) {
        std::collections::btree_map::Entry::Vacant(slot) => {
            slot.insert(value);
        }
        std::collections::btree_map::Entry::Occupied(mut slot) => {
            accumulate(slot.get_mut(), value);
        }
    }
}

/// Fold one shard's contribution into the running total for a series.
///
/// Counters and extensive gauges sum. Histograms merge bucket-wise, which is
/// exact — the payoff for fixed buckets over sampled quantiles.
fn accumulate(into: &mut Agg, value: Agg) {
    match (into, value) {
        (Agg::Counter(a), Agg::Counter(b)) => *a = a.saturating_add(b),
        (Agg::Gauge(a), Agg::Gauge(b)) => *a += b,
        (Agg::Histogram(a), Agg::Histogram(b)) => {
            for (slot, add) in a.buckets.iter_mut().zip(b.buckets.iter()) {
                *slot = slot.saturating_add(*add);
            }
            a.sum = a.sum.saturating_add(b.sum);
            a.count = a.count.saturating_add(b.count);
        }
        _ => debug_assert!(false, "merged two different metric kinds under one series"),
    }
}

fn help_text(desc: &Description) -> String {
    match desc.unit {
        Some(unit) => format!("{} ({})", desc.text, unit.as_str()),
        None => desc.text.clone(),
    }
}

fn description_of<'a>(schema: &'a Schema, name: &str) -> Option<&'a Description> {
    schema
        .descriptions
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, desc)| desc)
}

fn format_labels(labels: &[(String, String)], le: Option<&str>) -> String {
    if labels.is_empty() && le.is_none() {
        return String::new();
    }
    let mut parts: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape_label(v)))
        .collect();
    if let Some(le) = le {
        parts.push(format!("le=\"{le}\""));
    }
    format!("{{{}}}", parts.join(","))
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn escape_help(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n")
}

fn format_float(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_string()
    } else if v.is_infinite() {
        if v > 0.0 { "+Inf" } else { "-Inf" }.to_string()
    } else {
        format!("{v}")
    }
}

/// Owns the aggregator and answers scrapes.
pub(crate) async fn run(
    shard_count: usize,
    mut stats_rx: mailbox::Receiver<Box<ShardStatsReport>>,
    mut scrape_rx: mailbox::Receiver<oneshot::Sender<String>>,
    shutdown: CancellationToken,
) {
    let mut aggregator = StatsAggregator::new(shard_count);

    loop {
        tokio::select! {
            report = stats_rx.recv() => match report {
                Some(report) => aggregator.absorb(*report),
                None => break,
            },
            request = scrape_rx.recv() => match request {
                Some(reply) => { let _ = reply.send(aggregator.render()); }
                None => break,
            },
            _ = shutdown.cancelled() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(counters: &[&str], histograms: &[&str]) -> Schema {
        Schema {
            counters: counters.iter().map(|n| key(n)).collect(),
            gauges: Vec::new(),
            histograms: histograms.iter().map(|n| key(n)).collect(),
            descriptions: Vec::new(),
        }
    }

    fn key(name: &str) -> MetricKey {
        MetricKey {
            name: name.to_string(),
            labels: Vec::new(),
        }
    }

    fn report(
        shard: usize,
        epoch: u64,
        schema: Option<Schema>,
        counters: &[u64],
    ) -> ShardStatsReport {
        ShardStatsReport {
            shard: ShardId::new(shard),
            schema_epoch: epoch,
            schema,
            counters: counters.to_vec(),
            gauges: Vec::new(),
            histograms: Vec::new(),
        }
    }

    fn value_of(rendered: &str, series: &str) -> Option<f64> {
        rendered.lines().find_map(|line| {
            let (name, value) = line.rsplit_once(' ')?;
            (name == series).then(|| value.parse().ok())?
        })
    }

    #[test]
    fn counters_are_summed_across_shards_into_one_series() {
        let mut agg = StatsAggregator::new(2);
        agg.absorb(report(0, 1, Some(schema(&["drops"], &[])), &[7]));
        agg.absorb(report(1, 1, Some(schema(&["drops"], &[])), &[5]));

        let out = agg.render();
        assert_eq!(value_of(&out, "pulsebeam_shard_drops"), Some(12.0));
        assert!(
            !out.contains("shard=\""),
            "a non-allowlisted metric must not carry a shard label:\n{out}"
        );
    }

    #[test]
    fn allowlisted_metrics_keep_their_shard_label() {
        let mut agg = StatsAggregator::new(2);
        agg.absorb(report(
            0,
            1,
            Some(schema(&["participants_live"], &[])),
            &[3],
        ));
        agg.absorb(report(
            1,
            1,
            Some(schema(&["participants_live"], &[])),
            &[9],
        ));

        let out = agg.render();
        assert_eq!(
            value_of(&out, "pulsebeam_shard_participants_live{shard=\"0\"}"),
            Some(3.0)
        );
        assert_eq!(
            value_of(&out, "pulsebeam_shard_participants_live{shard=\"1\"}"),
            Some(9.0)
        );
    }

    #[test]
    fn a_newer_report_replaces_rather_than_adds() {
        // The property that makes the lossy lane free: reports are cumulative,
        // so absorbing two from one shard must not double-count.
        let mut agg = StatsAggregator::new(1);
        agg.absorb(report(0, 1, Some(schema(&["drops"], &[])), &[7]));
        agg.absorb(report(0, 1, None, &[9]));

        assert_eq!(value_of(&agg.render(), "pulsebeam_shard_drops"), Some(9.0));
    }

    #[test]
    fn a_silent_shard_keeps_contributing_its_last_totals() {
        // If a shard's contribution left the sum the exported counter would
        // fall, and Prometheus reads any counter decrease as a reset —
        // silently corrupting every rate() spanning that scrape.
        let mut agg = StatsAggregator::new(2);
        agg.absorb(report(0, 1, Some(schema(&["drops"], &[])), &[7]));
        agg.absorb(report(1, 1, Some(schema(&["drops"], &[])), &[5]));
        assert_eq!(value_of(&agg.render(), "pulsebeam_shard_drops"), Some(12.0));

        // Shard 1 goes silent; only shard 0 reports again.
        agg.absorb(report(0, 1, None, &[8]));
        assert_eq!(value_of(&agg.render(), "pulsebeam_shard_drops"), Some(13.0));
    }

    #[test]
    fn aggregated_counters_never_decrease_across_scrapes() {
        let mut agg = StatsAggregator::new(3);
        let mut previous = 0.0;
        for round in 1..=5u64 {
            for shard in 0..3 {
                // Shard 2 stops reporting after the second round.
                if shard == 2 && round > 2 {
                    continue;
                }
                let s = (round == 1).then(|| schema(&["drops"], &[]));
                agg.absorb(report(shard, 1, s, &[round * 10]));
            }
            let now = value_of(&agg.render(), "pulsebeam_shard_drops").expect("series");
            assert!(now >= previous, "counter fell from {previous} to {now}");
            previous = now;
        }
    }

    #[test]
    fn values_without_a_known_schema_are_ignored() {
        // A dropped schema report must not make the aggregator invent names.
        let mut agg = StatsAggregator::new(1);
        agg.absorb(report(0, 1, Some(schema(&["drops"], &[])), &[7]));
        agg.absorb(report(0, 2, None, &[999]));

        assert_eq!(
            value_of(&agg.render(), "pulsebeam_shard_drops"),
            Some(7.0),
            "values from an unknown schema epoch must be ignored"
        );
    }

    #[test]
    fn histograms_merge_bucket_wise_and_render_cumulatively() {
        let mut a = HistSnapshot::default();
        let mut b = HistSnapshot::default();
        a.buckets[1] = 2;
        a.buckets[3] = 1;
        a.sum = 10;
        a.count = 3;
        b.buckets[1] = 5;
        b.sum = 5;
        b.count = 5;

        let mut agg = StatsAggregator::new(2);
        for (shard, hist) in [(0, a), (1, b)] {
            agg.absorb(ShardStatsReport {
                shard: ShardId::new(shard),
                schema_epoch: 1,
                schema: Some(schema(&[], &["delay_us"])),
                counters: Vec::new(),
                gauges: Vec::new(),
                histograms: vec![hist],
            });
        }

        let out = agg.render();
        assert_eq!(value_of(&out, "pulsebeam_shard_delay_us_count"), Some(8.0));
        assert_eq!(value_of(&out, "pulsebeam_shard_delay_us_sum"), Some(15.0));

        let buckets: Vec<f64> = out
            .lines()
            .filter(|l| l.starts_with("pulsebeam_shard_delay_us_bucket"))
            .filter_map(|l| l.rsplit_once(' ')?.1.parse().ok())
            .collect();
        assert_eq!(buckets.len(), hist_buckets());
        assert!(
            buckets.windows(2).all(|w| w[1] >= w[0]),
            "buckets must be cumulative: {buckets:?}"
        );
        assert_eq!(
            *buckets.last().expect("a bucket"),
            8.0,
            "+Inf bucket must equal _count"
        );
    }

    #[test]
    fn labels_from_the_call_site_group_series_separately() {
        let mut s = schema(&[], &[]);
        s.counters = vec![
            MetricKey {
                name: "drops".to_string(),
                labels: vec![("reason".to_string(), "late".to_string())],
            },
            MetricKey {
                name: "drops".to_string(),
                labels: vec![("reason".to_string(), "full".to_string())],
            },
        ];

        let mut agg = StatsAggregator::new(2);
        agg.absorb(report(0, 1, Some(s.clone()), &[1, 2]));
        agg.absorb(report(1, 1, Some(s), &[10, 20]));

        let out = agg.render();
        assert_eq!(
            value_of(&out, "pulsebeam_shard_drops{reason=\"late\"}"),
            Some(11.0)
        );
        assert_eq!(
            value_of(&out, "pulsebeam_shard_drops{reason=\"full\"}"),
            Some(22.0)
        );
        assert_eq!(
            out.matches("# TYPE pulsebeam_shard_drops ").count(),
            1,
            "a metric name must carry exactly one TYPE line:\n{out}"
        );
    }

    #[test]
    fn a_real_recorder_renders_end_to_end() {
        // The seam the two halves' own tests cannot see: the aggregator reads
        // values positionally against the schema, so a misalignment between
        // what the recorder allocates and what it names would silently
        // mislabel every series.
        use crate::shard::recorder::ShardRecorder;
        use metrics::{counter, histogram, with_local_recorder};

        let mut agg = StatsAggregator::new(2);
        for shard in 0..2usize {
            let recorder = ShardRecorder::new();
            with_local_recorder(&recorder, || {
                metrics::describe_counter!("packets", "packets forwarded");
                counter!("packets").increment(shard.saturating_add(1) as u64);
                counter!("drops", "reason" => "late").increment(1);
                histogram!("delay_us").record(3.0);
            });
            agg.absorb(recorder.snapshot(ShardId::new(shard)));
        }

        let out = agg.render();
        assert_eq!(value_of(&out, "pulsebeam_shard_packets"), Some(3.0));
        assert_eq!(
            value_of(&out, "pulsebeam_shard_drops{reason=\"late\"}"),
            Some(2.0)
        );
        assert_eq!(value_of(&out, "pulsebeam_shard_delay_us_count"), Some(2.0));
        assert_eq!(value_of(&out, "pulsebeam_shard_delay_us_sum"), Some(6.0));
        assert!(
            out.contains("# HELP pulsebeam_shard_packets packets forwarded"),
            "descriptions must survive the trip:\n{out}"
        );
    }

    #[test]
    fn an_empty_aggregator_renders_nothing() {
        assert!(StatsAggregator::new(4).render().is_empty());
    }
}

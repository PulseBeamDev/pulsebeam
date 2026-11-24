//! Node-local authority. The control runtime samples the host and publishes
//! complete values; shards project them using their existing monotonic time.

use std::path::PathBuf;

use pulsebeam_clock::{
    ntp::{LeapTransition, UtcRules, UtcSecond},
    time::{GlobalDuration, GlobalInstant},
};
use pulsebeam_runtime::{
    sync::CancellationToken,
    time::{Duration, HostClockRead, Instant, MissedTickBehavior, interval, read_host_clocks},
};

const NS_PER_SEC: i64 = 1_000_000_000;
const NTP_UNIX_OFFSET: i64 = 2_208_988_800;
const UTC_1972: UtcSecond = UtcSecond(63_072_000);
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const MAX_AGE_NS: i64 = 3_000_000_000;
const MAX_BRACKET: Duration = Duration::from_millis(2);
const MAX_KERNEL_ERROR: Duration = Duration::from_millis(100);
// 500 ppm, both the rate allowance between observations and the slew limit.
const SLEW_DIVISOR: i64 = 2_000;
const MAX_TABLE_BYTES: usize = 16_384;

#[derive(Debug, PartialEq, Eq)]
struct Policy {
    start: UtcSecond,
    initial_offset: i16,
    end: UtcSecond,
    expires: GlobalInstant,
    transitions: Vec<LeapTransition>,
}

impl Policy {
    fn new(
        start: UtcSecond,
        end: UtcSecond,
        initial_offset: i16,
        transitions: Vec<LeapTransition>,
    ) -> anyhow::Result<Self> {
        UtcRules::new(start, end, initial_offset, &transitions)
            .map_err(|err| anyhow::anyhow!("invalid UTC transition table: {err:?}"))?;
        let offset = transitions
            .last()
            .map_or(initial_offset, |item| item.tai_minus_utc);
        let expires = end
            .0
            .checked_add(i64::from(offset))
            .and_then(|value| value.checked_mul(NS_PER_SEC))
            .and_then(|value| GlobalInstant::from_tai_1970_nanos(value).ok())
            .ok_or_else(|| anyhow::anyhow!("UTC table expiry is not convertible"))?;
        Ok(Self {
            start,
            initial_offset,
            end,
            expires,
            transitions,
        })
    }

    fn rules(&self) -> Option<UtcRules<'_>> {
        UtcRules::new(self.start, self.end, self.initial_offset, &self.transitions).ok()
    }

    fn parse(table: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(table.len() <= MAX_TABLE_BYTES, "oversized UTC table");
        let mut end = None;
        let mut start = false;
        let mut transitions = Vec::new();
        let unix = |value: &str| -> anyhow::Result<UtcSecond> {
            Ok(UtcSecond(
                value
                    .parse::<i64>()?
                    .checked_sub(NTP_UNIX_OFFSET)
                    .ok_or_else(|| anyhow::anyhow!("UTC table label overflow"))?,
            ))
        };
        for line in table.lines().map(str::trim) {
            if let Some(expiry) = line.strip_prefix("#@") {
                anyhow::ensure!(end.is_none(), "duplicate UTC table expiry");
                end = Some(unix(expiry.trim())?);
                continue;
            }
            let Some(row) = line.split('#').next() else {
                continue;
            };
            let mut fields = row.split_whitespace();
            let Some(second) = fields.next() else {
                continue;
            };
            let utc_after = unix(second)?;
            let offset = fields
                .next()
                .ok_or_else(|| anyhow::anyhow!("missing UTC offset"))?
                .parse::<i16>()?;
            anyhow::ensure!(fields.next().is_none(), "unexpected UTC table column");
            if !start {
                anyhow::ensure!(
                    utc_after == UTC_1972 && offset == 10,
                    "missing 1972 UTC origin"
                );
                start = true;
            } else {
                transitions.push(LeapTransition {
                    utc_after,
                    tai_minus_utc: offset,
                });
            }
        }
        anyhow::ensure!(start, "missing UTC table origin");
        Self::new(
            UTC_1972,
            end.ok_or_else(|| anyhow::anyhow!("missing UTC table expiry"))?,
            10,
            transitions,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Discipline {
    Unqualified,
    Locked,
    Holdover,
    Slewing,
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    at: Instant,
    global: GlobalInstant,
    error_ns: Option<u64>,
}

impl Sample {
    fn host(read: HostClockRead, policy: &Policy, unsmeared: bool) -> Option<Self> {
        let bracket = read.after.checked_duration_since(read.before)?;
        let at = read.before.checked_add(bracket.checked_div(2)?)?;
        let global = policy.rules()?.decode_unix_nanos(read.unix_nanos).ok()?;
        let error_ns = read
            .kernel_max_error
            .filter(|error| unsmeared && bracket <= MAX_BRACKET && *error <= MAX_KERNEL_ERROR)
            .and_then(|error| {
                let tai = GlobalInstant::from_tai_1970_nanos(read.tai_nanos?).ok()?;
                let skew = tai.as_nanos().abs_diff(global.as_nanos());
                let width = u64::try_from(bracket.as_nanos()).ok()?;
                // The calls need not be simultaneous, but a discrepancy wider
                // than the whole read bracket is not evidence for this UTC policy.
                (skew <= width).then_some(())?;
                u64::try_from(error.as_nanos()).ok()?.checked_add(width)
            });
        Some(Self {
            at,
            global,
            error_ns,
        })
    }

    fn consistent_with(self, previous: Self) -> Option<bool> {
        let delta = elapsed(self.at, previous.at)?;
        let expected = previous
            .global
            .checked_add(GlobalDuration::from_nanos(delta))
            .ok()?;
        let allowance = self
            .error_ns?
            .checked_add(previous.error_ns?)?
            .checked_add(u64::try_from(delta.checked_div(SLEW_DIVISOR)?).ok()?)?;
        Some(self.global.as_nanos().abs_diff(expected.as_nanos()) <= allowance)
    }
}

fn elapsed(now: Instant, earlier: Instant) -> Option<i64> {
    i64::try_from(now.checked_duration_since(earlier)?.as_nanos()).ok()
}

#[derive(Clone)]
pub(crate) struct Snapshot {
    // Immutable and cloned only on the control refresh and shard handoff.
    #[allow(
        clippy::disallowed_types,
        reason = "immutable UTC policy, not shared packet state"
    )]
    policy: std::sync::Arc<Policy>,
    anchor: Sample,
    correction: i64,
    discipline: Discipline,
    evidence_generation: u64,
    policy_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Estimate {
    pub(crate) global: GlobalInstant,
    pub(crate) uncertainty_ns: Option<u64>,
    pub(crate) discipline: Discipline,
    pub(crate) evidence_generation: u64,
    pub(crate) policy_generation: u64,
}

impl Snapshot {
    pub(crate) fn at(&self, now: Instant) -> Option<Estimate> {
        let age = elapsed(now, self.anchor.at)?;
        let budget = age.min(MAX_AGE_NS).checked_div(SLEW_DIVISOR)?;
        let shift = self.correction.clamp(budget.saturating_neg(), budget);
        let global = self
            .anchor
            .global
            .checked_add(GlobalDuration::from_nanos(age.checked_add(shift)?))
            .ok()?;
        let expired = age >= MAX_AGE_NS || global >= self.policy.expires;
        let discipline = if expired {
            Discipline::Holdover
        } else {
            self.discipline
        };
        let uncertainty_ns = if discipline == Discipline::Locked {
            self.anchor
                .error_ns?
                .checked_add(u64::try_from(age.checked_div(SLEW_DIVISOR)?).ok()?)
        } else {
            None
        };
        Some(Estimate {
            global,
            uncertainty_ns,
            discipline,
            evidence_generation: if expired
                && matches!(self.discipline, Discipline::Locked | Discipline::Slewing)
            {
                self.evidence_generation.saturating_add(1)
            } else {
                self.evidence_generation
            },
            policy_generation: self.policy_generation,
        })
    }
}

/// Shard-owned view of the authority. A delayed mailbox delivery may replace
/// one extrapolation with another that is slightly behind it. Do not send the
/// execution clock backwards or claim locked quality until it catches up.
pub(crate) struct ShardClock {
    snapshot: Snapshot,
    last: Option<(Instant, GlobalInstant)>,
}

impl ShardClock {
    #[cfg(test)]
    pub(crate) fn fixture(at: Instant, global: GlobalInstant) -> Self {
        let policy = Policy::new(
            UtcSecond(1_483_228_800),
            UtcSecond(4_102_444_800),
            37,
            Vec::new(),
        )
        .unwrap();
        let sample = Sample {
            at,
            global,
            error_ns: None,
        };
        Self::new(Authority::start(policy, sample).snapshot)
    }

    pub(crate) fn new(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            last: None,
        }
    }

    pub(crate) fn update(&mut self, snapshot: Snapshot) {
        self.snapshot = snapshot;
    }

    pub(crate) fn estimate_at(&self, at: Instant) -> Option<Estimate> {
        self.snapshot.at(at)
    }

    pub(crate) fn sender_report_time(
        &self,
        at: Instant,
    ) -> Option<pulsebeam_rtc::SenderReportTime> {
        let estimate = self.snapshot.at(at)?;
        use pulsebeam_clock::{ntp::OutgoingRtp, time::ClockRate};
        let opus = OutgoingRtp {
            rate: ClockRate::OPUS,
            modular_offset: 0,
        }
        .sender_report_pair(estimate.global, self.snapshot.policy.rules()?)
        .ok()?;
        let video = OutgoingRtp {
            rate: ClockRate::VIDEO,
            modular_offset: 0,
        }
        .timestamp(estimate.global)
        .ok()?;
        Some(pulsebeam_rtc::SenderReportTime {
            opus_rtp: opus.rtp.0,
            video_rtp: video.0,
            ntp_seconds: opus.ntp.timestamp.seconds,
            ntp_fraction: opus.ntp.timestamp.fraction,
        })
    }

    pub(crate) fn now(&mut self, now: Instant) -> Option<Estimate> {
        let mut estimate = self.snapshot.at(now)?;
        if let Some((previous, floor)) = self.last {
            if now < previous {
                return None;
            }
            if estimate.global < floor {
                estimate.global = floor;
                estimate.uncertainty_ns = None;
                if estimate.discipline == Discipline::Locked {
                    estimate.discipline = Discipline::Slewing;
                }
            }
        }
        self.last = Some((now, estimate.global));
        Some(estimate)
    }
}

struct Authority {
    snapshot: Snapshot,
    last_qualified: Option<Sample>,
}

impl Authority {
    fn start(policy: Policy, sample: Sample) -> Self {
        let discipline = if sample.error_ns.is_some() {
            Discipline::Locked
        } else {
            Discipline::Unqualified
        };
        Self {
            snapshot: Snapshot {
                #[allow(
                    clippy::disallowed_types,
                    reason = "one immutable UTC policy handed to shards"
                )]
                policy: std::sync::Arc::new(policy),
                anchor: sample,
                correction: 0,
                discipline,
                evidence_generation: 0,
                policy_generation: 0,
            },
            last_qualified: sample.error_ns.map(|_| sample),
        }
    }

    fn refresh(&mut self, now: Instant, replacement: Option<Policy>, sample: Option<Sample>) {
        let policy_changed = replacement
            .as_ref()
            .is_some_and(|policy| policy != &*self.snapshot.policy);
        let sample = sample.filter(|sample| sample.error_ns.is_some());
        let at = sample.map_or(now, |sample| sample.at);
        let Some(prior) = self.snapshot.at(at) else {
            return;
        };
        let stable = !policy_changed
            && sample
                .zip(self.last_qualified)
                .is_some_and(|(latest, previous)| {
                    elapsed(latest.at, previous.at).is_some_and(|age| age < MAX_AGE_NS)
                        && latest.consistent_with(previous) == Some(true)
                });
        self.last_qualified = sample;
        let mut anchor = Sample {
            at,
            global: prior.global,
            error_ns: None,
        };
        let mut discipline = if prior.discipline == Discipline::Unqualified && !policy_changed {
            Discipline::Unqualified
        } else {
            Discipline::Holdover
        };
        let mut correction = 0;
        if stable && let Some(sample) = sample {
            correction = sample
                .global
                .as_nanos()
                .saturating_sub(prior.global.as_nanos());
            anchor.error_ns = sample
                .error_ns
                .and_then(|error| error.checked_add(correction.unsigned_abs()));
            discipline = if anchor.error_ns.is_some()
                && sample
                    .error_ns
                    .is_some_and(|error| correction.unsigned_abs() <= error)
            {
                Discipline::Locked
            } else {
                Discipline::Slewing
            };
        }
        let was_tracking = matches!(
            self.snapshot.discipline,
            Discipline::Locked | Discipline::Slewing
        );
        let expired = prior.discipline == Discipline::Holdover;
        if policy_changed || (was_tracking && (!stable || expired)) {
            self.snapshot.evidence_generation = self.snapshot.evidence_generation.saturating_add(1);
        }
        if policy_changed && let Some(policy) = replacement {
            #[allow(clippy::disallowed_types, reason = "immutable replacement UTC policy")]
            let policy = std::sync::Arc::new(policy);
            self.snapshot.policy = policy;
            self.snapshot.policy_generation = self.snapshot.policy_generation.saturating_add(1);
        }
        self.snapshot.anchor = anchor;
        self.snapshot.discipline = discipline;
        self.snapshot.correction = correction;
    }
}

pub(crate) enum Source {
    Host {
        path: PathBuf,
        unsmeared: bool,
    },
    #[cfg(any(test, feature = "sim"))]
    Virtual {
        unix_nanos: fn() -> i64,
    },
}

impl Source {
    pub(crate) fn host(path: PathBuf, unsmeared: bool) -> Self {
        Self::Host { path, unsmeared }
    }

    async fn load_policy(&self) -> anyhow::Result<Policy> {
        match self {
            Self::Host { path, .. } => {
                use std::io::Read;
                let path = path.clone();
                pulsebeam_runtime::task::spawn_blocking(move || {
                    use anyhow::Context;
                    let mut table = String::new();
                    std::fs::File::open(&path)
                        .with_context(|| format!("opening UTC policy {}", path.display()))?
                        .take(u64::try_from(MAX_TABLE_BYTES.saturating_add(1))?)
                        .read_to_string(&mut table)?;
                    Policy::parse(&table)
                })
                .await?
            }
            #[cfg(any(test, feature = "sim"))]
            Self::Virtual { .. } => Policy::new(
                UtcSecond(1_483_228_800),
                UtcSecond(4_102_444_800),
                37,
                Vec::new(),
            ),
        }
    }

    fn sample(&self, policy: &Policy) -> Option<Sample> {
        match self {
            Self::Host { unsmeared, .. } => Sample::host(read_host_clocks()?, policy, *unsmeared),
            #[cfg(any(test, feature = "sim"))]
            Self::Virtual { unix_nanos } => Some(Sample {
                at: Instant::now(),
                global: policy.rules()?.decode_unix_nanos(unix_nanos()).ok()?,
                error_ns: Some(0),
            }),
        }
    }
}

pub(crate) struct ClockService {
    source: Source,
    authority: Authority,
}

impl ClockService {
    pub(crate) async fn start(source: Source) -> anyhow::Result<Self> {
        let policy = source.load_policy().await?;
        let sample = source
            .sample(&policy)
            .ok_or_else(|| anyhow::anyhow!("host UTC is outside convertible policy coverage"))?;
        Ok(Self {
            source,
            authority: Authority::start(policy, sample),
        })
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        self.authority.snapshot.clone()
    }

    pub(crate) async fn run(
        mut self,
        senders: Vec<pulsebeam_queue::Sender<Snapshot>>,
        shutdown: CancellationToken,
    ) {
        let mut timer = interval(SAMPLE_INTERVAL);
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut next_policy = Instant::now();
        loop {
            pulsebeam_runtime::select! {
                _ = shutdown.cancelled() => break,
                now = timer.tick() => {
                    let update = if now >= next_policy {
                        next_policy = now.checked_add(Duration::from_secs(60)).unwrap_or(now);
                        self.source.load_policy().await.ok().and_then(|policy| {
                            if policy == *self.authority.snapshot.policy {
                                return None;
                            }
                            let sample = self.source.sample(&policy)?;
                            Some((policy, sample))
                        })
                    } else {
                        None
                    };
                    let (replacement, sample) = match update {
                        Some((policy, sample)) => (Some(policy), Some(sample)),
                        None => (None, self.source.sample(&self.authority.snapshot.policy)),
                    };
                    self.authority.refresh(Instant::now(), replacement, sample);
                    for sender in &senders {
                        let _ = sender.try_send(self.snapshot());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;

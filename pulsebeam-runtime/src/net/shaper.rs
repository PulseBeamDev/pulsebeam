#![allow(
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::string_slice,
    clippy::indexing_slicing
)] // test / simulation support
//! A bottleneck link for the simulator: capacity, queueing delay, loss, and tail drop.
//!
//! turmoil models latency and loss but has no notion of capacity, so a simulated path will carry
//! any offered load. That makes a whole class of congestion-control behaviour untestable: with
//! nothing to saturate there is no queueing delay, the delay-based estimator never backs off, and
//! a probe that under-delivers cannot drag the estimate down however badly it performs. Every BWE
//! collapse seen in production is unreachable without this.
//!
//! Queueing delay is the point. Dropping excess packets alone would model *loss*, and GCC's
//! trendline estimator reads inter-packet delay, not loss - so a shaper that only drops would
//! leave the interesting signal missing. Packets are held and released on a schedule instead, and
//! dropped only once the backlog exceeds the bottleneck's buffer.
//!
//! Shaping is applied on egress, keyed by destination IP, so one SFU socket gives every client its
//! own downlink. Production builds never see this: the module is compiled only under `sim`.
//!
//! # Ground truth
//!
//! This module is the authority on what a link can actually carry, so it is also the reference an
//! assertion should be written against. [`capacity_at`] and [`stats`] exist for that: a test says
//! "the estimate must reach 80% of capacity" rather than "the viewer must receive 3,000,000
//! bytes". The latter silently encodes link rate, codec, fixture length and current behaviour all
//! at once, and stops meaning anything the moment any of them changes.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use tokio::time::{Duration, Instant};

/// How much delay the bottleneck's buffer may accumulate before it starts dropping.
///
/// This is the queue depth expressed as time rather than bytes, which is how a bottleneck
/// actually behaves and keeps the figure meaningful across rates. 200ms is a typical
/// consumer-router buffer - deep enough to show real bufferbloat before loss sets in.
const DEFAULT_MAX_BACKLOG: Duration = Duration::from_millis(200);

/// How a link's capacity behaves over time.
///
/// Real links do not change in instantaneous steps. A cell handover ramps, a congested shared
/// segment breathes, and a controller that only ever sees square waves is never tested against
/// the thing that actually destabilises it.
#[derive(Clone, Copy, Debug)]
pub enum Capacity {
    Fixed(u64),
    /// Linear transition from `from` to `to` over `over`, then holds at `to`.
    Ramp {
        from: u64,
        to: u64,
        over: Duration,
    },
    /// Triangle wave between `min` and `max`. Triangle rather than sine so the rate of change is
    /// constant and a failure is easy to attribute to a level rather than to a slope.
    Oscillate {
        min: u64,
        max: u64,
        period: Duration,
    },
}

impl Capacity {
    /// Interpolated bitrates arrive as `f64`. `as u64` on a NaN yields 0 and on
    /// a negative yields 0 silently, so the clamp is explicit and the
    /// non-finite case is asserted rather than absorbed.
    fn bps_from_f64(v: f64) -> u64 {
        debug_assert!(v.is_finite(), "interpolated bitrate {v} is not finite");
        if !v.is_finite() || v <= 0.0 {
            return 0;
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to a positive, finite value below u64::MAX above"
        )]
        {
            v.min(u64::MAX as f64) as u64
        }
    }

    fn bits_per_sec_at(&self, elapsed: Duration) -> u64 {
        match *self {
            Capacity::Fixed(bps) => bps,
            Capacity::Ramp { from, to, over } => {
                if elapsed >= over || over.is_zero() {
                    return to;
                }
                let t = elapsed.as_secs_f64() / over.as_secs_f64();
                Self::bps_from_f64(from as f64 + (to as f64 - from as f64) * t)
            }
            Capacity::Oscillate { min, max, period } => {
                if period.is_zero() {
                    return max;
                }
                let phase = (elapsed.as_secs_f64() / period.as_secs_f64()).fract();
                // Triangle: rise over the first half, fall over the second.
                let t = if phase < 0.5 {
                    phase * 2.0
                } else {
                    (1.0 - phase) * 2.0
                };
                Self::bps_from_f64(min as f64 + (max as f64 - min as f64) * t)
            }
        }
    }
}

/// Packet loss behaviour for a link.
#[derive(Clone, Copy, Debug)]
pub enum Loss {
    /// Each packet dropped independently. Easy to reason about, but the least realistic: real
    /// loss arrives in bursts, and a controller tuned against uniform loss is not tested against
    /// what wireless actually does.
    Independent(f64),
    /// Gilbert-Elliott: a good state and a bad state with per-packet transition probabilities.
    /// This is how wireless loss is normally modelled, and it produces the correlated bursts that
    /// a loss-based controller responds to very differently from the same average spread evenly.
    Burst {
        /// Probability of leaving the good state on any packet.
        to_bad: f64,
        /// Probability of leaving the bad state on any packet.
        to_good: f64,
        loss_in_good: f64,
        loss_in_bad: f64,
    },
}

impl Loss {
    pub const NONE: Loss = Loss::Independent(0.0);

    /// Typical Wi-Fi: rare short bursts.
    pub fn wifi() -> Self {
        Loss::Burst {
            to_bad: 0.002,
            to_good: 0.4,
            loss_in_good: 0.0,
            loss_in_bad: 0.35,
        }
    }

    /// Mobile: more frequent and longer bad periods.
    pub fn cellular() -> Self {
        Loss::Burst {
            to_bad: 0.01,
            to_good: 0.2,
            loss_in_good: 0.001,
            loss_in_bad: 0.5,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Limit {
    capacity: Capacity,
    max_backlog: Duration,
    /// When this limit was first applied. Set lazily on first use so callers do not have to hold
    /// a simulated clock at configuration time.
    since: Option<Instant>,
}

/// Per-destination limits. Keyed by IP so a single SFU socket shapes each client separately.
fn limits() -> &'static Mutex<HashMap<IpAddr, Limit>> {
    static LIMITS: std::sync::OnceLock<Mutex<HashMap<IpAddr, Limit>>> = std::sync::OnceLock::new();
    LIMITS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How often a packet is delivered out of order, and by how much.
///
/// Reordering is not loss and must not be modelled as it. GCC reads the spacing between arrivals,
/// so a packet arriving late behind its successors perturbs the delay signal directly; separately
/// the receiver counts a gap it has not yet filled as lost, so reordering shows up twice, in two
/// subsystems, with different time constants. A model that only drops cannot produce either
/// effect.
///
/// Real paths reorder for real reasons - ECMP across unequal paths, a wireless retransmit, a
/// queue serviced out of order - and it is common enough on the public internet that a controller
/// which has never met it is untested against a normal condition.
#[derive(Clone, Copy, Debug, Default)]
pub struct Reorder {
    /// Fraction of packets delayed behind their successors, 0.0..=1.0.
    pub probability: f64,
    /// How far back a reordered packet is pushed. Should exceed the gap between packets or the
    /// packet lands in the same place and nothing is reordered at all.
    pub delay: Duration,
}

impl Reorder {
    pub const NONE: Reorder = Reorder {
        probability: 0.0,
        delay: Duration::ZERO,
    };

    /// A path that occasionally delivers late: 1% of packets held back by 30ms.
    pub fn occasional() -> Self {
        Self {
            probability: 0.01,
            delay: Duration::from_millis(30),
        }
    }
}

fn reorders() -> &'static Mutex<HashMap<IpAddr, Reorder>> {
    static REORDERS: std::sync::OnceLock<Mutex<HashMap<IpAddr, Reorder>>> =
        std::sync::OnceLock::new();
    REORDERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn set_reorder(ip: IpAddr, reorder: Reorder) {
    assert!(
        (0.0..=1.0).contains(&reorder.probability),
        "reorder probability must be between 0 and 1"
    );
    reorders()
        .lock()
        .expect("shaper reorders poisoned")
        .insert(ip, reorder);
}

fn reorder_for(ip: &IpAddr) -> Reorder {
    reorders()
        .lock()
        .expect("shaper reorders poisoned")
        .get(ip)
        .copied()
        .unwrap_or(Reorder::NONE)
}

/// Fraction of datagrams delivered twice, 0.0..=1.0.
///
/// Duplication is rare but real - a retransmitting middlebox, an ECMP path that briefly forwards
/// both ways - and a receiver that has never met it is untested against a normal condition. It is
/// separated from loss because the failure it provokes is the opposite one: not a gap to recover
/// from, but a sequence number arriving twice.
fn duplicates() -> &'static Mutex<HashMap<IpAddr, f64>> {
    static DUPLICATES: std::sync::OnceLock<Mutex<HashMap<IpAddr, f64>>> =
        std::sync::OnceLock::new();
    DUPLICATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn gro_windows() -> &'static Mutex<HashMap<IpAddr, Duration>> {
    static GRO_WINDOWS: std::sync::OnceLock<Mutex<HashMap<IpAddr, Duration>>> =
        std::sync::OnceLock::new();
    GRO_WINDOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn set_gro_window(ip: IpAddr, window: Duration) {
    gro_windows()
        .lock()
        .expect("shaper GRO windows poisoned")
        .insert(ip, window);
}

pub fn gro_enabled(ip: IpAddr) -> bool {
    gro_windows()
        .lock()
        .expect("shaper GRO windows poisoned")
        .get(&ip)
        .is_some_and(|window| !window.is_zero())
}

pub fn set_duplicate(ip: IpAddr, probability: f64) {
    assert!(
        (0.0..=1.0).contains(&probability),
        "duplicate probability must be between 0 and 1"
    );
    duplicates()
        .lock()
        .expect("shaper duplicates poisoned")
        .insert(ip, probability);
}

fn duplicate_for(ip: &IpAddr) -> f64 {
    duplicates()
        .lock()
        .expect("shaper duplicates poisoned")
        .get(ip)
        .copied()
        .unwrap_or(0.0)
}

fn losses() -> &'static Mutex<HashMap<IpAddr, Loss>> {
    static LOSSES: std::sync::OnceLock<Mutex<HashMap<IpAddr, Loss>>> = std::sync::OnceLock::new();
    LOSSES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Observed behaviour of a link, for assertions that need more than throughput.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub delivered: u64,
    pub gso_batches: u64,
    pub gso_segments: u64,
    pub gro_batches: u64,
    pub gro_datagrams: u64,
    pub missing_tx_timestamps: u64,
    /// Dropped because the bottleneck buffer was full. Distinct from configured loss: this is
    /// congestion, and a controller that causes a lot of it is overusing the link.
    pub dropped_overflow: u64,
    /// Dropped by the configured loss model.
    pub dropped_loss: u64,
    /// Delivered behind a packet that was offered after it.
    pub reordered: u64,
    /// Delivered a second time by the duplication model.
    pub duplicated: u64,
    /// Deepest queue occupancy seen, as time. A transient spike, so it says what the worst
    /// moment was and nothing about whether the controller lives there.
    pub max_backlog: Duration,
    /// Queue occupancy summed over delivered packets, so `mean_backlog` can report the queue a
    /// controller actually sits behind. Weighted by packet rather than by time: on a link worth
    /// measuring the two agree closely, and per-packet needs no timer.
    pub backlog_sum: Duration,
}

pub fn record_gso_batch(ip: IpAddr, segments: usize) {
    debug_assert!(segments > 1);
    record(ip, |stats| {
        stats.gso_batches = stats.gso_batches.saturating_add(1);
        stats.gso_segments = stats
            .gso_segments
            .saturating_add(u64::try_from(segments).unwrap_or(u64::MAX));
    });
}

pub fn record_gro_batch(ip: IpAddr, datagrams: usize) {
    debug_assert!(datagrams > 1);
    record(ip, |stats| {
        stats.gro_batches = stats.gro_batches.saturating_add(1);
        stats.gro_datagrams = stats
            .gro_datagrams
            .saturating_add(u64::try_from(datagrams).unwrap_or(u64::MAX));
    });
}

impl Stats {
    /// The standing queue: what a packet typically waits behind, rather than the worst moment.
    ///
    /// This is the bufferbloat measure. A controller that keeps the link full but the queue
    /// shallow is behaving well; one that parks 200ms of queue is not, even though throughput
    /// looks fine in both. A peak cannot tell those apart - it is one sample, and every link
    /// that ever filled has a high one.
    pub fn mean_backlog(&self) -> Duration {
        self.backlog_sum
            .checked_div(u32::try_from(self.delivered).unwrap_or(u32::MAX))
            .unwrap_or_default()
    }
}

fn stats_map() -> &'static Mutex<HashMap<IpAddr, Stats>> {
    static STATS: std::sync::OnceLock<Mutex<HashMap<IpAddr, Stats>>> = std::sync::OnceLock::new();
    STATS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Rate-limit traffic sent to `ip`. Call before the hosts start.
pub fn set_downlink(ip: IpAddr, bits_per_sec: u64) {
    set_capacity(ip, Capacity::Fixed(bits_per_sec), DEFAULT_MAX_BACKLOG);
}

pub fn set_downlink_with_backlog(ip: IpAddr, bits_per_sec: u64, max_backlog: Duration) {
    set_capacity(ip, Capacity::Fixed(bits_per_sec), max_backlog);
}

/// Apply a capacity schedule to `ip`. Resets the schedule's clock.
pub fn set_capacity(ip: IpAddr, capacity: Capacity, max_backlog: Duration) {
    limits().lock().expect("shaper limits poisoned").insert(
        ip,
        Limit {
            capacity,
            max_backlog,
            since: None,
        },
    );
}

/// The link's capacity right now, in bits per second. `None` when unshaped.
///
/// This is the ground truth an assertion should compare against.
pub fn capacity_at(ip: IpAddr, now: Instant) -> Option<u64> {
    let mut guard = limits().lock().expect("shaper limits poisoned");
    let limit = guard.get_mut(&ip)?;
    let since = *limit.since.get_or_insert(now);
    Some(
        limit
            .capacity
            .bits_per_sec_at(now.saturating_duration_since(since)),
    )
}

/// Whether `ip`'s capacity is a constant rather than a schedule.
///
/// An assertion phrased as "within X% of capacity" has no single referent on a ramp or an
/// oscillation, so callers use this to refuse rather than silently compare against whatever the
/// instantaneous value happened to be.
pub fn capacity_is_fixed(ip: IpAddr) -> bool {
    limits()
        .lock()
        .expect("shaper limits poisoned")
        .get(&ip)
        .is_some_and(|l| matches!(l.capacity, Capacity::Fixed(_)))
}

/// Observed link behaviour for `ip` since the last [`reset_stats`].
pub fn stats(ip: IpAddr) -> Stats {
    stats_map()
        .lock()
        .expect("shaper stats poisoned")
        .get(&ip)
        .copied()
        .unwrap_or_default()
}

/// Clear observed behaviour so an assertion describes only the window just run.
/// Clear the counters for `ips` only.
///
/// The map is process-global and plans run in parallel, so clearing all of it would zero the
/// counters of every plan currently mid-window - producing a report that describes a fraction of
/// the traffic that actually flowed, and only when another plan happened to start a step at the
/// wrong moment.
pub fn reset_stats_for(ips: impl IntoIterator<Item = IpAddr>) {
    let mut guard = stats_map().lock().expect("shaper stats poisoned");
    for ip in ips {
        guard.remove(&ip);
    }
}

/// Drop every configured limit. The registry is process-global, so a plan that sets limits must
/// clear them or leak them into whatever runs next.
pub fn clear() {
    limits().lock().expect("shaper limits poisoned").clear();
    losses().lock().expect("shaper losses poisoned").clear();
    reorders().lock().expect("shaper reorders poisoned").clear();
    duplicates()
        .lock()
        .expect("shaper duplicates poisoned")
        .clear();
    gro_windows()
        .lock()
        .expect("shaper GRO windows poisoned")
        .clear();
    reorders().lock().expect("shaper reorders poisoned").clear();
    stats_map().lock().expect("shaper stats poisoned").clear();
}

/// Configure datagram loss for traffic sent to `ip`.
///
/// turmoil's `fail_rate` models a link partition and clears the link's in-flight queue. That is
/// useful for outage tests, but it is not a packet-loss percentage. The simulator uses this
/// bounded, deterministic model instead.
pub fn set_packet_loss(ip: IpAddr, rate: f64) {
    assert!(
        (0.0..=1.0).contains(&rate),
        "packet loss must be between 0 and 1"
    );
    set_loss(ip, Loss::Independent(rate));
}

pub fn set_loss(ip: IpAddr, loss: Loss) {
    losses()
        .lock()
        .expect("shaper losses poisoned")
        .insert(ip, loss);
}

fn loss_for(ip: &IpAddr) -> Loss {
    losses()
        .lock()
        .expect("shaper losses poisoned")
        .get(ip)
        .copied()
        .unwrap_or(Loss::NONE)
}

/// SplitMix64 gives a cheap, deterministic sequence without sharing an RNG between simulator
/// tasks. Only used to sample loss, not for cryptographic purposes.
fn next_uniform(counter: &mut u64) -> f64 {
    let mut value = *counter;
    *counter = counter.wrapping_add(1);
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^= value >> 31;
    (value >> 11) as f64 / (1u64 << 53) as f64
}

struct Queued {
    release_at: Instant,
    dst: SocketAddr,
    buf: Vec<u8>,
}

/// Egress bottleneck state for one socket.
///
/// Shared behind a handle rather than copied: `UdpTransportWriter` is `Clone`, and a bottleneck
/// that each clone owned outright would multiply the link's capacity by the number of clones.
/// One socket is one link.
#[derive(Default, Clone)]
pub struct Shaper(std::sync::Arc<Mutex<ShaperState>>);

/// Seed for the impairment stream, set per plan by the simulator harness.
///
/// Loss, reordering and duplication are all drawn from one SplitMix64 stream
/// per socket. That stream used to start at zero unconditionally, so every plan
/// replayed one fixed impairment sequence no matter how the rest of the
/// simulation was seeded — a seed sweep would have varied latency jitter and
/// key material while feeding the congestion controller the identical pattern
/// of drops every single time.
static IMPAIRMENT_SEED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Seed the impairment stream for this plan. Process-global, like the rest of
/// the shaper registries; nextest gives each plan its own process.
pub fn seed_impairments(seed: u64) {
    IMPAIRMENT_SEED.store(seed, std::sync::atomic::Ordering::Relaxed);
}

struct ShaperState {
    /// When the link to a destination next falls idle. The backlog is this minus now.
    next_free: HashMap<IpAddr, Instant>,
    queue: VecDeque<Queued>,
    loss_counter: u64,
    /// Gilbert-Elliott position per destination: true while in the bad state.
    in_bad_state: HashMap<IpAddr, bool>,
}

impl Default for ShaperState {
    fn default() -> Self {
        Self {
            next_free: HashMap::new(),
            queue: VecDeque::new(),
            loss_counter: IMPAIRMENT_SEED.load(std::sync::atomic::Ordering::Relaxed),
            in_bad_state: HashMap::new(),
        }
    }
}

/// What the caller should do with a packet offered to the shaper.
pub enum Shaped {
    /// No limit configured for this destination - send it now.
    PassThrough,
    /// Held for later release, or dropped because the buffer was full. Either way the caller
    /// must not send it.
    Absorbed,
}

impl Shaper {
    fn with<R>(&self, f: impl FnOnce(&mut ShaperState) -> R) -> R {
        f(&mut self.0.lock().expect("shaper state poisoned"))
    }

    /// Offer a packet to the bottleneck.
    pub fn offer(&mut self, now: Instant, dst: SocketAddr, buf: &[u8]) -> Shaped {
        let Some(bits_per_sec) = capacity_at(dst.ip(), now) else {
            // No capacity to model, but reordering is a property of the path rather than of its
            // rate: an uncapped link still delivers out of order. Hold the sampled packet in the
            // queue so the ones offered behind it genuinely leave first.
            let reorder = reorder_for(&dst.ip());
            if reorder.probability > 0.0
                && self.with(|st| next_uniform(&mut st.loss_counter) < reorder.probability)
            {
                record(dst.ip(), |s| s.reordered += 1);
                self.with(|st| {
                    st.queue.push_back(Queued {
                        release_at: now + reorder.delay,
                        dst,
                        buf: buf.to_vec(),
                    });
                    st.queue
                        .make_contiguous()
                        .sort_by_key(|q: &Queued| q.release_at);
                });
                return Shaped::Absorbed;
            }
            return Shaped::PassThrough;
        };
        let max_backlog = limits()
            .lock()
            .expect("shaper limits poisoned")
            .get(&dst.ip())
            .map(|l| l.max_backlog)
            .unwrap_or(DEFAULT_MAX_BACKLOG);
        let reorder = reorder_for(&dst.ip());
        self.with(|st| st.offer(now, dst, buf, bits_per_sec, max_backlog, reorder))
    }

    /// Take every packet whose turn on the wire has come.
    ///
    /// Driven by the socket's release task from `next_release`, not by send attempts. Tying
    /// departure to when the caller next sends would release everything already due back to back,
    /// destroying the inter-packet spacing a receiver measures - which is the whole signal a probe
    /// carries.
    pub fn drain_due(&mut self, now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        self.with(|st| st.drain_due(now))
    }

    pub fn is_empty(&self) -> bool {
        self.with(|st| st.queue.is_empty())
    }

    /// When the next queued packet is due, if anything is queued.
    ///
    /// Lets the release loop sleep until exactly that moment instead of polling, which is what
    /// makes the departure schedule faithful rather than quantised to whenever the caller
    /// happened to look.
    pub fn next_release(&self) -> Option<Instant> {
        self.with(|st| st.queue.front().map(|q| q.release_at))
    }

    /// Return whether the next datagram to `ip` should also be sent a second time.
    pub fn should_duplicate_packet(&mut self, ip: IpAddr) -> bool {
        let probability = duplicate_for(&ip);
        if probability == 0.0 {
            return false;
        }
        let duplicate = self.with(|st| next_uniform(&mut st.loss_counter) < probability);
        if duplicate {
            record(ip, |s| s.duplicated += 1);
        }
        duplicate
    }

    /// Return whether the next datagram to `ip` should be dropped by the loss model.
    pub fn should_drop_packet(&mut self, ip: IpAddr) -> bool {
        let loss = loss_for(&ip);
        let drop = self.with(|st| st.sample_loss(ip, loss));
        if drop {
            record(ip, |s| s.dropped_loss += 1);
        }
        drop
    }
}

fn record(ip: IpAddr, f: impl FnOnce(&mut Stats)) {
    let mut guard = stats_map().lock().expect("shaper stats poisoned");
    f(guard.entry(ip).or_default());
}

impl ShaperState {
    fn sample_loss(&mut self, ip: IpAddr, loss: Loss) -> bool {
        match loss {
            Loss::Independent(rate) => {
                if rate == 0.0 {
                    return false;
                }
                next_uniform(&mut self.loss_counter) < rate
            }
            Loss::Burst {
                to_bad,
                to_good,
                loss_in_good,
                loss_in_bad,
            } => {
                let bad = *self.in_bad_state.entry(ip).or_insert(false);
                let transition = next_uniform(&mut self.loss_counter);
                let bad = if bad {
                    transition >= to_good
                } else {
                    transition < to_bad
                };
                self.in_bad_state.insert(ip, bad);
                let rate = if bad { loss_in_bad } else { loss_in_good };
                next_uniform(&mut self.loss_counter) < rate
            }
        }
    }

    fn offer(
        &mut self,
        now: Instant,
        dst: SocketAddr,
        buf: &[u8],
        bits_per_sec: u64,
        max_backlog: Duration,
        reorder: Reorder,
    ) -> Shaped {
        // Serialisation delay: how long this packet occupies the link.
        let on_wire =
            Duration::from_secs_f64((buf.len() as f64 * 8.0) / bits_per_sec.max(1) as f64);

        let idle_at = self.next_free.entry(dst.ip()).or_insert(now);
        // A link idle in the past is idle now; it does not accrue credit.
        let release_at = (*idle_at).max(now);
        let backlog = release_at.saturating_duration_since(now);

        if backlog > max_backlog {
            // Buffer full. Tail drop, exactly as a bottleneck queue does.
            record(dst.ip(), |s| s.dropped_overflow += 1);
            return Shaped::Absorbed;
        }

        *idle_at = release_at + on_wire;
        record(dst.ip(), |s| {
            s.delivered += 1;
            s.max_backlog = s.max_backlog.max(backlog);
            s.backlog_sum = s.backlog_sum.saturating_add(backlog);
        });

        // Reordering is applied to the release time, not by shuffling the queue, so the packet
        // genuinely leaves after ones offered behind it. Delaying it in place would only add
        // jitter; the queue is re-sorted below so departure order actually changes.
        let mut release_at = release_at;
        if reorder.probability > 0.0 && next_uniform(&mut self.loss_counter) < reorder.probability {
            release_at += reorder.delay;
            record(dst.ip(), |s| s.reordered += 1);
        }

        self.queue.push_back(Queued {
            release_at,
            dst,
            buf: buf.to_vec(),
        });
        // `drain_due` releases from the front while the front is due, so the queue has to stay
        // ordered by release time for a delayed packet to be overtaken rather than to hold up
        // everything behind it - which would be a stall, not a reorder.
        self.queue
            .make_contiguous()
            .sort_by_key(|q: &Queued| q.release_at);
        Shaped::Absorbed
    }

    fn drain_due(&mut self, now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some(front) = self.queue.front() {
            if front.release_at > now {
                break;
            }
            let q = self.queue.pop_front().expect("front just checked");
            out.push((q.dst, q.buf));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_interpolates_then_holds() {
        let c = Capacity::Ramp {
            from: 1_000_000,
            to: 3_000_000,
            over: Duration::from_secs(10),
        };
        assert_eq!(c.bits_per_sec_at(Duration::ZERO), 1_000_000);
        assert_eq!(c.bits_per_sec_at(Duration::from_secs(5)), 2_000_000);
        assert_eq!(c.bits_per_sec_at(Duration::from_secs(10)), 3_000_000);
        assert_eq!(c.bits_per_sec_at(Duration::from_secs(60)), 3_000_000);
    }

    #[test]
    fn oscillate_is_a_triangle_between_bounds() {
        let c = Capacity::Oscillate {
            min: 1_000_000,
            max: 2_000_000,
            period: Duration::from_secs(4),
        };
        assert_eq!(c.bits_per_sec_at(Duration::ZERO), 1_000_000);
        assert_eq!(c.bits_per_sec_at(Duration::from_secs(2)), 2_000_000);
        assert_eq!(c.bits_per_sec_at(Duration::from_secs(4)), 1_000_000);
    }

    /// The bad state has to persist across packets, otherwise "burst" loss is just independent
    /// loss with extra steps - which is the whole reason this model exists.
    #[test]
    fn burst_loss_is_correlated() {
        let mut st = ShaperState::default();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        let loss = Loss::Burst {
            to_bad: 0.05,
            to_good: 0.2,
            loss_in_good: 0.0,
            loss_in_bad: 1.0,
        };

        let drops: Vec<bool> = (0..5000).map(|_| st.sample_loss(ip, loss)).collect();
        let total: usize = drops.iter().filter(|d| **d).count();
        assert!(total > 0, "expected some loss");

        // Consecutive-drop runs longer than 1 are the signature of correlation; independent loss
        // at this rate would essentially never produce them.
        let runs = drops.windows(2).filter(|w| w[0] && w[1]).count();
        assert!(
            runs > total / 10,
            "loss should arrive in runs, got {runs} adjacent pairs out of {total} drops"
        );
    }

    /// A burst offered all at once must leave spread over the time the link needs to carry it.
    ///
    /// This is the property every probe measurement depends on: the receiver reads a probe's
    /// rate from how far apart its packets arrive, so if a burst leaves faster than the link can
    /// carry it, the estimate is of the shaper rather than of the link.
    #[test]
    fn a_burst_is_released_over_the_time_the_link_needs() {
        // Note the unique IP and the absence of `clear()`: the registries are process-global and
        // tests run in parallel, so clearing them is clearing somebody else's link.
        let ip: IpAddr = "9.9.9.9".parse().unwrap();
        let dst = SocketAddr::new(ip, 1234);
        // Buffer deep enough that nothing is dropped for arriving early.
        set_downlink_with_backlog(ip, 3_000_000, Duration::from_secs(5));

        let mut shaper = Shaper::default();
        let start = Instant::now();
        // 13 x 1054 B = 13702 B, which at 3 Mbps is 36.5ms of serialisation.
        for _ in 0..13 {
            shaper.offer(start, dst, &[0u8; 1054]);
        }
        let expected = Duration::from_secs_f64(13.0 * 1054.0 * 8.0 / 3_000_000.0);

        let half = shaper.drain_due(start + expected / 2).len();
        assert!(
            half < 13,
            "half the serialisation time released the whole burst ({half} packets); a probe \
             measured against this would read the shaper, not the link"
        );
        shaper.drain_due(start + expected + Duration::from_millis(1));
        assert!(
            shaper.is_empty(),
            "the burst should be fully released once its serialisation time has elapsed"
        );
    }

    /// A reordered packet must be *overtaken*, not merely delayed, and must not hold up the
    /// packets behind it.
    ///
    /// The distinction is the whole point. Delaying a packet in place is jitter, and holding the
    /// queue behind it is a stall - neither is reordering, and a controller responds to the three
    /// quite differently.
    #[test]
    fn a_reordered_packet_is_overtaken_rather_than_stalling_the_queue() {
        let ip: IpAddr = "9.9.9.10".parse().unwrap();
        let dst = SocketAddr::new(ip, 1234);
        set_downlink_with_backlog(ip, 10_000_000, Duration::from_secs(5));
        // Every packet is pushed back, so ordering is decided purely by the delay.
        set_reorder(
            ip,
            Reorder {
                probability: 1.0,
                delay: Duration::from_millis(50),
            },
        );

        let mut shaper = Shaper::default();
        let start = Instant::now();
        // First packet is reordered; the rest are offered far enough behind that they overtake it.
        shaper.offer(start, dst, &[0u8; 200]);
        set_reorder(ip, Reorder::NONE);
        for _ in 0..3 {
            shaper.offer(start + Duration::from_millis(10), dst, &[0u8; 200]);
        }

        let early = shaper.drain_due(start + Duration::from_millis(20));
        assert_eq!(
            early.len(),
            3,
            "the three later packets should have overtaken the delayed one, not queued behind it"
        );
        assert!(
            !shaper.is_empty(),
            "the delayed packet should still be waiting for its turn"
        );

        let late = shaper.drain_due(start + Duration::from_millis(60));
        assert_eq!(late.len(), 1, "the delayed packet should arrive afterwards");
        assert_eq!(stats(ip).reordered, 1);
    }

    /// A brief dip into the buffer must not read as a standing queue.
    ///
    /// The two are opposite verdicts on a controller: a burst that drains is correct behaviour on
    /// any link that changes, while a queue held at the same depth is bufferbloat — the link looks
    /// full and the call is unusable. A peak cannot tell them apart, because every link that ever
    /// filled has a high one, so a bufferbloat check written against the peak passes the
    /// controller that parks and fails the one that recovers.
    #[test]
    fn a_burst_that_drains_does_not_read_as_a_standing_queue() {
        let ip: IpAddr = "9.10.11.12".parse().unwrap();
        let dst = SocketAddr::new(ip, 1234);
        set_downlink_with_backlog(ip, 1_000_000, Duration::from_millis(500));

        let mut shaper = Shaper::default();
        let start = Instant::now();

        // A burst deep enough to build a queue, then a long quiet stretch where each packet finds
        // the link idle — the shape of a controller that overshot once and backed off.
        for _ in 0..20 {
            shaper.offer(start, dst, &[0u8; 1200]);
        }
        for i in 0..200u32 {
            shaper.offer(
                start + Duration::from_millis(500 + u64::from(i) * 20),
                dst,
                &[0u8; 1200],
            );
        }

        let s = stats(ip);
        assert!(
            s.max_backlog > Duration::from_millis(100),
            "the burst should have built a real queue, or this proves nothing (max {:?})",
            s.max_backlog
        );
        assert!(
            s.mean_backlog() < Duration::from_millis(20),
            "a queue that drained and stayed drained reported a standing depth of {:?} behind a \
             {:?} peak; the standing measure is tracking the spike instead of the steady state",
            s.mean_backlog(),
            s.max_backlog
        );
    }

    #[test]
    fn overflow_is_recorded_separately_from_configured_loss() {
        let ip: IpAddr = "5.6.7.8".parse().unwrap();
        let dst = SocketAddr::new(ip, 1234);
        set_downlink_with_backlog(ip, 100_000, Duration::from_millis(50));

        let mut shaper = Shaper::default();
        let now = Instant::now();
        // Far more than a 100kbps link can hold in 50ms of buffer.
        for _ in 0..200 {
            shaper.offer(now, dst, &[0u8; 1200]);
        }

        let s = stats(ip);
        assert!(s.delivered > 0, "some packets should be queued");
        assert!(s.dropped_overflow > 0, "the buffer should have overflowed");
        assert_eq!(s.dropped_loss, 0, "no loss model was configured");
    }
}

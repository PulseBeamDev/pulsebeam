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
    fn bits_per_sec_at(&self, elapsed: Duration) -> u64 {
        match *self {
            Capacity::Fixed(bps) => bps,
            Capacity::Ramp { from, to, over } => {
                if elapsed >= over || over.is_zero() {
                    return to;
                }
                let t = elapsed.as_secs_f64() / over.as_secs_f64();
                (from as f64 + (to as f64 - from as f64) * t) as u64
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
                (min as f64 + (max as f64 - min as f64) * t) as u64
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

fn losses() -> &'static Mutex<HashMap<IpAddr, Loss>> {
    static LOSSES: std::sync::OnceLock<Mutex<HashMap<IpAddr, Loss>>> = std::sync::OnceLock::new();
    LOSSES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Observed behaviour of a link, for assertions that need more than throughput.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub delivered: u64,
    /// Dropped because the bottleneck buffer was full. Distinct from configured loss: this is
    /// congestion, and a controller that causes a lot of it is overusing the link.
    pub dropped_overflow: u64,
    /// Dropped by the configured loss model.
    pub dropped_loss: u64,
    /// Delivered behind a packet that was offered after it.
    pub reordered: u64,
    /// Deepest queue occupancy seen, as time. This is the bufferbloat measure - a controller that
    /// keeps the link full but the queue shallow is behaving well; one that drives 200ms of
    /// standing queue is not, even if throughput looks fine.
    pub max_backlog: Duration,
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
pub fn reset_stats() {
    stats_map().lock().expect("shaper stats poisoned").clear();
}

/// Drop every configured limit. The registry is process-global, so a plan that sets limits must
/// clear them or leak them into whatever runs next.
pub fn clear() {
    limits().lock().expect("shaper limits poisoned").clear();
    losses().lock().expect("shaper losses poisoned").clear();
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

#[derive(Default)]
struct ShaperState {
    /// When the link to a destination next falls idle. The backlog is this minus now.
    next_free: HashMap<IpAddr, Instant>,
    queue: VecDeque<Queued>,
    loss_counter: u64,
    /// Gilbert-Elliott position per destination: true while in the bad state.
    in_bad_state: HashMap<IpAddr, bool>,
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
    /// The caller drains on each send attempt rather than from a timer, so release is quantised
    /// to how often the event loop sends. While media is flowing that is sub-millisecond; a
    /// socket that goes completely idle holds its backlog until the next send, which is
    /// immaterial for a plan that is measuring a link under load.
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

//! A bottleneck link for the simulator: rate limit, queueing delay, and tail drop.
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

#[derive(Clone, Copy, Debug)]
struct Limit {
    bits_per_sec: u64,
    max_backlog: Duration,
}

/// Per-destination limits. Keyed by IP so a single SFU socket shapes each client separately.
fn limits() -> &'static Mutex<HashMap<IpAddr, Limit>> {
    static LIMITS: std::sync::OnceLock<Mutex<HashMap<IpAddr, Limit>>> = std::sync::OnceLock::new();
    LIMITS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Rate-limit traffic sent to `ip`. Call before the hosts start.
pub fn set_downlink(ip: IpAddr, bits_per_sec: u64) {
    set_downlink_with_backlog(ip, bits_per_sec, DEFAULT_MAX_BACKLOG);
}

pub fn set_downlink_with_backlog(ip: IpAddr, bits_per_sec: u64, max_backlog: Duration) {
    limits().lock().expect("shaper limits poisoned").insert(
        ip,
        Limit {
            bits_per_sec,
            max_backlog,
        },
    );
}

/// Drop every configured limit. The registry is process-global, so a plan that sets limits must
/// clear them or leak them into whatever runs next.
pub fn clear() {
    limits().lock().expect("shaper limits poisoned").clear();
}

fn limit_for(ip: &IpAddr) -> Option<Limit> {
    limits()
        .lock()
        .expect("shaper limits poisoned")
        .get(ip)
        .copied()
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
    dropped: u64,
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
        let Some(limit) = limit_for(&dst.ip()) else {
            return Shaped::PassThrough;
        };
        self.with(|st| st.offer(now, dst, buf, limit))
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
}

impl ShaperState {
    fn offer(&mut self, now: Instant, dst: SocketAddr, buf: &[u8], limit: Limit) -> Shaped {

        // Serialisation delay: how long this packet occupies the link.
        let on_wire =
            Duration::from_secs_f64((buf.len() as f64 * 8.0) / limit.bits_per_sec.max(1) as f64);

        let idle_at = self.next_free.entry(dst.ip()).or_insert(now);
        // A link idle in the past is idle now; it does not accrue credit.
        let release_at = (*idle_at).max(now);

        if release_at.saturating_duration_since(now) > limit.max_backlog {
            // Buffer full. Tail drop, exactly as a bottleneck queue does.
            self.dropped += 1;
            if self.dropped.is_multiple_of(200) {
                tracing::debug!(
                    dropped = self.dropped,
                    %dst,
                    "shaper: bottleneck buffer overflowing"
                );
            }
            return Shaped::Absorbed;
        }

        *idle_at = release_at + on_wire;
        self.queue.push_back(Queued {
            release_at,
            dst,
            buf: buf.to_vec(),
        });
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

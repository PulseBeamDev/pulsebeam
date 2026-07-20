use crate::media::KeyframeNotifier;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use str0m::bwe::Bitrate;
use str0m::media::{KeyframeRequestKind, Mid, Rid};
use tokio::time::Instant;

const BWE_DEFAULT: Bitrate = Bitrate::kbps(500);
const DEBT_LINGER_THRESHOLD: f64 = 0.10;
const DEBT_LINGER_MAX_TICKS: u32 = 5;
const DEBT_IMMEDIATE_THRESHOLD: f64 = 0.25;
const KEYFRAME_REQUEST_THROTTLE: Duration = Duration::from_secs(1);

fn layer_seed_bps(rid: Option<Rid>) -> f64 {
    match rid_quality_rank(rid) {
        0 => 30_000.0,
        1 => 900_000.0,
        2 => 1_400_000.0,
        _ => 30_000.0,
    }
}

fn rid_quality_rank(rid: Option<Rid>) -> u8 {
    match rid.as_ref().map(|r| r.as_ref()) {
        Some("q") => 0,
        Some("h") => 1,
        Some("f") => 2,
        _ => 255,
    }
}

pub struct BitrateEstimate {
    tick_start: Option<Instant>,
    accumulated_bytes: usize,
    raw_ticks: VecDeque<f64>,
    sma1_ticks: VecDeque<f64>,
    max_window_ticks: usize,
    baseline_bps: f64,
    fast_trend_bps: f64,
}

impl BitrateEstimate {
    const HEADROOM: f64 = 1.05;
    const TICK_MS: f64 = 500.0;

    pub fn new_with_seed(seed_bps: f64) -> Self {
        let mut raw_ticks = VecDeque::with_capacity(6);
        let mut sma1_ticks = VecDeque::with_capacity(6);
        for _ in 0..6 {
            raw_ticks.push_back(seed_bps);
            sma1_ticks.push_back(seed_bps);
        }
        Self {
            tick_start: None,
            accumulated_bytes: 0,
            raw_ticks,
            sma1_ticks,
            max_window_ticks: 6,
            baseline_bps: seed_bps,
            fast_trend_bps: seed_bps,
        }
    }

    pub fn record_bytes(&mut self, bytes: usize, now: Instant) {
        self.advance_time(now);
        self.accumulated_bytes += bytes;
    }

    pub fn poll(&mut self, current_time: Instant) {
        self.advance_time(current_time);
    }

    fn advance_time(&mut self, time: Instant) {
        let current_tick = *self.tick_start.get_or_insert(time);

        if time < current_tick + Duration::from_millis(Self::TICK_MS as u64) {
            return;
        }

        let elapsed = time.saturating_duration_since(current_tick);
        let ticks_passed = (elapsed.as_millis() / Self::TICK_MS as u128) as usize;

        let instant_bps = (self.accumulated_bytes as f64 * 8.0 * 1000.0) / Self::TICK_MS;
        self.push_tick(instant_bps);

        let empty_ticks = ticks_passed.saturating_sub(1).min(1000);
        for _ in 0..empty_ticks {
            self.push_tick(0.0);
        }

        self.accumulated_bytes = 0;
        self.tick_start = Some(
            current_tick + Duration::from_millis((ticks_passed as u64) * Self::TICK_MS as u64),
        );
    }

    fn push_tick(&mut self, bps: f64) {
        if self.raw_ticks.len() == self.max_window_ticks {
            self.raw_ticks.pop_front();
        }
        self.raw_ticks.push_back(bps);

        let sma1 = self.raw_ticks.iter().sum::<f64>() / self.raw_ticks.len() as f64;

        if self.sma1_ticks.len() == self.max_window_ticks {
            self.sma1_ticks.pop_front();
        }
        self.sma1_ticks.push_back(sma1);

        self.baseline_bps = self.sma1_ticks.iter().sum::<f64>() / self.sma1_ticks.len() as f64;

        let recent_len = self.raw_ticks.len();
        if recent_len >= 3 {
            let a = self.raw_ticks[recent_len - 1];
            let b = self.raw_ticks[recent_len - 2];
            let c = self.raw_ticks[recent_len - 3];
            self.fast_trend_bps = a.max(b.min(c)).min(b.max(c));
        } else {
            self.fast_trend_bps = 0.0;
        }
    }

    pub fn estimate_bps(&self) -> f64 {
        self.baseline_bps.max(self.fast_trend_bps) * Self::HEADROOM
    }
}

pub struct BitrateControllerConfig {
    pub min_bitrate: Bitrate,
    pub max_bitrate: Bitrate,
    pub default_bitrate: Bitrate,
    pub headroom_factor: f64,
    pub down_smoothing: f64,
    pub quantization_step: Bitrate,
    pub hysteresis: Bitrate,
}

impl Default for BitrateControllerConfig {
    fn default() -> Self {
        Self {
            min_bitrate: BWE_DEFAULT,
            max_bitrate: Bitrate::mbps(10),
            default_bitrate: BWE_DEFAULT,
            headroom_factor: 1.10,
            down_smoothing: 0.99,
            quantization_step: Bitrate::kbps(200),
            hysteresis: Bitrate::kbps(250),
        }
    }
}

impl BitrateControllerConfig {
    pub fn build(self) -> BitrateController {
        BitrateController::new(self)
    }
}

pub struct BitrateController {
    config: BitrateControllerConfig,
    current_bitrate: f64,
    down_estimate: f64,
}

impl BitrateController {
    pub fn new(config: BitrateControllerConfig) -> Self {
        let initial_bitrate = config.default_bitrate.as_f64();
        Self {
            config,
            current_bitrate: initial_bitrate,
            down_estimate: initial_bitrate,
        }
    }

    pub fn update(&mut self, desired_bitrate: Bitrate) -> Bitrate {
        let raw = desired_bitrate.as_f64() * self.config.headroom_factor;

        if raw > self.down_estimate {
            self.down_estimate = raw;
        } else {
            self.down_estimate = self.down_estimate * self.config.down_smoothing
                + raw * (1.0 - self.config.down_smoothing);
            if self.down_estimate - raw < 1.0 {
                self.down_estimate = raw;
            }
        }

        let deadband = self.config.hysteresis.as_f64();
        let step = self.config.quantization_step.as_f64();
        let target = ((self.down_estimate / step) - 1e-9).max(0.0).ceil() * step;

        if target > self.current_bitrate || self.current_bitrate - target >= deadband {
            self.current_bitrate = target;
        }

        self.current()
    }

    pub fn current(&self) -> Bitrate {
        let current_bitrate = self.current_bitrate * 1.50;
        let current_bitrate = current_bitrate.clamp(
            self.config.min_bitrate.as_f64(),
            self.config.max_bitrate.as_f64(),
        );
        Bitrate::from(current_bitrate)
    }
}

pub struct LayerController {
    available_bps: f64,
    last_keyframe_request: HashMap<(Mid, Option<Rid>), Instant>,
    order: Vec<(Mid, Option<Rid>)>,
    states: HashMap<(Mid, Option<Rid>), LayerState>,
    notifiers: HashMap<(Mid, Option<Rid>), KeyframeNotifier>,
    debt_ticks: u32,
}

struct LayerState {
    bps: f64,
    paused: bool,
    estimate: BitrateEstimate,
}

impl LayerController {
    pub fn new() -> Self {
        Self {
            available_bps: f64::MAX,
            last_keyframe_request: HashMap::new(),
            order: Vec::new(),
            states: HashMap::new(),
            notifiers: HashMap::new(),
            debt_ticks: 0,
        }
    }

    pub fn register(&mut self, mid: Mid, rid: Option<Rid>, notifier: KeyframeNotifier) {
        let key = (mid, rid);
        self.order.push(key);
        self.states.insert(
            key,
            LayerState {
                bps: layer_seed_bps(rid),
                paused: true,
                estimate: BitrateEstimate::new_with_seed(layer_seed_bps(rid)),
            },
        );
        self.notifiers.insert(key, notifier);
    }

    pub fn update_available(&mut self, bw: Bitrate) {
        self.available_bps = bw.as_f64();
    }

    pub fn record_frame(&mut self, mid: Mid, rid: Option<Rid>, byte_len: usize, now: Instant) {
        let key = (mid, rid);
        let Some(s) = self.states.get_mut(&key) else {
            return;
        };
        s.estimate.record_bytes(byte_len, now);
    }

    fn refresh_bitrate_estimates(&mut self, now: Instant) {
        for s in self.states.values_mut() {
            s.estimate.poll(now);
            s.bps = s.estimate.estimate_bps();
        }
    }

    pub fn is_paused(&self, mid: Mid, rid: Option<Rid>) -> bool {
        self.states.get(&(mid, rid)).is_some_and(|s| s.paused)
    }

    pub fn request_keyframe(&mut self, mid: Mid, rid: Option<Rid>, kind: KeyframeRequestKind) {
        if kind == KeyframeRequestKind::Pli {
            let key = (mid, rid);
            let now = Instant::now();
            if let Some(last) = self.last_keyframe_request.get(&key)
                && now.duration_since(*last) < KEYFRAME_REQUEST_THROTTLE
            {
                tracing::debug!(mid = ?mid, rid = ?rid, "throttling repeated PLI");
                return;
            }
            self.last_keyframe_request.insert(key, now);
        }

        if let Some(n) = self.notifiers.get(&(mid, rid)) {
            n.notify();
        }
    }

    pub fn tick(&mut self, now: Instant) -> f64 {
        if self.order.is_empty() {
            return 0.0;
        }

        self.refresh_bitrate_estimates(now);

        let desired: f64 = self
            .order
            .iter()
            .filter_map(|k| self.states.get(k))
            .map(|s| s.bps)
            .sum();

        if self.available_bps == f64::MAX {
            return desired;
        }

        let allocated: f64 = self
            .order
            .iter()
            .filter_map(|k| self.states.get(k))
            .filter(|s| !s.paused)
            .map(|s| s.bps)
            .sum();

        let debt = allocated - self.available_bps;

        if debt > self.available_bps * DEBT_LINGER_THRESHOLD {
            self.debt_ticks += 1;
        } else {
            self.debt_ticks = 0;
        }

        let must_shed = debt > self.available_bps * DEBT_IMMEDIATE_THRESHOLD
            || self.debt_ticks >= DEBT_LINGER_MAX_TICKS;

        if must_shed {
            let unpaused_simulcast_layers = self
                .order
                .iter()
                .filter(|k| k.1.is_some() && self.states.get(*k).is_some_and(|s| !s.paused))
                .count();
            // Never pause the last remaining layer — uplink BWE can read
            // low from transient jitter, not just a real capacity limit, so
            // degrade to the cheapest layer rather than going silent.
            if unpaused_simulcast_layers > 1
                // Shed the most expensive layer first; rank by quality, not
                // registration order (insertion order isn't guaranteed high-to-low).
                && let Some(key) = self
                    .order
                    .iter()
                    .filter(|k| k.1.is_some() && self.states.get(*k).is_some_and(|s| !s.paused))
                    .max_by_key(|k| rid_quality_rank(k.1))
                    .cloned()
            {
                if let Some(s) = self.states.get_mut(&key) {
                    s.paused = true;
                }
                self.debt_ticks = 0;
                tracing::info!(mid = ?key.0, rid = ?key.1, "bwe: pause layer");
            }
        } else if debt <= 0.0
            && let Some(key) = self
                .order
                .iter()
                // Restore the cheapest paused layer first, climbing the
                // quality ladder from the bottom as capacity returns.
                .filter(|k| self.states.get(*k).is_some_and(|s| s.paused))
                .min_by_key(|k| rid_quality_rank(k.1))
                .cloned()
        {
            let candidate_bps = self.states.get(&key).map_or(0.0, |s| s.bps);
            let surplus = -debt;
            if candidate_bps > 0.0 && candidate_bps <= surplus {
                if let Some(s) = self.states.get_mut(&key) {
                    s.paused = false;
                }
                if let Some(n) = self.notifiers.get(&key) {
                    n.notify();
                }
            }
        }

        desired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use str0m::media::{Mid, Rid};

    /// Registers Low/Medium/High out of quality order, to also prove
    /// shed/resume priority is ranked by quality and not registration order.
    fn register_three_layers(ctrl: &mut LayerController, mid: Mid) {
        let (n_h, _rx_h) = KeyframeNotifier::pair();
        ctrl.register(mid, Some(Rid::from("h")), n_h);
        let (n_f, _rx_f) = KeyframeNotifier::pair();
        ctrl.register(mid, Some(Rid::from("f")), n_f);
        let (n_q, _rx_q) = KeyframeNotifier::pair();
        ctrl.register(mid, Some(Rid::from("q")), n_q);
    }

    #[test]
    fn no_layers_resume_until_available_bandwidth_is_known() {
        let mut ctrl = LayerController::new();
        let mid = Mid::from("v0");
        register_three_layers(&mut ctrl, mid);

        // available_bps starts at the f64::MAX sentinel: tick() must not
        // treat that as "unlimited bandwidth, resume everything."
        ctrl.tick(Instant::now());

        assert!(ctrl.is_paused(mid, Some(Rid::from("q"))));
        assert!(ctrl.is_paused(mid, Some(Rid::from("h"))));
        assert!(ctrl.is_paused(mid, Some(Rid::from("f"))));
    }

    #[test]
    fn resumes_cheapest_layer_first_as_soon_as_it_fits() {
        let mut ctrl = LayerController::new();
        let mid = Mid::from("v0");
        register_three_layers(&mut ctrl, mid);

        let now = Instant::now();
        // Enough for Low only.
        ctrl.update_available(Bitrate::from(40_000u64));
        ctrl.tick(now);

        assert!(
            !ctrl.is_paused(mid, Some(Rid::from("q"))),
            "Low should resume immediately once it fits"
        );
        assert!(ctrl.is_paused(mid, Some(Rid::from("h"))));
        assert!(ctrl.is_paused(mid, Some(Rid::from("f"))));
    }

    #[test]
    fn resumes_progressively_one_layer_per_tick_as_bandwidth_grows() {
        let mut ctrl = LayerController::new();
        let mid = Mid::from("v0");
        register_three_layers(&mut ctrl, mid);

        let now = Instant::now();
        // Enough for all three layers, offered all at once: resume must
        // still climb the quality ladder one layer per tick, cheapest
        // first, rather than jumping straight to the top.
        ctrl.update_available(Bitrate::from(2_500_000u64));

        ctrl.tick(now);
        assert!(!ctrl.is_paused(mid, Some(Rid::from("q"))), "tick 1: Low resumes");
        assert!(
            ctrl.is_paused(mid, Some(Rid::from("h"))),
            "tick 1: Medium still waits its turn"
        );
        assert!(
            ctrl.is_paused(mid, Some(Rid::from("f"))),
            "tick 1: High still waits its turn"
        );

        ctrl.tick(now + Duration::from_millis(10));
        assert!(
            !ctrl.is_paused(mid, Some(Rid::from("h"))),
            "tick 2: Medium resumes next"
        );
        assert!(
            ctrl.is_paused(mid, Some(Rid::from("f"))),
            "tick 2: High still waits"
        );

        ctrl.tick(now + Duration::from_millis(20));
        assert!(
            !ctrl.is_paused(mid, Some(Rid::from("f"))),
            "tick 3: High resumes last"
        );
    }

    #[test]
    fn resume_applies_the_instant_surplus_covers_it_no_extra_delay() {
        // Unlike shedding (DEBT_LINGER_MAX_TICKS), resuming has no
        // confirmation delay — the first tick where surplus covers a
        // paused layer's cost resumes it.
        let mut ctrl = LayerController::new();
        let mid = Mid::from("v0");
        register_three_layers(&mut ctrl, mid);

        let now = Instant::now();
        ctrl.update_available(Bitrate::from(40_000u64));
        ctrl.tick(now);
        assert!(!ctrl.is_paused(mid, Some(Rid::from("q"))));

        // Bandwidth jumps to cover Medium too, in a single step.
        ctrl.update_available(Bitrate::from(1_000_000u64));
        ctrl.tick(now + Duration::from_millis(5));
        assert!(
            !ctrl.is_paused(mid, Some(Rid::from("h"))),
            "Medium should resume on the very next tick, not after a delay"
        );
    }

    #[test]
    fn never_pauses_the_last_remaining_layer() {
        let mut ctrl = LayerController::new();
        let mid = Mid::from("v0");
        register_three_layers(&mut ctrl, mid);

        let now = Instant::now();
        ctrl.update_available(Bitrate::from(2_500_000u64));
        for i in 0..3 {
            ctrl.tick(now + Duration::from_millis(i));
        }
        assert!(!ctrl.is_paused(mid, Some(Rid::from("f"))));

        // Available bandwidth collapses to ~nothing: severe, sustained debt
        // should shed down toward the cheapest layer, but never past it.
        ctrl.update_available(Bitrate::from(0u64));
        for i in 0..10 {
            ctrl.tick(now + Duration::from_millis(3 + i));
        }
        assert!(
            !ctrl.is_paused(mid, Some(Rid::from("q"))),
            "must always keep at least one live layer"
        );
    }

    #[test]
    fn sheds_the_most_expensive_layer_first() {
        let mut ctrl = LayerController::new();
        let mid = Mid::from("v0");
        register_three_layers(&mut ctrl, mid);

        let now = Instant::now();
        ctrl.update_available(Bitrate::from(2_500_000u64));
        for i in 0..3 {
            ctrl.tick(now + Duration::from_millis(i));
        }
        assert!(!ctrl.is_paused(mid, Some(Rid::from("f"))));

        // A severe, immediate overage should shed High before Medium.
        ctrl.update_available(Bitrate::from(500_000u64));
        ctrl.tick(now + Duration::from_millis(10));

        assert!(
            ctrl.is_paused(mid, Some(Rid::from("f"))),
            "High should be shed first"
        );
        assert!(
            !ctrl.is_paused(mid, Some(Rid::from("h"))),
            "Medium should still be alive"
        );
    }
}

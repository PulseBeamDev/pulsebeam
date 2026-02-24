use str0m::bwe::Bitrate;

#[derive(Clone, Copy, Debug)]
pub struct BitrateControllerConfig {
    pub min_bitrate: Bitrate,
    pub max_bitrate: Bitrate,
    pub default_bitrate: Bitrate,
    pub headroom_factor: f64,
    // Max percentage to decrease per tick (e.g. 0.95 = 5% max drop)
    pub max_decay_factor: f64,
    // Drop immediately if target is below this % of current (e.g. 0.50)
    pub emergency_drop_threshold: f64,
    // Number of ticks constraint must be satisfied to upgrade
    pub required_up_samples: usize,
    // Number of ticks constraint must be satisfied to downgrade (non-emergency)
    pub required_down_samples: usize,
    pub quantization_step: Bitrate,
    // Exponential Moving Average alpha (1.0 = no smoothing, 0.1 = heavy smoothing)
    pub ema_alpha: f64,
}

impl Default for BitrateControllerConfig {
    fn default() -> Self {
        Self::available_bandwidth()
    }
}

impl BitrateControllerConfig {
    pub fn available_bandwidth() -> Self {
        Self {
            min_bitrate: Bitrate::kbps(300),
            max_bitrate: Bitrate::mbps(10),
            default_bitrate: Bitrate::kbps(300),
            headroom_factor: 1.0,
            max_decay_factor: 1.0, // allowed to drop instantly to target, EMA does the smoothing
            emergency_drop_threshold: 0.25, // only extreme drops
            required_up_samples: 1,
            required_down_samples: 3, // debounce drops
            quantization_step: Bitrate::kbps(10),
            ema_alpha: 0.1, // heavily smooth input bandwidth
        }
    }

    pub fn desired_bitrate() -> Self {
        Self {
            min_bitrate: Bitrate::kbps(10000),
            max_bitrate: Bitrate::mbps(10),
            default_bitrate: Bitrate::kbps(10000),
            headroom_factor: 0.95,  // leave 5% headroom
            max_decay_factor: 0.95, // max 5% drop per tick
            emergency_drop_threshold: 0.50,
            required_up_samples: 1,
            required_down_samples: 1, // react immediately to drops (BWE already filtered)
            quantization_step: Bitrate::kbps(10),
            ema_alpha: 1.0, // no EMA for desired bitrate, we output sharp target steps
        }
    }
}

#[derive(Debug)]
pub struct BitrateController {
    config: BitrateControllerConfig,
    smoothed_input: f64,
    current_bitrate: f64,

    pending_up_target: Option<f64>,
    up_stability_counter: usize,

    pending_down_target: Option<f64>,
    down_stability_counter: usize,
}

impl BitrateController {
    pub fn new(config: BitrateControllerConfig) -> Self {
        Self {
            config,
            smoothed_input: config.default_bitrate.as_f64(),
            current_bitrate: config.default_bitrate.as_f64(),
            pending_up_target: None,
            up_stability_counter: 0,
            pending_down_target: None,
            down_stability_counter: 0,
        }
    }

    pub fn update(&mut self, available_bandwidth: Bitrate) -> Bitrate {
        let raw_bw = available_bandwidth.as_f64();

        if self.smoothed_input == self.config.default_bitrate.as_f64() && raw_bw > 0.0 {
            // First valid sample
            self.smoothed_input = raw_bw;
        } else {
            self.smoothed_input = self.config.ema_alpha * raw_bw
                + (1.0 - self.config.ema_alpha) * self.smoothed_input;
        }

        let safe_bw = self.smoothed_input * self.config.headroom_factor;

        let step = self.config.quantization_step.as_f64();
        let quantized_target = (safe_bw / step).floor() * step;

        let target = quantized_target.clamp(
            self.config.min_bitrate.as_f64(),
            self.config.max_bitrate.as_f64(),
        );

        if target < self.current_bitrate {
            self.reset_up_stability();

            if target < self.current_bitrate * self.config.emergency_drop_threshold {
                self.current_bitrate = target;
                self.reset_down_stability();
            } else {
                match self.pending_down_target {
                    Some(mut pending) => {
                        if target > pending {
                            pending = target;
                            self.pending_down_target = Some(pending);
                        }

                        if pending >= self.current_bitrate {
                            self.reset_down_stability();
                        } else {
                            self.down_stability_counter += 1;
                        }
                    }
                    None => {
                        self.pending_down_target = Some(target);
                        self.down_stability_counter = 1;
                    }
                }

                if self.down_stability_counter >= self.config.required_down_samples {
                    if let Some(final_target) = self.pending_down_target {
                        let decay_limit = self.current_bitrate * self.config.max_decay_factor;
                        self.current_bitrate = final_target.max(decay_limit);
                    }
                    self.reset_down_stability();
                }
            }
        } else if target > self.current_bitrate {
            self.reset_down_stability();

            match self.pending_up_target {
                Some(mut pending) => {
                    if target < pending {
                        pending = target;
                        self.pending_up_target = Some(pending);
                    }

                    if pending <= self.current_bitrate {
                        self.reset_up_stability();
                    } else {
                        self.up_stability_counter += 1;
                    }
                }
                None => {
                    self.pending_up_target = Some(target);
                    self.up_stability_counter = 1;
                }
            }

            if self.up_stability_counter >= self.config.required_up_samples {
                if let Some(final_target) = self.pending_up_target {
                    self.current_bitrate = final_target;
                }
                self.reset_up_stability();
            }
        } else {
            self.reset_up_stability();
            self.reset_down_stability();
        }

        self.current()
    }

    fn reset_up_stability(&mut self) {
        self.up_stability_counter = 0;
        self.pending_up_target = None;
    }

    fn reset_down_stability(&mut self) {
        self.down_stability_counter = 0;
        self.pending_down_target = None;
    }

    pub fn current(&self) -> Bitrate {
        Bitrate::from(self.current_bitrate)
    }
}

#[cfg(test)]
mod tests {
    use more_asserts::{assert_gt, assert_lt};

    use super::*;

    // A configuration matching the original test assumptions.
    fn test_config() -> BitrateControllerConfig {
        BitrateControllerConfig {
            min_bitrate: Bitrate::kbps(300),
            max_bitrate: Bitrate::mbps(5),
            default_bitrate: Bitrate::kbps(300),
            headroom_factor: 1.0,
            max_decay_factor: 0.95,
            emergency_drop_threshold: 0.50,
            required_up_samples: 1,
            required_down_samples: 1,
            quantization_step: Bitrate::kbps(10),
            ema_alpha: 1.0,
        }
    }

    #[test]
    fn test_steady_state() {
        let mut ctrl = BitrateController::new(test_config());
        let bw = Bitrate::kbps(500);

        // Stabilize at 500
        for _ in 0..10 {
            ctrl.update(bw);
        }
        assert_eq!(ctrl.current().as_f64(), 500_000.0);
    }

    #[test]
    fn test_transient_drop_filtering() {
        let mut ctrl = BitrateController::new(test_config());
        // Start stable at 1000 kbps
        for _ in 0..10 {
            ctrl.update(Bitrate::kbps(1000));
        }
        assert_eq!(ctrl.current().as_f64(), 1_000_000.0);

        // One tick drop to 800 kbps (simulating bad estimate/jitter)
        // With decay 0.95, should only drop to ~950k, not 800k
        let res = ctrl.update(Bitrate::kbps(800));
        assert_gt!(res.as_f64(), 900_000.0);
        assert_lt!(res.as_f64(), 1_000_000.0);

        // Input recovers immediately
        let res = ctrl.update(Bitrate::kbps(1000));

        // Should climb back up (or stay high) rather than waiting for full debounce
        // because we never actually dropped low.
        assert!(res.as_f64() > 900_000.0);
    }

    #[test]
    fn test_real_congestion_decay() {
        let mut ctrl = BitrateController::new(test_config());
        for _ in 0..10 {
            ctrl.update(Bitrate::kbps(1000));
        }

        // Sustained drop to 500
        for _ in 0..10 {
            ctrl.update(Bitrate::kbps(500));
        }

        // Should be sliding down
        let current = ctrl.current().as_f64();
        assert!(current < 1_000_000.0);
        assert!(current > 500_000.0);
    }

    #[test]
    fn test_emergency_drop() {
        let mut ctrl = BitrateController::new(test_config());
        for _ in 0..10 {
            ctrl.update(Bitrate::kbps(2000));
        }

        // Massive drop (e.g. WiFi loss), < 50%
        let res = ctrl.update(Bitrate::kbps(300));

        // Should snap immediately
        assert_eq!(res.as_f64(), 300_000.0);
    }

    #[test]
    fn test_debounce_up() {
        let mut config = test_config();
        config.required_up_samples = 3;
        let mut ctrl = BitrateController::new(config);

        // Start 300
        ctrl.update(Bitrate::kbps(300));

        // Jump to 500
        ctrl.update(Bitrate::kbps(500)); // Tick 1 (Pending)
        assert_eq!(ctrl.current().as_f64(), 300_000.0);

        ctrl.update(Bitrate::kbps(500)); // Tick 2
        assert_eq!(ctrl.current().as_f64(), 300_000.0);

        ctrl.update(Bitrate::kbps(500)); // Tick 3 (Commit)
        assert_eq!(ctrl.current().as_f64(), 500_000.0);
    }

    #[test]
    fn test_ema_smoothing() {
        let mut ctrl = BitrateController::new(BitrateControllerConfig::available_bandwidth());
        // Start stable at 5Mbps
        for _ in 0..10 {
            ctrl.update(Bitrate::mbps(5));
        }
        assert_eq!(ctrl.current().as_f64(), 5_000_000.0);

        // Flapping drop to 1Mbps, EMA alpha is 0.1 so it should only drop by 10% toward 1Mbps
        let res = ctrl.update(Bitrate::mbps(1));
        assert_gt!(res.as_f64(), 4_000_000.0);

        // Recovers next tick
        let res = ctrl.update(Bitrate::mbps(5));
        assert_gt!(res.as_f64(), 4_000_000.0);
    }
}

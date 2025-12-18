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
    pub required_up_samples: usize,
    pub quantization_step: Bitrate,
}

impl Default for BitrateControllerConfig {
    fn default() -> Self {
        Self {
            min_bitrate: Bitrate::kbps(300),
            max_bitrate: Bitrate::mbps(5),
            default_bitrate: Bitrate::kbps(300),
            headroom_factor: 1.0,
            max_decay_factor: 0.95,
            emergency_drop_threshold: 0.50,
            required_up_samples: 5,
            quantization_step: Bitrate::kbps(10),
        }
    }
}

#[derive(Debug)]
pub struct BitrateController {
    config: BitrateControllerConfig,
    current_bitrate: f64,
    pending_target: Option<f64>,
    stability_counter: usize,
}

impl Default for BitrateController {
    fn default() -> Self {
        Self::new(BitrateControllerConfig::default())
    }
}

impl BitrateController {
    pub fn new(config: BitrateControllerConfig) -> Self {
        Self {
            config,
            current_bitrate: config.default_bitrate.as_f64(),
            pending_target: None,
            stability_counter: 0,
        }
    }

    pub fn update(&mut self, available_bandwidth: Bitrate) -> Bitrate {
        let raw_bw = available_bandwidth.as_f64();
        let safe_bw = raw_bw * self.config.headroom_factor;

        let step = self.config.quantization_step.as_f64();
        let quantized_target = (safe_bw / step).floor() * step;

        let target = quantized_target.clamp(
            self.config.min_bitrate.as_f64(),
            self.config.max_bitrate.as_f64(),
        );

        if target < self.current_bitrate {
            self.reset_stability();

            if target < self.current_bitrate * self.config.emergency_drop_threshold {
                self.current_bitrate = target;
            } else {
                let decay_limit = self.current_bitrate * self.config.max_decay_factor;
                self.current_bitrate = target.max(decay_limit);
            }
        } else if target > self.current_bitrate {
            match self.pending_target {
                Some(mut pending) => {
                    if target < pending {
                        pending = target;
                        self.pending_target = Some(pending);
                    }

                    if pending <= self.current_bitrate {
                        self.reset_stability();
                    } else {
                        self.stability_counter += 1;
                    }
                }
                None => {
                    self.pending_target = Some(target);
                    self.stability_counter = 1;
                }
            }

            if self.stability_counter >= self.config.required_up_samples {
                if let Some(final_target) = self.pending_target {
                    self.current_bitrate = final_target;
                }
                self.reset_stability();
            }
        } else {
            self.reset_stability();
        }

        self.current()
    }

    fn reset_stability(&mut self) {
        self.stability_counter = 0;
        self.pending_target = None;
    }

    pub fn current(&self) -> Bitrate {
        Bitrate::from(self.current_bitrate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_steady_state() {
        let mut ctrl = BitrateController::default();
        let bw = Bitrate::kbps(500);

        // Stabilize at 500
        for _ in 0..10 {
            ctrl.update(bw);
        }
        assert_eq!(ctrl.current().as_f64(), 500_000.0);
    }

    #[test]
    fn test_transient_drop_filtering() {
        let mut ctrl = BitrateController::default();
        // Start stable at 1000 kbps
        for _ in 0..10 {
            ctrl.update(Bitrate::kbps(1000));
        }
        assert_eq!(ctrl.current().as_f64(), 1_000_000.0);

        // One tick drop to 300 kbps (simulating bad estimate/jitter)
        // With decay 0.95, should only drop to ~950k, not 300k
        let res = ctrl.update(Bitrate::kbps(300));
        assert!(res.as_f64() > 900_000.0);
        assert!(res.as_f64() < 1_000_000.0);

        // Input recovers immediately
        let res = ctrl.update(Bitrate::kbps(1000));

        // Should climb back up (or stay high) rather than waiting for full debounce
        // because we never actually dropped low.
        assert!(res.as_f64() > 900_000.0);
    }

    #[test]
    fn test_real_congestion_decay() {
        let mut ctrl = BitrateController::default();
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
        let mut ctrl = BitrateController::default();
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
        let config = BitrateControllerConfig {
            required_up_samples: 3,
            ..Default::default()
        };
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
}

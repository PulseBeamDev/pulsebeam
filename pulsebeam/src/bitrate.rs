use str0m::bwe::Bitrate;

#[derive(Clone, Copy, Debug)]
pub struct BitrateControllerConfig {
    pub min_bitrate: Bitrate,
    pub max_bitrate: Bitrate,
    pub default_bitrate: Bitrate,

    // Safety headroom (e.g. 0.85 of estimate)
    pub headroom_factor: f64,

    // Immediate downgrade if target drops below this % of current (e.g. 0.90)
    pub downgrade_threshold: f64,

    // Number of ticks to hold a higher target before committing
    // Since input is already smoothed, 5-8 ticks is usually enough stability.
    pub required_up_samples: usize,

    // Step size to prevent micro-fluctuations (e.g. 50kbps)
    pub quantization_step: Bitrate,
}

impl Default for BitrateControllerConfig {
    fn default() -> Self {
        Self {
            min_bitrate: Bitrate::kbps(300),
            max_bitrate: Bitrate::mbps(5),
            default_bitrate: Bitrate::kbps(300),
            headroom_factor: 1.0,
            downgrade_threshold: 0.90,
            required_up_samples: 1,
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
        let config = BitrateControllerConfig::default();
        Self::new(config)
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
        // 1. Give headroom
        let raw_bw = available_bandwidth.as_f64();
        let safe_bw = raw_bw * self.config.headroom_factor;

        // 2. Quantize (Floor)
        // This stabilizes the input "jitter" into steps
        let step = self.config.quantization_step.as_f64();
        let quantized_target = (safe_bw / step).floor() * step;

        let target = quantized_target.clamp(
            self.config.min_bitrate.as_f64(),
            self.config.max_bitrate.as_f64(),
        );

        // 3. Logic

        // A: Immediate Downgrade
        // If external BWE says we dropped significant bandwidth, believe it immediately.
        if target < self.current_bitrate * self.config.downgrade_threshold {
            self.current_bitrate = target;
            self.reset_stability();
            return self.current();
        }

        // B: Debounced Upgrade
        // If target is higher, wait for N samples to confirm it's not a temporary spike.
        if target > self.current_bitrate {
            match self.pending_target {
                Some(mut pending) => {
                    // Track the "Floor" of the new bandwidth during the window.
                    // If bandwidth dips during the wait, lower our expectations
                    // to that dip, but keep the counter running.
                    if target < pending {
                        pending = target;
                        self.pending_target = Some(pending);
                    }

                    // If the dip made it drop below current, the upgrade is invalid.
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
            // Target is roughly equal or slightly below (within threshold). Stable.
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

use str0m::bwe::Bitrate;

#[derive(Clone, Debug)]
pub struct BitrateControllerConfig {
    pub min_bitrate: Bitrate,
    pub max_bitrate: Bitrate,
    pub default_bitrate: Bitrate,
    pub headroom_factor: f64,
    // Downward smoothing factor for EWMA. A value close to 1 makes decay very slow.
    pub down_smoothing: f64,
    // Bucket size for upward quantization
    pub quantization_step: Bitrate,
    // Deadband hysteresis: the raw required bandwidth must drop by AT LEAST this amount
    // below the previous raw estimate before the controller will step down.
    pub hysteresis: Bitrate,
}

impl Default for BitrateControllerConfig {
    fn default() -> Self {
        Self {
            min_bitrate: Bitrate::kbps(30),
            max_bitrate: Bitrate::mbps(10),
            default_bitrate: Bitrate::kbps(30),
            headroom_factor: 1.0,
            down_smoothing: 0.99, // Very slow exponential decay on downward motion
            quantization_step: Bitrate::kbps(200),
            hysteresis: Bitrate::kbps(250), // High deadband to prevent downward churn
        }
    }
}

impl BitrateControllerConfig {
    pub fn build(self) -> BitrateController {
        BitrateController::new(self)
    }
}

#[derive(Debug)]
pub struct BitrateController {
    config: BitrateControllerConfig,
    current_bitrate: f64,
    down_estimate: f64,
}

impl BitrateController {
    pub fn new(config: BitrateControllerConfig) -> Self {
        let initial_bitrate = config.default_bitrate.as_f64();
        Self {
            current_bitrate: initial_bitrate,
            down_estimate: initial_bitrate,
            config,
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
        let current_bitrate = self.current_bitrate.clamp(
            self.config.min_bitrate.as_f64(),
            self.config.max_bitrate.as_f64(),
        );
        Bitrate::from(current_bitrate)
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    #![allow(clippy::disallowed_types)]
    use more_asserts::assert_le;

    use super::*;

    // The controller's default `headroom_factor` is 1.0; these expectations
    // predate that field when headroom was a bare constant.
    const HEADROOM: f64 = 1.0;

    #[test]
    fn test_always_higher_quantization() {
        let mut ctrl = BitrateControllerConfig::default().build();

        // 401kbps should force a jump to the 600kbps bucket
        let res = ctrl.update(Bitrate::kbps(401));
        assert_eq!(res.as_f64(), 600_000.0 * HEADROOM);
    }

    #[test]
    fn test_hysteresis_deadband_prevents_drop() {
        let mut ctrl = BitrateControllerConfig::default().build();

        // Push target to 800k via default headroom + quantization.
        ctrl.update(Bitrate::kbps(650));
        assert_eq!(ctrl.current().as_f64(), 800_000.0 * HEADROOM);

        // Signal drops to 600k. This is a modest decrease that should stay
        // within the configured deadband and not churn the current target.
        for _ in 0..10 {
            ctrl.update(Bitrate::kbps(600));
        }
        assert_eq!(ctrl.current().as_f64(), 800_000.0 * HEADROOM);
    }

    #[test]
    fn test_hysteresis_deadband_allows_deep_drop() {
        let mut config = BitrateControllerConfig::default();
        config.down_smoothing = 0.90;
        let mut ctrl = config.build();

        // Push target to 800k via default headroom + quantization.
        ctrl.update(Bitrate::kbps(650));
        assert_eq!(ctrl.current().as_f64(), 800_000.0 * HEADROOM);

        // A much deeper drop should eventually bypass the deadband.
        let mut res = ctrl.current();
        for _ in 0..40 {
            res = ctrl.update(Bitrate::kbps(300));
            if res.as_f64() < 800_000.0 * HEADROOM {
                break;
            }
        }

        assert_eq!(res.as_f64(), 400_000.0 * HEADROOM);
    }

    #[test]
    fn test_small_drop_stays_stable() {
        let mut ctrl = BitrateControllerConfig::default().build();

        ctrl.update(Bitrate::kbps(650));
        assert_eq!(ctrl.current().as_f64(), 800_000.0 * HEADROOM);

        ctrl.update(Bitrate::kbps(600));
        assert_eq!(ctrl.current().as_f64(), 800_000.0 * HEADROOM);

        ctrl.update(Bitrate::kbps(610));
        assert_eq!(ctrl.current().as_f64(), 800_000.0 * HEADROOM);
    }

    #[test]
    fn test_spike_filtering() {
        let mut ctrl = BitrateControllerConfig::default().build();

        // A step up is adopted promptly.
        let peak = ctrl.update(Bitrate::kbps(5000)).as_f64();
        assert!(peak >= 5_000_000.0, "spike not adopted: {peak}");

        // Small dips below the peak stay put inside the hysteresis dead-band.
        for _ in 0..4 {
            assert_eq!(
                ctrl.update(Bitrate::kbps(4900)).as_f64(),
                peak,
                "a small dip escaped the dead-band"
            );
        }

        // A deep, sustained drop eventually steps the estimate down.
        let mut res = peak;
        for _ in 0..200 {
            res = ctrl.update(Bitrate::kbps(1000)).as_f64();
        }
        assert!(res < peak, "sustained drop never stepped down: {res}");
    }

    #[test]
    fn test_maximum_cap_hold_while_still_clamped() {
        let mut config = BitrateControllerConfig::default();
        config.max_bitrate = Bitrate::mbps(5);
        config.down_smoothing = 0.90;
        let mut ctrl = config.build();

        // Demand above the configured maximum clamps to the cap.
        ctrl.update(Bitrate::kbps(6000));
        assert_eq!(ctrl.current().as_f64(), 5_000_000.0);

        // While demand stays above the cap, hold there without decaying.
        for _ in 0..20 {
            assert_eq!(ctrl.update(Bitrate::kbps(6000)).as_f64(), 5_000_000.0);
        }

        // Once demand falls well below, it steps down but never above the cap.
        let mut res = ctrl.current().as_f64();
        for _ in 0..100 {
            res = ctrl.update(Bitrate::kbps(1000)).as_f64();
            assert_le!(res, 5_000_000.0);
        }
        assert!(res < 5_000_000.0, "never stepped down from the cap: {res}");
    }

    #[test]
    fn test_default_config_ramps_up_immediately() {
        let mut ctrl = BitrateControllerConfig::default().build();

        // Starting from the default idle bitrate, a larger demand should be reflected immediately.
        let res = ctrl.update(Bitrate::kbps(500));
        assert_eq!(res.as_f64(), 600_000.0 * HEADROOM);
    }
}

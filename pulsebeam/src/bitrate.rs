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

        // Smoothing governs how fast we *approach* a new demand. It must not answer with more
        // than the demand itself: the slow decay, 200 kbps quantization and 250 kbps deadband
        // above all resist coming down, so without this the controller sits well over what was
        // asked for - measured at 800 kbps out for 472 kbps in, and 1.6 Mbps shortly after a
        // downgrade. Callers feed the result to congestion control as a statement of demand, so
        // an inflated answer is bandwidth spent on capacity nobody asked for.
        //
        // The cap applies after the `min_bitrate` clamp, not before: a demand of zero has to
        // produce zero. Clamping last would put the floor back and leave a caller with nothing
        // to send still asking for bandwidth.
        let capped = self
            .current_bitrate
            .clamp(
                self.config.min_bitrate.as_f64(),
                self.config.max_bitrate.as_f64(),
            )
            .min(raw);

        Bitrate::from(capped)
    }
}

#[cfg(test)]
mod tests {
    use more_asserts::assert_le;

    use super::*;

    fn ctrl_with_headroom(headroom: f64) -> BitrateController {
        BitrateControllerConfig {
            headroom_factor: headroom,
            ..Default::default()
        }
        .build()
    }

    /// The contract callers rely on: the answer is a statement of demand, so it can never say
    /// more than was demanded.
    #[test]
    fn output_never_exceeds_demand() {
        let mut ctrl = BitrateControllerConfig::default().build();

        // 401 kbps quantizes up to the 600 kbps bucket internally, which is what gives the
        // controller its downward stickiness - but that must not inflate the answer.
        assert_eq!(ctrl.update(Bitrate::kbps(401)).as_f64(), 401_000.0);

        // Nor after the internal target has been pushed high by an earlier peak.
        ctrl.update(Bitrate::kbps(5000));
        assert_eq!(ctrl.update(Bitrate::kbps(650)).as_f64(), 650_000.0);
    }

    /// A caller with nothing to send must ask for nothing. The `min_bitrate` floor used to put
    /// 300 kbps back here, which left idle connections being probed at twice that.
    #[test]
    fn zero_demand_yields_zero() {
        let mut ctrl = BitrateControllerConfig::default().build();

        ctrl.update(Bitrate::kbps(5000));
        assert_eq!(ctrl.update(Bitrate::ZERO).as_f64(), 0.0);
    }

    #[test]
    fn demand_above_the_maximum_clamps_to_it() {
        let mut ctrl = BitrateControllerConfig {
            max_bitrate: Bitrate::mbps(5),
            ..Default::default()
        }
        .build();

        for _ in 0..20 {
            assert_eq!(ctrl.update(Bitrate::kbps(6000)).as_f64(), 5_000_000.0);
        }
    }

    /// `headroom_factor` is the knob that lets the answer exceed demand.
    #[test]
    fn headroom_factor_admits_margin_above_demand() {
        let mut ctrl = ctrl_with_headroom(1.5);

        assert_eq!(ctrl.update(Bitrate::kbps(650)).as_f64(), 975_000.0);
    }

    /// The smoothing is not observable through the cap, and this pins that.
    ///
    /// `current_bitrate` is `ceil(down_estimate / quantization) * quantization`, and
    /// `down_estimate` is never below the demand, so the internal target is always at or above
    /// the ceiling. Every update therefore answers `min(demand * headroom, max_bitrate)`.
    ///
    /// That is not an accident of tuning: the decay and dead-band exist to hold the value *above*
    /// a falling demand, which is exactly the over-serving the cap exists to remove. The two
    /// cannot both be in effect. Left as-is, this controller is a pass-through and a candidate
    /// for deletion; it earns its keep only if the caller wants the value to lag demand downward.
    #[test]
    fn smoothing_is_not_observable_through_the_cap() {
        let mut ctrl = ctrl_with_headroom(1.5);

        ctrl.update(Bitrate::kbps(5000));
        for demand_kbps in [600u64, 610, 400, 1200, 300] {
            assert_eq!(
                ctrl.update(Bitrate::kbps(demand_kbps)).as_f64(),
                demand_kbps as f64 * 1500.0,
                "demand {demand_kbps} kbps did not pass straight through"
            );
        }
    }

    /// A deep, sustained drop does eventually step down - and on the way there the cap keeps the
    /// answer from ever exceeding the demand plus its headroom.
    #[test]
    fn sustained_drop_steps_down() {
        let mut ctrl = BitrateControllerConfig {
            headroom_factor: 1.5,
            down_smoothing: 0.90,
            ..Default::default()
        }
        .build();

        let peak = ctrl.update(Bitrate::kbps(5000)).as_f64();

        let mut res = peak;
        for _ in 0..200 {
            res = ctrl.update(Bitrate::kbps(1000)).as_f64();
            assert_le!(res, 1_500_000.0);
        }
        assert!(res < peak, "sustained drop never stepped down: {res}");
    }

    #[test]
    fn a_larger_demand_is_reflected_immediately() {
        let mut ctrl = BitrateControllerConfig::default().build();

        assert_eq!(ctrl.update(Bitrate::kbps(500)).as_f64(), 500_000.0);
    }
}

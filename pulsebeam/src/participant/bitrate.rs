use std::time::Duration;
use str0m::bwe::Bitrate;
use tokio::time::Instant;

/// Controller config for stabilizing desired bitrate
pub struct BitrateControllerConfig {
    pub min_bitrate: Bitrate,
    pub max_bitrate: Bitrate,
    pub headroom: f64,           // fraction of available bandwidth to target
    pub tau: f64,                // EMA smoothing constant (s)
    pub max_ramp_up: Bitrate,    // max increase per second
    pub max_ramp_down: Bitrate,  // max decrease per second
    pub hysteresis_up: f64,      // fraction above current to trigger increase
    pub hysteresis_down: f64,    // fraction below current to trigger decrease
    pub min_hold_time: Duration, // min time between updates
}

impl Default for BitrateControllerConfig {
    fn default() -> Self {
        Self {
            min_bitrate: Bitrate::kbps(300),
            max_bitrate: Bitrate::mbps(5),

            // Headroom: 15% is a safe and standard value.
            headroom: 0.85,

            // EMA smoothing
            tau: 1.0,

            // Ramp limits (per second): Be aggressive in our reactions.
            // Allow ramping up reasonably fast to achieve good quality in a few seconds.
            max_ramp_up: Bitrate::kbps(1500),

            // React to congestion IMMEDIATELY. Allow dropping several megabits per second.
            max_ramp_down: Bitrate::mbps(3),

            // Hysteresis: Require a SIGNIFICANT and sustained improvement before upgrading.
            // A 15% increase in the smoothed, headroom-adjusted target is a strong signal.
            hysteresis_up: 1.0,

            // Be slightly more sensitive to decreases. A 15% drop is also a clear signal.
            hysteresis_down: 0.85,

            // Hold time: After a change, wait a bit longer to observe its effect on the network.
            // This prevents the controller from chasing noisy BWE feedback too quickly.
            min_hold_time: Duration::ZERO,
        }
    }
}

impl BitrateControllerConfig {
    pub fn build(self) -> BitrateController {
        let init = self.min_bitrate;
        BitrateController::new(self, init, Instant::now())
    }
}

pub struct BitrateController {
    config: BitrateControllerConfig,
    smoothed_bw: f64,
    bitrate: f64,
    last_change: Instant,
    last_update: Instant,
}

impl Default for BitrateController {
    fn default() -> Self {
        BitrateControllerConfig::default().build()
    }
}

impl BitrateController {
    pub fn new(config: BitrateControllerConfig, initial_bitrate: Bitrate, now: Instant) -> Self {
        Self {
            smoothed_bw: initial_bitrate.as_f64(),
            bitrate: initial_bitrate.as_f64(),
            last_change: now,
            last_update: now,
            config,
        }
    }

    /// Update the desired bitrate given an available bandwidth measurement
    pub fn update(&mut self, available_bw: Bitrate, now: Instant) -> Bitrate {
        let dt = now.duration_since(self.last_update).as_secs_f64();
        self.last_update = now;

        // 1️⃣ EMA smoothing of available bandwidth
        let alpha = dt / (self.config.tau + dt);
        self.smoothed_bw = alpha * available_bw.as_f64() + (1.0 - alpha) * self.smoothed_bw;

        // 2️⃣ Compute conservative target with headroom
        let mut target = self.smoothed_bw * self.config.headroom;
        target = target.clamp(
            self.config.min_bitrate.as_f64(),
            self.config.max_bitrate.as_f64(),
        );

        // 3️⃣ Rate-limit changes
        let diff = target - self.bitrate;
        let delta = if diff > 0.0 {
            diff.min(self.config.max_ramp_up.as_f64() * dt)
        } else {
            diff.max(-self.config.max_ramp_down.as_f64() * dt)
        };
        let candidate_bitrate = self.bitrate + delta;

        // 4️⃣ Apply hysteresis and hold time
        let time_since_last = now.duration_since(self.last_change);
        let mut new_bitrate = self.bitrate;

        if candidate_bitrate >= self.bitrate * self.config.hysteresis_up {
            if time_since_last >= self.config.min_hold_time {
                new_bitrate = candidate_bitrate;
                self.last_change = now;
            }
        } else if candidate_bitrate <= self.bitrate * self.config.hysteresis_down {
            if time_since_last >= self.config.min_hold_time {
                new_bitrate = candidate_bitrate;
                self.last_change = now;
            }
        }

        self.bitrate = new_bitrate;
        self.bitrate.into()
    }

    pub fn current(&self) -> Bitrate {
        self.bitrate.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stable_desired_bitrate() {
        let config = BitrateControllerConfig {
            min_bitrate: Bitrate::kbps(100),
            max_bitrate: Bitrate::mbps(2),
            headroom: 0.85,
            tau: 1.0,
            max_ramp_up: Bitrate::kbps(50),
            max_ramp_down: Bitrate::kbps(200),
            hysteresis_up: 1.1,
            hysteresis_down: 0.9,
            min_hold_time: Duration::from_secs(3),
        };

        let mut controller = BitrateController::new(config, Bitrate::kbps(500), Instant::now());

        let mut now = Instant::now();
        let dt = 0.25; // 250 ms
        let samples = vec![
            Bitrate::kbps(600),
            Bitrate::kbps(1200),
            Bitrate::kbps(800),
            Bitrate::kbps(1500),
            Bitrate::kbps(1000),
        ];

        for bw in samples {
            let desired = controller.update(bw, now);
            println!("Available BW: {}, Desired: {}", bw, desired);
            now += Duration::from_secs_f64(dt);
        }
    }
}

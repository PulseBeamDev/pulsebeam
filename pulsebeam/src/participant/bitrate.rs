use std::time::Duration;
use str0m::bwe::Bitrate;
use tokio::time::Instant;

/// Controller config for stabilizing desired bitrate
pub struct BirateControllerConfig {
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

impl Default for BirateControllerConfig {
    fn default() -> Self {
        Self {
            min_bitrate: Bitrate::kbps(300),
            max_bitrate: Bitrate::mbps(20),

            // Headroom: conservative, avoids overshooting bandwidth
            headroom: 0.85,

            // EMA smoothing constant: 1s
            tau: 1.0,

            // Ramp limits (per second) - tick is 0.5s
            max_ramp_up: Bitrate::kbps(150), // 0.5s tick → 75 kbps per tick
            max_ramp_down: Bitrate::kbps(200), // 0.5s tick → 100 kbps per tick

            // Hysteresis to prevent toggling
            hysteresis_up: 1.03,   // 3% increase triggers upgrade
            hysteresis_down: 0.90, // 10% decrease triggers downgrade

            // Hold time: minimum between desired bitrate changes
            min_hold_time: Duration::from_secs(1), // allows faster upgrades
        }
    }
}

pub struct BitrateController {
    config: BirateControllerConfig,
    smoothed_bw: f64,
    desired_bitrate: f64,
    last_change: Instant,
    last_update: Instant,
}

impl Default for BitrateController {
    fn default() -> Self {
        let cfg = BirateControllerConfig::default();
        let init = cfg.min_bitrate;
        Self::new(cfg, init, Instant::now())
    }
}

impl BitrateController {
    pub fn new(config: BirateControllerConfig, initial_bitrate: Bitrate, now: Instant) -> Self {
        Self {
            smoothed_bw: initial_bitrate.as_f64(),
            desired_bitrate: initial_bitrate.as_f64(),
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
        let diff = target - self.desired_bitrate;
        let delta = if diff > 0.0 {
            diff.min(self.config.max_ramp_up.as_f64() * dt)
        } else {
            diff.max(-self.config.max_ramp_down.as_f64() * dt)
        };
        let candidate_bitrate = self.desired_bitrate + delta;

        // 4️⃣ Apply hysteresis and hold time
        let time_since_last = now.duration_since(self.last_change);
        let mut new_bitrate = self.desired_bitrate;

        if candidate_bitrate >= self.desired_bitrate * self.config.hysteresis_up {
            if time_since_last >= self.config.min_hold_time {
                new_bitrate = candidate_bitrate;
                self.last_change = now;
            }
        } else if candidate_bitrate <= self.desired_bitrate * self.config.hysteresis_down {
            if time_since_last >= self.config.min_hold_time {
                new_bitrate = candidate_bitrate;
                self.last_change = now;
            }
        }

        self.desired_bitrate = new_bitrate;
        self.desired_bitrate.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stable_desired_bitrate() {
        let config = BirateControllerConfig {
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
            let desired = controller.update(bw, dt, now);
            println!("Available BW: {}, Desired: {}", bw, desired);
            now += Duration::from_secs_f64(dt);
        }
    }
}

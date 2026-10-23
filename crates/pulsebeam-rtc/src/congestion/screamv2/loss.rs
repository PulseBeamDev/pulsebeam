// Copyright 2026 The WebRTC Project Authors. All rights reserved.
// Rust port of loss_estimator.{cc,h} at libwebrtc
// 326fd06bb4d93900ed6947205b94cefd2413afca. See LICENSE and PATENTS.

use std::time::Duration;

use super::{elapsed, parameters::Parameters};

#[derive(Default)]
pub(super) struct LossEstimator {
    pub congestion_level: f64,
    unrecovered_lost_packets: u64,
    loss_event_this_rtt: bool,
    last_loss_or_recovery_time: Option<Duration>,
    last_rtt_update_time: Option<Duration>,
}

impl LossEstimator {
    pub fn update(
        &mut self,
        now: Duration,
        lost: u64,
        recovered: u64,
        rtt: Duration,
        params: Parameters,
    ) -> bool {
        let max_rtt = params.virtual_rtt.max(rtt);
        if elapsed(now, self.last_loss_or_recovery_time) > max_rtt {
            self.unrecovered_lost_packets = 0;
        }
        if lost > 0 || recovered > 0 {
            self.last_loss_or_recovery_time = Some(now);
        }
        if recovered > 0 {
            self.unrecovered_lost_packets = self.unrecovered_lost_packets.saturating_sub(recovered);
            if self.unrecovered_lost_packets == 0 {
                self.congestion_level = 0.0;
                self.loss_event_this_rtt = false;
            }
        }
        if lost > 0 {
            self.unrecovered_lost_packets = self.unrecovered_lost_packets.saturating_add(lost);
            self.loss_event_this_rtt = true;
        }
        if elapsed(now, self.last_rtt_update_time) >= max_rtt {
            self.last_rtt_update_time = Some(now);
            self.congestion_level = if self.loss_event_this_rtt {
                (self.congestion_level + 1.0 / f64::from(params.rtts_with_loss_before_backoff))
                    .min(1.0)
            } else {
                (self.congestion_level - 1.0 / f64::from(params.lossless_rtts_before_clear))
                    .max(0.0)
            };
            self.loss_event_this_rtt = false;
        }
        lost > 0
    }

    pub fn congested(&self) -> bool {
        self.congestion_level >= 0.99
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::congestion::screamv2::parameters::DEFAULT;

    #[test]
    fn upstream_asymmetric_rtt_steps_and_recovery() {
        let mut loss = LossEstimator::default();
        for step in 1_u32..=3 {
            loss.update(
                Duration::from_millis(u64::from(step) * 25),
                1,
                0,
                Duration::ZERO,
                DEFAULT,
            );
            assert!((loss.congestion_level - f64::from(step) / 3.0).abs() < 1e-9);
        }
        assert!(loss.congested());
        loss.update(Duration::from_millis(100), 0, 0, Duration::ZERO, DEFAULT);
        assert_eq!(loss.congestion_level, 0.5);
        loss.update(Duration::from_millis(125), 1, 0, Duration::ZERO, DEFAULT);
        assert!((loss.congestion_level - (0.5 + 1.0 / 3.0)).abs() < 1e-9);
        loss.update(Duration::from_millis(126), 0, 1, Duration::ZERO, DEFAULT);
        assert_eq!(loss.congestion_level, 0.0);
    }

    #[test]
    fn several_feedbacks_in_one_rtt_do_not_advance_the_step_clock() {
        let mut loss = LossEstimator::default();
        for now in 0..25 {
            loss.update(
                Duration::from_millis(now),
                1,
                0,
                Duration::from_millis(100),
                DEFAULT,
            );
        }
        assert!((loss.congestion_level - 1.0 / 3.0).abs() < 1e-9);
        assert!(!loss.congested());
    }
}

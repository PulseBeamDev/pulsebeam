// Copyright 2025 The WebRTC Project Authors. All rights reserved.
// Rust port of delay_based_congestion_control.{cc,h} at libwebrtc
// 326fd06bb4d93900ed6947205b94cefd2413afca. See LICENSE and PATENTS.

use std::{collections::VecDeque, time::Duration};

use super::{
    duration_micros, elapsed, ewma_duration, feedback::DelayMetrics, parameters::Parameters,
};

pub(super) struct DelayController {
    pub rtt: Duration,
    pub queue_delay: Option<Duration>,
    pub queue_delay_min_avg: Duration,
    pub latency_difference_avg: Duration,
    next_base_delay: Option<i128>,
    base_delay_history: VecDeque<i128>,
    last_base_delay_update: Option<Duration>,
    min_queue_delay_above_threshold_start: Option<Duration>,
    last_queue_delay_sample: Option<Duration>,
}

impl DelayController {
    pub fn new(params: Parameters) -> Self {
        Self {
            rtt: Duration::ZERO,
            queue_delay: None,
            queue_delay_min_avg: Duration::ZERO,
            latency_difference_avg: Duration::ZERO,
            next_base_delay: None,
            base_delay_history: VecDeque::with_capacity(params.base_delay_window_length),
            last_base_delay_update: None,
            min_queue_delay_above_threshold_start: None,
            last_queue_delay_sample: None,
        }
    }

    pub fn update(&mut self, now: Duration, metrics: DelayMetrics, alr: bool, params: Parameters) {
        self.next_base_delay = Some(
            self.next_base_delay
                .map_or(metrics.min_one_way_delay, |old| {
                    old.min(metrics.min_one_way_delay)
                }),
        );
        self.rtt = if self.rtt.is_zero() {
            metrics.rtt_sample
        } else {
            ewma_duration(
                self.rtt,
                metrics.rtt_sample,
                if alr {
                    params.smoothed_rtt_avg_in_alr_g
                } else {
                    params.smoothed_rtt_avg_g
                },
            )
        };
        let base = self
            .base_delay_history
            .iter()
            .copied()
            .chain(self.next_base_delay)
            .min()
            .unwrap_or(metrics.min_one_way_delay);
        let min_queue_delay = duration_micros(metrics.min_one_way_delay.saturating_sub(base));
        if min_queue_delay > params.queue_delay_drain_threshold {
            self.min_queue_delay_above_threshold_start
                .get_or_insert(now);
        } else {
            self.min_queue_delay_above_threshold_start = None;
        }
        let sample = self
            .last_queue_delay_sample
            .map_or(min_queue_delay, |last| min_queue_delay.min(last));
        self.queue_delay = Some(match self.queue_delay {
            Some(old) if sample >= old => ewma_duration(old, sample, params.queue_delay_avg_g),
            _ => sample,
        });
        self.last_queue_delay_sample = Some(min_queue_delay);
        self.queue_delay_min_avg = ewma_duration(
            self.queue_delay_min_avg,
            min_queue_delay.min(params.queue_delay_min_threshold.saturating_mul(2)),
            params.delay_min_and_latency_diff_avg_g,
        );
        self.latency_difference_avg = ewma_duration(
            self.latency_difference_avg,
            duration_micros(
                metrics
                    .max_one_way_delay
                    .saturating_sub(metrics.min_one_way_delay),
            )
            .min(params.latency_diff_threshold.saturating_mul(2)),
            params.delay_min_and_latency_diff_avg_g,
        );
        if elapsed(now, self.last_base_delay_update) >= params.base_delay_history_update_interval {
            if self.base_delay_history.len() >= params.base_delay_window_length.max(1) {
                self.base_delay_history.pop_front();
            }
            if let Some(next) = self.next_base_delay.take() {
                self.base_delay_history.push_back(next);
            }
            self.last_base_delay_update = Some(now);
        }
    }

    pub fn reset_queue_delay(&mut self) {
        self.last_base_delay_update = None;
        self.next_base_delay = None;
        self.base_delay_history.clear();
        self.min_queue_delay_above_threshold_start = None;
        self.queue_delay = None;
        self.queue_delay_min_avg = Duration::ZERO;
        self.latency_difference_avg = Duration::ZERO;
    }

    pub fn is_queue_delay_detected(&self, params: Parameters) -> bool {
        self.queue_delay
            .is_some_and(|delay| delay > params.queue_delay_target / 2)
    }

    pub fn is_queue_drained_in_time(&self, now: Duration, params: Parameters) -> bool {
        self.min_queue_delay_above_threshold_start
            .is_none_or(|start| now.saturating_sub(start) < params.queue_delay_drain_period)
    }

    pub fn min_delay_scale(&self, allow_zero: bool, params: Parameters) -> f64 {
        let norm = params.queue_delay_min_threshold.as_secs_f64();
        (0.1 + 1.2 * (norm - self.queue_delay_min_avg.as_secs_f64()) / norm)
            .clamp(if allow_zero { 0.0 } else { 0.1 }, 1.0)
    }

    pub fn latency_difference_scale(&self, params: Parameters) -> f64 {
        let norm = params.latency_diff_threshold.as_secs_f64();
        (0.1 + 1.2 * (norm - self.latency_difference_avg.as_secs_f64()) / norm).clamp(0.1, 1.0)
    }

    pub fn virtual_alpha(&self, params: Parameters) -> f64 {
        let half_target = (params.queue_delay_target / 2).as_secs_f64();
        self.queue_delay.map_or(1.0, |delay| {
            ((delay.as_secs_f64() - half_target) / half_target).clamp(0.0, 1.0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::congestion::screamv2::parameters::DEFAULT;

    fn metrics(delay: i128) -> DelayMetrics {
        DelayMetrics {
            min_one_way_delay: delay,
            max_one_way_delay: delay,
            feedback_hold: Duration::ZERO,
            rtt_sample: Duration::from_millis(100),
            last_packet_receive_time: 0,
        }
    }

    #[test]
    fn upstream_burst_minimum_uses_two_samples_and_fast_decay() {
        let mut delay = DelayController::new(DEFAULT);
        delay.update(Duration::ZERO, metrics(-500_000), false, DEFAULT);
        delay.update(Duration::from_millis(25), metrics(-460_000), false, DEFAULT);
        assert_eq!(delay.queue_delay, Some(Duration::ZERO));
        delay.update(Duration::from_millis(50), metrics(-460_000), false, DEFAULT);
        assert_eq!(delay.queue_delay, Some(Duration::from_millis(10)));
        delay.update(Duration::from_millis(75), metrics(-500_000), false, DEFAULT);
        assert_eq!(delay.queue_delay, Some(Duration::ZERO));
    }

    #[test]
    fn upstream_alr_slows_rtt_and_history_is_bounded_by_minutes_not_packets() {
        let mut delay = DelayController::new(DEFAULT);
        delay.update(Duration::ZERO, metrics(0), false, DEFAULT);
        let mut longer = metrics(0);
        longer.rtt_sample = Duration::from_millis(228);
        delay.update(Duration::from_millis(25), longer, true, DEFAULT);
        assert_eq!(delay.rtt, Duration::from_millis(101));
        for minute in 1..=20 {
            delay.update(Duration::from_secs(minute * 60), metrics(0), false, DEFAULT);
        }
        assert_eq!(delay.base_delay_history.len(), 10);
        assert_eq!(delay.next_base_delay, None);
    }
}

// Copyright 2025 The WebRTC Project Authors. All rights reserved.
// Rust port of delay_based_congestion_control.{cc,h} at libwebrtc
// 7cfcf6dc8e6573d09d967d1641fc7bbcf65bd499. See LICENSE and PATENTS.

use std::{collections::VecDeque, time::Duration};

use super::{
    duration_micros, elapsed, ewma_duration, feedback::DelayMetrics, parameters::Parameters,
};

pub(super) struct DelayController {
    pub rtt: Duration,
    pub queue_delay: Option<Duration>,
    pub queue_delay_min_avg: Duration,
    pub latency_difference_avg: Duration,
    qdelay_short_avg: Duration,
    qdelay_long_avg: Duration,
    pub(super) queue_delay_trend: f64,
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
            qdelay_short_avg: Duration::ZERO,
            qdelay_long_avg: Duration::ZERO,
            queue_delay_trend: 0.0,
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
        if params.reduce_jitter {
            self.calculate_queue_delay_trend(
                duration_micros(metrics.max_one_way_delay.saturating_sub(base)),
                params,
            );
        } else {
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
        }
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
        self.qdelay_short_avg = Duration::ZERO;
        self.qdelay_long_avg = Duration::ZERO;
        self.queue_delay_trend = 0.0;
    }

    fn calculate_queue_delay_trend(&mut self, max_queue_delay: Duration, params: Parameters) {
        self.qdelay_short_avg = ewma_duration(
            self.qdelay_short_avg,
            max_queue_delay,
            params.queue_delay_short_avg_g,
        );
        self.qdelay_long_avg = ewma_duration(
            self.qdelay_long_avg,
            max_queue_delay,
            params.queue_delay_long_avg_g,
        );

        let diff_micros = i128::try_from(self.qdelay_short_avg.as_micros()).unwrap_or(i128::MAX)
            - i128::try_from(self.qdelay_long_avg.as_micros()).unwrap_or(i128::MAX)
            - i128::try_from(params.latency_diff_margin_1.as_micros()).unwrap_or(i128::MAX);
        let margin = i128::try_from(params.latency_diff_margin_1.as_micros()).unwrap_or(i128::MAX);
        let diff = diff_micros.clamp(-margin, margin) as f64 / 1_000_000.0;
        if diff > 0.0 {
            let low_queue_delay_scale = (self.queue_delay.unwrap_or_default().as_secs_f64()
                / params.latency_diff_margin_2.as_secs_f64())
            .min(1.0);
            self.queue_delay_trend += params.latency_diff_up_gain * diff * low_queue_delay_scale;
        } else {
            self.queue_delay_trend += params.latency_diff_down_gain * diff;
        }
        self.queue_delay_trend = self.queue_delay_trend.clamp(0.0, 1.0);
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
        if params.reduce_jitter {
            1.0 - self.queue_delay_trend
        } else {
            let norm = params.latency_diff_threshold.as_secs_f64();
            (0.1 + 1.2 * (norm - self.latency_difference_avg.as_secs_f64()) / norm).clamp(0.1, 1.0)
        }
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
        // Native QDelayAvgG is 1/2, so this attack step averages 40 ms with 0.
        assert_eq!(delay.queue_delay, Some(Duration::from_millis(20)));
        delay.update(Duration::from_millis(75), metrics(-500_000), false, DEFAULT);
        assert_eq!(delay.queue_delay, Some(Duration::ZERO));
    }

    #[test]
    fn native_reduce_jitter_trend_uses_short_long_deadband_gain_and_clamp() {
        let mut delay = DelayController::new(DEFAULT);
        delay.queue_delay = Some(Duration::from_millis(10));
        delay.calculate_queue_delay_trend(Duration::from_millis(20), DEFAULT);
        assert_eq!(delay.qdelay_short_avg, Duration::from_millis(4));
        assert_eq!(delay.qdelay_long_avg, Duration::from_micros(400));
        assert!((delay.queue_delay_trend - 0.02).abs() < 1e-12);

        delay.queue_delay_trend = 0.999;
        delay.calculate_queue_delay_trend(Duration::from_millis(20), DEFAULT);
        assert_eq!(delay.queue_delay_trend, 1.0);

        delay.qdelay_short_avg = Duration::from_millis(20);
        delay.qdelay_long_avg = Duration::from_millis(20);
        delay.queue_delay_trend = 0.01;
        delay.calculate_queue_delay_trend(Duration::ZERO, DEFAULT);
        assert_eq!(delay.queue_delay_trend, 0.008);
        assert_eq!(delay.latency_difference_scale(DEFAULT), 0.992);

        delay.reset_queue_delay();
        assert_eq!(delay.qdelay_short_avg, Duration::ZERO);
        assert_eq!(delay.qdelay_long_avg, Duration::ZERO);
        assert_eq!(delay.queue_delay_trend, 0.0);
        assert_eq!(delay.latency_difference_scale(DEFAULT), 1.0);
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

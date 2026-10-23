// Copyright 2026 The WebRTC Project Authors. All rights reserved.
// Rust port of scream_feedback.{cc,h} at libwebrtc
// 326fd06bb4d93900ed6947205b94cefd2413afca. See LICENSE and PATENTS.

use std::time::Duration;

use super::{ControllerInput, EcnMark, EcnMode, micros_i128, parameters::Parameters};

#[derive(Clone, Copy, Debug)]
pub(super) struct DelayMetrics {
    pub min_one_way_delay: i128,
    pub max_one_way_delay: i128,
    pub feedback_hold: Duration,
    pub rtt_sample: Duration,
    pub last_packet_receive_time: i64,
}

#[derive(Default)]
pub(super) struct Feedback {
    pub received: u64,
    pub acked_not_marked: u64,
    pub num_received: u64,
    pub num_ce: u64,
    pub num_lost: u64,
    pub num_recovered: u64,
    pub delay: Option<DelayMetrics>,
}

impl Feedback {
    pub fn parse(now: Duration, input: ControllerInput<'_>, params: Parameters) -> Self {
        let mut parsed = Self::default();
        let mut timed = Vec::new();
        for sample in input.feedback {
            let ce = input.ecn_mode != EcnMode::Disabled && sample.ecn == Some(EcnMark::Ce);
            // SentHistory already owns ACK-edge credit, including unreported gaps.
            // A late recovery must not grant that same transport credit a second time.
            if sample.newly_acked && !ce {
                parsed.acked_not_marked = parsed
                    .acked_not_marked
                    .saturating_add(u64::from(sample.transport_bytes));
            }
            if !sample.received && sample.lost {
                parsed.num_lost = parsed.num_lost.saturating_add(1);
            } else if sample.recovered {
                parsed.num_recovered = parsed.num_recovered.saturating_add(1);
            }
            if sample.received {
                parsed.received = parsed
                    .received
                    .saturating_add(u64::from(sample.transport_bytes));
                parsed.num_received = parsed.num_received.saturating_add(1);
                parsed.num_ce = parsed.num_ce.saturating_add(u64::from(ce));
                if let Some(arrival) = sample.receiver_arrival_micros {
                    timed.push((sample, arrival));
                }
            }
        }
        if timed.is_empty() {
            return parsed;
        }
        // Native PacketResults are in send order. RFC 8888 can join several
        // SSRC reports, so normalize that private adapter boundary here.
        timed.sort_by_key(|(sample, _)| sample.sent_at);
        let first = timed
            .iter()
            .min_by_key(|(sample, arrival)| (*arrival, sample.sent_at));
        let last = timed
            .iter()
            .max_by_key(|(sample, arrival)| (*arrival, sample.sent_at));
        let latest_send = timed
            .last()
            .map(|(sample, _)| sample.sent_at)
            .unwrap_or_default();
        let mut newer_send = latest_send;
        let mut min_delay = i128::MAX;
        let mut max_delay = i128::MIN;
        for (sample, arrival) in timed.iter().rev() {
            let delta = latest_send.saturating_sub(sample.sent_at);
            let gap = newer_send.saturating_sub(sample.sent_at);
            if delta > params.burst_window_min
                && (delta > params.burst_window_max || gap > params.burst_window_max_gap)
            {
                break;
            }
            let delay = i128::from(*arrival).saturating_sub(micros_i128(sample.sent_at));
            min_delay = min_delay.min(delay);
            max_delay = max_delay.max(delay);
            newer_send = sample.sent_at;
        }
        if let (Some((_, first_arrival)), Some((last_sample, last_arrival))) = (first, last) {
            let span = Duration::from_micros(
                u64::try_from(i128::from(*last_arrival) - i128::from(*first_arrival))
                    .unwrap_or(u64::MAX),
            );
            parsed.delay = Some(DelayMetrics {
                min_one_way_delay: min_delay,
                max_one_way_delay: max_delay,
                feedback_hold: span.saturating_add(input.feedback_hold),
                rtt_sample: now
                    .saturating_sub(last_sample.sent_at)
                    .saturating_sub(input.feedback_hold),
                last_packet_receive_time: *last_arrival,
            });
        }
        parsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::congestion::screamv2::{
        FeedbackSample,
        parameters::DEFAULT,
        tests::{input, sample},
    };

    #[test]
    fn upstream_sparse_feedback_uses_latest_burst_not_old_jitter() {
        let mut older = sample(0, 400_000);
        older.sent_at = Duration::ZERO;
        let latest = sample(250, 280_000);
        let result = Feedback::parse(Duration::from_millis(300), input(&[older, latest]), DEFAULT);
        let metrics = result.delay.unwrap();
        assert_eq!(metrics.min_one_way_delay, 30_000);
        assert_eq!(metrics.max_one_way_delay, 30_000);
        assert_eq!(metrics.feedback_hold, Duration::from_millis(120));
        assert_eq!(metrics.rtt_sample, Duration::from_millis(300));
    }

    #[test]
    fn recovered_ce_has_no_new_ack_credit_and_ambiguous_receipts_have_no_delay() {
        let recovered = FeedbackSample {
            newly_acked: false,
            recovered: true,
            ecn: Some(EcnMark::Ce),
            ..sample(0, 10_000)
        };
        let ambiguous = FeedbackSample {
            receiver_arrival_micros: None,
            ..sample(1, 11_000)
        };
        let mut data = input(&[]);
        let samples = [recovered, ambiguous];
        data.feedback = &samples;
        data.ecn_mode = EcnMode::L4s;
        let result = Feedback::parse(Duration::from_millis(100), data, DEFAULT);
        assert_eq!(result.acked_not_marked, 1_000);
        assert_eq!(result.received, 2_000);
        assert_eq!(result.num_ce, 1);
        assert_eq!(result.num_recovered, 1);
        assert_eq!(result.delay.unwrap().max_one_way_delay, 10_000);
        let result = Feedback::parse(Duration::from_millis(100), input(&[ambiguous]), DEFAULT);
        assert!(result.delay.is_none());
    }
}

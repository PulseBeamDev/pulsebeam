#[path = "../src/congestion/screamv2.rs"]
mod screamv2;

use std::time::Duration;

use proptest::prelude::*;
use screamv2::{ControllerInput, EcnMark, EcnMode, FeedbackSample, Output, PROFILE, ScreamV2};

fn run_trace(events: &[(u64, u32, u64, bool, bool, bool, bool)]) -> Vec<Output> {
    let mut controller = ScreamV2::new(100_000_000, None);
    let mut now = Duration::ZERO;
    let mut outputs = Vec::with_capacity(events.len());
    for &(advance_us, bytes, bytes_in_flight, received, newly_acked, lost, ce) in events {
        now = now.saturating_add(Duration::from_micros(advance_us.max(1)));
        let sent_at = now.saturating_sub(Duration::from_micros(advance_us.min(1_000_000)));
        let sample = FeedbackSample {
            sent_at,
            received_at: now,
            transport_bytes: bytes,
            received,
            newly_acked,
            lost,
            receiver_arrival_micros: received.then_some(
                i64::try_from(now.as_micros().min(i128::from(i64::MAX) as u128))
                    .unwrap_or(i64::MAX),
            ),
            ecn: ce.then_some(EcnMark::Ce),
        };
        let feedback = [sample];
        outputs.push(controller.update(
            now,
            ControllerInput {
                feedback: &feedback,
                feedback_hold: Duration::ZERO,
                bytes_in_flight,
                target_bitrate_max: 100_000_000,
                ecn_mode: if ce { EcnMode::Classic } else { EcnMode::Disabled },
            },
        ));
    }
    outputs
}

proptest! {
    #[test]
    fn arbitrary_extreme_inputs_keep_the_envelope_bounded(
        now_us in 1_u64..=u64::MAX,
        age_us in 0_u64..=10_000_000,
        bytes in any::<u32>(),
        bytes_in_flight in any::<u64>(),
        target_max in 1_u64..=100_000_000,
        received in any::<bool>(),
        newly_acked in any::<bool>(),
        lost in any::<bool>(),
        ce in any::<bool>(),
    ) {
        let now = Duration::from_micros(now_us);
        let sent_at = now.saturating_sub(Duration::from_micros(age_us));
        let sample = FeedbackSample {
            sent_at,
            received_at: now,
            transport_bytes: bytes,
            received,
            newly_acked,
            lost,
            receiver_arrival_micros: received.then_some(i64::MAX),
            ecn: ce.then_some(EcnMark::Ce),
        };
        let feedback = [sample];
        let mut controller = ScreamV2::new(target_max, Some(u32::MAX));
        let output = controller.update(
            now,
            ControllerInput {
                feedback: &feedback,
                feedback_hold: Duration::MAX,
                bytes_in_flight,
                target_bitrate_max: target_max,
                ecn_mode: if ce { EcnMode::L4s } else { EcnMode::Disabled },
            },
        );

        prop_assert!(output.target_bitrate <= target_max);
        prop_assert!(output.reference_window >= PROFILE.min_reference_window);
        prop_assert!(output.queue_delay_target >= PROFILE.queue_target_low);
        prop_assert!(output.queue_delay_target <= PROFILE.queue_target_high);
    }

    #[test]
    fn arbitrary_trace_replay_is_deterministic(
        events in prop::collection::vec(
            (
                1_u64..=1_000_000,
                any::<u32>(),
                any::<u64>(),
                any::<bool>(),
                any::<bool>(),
                any::<bool>(),
                any::<bool>(),
            ),
            1..64,
        )
    ) {
        let first = run_trace(&events);
        let second = run_trace(&events);
        prop_assert_eq!(first, second);
    }
}

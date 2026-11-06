use super::*;

#[test]
fn sustained_large_ack_credit_is_not_capped_at_one_hundred_megabits() {
    fn run(cap: Option<u64>) -> u64 {
        let mut controller = Controller::new(Config {
            maximum_payload_rate: cap,
        })
        .unwrap();
        fn base(feedback: &[FeedbackSample]) -> Observation<'_> {
            Observation {
                path_epoch: Some(1),
                path_available: true,
                feedback,
                feedback_hold: Duration::ZERO,
                fresh_network_feedback: !feedback.is_empty(),
                bytes_in_flight: u64::MAX,
                paced_queue_bytes: 0,
                desired_media_rate: u64::MAX,
                ecn: EcnValidation::default(),
            }
        }
        controller.update(Duration::ZERO, base(&[])).unwrap();
        for i in 1..=40 {
            let now = Duration::from_millis(i * 100);
            let sample = FeedbackSample {
                sent_at: now.saturating_sub(Duration::from_millis(100)),
                received_at: now,
                transport_bytes: u32::MAX,
                received: true,
                newly_acked: true,
                lost: false,
                recovered: false,
                receiver_arrival_micros: Some(i64::try_from(i * 100_000).unwrap_or(i64::MAX)),
                ecn: None,
            };
            controller.update(now, base(&[sample])).unwrap();
        }
        controller.snapshot().target_media_payload_rate
    }
    assert!(run(None) > 100_000_000);
    assert_eq!(run(Some(50_000_000)), 50_000_000);
}

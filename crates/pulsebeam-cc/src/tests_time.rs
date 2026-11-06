use super::*;

#[test]
fn extreme_valid_times_and_receiver_clocks_are_bounded() {
    let mut controller = Controller::new(Config::default()).unwrap();
    let base = Observation {
        path_epoch: Some(1),
        path_available: true,
        feedback: &[],
        feedback_hold: Duration::ZERO,
        fresh_network_feedback: false,
        bytes_in_flight: u64::MAX,
        paced_queue_bytes: u64::MAX,
        desired_media_rate: u64::MAX,
        ecn: EcnValidation::default(),
    };
    controller.update(Duration::ZERO, base).unwrap();
    let samples = [
        FeedbackSample {
            sent_at: MAX_TIME,
            received_at: MAX_TIME,
            transport_bytes: u32::MAX,
            received: true,
            newly_acked: true,
            lost: false,
            recovered: false,
            receiver_arrival_micros: Some(i64::MIN),
            ecn: Some(EcnMark::Ect1),
        },
        FeedbackSample {
            sent_at: MAX_TIME.saturating_sub(Duration::from_micros(1)),
            received_at: MAX_TIME,
            transport_bytes: u32::MAX,
            received: true,
            newly_acked: false,
            lost: false,
            recovered: true,
            receiver_arrival_micros: Some(i64::MAX),
            ecn: Some(EcnMark::Ce),
        },
    ];
    let observed = Observation {
        feedback: &samples,
        feedback_hold: MAX_TIME,
        fresh_network_feedback: true,
        ..base
    };
    controller.update(MAX_TIME, observed).unwrap();
    let before = controller.snapshot();
    assert_eq!(
        controller.update(MAX_TIME.saturating_add(Duration::from_nanos(1)), base),
        Err(UpdateError::InvalidTime)
    );
    assert_eq!(controller.snapshot(), before);
    controller.commit_send(Duration::MAX, usize::MAX, u64::MAX, false);
    assert_eq!(controller.diagnostics().charged_bytes, usize::MAX as u128);
    assert_ne!(controller.ready(Duration::MAX, 0, 1), Readiness::Ready);
}

use super::*;

fn observation<'a>(feedback: &'a [FeedbackSample]) -> Observation<'a> {
    Observation {
        path_epoch: Some(1),
        path_available: true,
        feedback,
        feedback_hold: Duration::ZERO,
        fresh_network_feedback: !feedback.is_empty(),
        bytes_in_flight: 0,
        paced_queue_bytes: 0,
        desired_media_rate: 1_000_000,
        ecn: EcnValidation::default(),
    }
}

fn sample() -> FeedbackSample {
    FeedbackSample {
        sent_at: Duration::ZERO,
        received_at: Duration::from_millis(10),
        transport_bytes: 1200,
        received: true,
        newly_acked: true,
        lost: false,
        recovered: false,
        receiver_arrival_micros: Some(-1000),
        ecn: Some(EcnMark::Ect0),
    }
}

fn started() -> Controller {
    let mut controller = Controller::new(Config::default()).unwrap();
    controller.update(Duration::ZERO, observation(&[])).unwrap();
    controller
}

#[test]
fn invalid_batch_is_transactional_and_time_is_not_consumed() {
    let mut controller = started();
    let before = controller.snapshot();
    let diag = controller.diagnostics();
    let good = sample();
    let invalids = [
        FeedbackSample {
            transport_bytes: 0,
            ..good
        },
        FeedbackSample { lost: true, ..good },
        FeedbackSample {
            received: false,
            recovered: true,
            newly_acked: false,
            receiver_arrival_micros: None,
            ecn: None,
            ..good
        },
        FeedbackSample {
            recovered: true,
            ..good
        },
        FeedbackSample {
            received: false,
            ecn: None,
            ..good
        },
        FeedbackSample {
            received: false,
            receiver_arrival_micros: None,
            ..good
        },
        FeedbackSample {
            sent_at: Duration::from_millis(11),
            ..good
        },
        FeedbackSample {
            received_at: Duration::from_secs(2),
            ..good
        },
    ];
    for bad in invalids {
        let batch = [good, bad];
        assert_eq!(
            controller.update(Duration::from_secs(1), observation(&batch)),
            Err(UpdateError::InvalidFeedback)
        );
        assert_eq!(controller.snapshot(), before);
        assert_eq!(controller.diagnostics(), diag);
    }
    assert!(
        controller
            .update(Duration::from_millis(10), observation(&[good]))
            .is_ok()
    );
    assert_eq!(
        controller.update(Duration::ZERO, observation(&[])),
        Err(UpdateError::InvalidTime)
    );
}

#[test]
fn batch_bound_path_change_and_ack_gap_rules() {
    let mut controller = started();
    let gap = FeedbackSample {
        received: false,
        newly_acked: true,
        lost: false,
        recovered: false,
        receiver_arrival_micros: None,
        ecn: None,
        ..sample()
    };
    let bounded = vec![gap; MAX_FEEDBACK_SAMPLES];
    assert!(
        controller
            .update(Duration::from_millis(10), observation(&bounded))
            .is_ok()
    );
    let too_many = vec![gap; MAX_FEEDBACK_SAMPLES + 1];
    let before = controller.snapshot();
    assert_eq!(
        controller.update(Duration::from_secs(1), observation(&too_many)),
        Err(UpdateError::TooManyFeedbackSamples)
    );
    let gap_batch = [gap];
    let changed = Observation {
        path_epoch: Some(2),
        ..observation(&gap_batch)
    };
    assert_eq!(
        controller.update(Duration::from_secs(1), changed),
        Err(UpdateError::PathChangeWithFeedback)
    );
    let missing = Observation {
        path_epoch: None,
        ..observation(&[])
    };
    assert_eq!(
        controller.update(Duration::from_secs(1), missing),
        Err(UpdateError::MissingPathEpoch)
    );
    assert_eq!(controller.snapshot(), before);
    assert!(
        controller
            .update(
                Duration::from_millis(10),
                Observation {
                    path_epoch: Some(2),
                    ..observation(&[])
                }
            )
            .is_ok()
    );
}

#[test]
fn full_packet_readiness_is_checked_and_nonreserving() {
    let mut controller = started();
    let window = controller.snapshot().max_rtp_bytes_in_flight;
    assert_eq!(controller.ready(Duration::ZERO, 0, 1), Readiness::Ready);
    assert_eq!(controller.ready(Duration::ZERO, 0, 1), Readiness::Ready);
    assert_eq!(
        controller.ready(Duration::ZERO, window, 1),
        Readiness::CongestionWindow
    );
    assert_eq!(
        controller.ready(Duration::ZERO, u64::MAX, usize::MAX),
        Readiness::CongestionWindow
    );
    assert_eq!(
        controller.ready(Duration::ZERO, 0, 0),
        Readiness::InvalidPacket
    );
    assert_eq!(
        controller.ready(Duration::MAX, 0, 1),
        Readiness::InvalidTime
    );
    controller.envelope.max_rtp_bytes_in_flight = u64::MAX;
    assert_eq!(
        controller.ready(Duration::ZERO, u64::MAX - 1, 2),
        Readiness::CongestionWindow
    );
    // A rejected packet must not advance the fence and drain debt later.
    controller.commit_send(Duration::ZERO, 50_000, 0, false);
    assert_eq!(
        controller.ready(Duration::from_secs(1), 0, 0),
        Readiness::InvalidPacket
    );
    assert_eq!(
        controller.next_deadline(Duration::from_secs(1), Some((0, 0)), None),
        None
    );
    controller.commit_send(Duration::ZERO, 1, 0, false);
    assert!(matches!(
        controller.ready(Duration::ZERO, 0, 1),
        Readiness::Pacing { .. }
    ));
}

#[test]
fn commits_charge_even_with_invalid_time_outage_and_queue_changes() {
    let mut controller = started();
    let t = Duration::from_secs(1);
    controller.commit_send(t, 50_000, 50_000, false);
    controller.commit_send(Duration::ZERO, 500, 50_500, false);
    controller.commit_send(Duration::MAX, 1000, 51_500, true);
    assert_eq!(controller.diagnostics().charged_bytes, 51_500);
    assert_eq!(controller.diagnostics().flight_peak, 51_500);
    assert!(matches!(
        controller.ready(t, 0, 1),
        Readiness::Pacing { .. }
    ));
    controller.update_queue(100_000, Duration::from_secs(1));
    let with_queue = controller.snapshot();
    assert!(with_queue.pacing_transport_rate > with_queue.nominal_pacing_transport_rate);
    controller.update_queue(0, Duration::ZERO);
    let outage = Observation {
        path_available: false,
        ..observation(&[])
    };
    controller.update(t, outage).unwrap();
    assert_eq!(controller.ready(t, 0, 1), Readiness::PathUnavailable);
    controller.update(t, observation(&[])).unwrap();
    assert!(matches!(
        controller.ready(t, 0, 1),
        Readiness::Pacing { .. }
    ));
    let target = controller.snapshot().target_media_payload_rate;
    controller
        .update(
            t,
            Observation {
                path_epoch: Some(2),
                ..observation(&[])
            },
        )
        .unwrap();
    assert_eq!(controller.snapshot().target_media_payload_rate, target);
    assert_eq!(controller.diagnostics().charged_bytes, 51_500);
    assert_eq!(controller.diagnostics().flight_peak, 0);
    assert_eq!(controller.ready(t, 0, 1), Readiness::Ready);
}

#[test]
fn idle_does_not_grow_and_blocked_deadlines_do_not_spin() {
    let mut controller = started();
    let window = controller.snapshot().max_rtp_bytes_in_flight;
    controller.process_idle(Duration::from_secs(1));
    assert_eq!(controller.snapshot().max_rtp_bytes_in_flight, window);
    assert_eq!(
        controller.next_deadline(Duration::from_secs(1), Some((window, 1)), None),
        Some(Duration::from_secs(3))
    );
    assert_eq!(
        controller.next_deadline(Duration::from_secs(1), Some((window, 1)), Some((window, 1))),
        Some(Duration::from_secs(3))
    );
    controller
        .update(
            Duration::from_secs(1),
            Observation {
                path_available: false,
                ..observation(&[])
            },
        )
        .unwrap();
    assert_eq!(
        controller.next_deadline(Duration::from_secs(1), Some((0, 1)), Some((0, 1))),
        None
    );
    assert_eq!(controller.prepare_padding(Duration::from_secs(1)), None);
}

#[test]
fn cap_is_optional_positive_and_enforced() {
    assert!(matches!(
        Controller::new(Config {
            maximum_payload_rate: Some(0)
        }),
        Err(ConfigError::ZeroMaximumPayloadRate)
    ));
    let capped = Controller::new(Config {
        maximum_payload_rate: Some(20_000),
    })
    .unwrap();
    assert_eq!(capped.snapshot().target_media_payload_rate, 20_000);
    let unlimited = Controller::new(Config::default()).unwrap();
    assert_eq!(unlimited.snapshot().target_media_payload_rate, 300_000);
}

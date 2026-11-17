use super::*;

#[test]
fn validated_ce_reacts_while_bleaching_and_path_reset_isolate_ecn_evidence() {
    let observe_ce = |ecn| {
        let mut controller = Controller::new(Config::default()).unwrap();
        controller
            .update(
                Duration::ZERO,
                Observation {
                    ecn,
                    ..observation(&[])
                },
            )
            .unwrap();
        assert_eq!(controller.ready(Duration::ZERO, 0, 1_200), Readiness::Ready);
        controller.commit_send(Duration::ZERO, 1_200, 1_200, false);
        let feedback = [FeedbackSample {
            received_at: Duration::from_millis(100),
            ecn: Some(EcnMark::Ce),
            ..sample()
        }];
        let envelope = controller
            .update(
                Duration::from_millis(100),
                Observation {
                    ecn,
                    ..observation(&feedback)
                },
            )
            .unwrap();
        (controller, envelope)
    };
    let unvalidated = observe_ce(EcnValidation::default()).1;
    let bleached = observe_ce(EcnValidation {
        classic: true,
        l4s: true,
        bleached: true,
    })
    .1;
    assert_eq!(bleached, unvalidated);
    let initial_reference = started().snapshot().reference_window;
    for ecn in [
        EcnValidation {
            classic: true,
            ..EcnValidation::default()
        },
        EcnValidation {
            l4s: true,
            ..EcnValidation::default()
        },
    ] {
        let (mut controller, congested) = observe_ce(ecn);
        assert!(
            congested.reference_window < unvalidated.reference_window,
            "validated congestion must constrain the reference window: {ecn:?}: {congested:?} versus {unvalidated:?}"
        );
        assert_eq!(congested.l4s_enabled, ecn.l4s);
        let reset = controller
            .update(
                Duration::from_millis(100),
                Observation {
                    path_epoch: Some(2),
                    ..observation(&[])
                },
            )
            .unwrap();
        assert!(!reset.l4s_enabled);
        assert_eq!(reset.reference_window, initial_reference);
        assert_eq!(reset.smoothed_rtt, Duration::ZERO);
        assert_eq!(reset.queue_delay, Duration::ZERO);
        assert_eq!(
            reset.target_media_payload_rate,
            congested.target_media_payload_rate
        );
        assert_eq!(controller.diagnostics().charged_bytes, 1_200);
        assert_eq!(
            controller.ready(Duration::from_millis(100), 0, 1_200),
            Readiness::Ready
        );
    }
}

#[test]
fn queue_pushback_and_debt_survive_rate_changes_until_path_replacement() {
    let mut controller = started();
    let initial = controller.snapshot();
    assert!(initial.target_media_payload_rate > 0);
    assert_eq!(
        initial.encoder_target_media_payload_rate,
        initial.target_media_payload_rate
    );
    assert_eq!(controller.ready(Duration::ZERO, 0, 1_200), Readiness::Ready);
    let mut charged = 0;
    let mut flight = 0;
    while controller.ready(Duration::ZERO, flight, 1_200) == Readiness::Ready {
        flight += 1_200;
        controller.commit_send(Duration::ZERO, 1_200, flight, false);
        charged += 1_200_u128;
        assert!(charged <= 100_000, "pacer must bound its initial burst");
    }
    assert!(charged > 0);
    assert_eq!(controller.diagnostics().charged_bytes, charged);
    assert!(matches!(
        controller.ready(Duration::ZERO, flight, 1_200),
        Readiness::Pacing { deadline: Some(_) }
    ));

    for demand in [2_000_000, 200_000, 4_000_000] {
        let now = Duration::ZERO;
        controller.update_queue(100_000, Duration::from_secs(1));
        let queued = controller.snapshot();
        assert!(queued.pacing_transport_rate > queued.nominal_pacing_transport_rate);
        controller
            .update(
                now,
                Observation {
                    desired_media_rate: demand,
                    bytes_in_flight: flight,
                    paced_queue_bytes: 100_000,
                    ..observation(&[])
                },
            )
            .unwrap();
        let pressured = controller.snapshot();
        assert!(pressured.cwnd_reduce_ratio > initial.cwnd_reduce_ratio);
        assert_eq!(pressured.cwnd_reduce_ratio, u16::MAX);
        assert!(!pressured.encoder_paused);
        assert_eq!(
            pressured.target_media_payload_rate,
            initial.target_media_payload_rate
        );
        assert_eq!(
            pressured.encoder_target_media_payload_rate,
            pressured.target_media_payload_rate
        );
        assert_eq!(controller.diagnostics().charged_bytes, charged);
    }
    let now = Duration::ZERO;
    controller.update_queue(0, Duration::ZERO);
    let drained = controller
        .update(
            now,
            Observation {
                bytes_in_flight: flight,
                ..observation(&[])
            },
        )
        .unwrap();
    assert!(!drained.encoder_paused);
    assert_eq!(drained.cwnd_reduce_ratio, initial.cwnd_reduce_ratio);
    assert_eq!(
        drained.encoder_target_media_payload_rate,
        drained.target_media_payload_rate
    );
    assert_eq!(
        drained.pacing_transport_rate,
        drained.nominal_pacing_transport_rate
    );
    controller
        .update(
            now,
            Observation {
                path_available: false,
                bytes_in_flight: flight,
                ..observation(&[])
            },
        )
        .unwrap();
    assert_eq!(
        controller.ready(now, flight, 1_200),
        Readiness::PathUnavailable
    );
    controller
        .update(
            now,
            Observation {
                bytes_in_flight: flight,
                ..observation(&[])
            },
        )
        .unwrap();
    assert!(matches!(
        controller.ready(now, flight, 1_200),
        Readiness::Pacing { deadline: Some(_) }
    ));
    controller.process_idle(Duration::from_secs(60));
    // The host expires old flight during the idle interval, without network feedback.
    let expired = controller
        .update(Duration::from_secs(60), observation(&[]))
        .unwrap();
    assert_eq!(
        expired.target_media_payload_rate,
        initial.target_media_payload_rate
    );
    assert_eq!(
        controller.ready(Duration::from_secs(60), 0, 1_200),
        Readiness::Ready
    );
    let mut flight = 0;
    while controller.ready(Duration::from_secs(60), flight, 1_200) == Readiness::Ready {
        flight += 1_200;
        controller.commit_send(Duration::from_secs(60), 1_200, flight, false);
        charged += 1_200;
        assert!(charged <= 200_000, "pacer must bound its resumed burst");
    }
    controller
        .update(
            Duration::from_secs(60),
            Observation {
                path_epoch: Some(2),
                ..observation(&[])
            },
        )
        .unwrap();
    assert_eq!(
        controller.ready(Duration::from_secs(60), 0, 1_200),
        Readiness::Ready
    );
    assert_eq!(controller.diagnostics().charged_bytes, charged);
}

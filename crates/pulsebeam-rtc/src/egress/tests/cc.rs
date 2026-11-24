use super::*;

#[test]
fn positive_insufficient_window_slack_has_no_immediate_data_or_padding_wake() {
    let start = Instant::now();
    let mut egress = repair_egress(start, 4096);
    egress.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(2_000_000)));
    egress
        .update_controller(start, Some((1, true)), &[], Duration::ZERO, false, 0)
        .unwrap();
    let window = egress.envelope.unwrap().max_rtp_bytes_in_flight;
    egress
        .update_controller(start, None, &[], Duration::ZERO, false, window - 1)
        .unwrap();
    assert!(egress.padding_candidate().is_some());
    assert!(egress.next_deadline(start).is_none_or(|at| at > start));
    egress
        .admit(
            start,
            "audio",
            RtpWrite {
                sequence: 0,
                timestamp: 0,
                marker: false,
                payload: vec![0; 128].into(),
                extensions: Vec::new(),
            },
        )
        .unwrap();
    assert!(egress.next_deadline(start).is_none_or(|at| at > start));
}

#[test]
fn rtc_adapter_grows_above_old_cap_and_keeps_snapshot_demand_and_route_uncapped() {
    let start = Instant::now();
    let mut egress = audio_egress(start);
    egress.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(2_000_000_000)));
    egress
        .update_controller(start, Some((1, true)), &[], Duration::ZERO, false, 0)
        .unwrap();
    assert_eq!(egress.bandwidth_estimate, 300_000);
    let mut feedback = Vec::with_capacity(128);
    let mut elapsed = Duration::ZERO;
    for tick in 1..=15_000 {
        elapsed = Duration::from_millis(tick);
        egress
            .update_controller(
                start + elapsed,
                None,
                &feedback,
                Duration::ZERO,
                !feedback.is_empty(),
                0,
            )
            .unwrap();
        feedback.clear();
        let mut flight = 0_u64;
        for _ in 0..128 {
            if egress.controller.ready(elapsed, flight, 1200) != Readiness::Ready {
                break;
            }
            flight += 1200;
            egress.controller.commit_send(elapsed, 1200, flight, false);
            feedback.push(FeedbackSample {
                sent_at: elapsed,
                received_at: elapsed + Duration::from_millis(1),
                transport_bytes: 1200,
                received: true,
                newly_acked: true,
                lost: false,
                recovered: false,
                receiver_arrival_micros: Some(
                    i64::try_from(elapsed.as_micros()).unwrap() - 1_000_000,
                ),
                ecn: None,
            });
        }
        if egress.bandwidth_estimate > 100_000_000 {
            break;
        }
    }
    assert!(
        egress.bandwidth_estimate > 100_000_000,
        "{}",
        egress.bandwidth_estimate
    );
    elapsed += Duration::from_millis(1);
    egress
        .update_controller(
            start + elapsed,
            None,
            &feedback,
            Duration::ZERO,
            !feedback.is_empty(),
            0,
        )
        .unwrap();
    let target = egress.bandwidth_estimate;
    egress.set_desired_bitrate(None);
    egress
        .update_controller(start + elapsed, None, &[], Duration::ZERO, false, 0)
        .unwrap();
    assert_eq!(egress.bandwidth_estimate, target);
    assert_eq!(egress.envelope.unwrap().padding_transport_rate, 0);
    egress.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(2_000_000_000)));
    egress
        .update_controller(
            start + elapsed,
            Some((2, true)),
            &[],
            Duration::ZERO,
            false,
            0,
        )
        .unwrap();
    assert_eq!(egress.bandwidth_estimate, target);
    assert_eq!(
        egress.controller.snapshot().target_media_payload_rate,
        target
    );
    assert!(target > 100_000_000);
}

#[test]
fn encoder_budget_churn_and_same_epoch_pause_cannot_refund_an_accepted_burst() {
    let start = Instant::now();
    let mut baseline = audio_egress(start);
    let mut changed = audio_egress(start);
    for egress in [&mut baseline, &mut changed] {
        egress.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(192_000)));
        egress
            .update_controller(start, Some((1, true)), &[], Duration::ZERO, false, 0)
            .unwrap();
        for flight in [0, 1_180] {
            for _ in 0..2 {
                assert_eq!(
                    egress.controller.ready(Duration::ZERO, flight, 1_180),
                    Readiness::Ready
                );
            }
            assert_eq!(
                egress.controller.diagnostics().charged_bytes,
                u128::from(flight),
                "readiness is not acceptance"
            );
            egress
                .controller
                .commit_send(Duration::ZERO, 1_180, flight + 1_180, false);
        }
        assert_eq!(egress.controller.diagnostics().charged_bytes, 2_360);
    }
    let before = baseline.controller.ready(Duration::ZERO, 2_360, 320);
    assert!(matches!(before, Readiness::Pacing { deadline: Some(at) } if at > Duration::ZERO));

    for (sequence, demand) in [Some(96_000), None, Some(384_000), Some(192_000)]
        .into_iter()
        .enumerate()
    {
        changed.set_desired_bitrate(demand.map(MediaPayloadBitrate::from_bps));
        changed
            .admit(
                start,
                "audio",
                RtpWrite {
                    sequence: u64::try_from(sequence).unwrap(),
                    timestamp: 0,
                    marker: true,
                    payload: vec![0; 320].into(),
                    extensions: Vec::new(),
                },
            )
            .unwrap();
        changed
            .update_controller(start, Some((1, false)), &[], Duration::ZERO, false, 2_360)
            .unwrap();
        assert_eq!(
            changed.controller.ready(Duration::ZERO, 2_360, 320),
            Readiness::PathUnavailable
        );
        changed.discard_pending("audio").unwrap();
        changed
            .update_controller(start, Some((1, true)), &[], Duration::ZERO, false, 2_360)
            .unwrap();
        assert_eq!(changed.controller.ready(Duration::ZERO, 2_360, 320), before);
        assert_eq!(changed.controller.diagnostics().charged_bytes, 2_360);
    }

    let feedback = [FeedbackSample {
        sent_at: Duration::ZERO,
        received_at: Duration::from_millis(200),
        transport_bytes: 1_180,
        received: true,
        newly_acked: true,
        lost: false,
        recovered: false,
        receiver_arrival_micros: Some(-800_000),
        ecn: None,
    }; 2];
    for egress in [&mut baseline, &mut changed] {
        egress
            .update_controller(
                start + Duration::from_millis(200),
                None,
                &feedback,
                Duration::from_millis(40),
                true,
                0,
            )
            .unwrap();
        let now = Duration::from_millis(200);
        assert_eq!(egress.controller.ready(now, 0, 320), Readiness::Ready);
        let window = egress.controller.snapshot().max_rtp_bytes_in_flight;
        assert_eq!(
            egress.controller.ready(now, window - 319, 320),
            Readiness::CongestionWindow
        );
        assert_eq!(egress.controller.diagnostics().charged_bytes, 2_360);
        egress.controller.commit_send(now, 320, 320, false);
        assert_eq!(egress.controller.diagnostics().charged_bytes, 2_680);
    }
}

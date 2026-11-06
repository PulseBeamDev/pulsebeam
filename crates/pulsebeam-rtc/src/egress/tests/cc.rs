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
                wallclock: start,
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

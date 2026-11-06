use super::*;

#[test]
fn twcc_wire_clocks_wrap_without_synchronizing_sender_time() {
    let origin = Instant::now();
    let epoch = PathEpoch::from_value(1);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide, origin);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(context(origin, epoch, sequence)).unwrap();
    }
    for (sequence, reference, generation, expected) in [
        (1, 0xff_ffff, 255, 0xff_ffff_i64 * 64_000 + 250),
        (2, 0, 0, 0x100_0000_i64 * 64_000 + 250),
        (3, 1, 1, 0x100_0001_i64 * 64_000 + 250),
    ] {
        history.clear_controller_inputs();
        if sequence == 3 {
            let mut ambiguous = batch(
                origin + Duration::from_millis(9),
                epoch,
                vec![TwccStatus::Received { delta_250us: 1 }],
            );
            if let FeedbackReport::Twcc {
                base_sequence,
                reference_time,
                feedback_count,
                ..
            } = &mut ambiguous.report
            {
                *base_sequence = 3;
                *reference_time = 0x80_0000;
                *feedback_count = 128;
            }
            history.process_feedback(ambiguous);
            assert_eq!(history.controller_inputs().bytes_in_flight, 1_200);
            assert!(!history.controller_inputs().fresh_network_feedback);
            assert_eq!(history.controller_inputs().timing, None);
        }
        let arrival = origin + Duration::from_millis(10);
        let mut report = batch(
            arrival,
            epoch,
            vec![TwccStatus::Received { delta_250us: 1 }],
        );
        if let FeedbackReport::Twcc {
            base_sequence,
            reference_time,
            feedback_count,
            ..
        } = &mut report.report
        {
            *base_sequence = sequence;
            *reference_time = reference;
            *feedback_count = generation;
        }
        history.process_feedback(report);
        let packet = history.controller_inputs().feedback.iter().last().unwrap();
        assert_eq!(
            packet.receiver_arrival,
            Some(ReceiverTime::Micros(expected))
        );
        let sample = packet.controller_sample(origin + Duration::from_secs(3), origin);
        assert_eq!(sample.sent_at, Duration::ZERO);
        assert_eq!(sample.received_at, Duration::from_millis(10));
        assert!(sample.received && sample.newly_acked);
        assert!(!sample.lost && !sample.recovered);
        assert_eq!(
            history.controller_inputs().timing.unwrap().feedback_hold,
            None
        );
    }
    assert_eq!(history.counters().received, 3);
    assert_eq!(history.counters().unknown_feedback, 1);
    assert_eq!(history.controller_inputs().bytes_in_flight, 0);
}

#[test]
fn rfc8888_wire_clock_wrap_retains_hold_ecn_and_unavailable_arrivals() {
    let origin = Instant::now();
    let epoch = PathEpoch::from_value(1);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888, origin);
    history.path_changed(epoch, true);
    for sequence in 1..=4 {
        history
            .commit(rfc_context(origin, epoch, sequence))
            .unwrap();
    }
    let report = |timestamp, base, statuses: Vec<Rfc8888Status>| FeedbackBatch {
        received_at: origin + Duration::from_millis(10),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Rfc8888 {
            report_timestamp: timestamp,
            reports: vec![crate::rtcp::Rfc8888Report {
                ssrc: 7,
                begin_sequence: base,
                report_count: u16::try_from(statuses.len()).unwrap(),
                statuses: statuses.into(),
            }]
            .into(),
        },
    };
    history.clear_controller_inputs();
    history.process_feedback(report(
        u32::MAX,
        1,
        vec![Rfc8888Status::Received {
            ecn: 2,
            arrival_offset: ArrivalOffset::Ticks(1),
        }],
    ));
    let packet = history.controller_inputs().feedback.iter().last().unwrap();
    assert_eq!(
        packet.receiver_arrival,
        Some(ReceiverTime::Micros(65_535_999_008))
    );
    assert_eq!(
        packet.controller_sample(origin, origin).ecn,
        Some(ControllerEcnMark::Ect0)
    );
    assert_eq!(
        history.controller_inputs().timing.unwrap().feedback_hold,
        Some(Duration::from_micros(976))
    );
    history.clear_controller_inputs();
    history.process_feedback(report(
        0,
        2,
        vec![
            Rfc8888Status::Received {
                ecn: 3,
                arrival_offset: ArrivalOffset::OverRange,
            },
            Rfc8888Status::Received {
                ecn: 1,
                arrival_offset: ArrivalOffset::Unavailable,
            },
        ],
    ));
    let packets: Vec<_> = history.controller_inputs().feedback.iter().collect();
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].receiver_arrival, Some(ReceiverTime::OverRange));
    assert_eq!(packets[1].receiver_arrival, Some(ReceiverTime::Unavailable));
    for (packet, ecn) in packets
        .iter()
        .zip([ControllerEcnMark::Ce, ControllerEcnMark::Ect1])
    {
        let sample = packet.controller_sample(origin + Duration::from_secs(3), origin);
        assert_eq!(sample.received_at, Duration::from_millis(10));
        assert_eq!(sample.receiver_arrival_micros, None);
        assert_eq!(sample.ecn, Some(ecn));
    }
    assert_eq!(
        history.controller_inputs().timing.unwrap().feedback_hold,
        None
    );
    history.clear_controller_inputs();
    history.process_feedback(report(
        0x8000_0000,
        4,
        vec![Rfc8888Status::Received {
            ecn: 0,
            arrival_offset: ArrivalOffset::Ticks(0),
        }],
    ));
    assert_eq!(history.controller_inputs().bytes_in_flight, 1_200);
    assert!(!history.controller_inputs().fresh_network_feedback);
    assert_eq!(history.controller_inputs().timing, None);
    history.process_feedback(report(
        1,
        4,
        vec![Rfc8888Status::Received {
            ecn: 0,
            arrival_offset: ArrivalOffset::Ticks(0),
        }],
    ));
    assert_eq!(history.controller_inputs().bytes_in_flight, 0);
    assert_eq!(history.counters().received, 4);
    assert_eq!(history.counters().unknown_feedback, 1);
}

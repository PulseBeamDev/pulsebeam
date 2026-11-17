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

#[test]
fn equivalent_arrival_spacing_survives_clock_wrap_and_delayed_controller_observation() {
    use pulsebeam_cc::{Config, Controller, EcnValidation, Observation, Readiness};

    let origin = Instant::now();
    let epoch = PathEpoch::from_value(1);
    for mode in [
        PacketFeedbackKind::TransportWide,
        PacketFeedbackKind::Rfc8888,
    ] {
        for wrapping in [false, true] {
            let mut history = SentHistory::new(mode, origin);
            history.path_changed(epoch, true);
            let mut controller = Controller::new(Config::default()).unwrap();
            let observation = |flight| Observation {
                path_epoch: Some(1),
                path_available: true,
                feedback: &[],
                feedback_hold: Duration::ZERO,
                fresh_network_feedback: false,
                bytes_in_flight: flight,
                paced_queue_bytes: 0,
                desired_media_rate: 64_000,
                ecn: EcnValidation::default(),
            };
            controller.update(Duration::ZERO, observation(0)).unwrap();
            for sequence in 1..=4 {
                let sent_at = Duration::from_millis(u64::from(sequence - 1) * 125);
                let mut packet = if mode == PacketFeedbackKind::TransportWide {
                    context(origin + sent_at, epoch, sequence)
                } else {
                    rfc_context(origin + sent_at, epoch, sequence)
                };
                packet.wire_len = 200;
                history.commit(packet).unwrap();
                let flight = u64::from(sequence - 1) * 200;
                assert_eq!(controller.ready(sent_at, flight, 200), Readiness::Ready);
                controller.commit_send(sent_at, 200, flight + 200, false);
            }
            let receiver_base = match (mode, wrapping) {
                (PacketFeedbackKind::TransportWide, false) => 128_000_i64,
                (PacketFeedbackKind::TransportWide, true) => (1_i64 << 24) * 64_000 - 128_000,
                (PacketFeedbackKind::Rfc8888, false) => 125_000,
                (PacketFeedbackKind::Rfc8888, true) => 65_536_000_000 - 125_000,
            };
            let mut arrivals = Vec::new();
            for sequence in 1..=4_u16 {
                let elapsed = u64::from(sequence - 1) * 125;
                let arrival_micros = receiver_base + i64::try_from(elapsed).unwrap() * 1_000;
                let received_at = origin + Duration::from_millis(400 + elapsed);
                let observed_at = received_at + Duration::from_millis(300);
                let report = if mode == PacketFeedbackKind::TransportWide {
                    FeedbackReport::Twcc {
                        media_ssrc: 7,
                        base_sequence: sequence,
                        reference_time: u32::try_from((arrival_micros / 64_000) % (1 << 24))
                            .unwrap(),
                        feedback_count: u8::try_from((254 + sequence) % 256).unwrap(),
                        statuses: vec![TwccStatus::Received {
                            delta_250us: i16::try_from((arrival_micros % 64_000) / 250).unwrap(),
                        }]
                        .into(),
                    }
                } else {
                    let report_ticks = (arrival_micros + 62_500) * 65_536 / 1_000_000;
                    FeedbackReport::Rfc8888 {
                        report_timestamp: u32::try_from(report_ticks % (1_i64 << 32)).unwrap(),
                        reports: vec![crate::rtcp::Rfc8888Report {
                            ssrc: 7,
                            begin_sequence: sequence,
                            report_count: 1,
                            statuses: vec![Rfc8888Status::Received {
                                ecn: 0,
                                arrival_offset: ArrivalOffset::Ticks(64),
                            }]
                            .into(),
                        }]
                        .into(),
                    }
                };
                let batch = FeedbackBatch {
                    received_at,
                    path_epoch: epoch,
                    sender_ssrc: 9,
                    report,
                };
                history.clear_controller_inputs();
                history.process_feedback(batch.clone());
                let inputs = history.controller_inputs();
                assert!(inputs.fresh_network_feedback);
                assert_eq!(inputs.feedback.len(), 1);
                let sample = inputs
                    .feedback
                    .iter()
                    .next()
                    .unwrap()
                    .controller_sample(observed_at, origin);
                assert_eq!(sample.sent_at, Duration::from_millis(elapsed));
                assert_eq!(sample.received_at, Duration::from_millis(400 + elapsed));
                assert!(sample.received && sample.newly_acked && !sample.lost && !sample.recovered);
                arrivals.push(sample.receiver_arrival_micros.unwrap());
                let samples = [sample];
                let envelope = controller
                    .update(
                        observed_at.duration_since(origin),
                        Observation {
                            feedback: &samples,
                            feedback_hold: inputs.timing.unwrap().feedback_hold.unwrap_or_default(),
                            fresh_network_feedback: true,
                            ..observation(inputs.bytes_in_flight)
                        },
                    )
                    .unwrap();
                assert!(
                    envelope.queue_delay <= Duration::from_millis(1),
                    "constant transit is not a queue trend: {mode:?}, wrap={wrapping}"
                );

                history.clear_controller_inputs();
                history.process_feedback(batch);
                assert!(!history.controller_inputs().fresh_network_feedback);
                assert!(history.controller_inputs().feedback.is_empty());
                assert!(history.controller_inputs().timing.is_none());
            }
            for (first, second) in arrivals.iter().zip(arrivals.iter().skip(1)) {
                assert!((second - first - 125_000).abs() <= 1);
            }
            assert_eq!(history.controller_inputs().bytes_in_flight, 0);
            assert_eq!(history.counters().received, 4);
            assert_eq!(history.counters().not_received, 0);
        }
    }
}

#[test]
fn missing_tail_recovery_projects_to_feedback_the_controller_can_consume() {
    use pulsebeam_cc::{Config, Controller, EcnValidation, Observation, Readiness};

    let origin = Instant::now();
    let epoch = PathEpoch::from_value(1);
    for mode in [
        PacketFeedbackKind::TransportWide,
        PacketFeedbackKind::Rfc8888,
    ] {
        let mut history = SentHistory::new(mode, origin);
        history.path_changed(epoch, true);
        let mut controller = Controller::new(Config::default()).unwrap();
        let observation = |flight| Observation {
            path_epoch: Some(1),
            path_available: true,
            feedback: &[],
            feedback_hold: Duration::ZERO,
            fresh_network_feedback: false,
            bytes_in_flight: flight,
            paced_queue_bytes: 0,
            desired_media_rate: 192_000,
            ecn: EcnValidation::default(),
        };
        controller.update(Duration::ZERO, observation(0)).unwrap();
        for sequence in 1..=2 {
            let sent_at = Duration::from_millis(u64::from(sequence - 1) * 40);
            let packet = if mode == PacketFeedbackKind::TransportWide {
                context(origin + sent_at, epoch, sequence)
            } else {
                rfc_context(origin + sent_at, epoch, sequence)
            };
            history.commit(packet).unwrap();
            let flight = u64::from(sequence - 1) * 1_200;
            assert_eq!(controller.ready(sent_at, flight, 1_200), Readiness::Ready);
            controller.commit_send(sent_at, 1_200, flight + 1_200, false);
        }
        for (generation, base, delay) in [(1, 1, 200), (2, 2, 400)] {
            let report = if mode == PacketFeedbackKind::TransportWide {
                FeedbackReport::Twcc {
                    media_ssrc: 7,
                    base_sequence: base,
                    reference_time: generation,
                    feedback_count: u8::try_from(generation).unwrap(),
                    statuses: if base == 1 {
                        vec![
                            TwccStatus::Received { delta_250us: 1 },
                            TwccStatus::NotReceived,
                        ]
                    } else {
                        vec![TwccStatus::Received { delta_250us: 1 }]
                    }
                    .into(),
                }
            } else {
                let received = Rfc8888Status::Received {
                    ecn: 0,
                    arrival_offset: ArrivalOffset::Ticks(64),
                };
                FeedbackReport::Rfc8888 {
                    report_timestamp: generation * 16_384,
                    reports: vec![crate::rtcp::Rfc8888Report {
                        ssrc: 7,
                        begin_sequence: base,
                        report_count: if base == 1 { 2 } else { 1 },
                        statuses: if base == 1 {
                            vec![received, Rfc8888Status::NotReceived]
                        } else {
                            vec![received]
                        }
                        .into(),
                    }]
                    .into(),
                }
            };
            let now = Duration::from_millis(delay);
            history.clear_controller_inputs();
            history.process_feedback(FeedbackBatch {
                received_at: origin + now,
                path_epoch: epoch,
                sender_ssrc: 9,
                report,
            });
            let inputs = history.controller_inputs();
            assert!(inputs.fresh_network_feedback);
            assert_eq!(inputs.bytes_in_flight, if base == 1 { 1_200 } else { 0 });
            let samples = inputs
                .feedback
                .iter()
                .map(|packet| packet.controller_sample(origin + now, origin))
                .collect::<Vec<_>>();
            controller
                .update(
                    now,
                    Observation {
                        feedback: &samples,
                        feedback_hold: inputs.timing.unwrap().feedback_hold.unwrap_or_default(),
                        fresh_network_feedback: true,
                        ..observation(inputs.bytes_in_flight)
                    },
                )
                .unwrap_or_else(|error| {
                    panic!(
                        "valid {mode:?} tail feedback rejected at generation {generation}: {error:?}: {samples:?}"
                    )
                });
        }
        assert_eq!(history.counters().not_received, 0);
        assert_eq!(history.counters().received, 2);
    }
}

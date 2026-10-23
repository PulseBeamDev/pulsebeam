use super::*;
use crate::transport::PreparedRtpIdentity;

fn context(at: Instant, epoch: PathEpoch, sequence: u16) -> TransmitCommitContext {
    TransmitCommitContext {
        at,
        kind: DatagramKind::Rtp,
        wire_len: 1_200,
        path_epoch: Some(epoch),
        rtp: Some(PreparedRtpIdentity {
            ssrc: 7,
            sequence,
            index: u64::from(sequence),
            twcc_sequence: Some(sequence),
            service: RtpService::Original,
        }),
    }
}

fn at(monotonic: Instant) -> Instant {
    monotonic
}

fn batch(now: Instant, epoch: PathEpoch, statuses: Vec<TwccStatus>) -> FeedbackBatch {
    FeedbackBatch {
        received_at: at(now),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Twcc {
            media_ssrc: 7,
            base_sequence: 1,
            reference_time: 0,
            feedback_count: 1,
            statuses: statuses.into(),
        },
    }
}

#[test]
fn unreported_twcc_prefix_gets_span_credit_without_loss_evidence() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(1);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    let mut report = batch(
        now + Duration::from_millis(10),
        epoch,
        vec![TwccStatus::Received { delta_250us: 1 }],
    );
    if let FeedbackReport::Twcc { base_sequence, .. } = &mut report.report {
        *base_sequence = 3;
    }
    history.process_feedback(report);
    assert_eq!(history.bytes_in_flight, 0);
    assert_eq!(
        history
            .inputs
            .feedback
            .iter()
            .filter(|sample| sample.newly_acked)
            .count(),
        3
    );
    for sequence in 1..=2 {
        let id = history.twcc_sent_id(sequence).unwrap();
        assert_eq!(
            history.entry(id).unwrap().acknowledgment,
            Acknowledgment::Pending
        );
    }
    assert!(history.missing.is_empty());
    history.confirm_losses(now + Duration::from_secs(1), MAX_EXPIRATIONS_PER_POLL);
    assert!(!history.inputs.feedback.iter().any(|sample| sample.lost));
    history.clear_controller_inputs();
    history.process_feedback(batch(
        now + Duration::from_secs(1),
        epoch,
        vec![TwccStatus::Received { delta_250us: 1 }; 2],
    ));
    assert_eq!(
        history
            .inputs
            .feedback
            .iter()
            .filter(|sample| sample.received)
            .count(),
        2
    );
    assert!(
        !history
            .inputs
            .feedback
            .iter()
            .any(|sample| sample.newly_acked || sample.lost)
    );
}

#[test]
fn absent_feedback_mode_rejects_rtp_commit() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(1);
    let mut history = SentHistory::new(None);
    history.path_changed(epoch, true);
    let before = history.counters();
    history
        .commit(TransmitCommitContext {
            at: now,
            kind: DatagramKind::Sctp,
            wire_len: 100,
            path_epoch: Some(epoch),
            rtp: None,
        })
        .expect("SCTP commit without RTP feedback");
    assert_eq!(history.counters(), before);
    assert_eq!(
        history.preflight_commit(context(now, epoch, 1)),
        Err(HistoryError::InvalidCommit)
    );
    assert_eq!(history.bytes_in_flight, 0);
    assert_eq!(history.next_sent_id, 0);
}

proptest::proptest! {
    #[test]
    fn received_edge_credit_is_once_only_across_gaps_and_overlap(
        received in proptest::collection::vec(proptest::bool::ANY, 1..128)
    ) {
        let now = Instant::now();
        let epoch = PathEpoch::from_value(35);
        let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
        history.path_changed(epoch, true);
        for sequence in 1..=received.len() {
            history.commit(context(now, epoch, u16::try_from(sequence).unwrap())).unwrap();
        }
        let first = received.iter().map(|received| if *received {
            TwccStatus::Received { delta_250us: 1 }
        } else {
            TwccStatus::NotReceived
        }).collect::<Vec<_>>();
        history.process_feedback(batch(now + Duration::from_millis(10), epoch, first));
        let first_credit = history.inputs.feedback.iter().filter(|sample| sample.newly_acked).count();
        history.clear_controller_inputs();
        history.process_feedback(FeedbackBatch {
            received_at: at(now + Duration::from_millis(20)),
            path_epoch: epoch,
            sender_ssrc: 9,
            report: FeedbackReport::Twcc {
                media_ssrc: 7,
                base_sequence: 1,
                reference_time: 0,
                feedback_count: 2,
                statuses: vec![TwccStatus::Received { delta_250us: 1 }; received.len()].into(),
            },
        });
        let second_credit = history.inputs.feedback.iter().filter(|sample| sample.newly_acked).count();
        proptest::prop_assert_eq!(first_credit + second_credit, received.len());
        proptest::prop_assert_eq!(history.bytes_in_flight, 0);
        proptest::prop_assert_eq!(history.counters.received, received.len() as u64);
        proptest::prop_assert!(history.inputs.feedback.len() <= MAX_FEEDBACK_STATUSES);
    }
}

#[test]
fn ack_edge_retires_missing_bytes_but_defers_loss() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(1);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    history.process_feedback(batch(
        now + Duration::from_millis(20),
        epoch,
        vec![
            TwccStatus::Received { delta_250us: 1 },
            TwccStatus::NotReceived,
            TwccStatus::Received { delta_250us: 1 },
        ],
    ));
    assert_eq!(history.inputs.bytes_in_flight, 0);
    assert_eq!(
        history
            .inputs
            .feedback
            .iter()
            .filter(|sample| sample.newly_acked)
            .count(),
        3
    );
    assert!(!history.inputs.feedback.iter().any(|sample| sample.lost));
}

#[test]
fn reordered_missing_packet_can_recover_without_double_ack() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(2);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    history.process_feedback(batch(
        now + Duration::from_millis(20),
        epoch,
        vec![
            TwccStatus::Received { delta_250us: 1 },
            TwccStatus::NotReceived,
            TwccStatus::Received { delta_250us: 1 },
        ],
    ));
    history.clear_controller_inputs();
    history.process_feedback(FeedbackBatch {
        received_at: at(now + Duration::from_millis(40)),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Twcc {
            media_ssrc: 7,
            base_sequence: 2,
            reference_time: 0,
            feedback_count: 2,
            statuses: vec![TwccStatus::Received { delta_250us: 1 }].into(),
        },
    });
    assert_eq!(history.inputs.feedback.len(), 1);
    assert!(history.inputs.feedback[0].received);
    assert!(!history.inputs.feedback[0].newly_acked);
    assert!(!history.inputs.feedback[0].lost);
    assert!(history.reordering_window >= Duration::from_millis(20));
}

#[test]
fn missing_packet_becomes_loss_only_after_reordering_window() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(3);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    history.process_feedback(batch(
        now + Duration::from_millis(20),
        epoch,
        vec![
            TwccStatus::Received { delta_250us: 1 },
            TwccStatus::NotReceived,
            TwccStatus::Received { delta_250us: 1 },
        ],
    ));
    assert_eq!(
        history
            .inputs
            .feedback
            .iter()
            .filter(|sample| sample.reported_lost_for_the_first_time)
            .count(),
        0
    );
    history.clear_controller_inputs();
    history.expire(now + Duration::from_millis(49), 256);
    assert!(history.inputs.synthetic.is_empty());
    history.expire(now + Duration::from_millis(50), 256);
    assert!(history.inputs.synthetic.iter().any(|sample| sample.lost));
    assert!(history.inputs.synthetic.iter().all(|sample| {
        sample.loss_origin == Some(LossOrigin::Reordering)
            && !sample
                .controller_sample(now + Duration::from_millis(50), now)
                .lost
    }));
}

#[test]
fn expiry_retirement_preserves_an_earlier_confirmation_under_backlog() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(3);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=513 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    let mut statuses = vec![TwccStatus::NotReceived; 512];
    statuses.push(TwccStatus::Received { delta_250us: 1 });
    history.process_feedback(batch(now + Duration::from_millis(20), epoch, statuses));
    history.clear_controller_inputs();
    history.missing.make_contiguous().reverse();
    let expired_at = now + SENT_HISTORY_MAX_AGE;
    assert_eq!(history.expire(expired_at, MAX_EXPIRATIONS_PER_POLL), 256);
    assert_eq!(history.inputs.synthetic.len(), MAX_EXPIRATIONS_PER_POLL);
    assert!(history.inputs.synthetic.iter().all(|sample| {
        sample.lost
            && sample.loss_origin == Some(LossOrigin::Reordering)
            && !sample.controller_sample(expired_at, now).lost
    }));
    assert_eq!(history.bytes_in_flight, 0);
}

#[test]
fn stale_twcc_generation_is_rejected_without_advancing_ack_state() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(4);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=5_000_u16 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    let bytes_in_flight = history.inputs.bytes_in_flight;
    history.process_feedback(batch(
        now + Duration::from_millis(20),
        epoch,
        vec![TwccStatus::Received { delta_250us: 1 }],
    ));
    assert_eq!(history.counters.stale_feedback, 1);
    assert_eq!(history.inputs.bytes_in_flight, bytes_in_flight);
    assert!(history.inputs.feedback.is_empty());
    assert!(history.highest_twcc_acked.is_none());
}

fn rfc_context(at: Instant, epoch: PathEpoch, sequence: u16) -> TransmitCommitContext {
    TransmitCommitContext {
        at,
        kind: DatagramKind::Rtp,
        wire_len: 1_200,
        path_epoch: Some(epoch),
        rtp: Some(PreparedRtpIdentity {
            ssrc: 7,
            sequence,
            index: u64::from(sequence),
            twcc_sequence: None,
            service: RtpService::Original,
        }),
    }
}

#[test]
fn stale_rfc8888_generation_is_rejected_per_ssrc() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(5);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
    history.path_changed(epoch, true);
    for sequence in 1..=5_000_u16 {
        history.commit(rfc_context(now, epoch, sequence)).unwrap();
    }
    let bytes_in_flight = history.inputs.bytes_in_flight;
    history.process_feedback(FeedbackBatch {
        received_at: at(now + Duration::from_millis(20)),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Rfc8888 {
            reports: vec![crate::rtcp::Rfc8888Report {
                ssrc: 7,
                begin_sequence: 1,
                report_count: 1,
                statuses: vec![crate::rtcp::Rfc8888Status::Received {
                    ecn: 0,
                    arrival_offset: crate::rtcp::ArrivalOffset::Ticks(1),
                }]
                .into(),
            }]
            .into(),
            report_timestamp: 1,
        },
    });
    assert_eq!(history.counters.stale_feedback, 1);
    assert_eq!(history.inputs.bytes_in_flight, bytes_in_flight);
    assert!(history.inputs.feedback.is_empty());
    assert!(history.ssrcs[0].highest_acked_sequence.is_none());
}

#[test]
fn mixed_path_feedback_only_credits_the_committed_current_suffix_including_rollover() {
    for mode in [
        PacketFeedbackKind::TransportWide,
        PacketFeedbackKind::Rfc8888,
    ] {
        for rollover in [false, true] {
            let now = Instant::now();
            let old = PathEpoch::from_value(40);
            let current = PathEpoch::from_value(41);
            let mut history = SentHistory::new(mode);
            let first = if rollover { 65_534_u64 } else { 1 };
            history.path_changed(old, true);
            for offset in 0..4 {
                if offset == 2 {
                    history.path_changed(current, true);
                }
                let epoch = if offset < 2 { old } else { current };
                let index = first + offset;
                let sequence = u16::try_from(index % 65_536).unwrap();
                let mut packet = if mode == PacketFeedbackKind::TransportWide {
                    context(now, epoch, sequence)
                } else {
                    rfc_context(now, epoch, sequence)
                };
                packet.rtp.as_mut().unwrap().index = index;
                history.commit(packet).unwrap();
            }
            history.clear_controller_inputs();
            let report = |count, delay| FeedbackBatch {
                received_at: at(now + Duration::from_millis(delay)),
                path_epoch: current,
                sender_ssrc: 9,
                report: if mode == PacketFeedbackKind::TransportWide {
                    FeedbackReport::Twcc {
                        media_ssrc: 7,
                        base_sequence: u16::try_from(first).unwrap(),
                        reference_time: 0,
                        feedback_count: 1,
                        statuses: vec![TwccStatus::Received { delta_250us: 1 }; count].into(),
                    }
                } else {
                    FeedbackReport::Rfc8888 {
                        report_timestamp: 65_536,
                        reports: vec![crate::rtcp::Rfc8888Report {
                            ssrc: 7,
                            begin_sequence: u16::try_from(first).unwrap(),
                            report_count: u16::try_from(count).unwrap(),
                            statuses: vec![
                                Rfc8888Status::Received {
                                    ecn: 0,
                                    arrival_offset: ArrivalOffset::Ticks(1)
                                };
                                count
                            ]
                            .into(),
                        }]
                        .into(),
                    }
                },
            };
            history.process_feedback(report(2, 10));
            assert_eq!(history.bytes_in_flight, 2_400);
            assert!(history.inputs.feedback.is_empty());
            assert!(!history.inputs.fresh_network_feedback);
            history.process_feedback(report(4, 20));
            assert_eq!(history.bytes_in_flight, 0);
            assert_eq!(history.counters.received, 2);
            assert_eq!(history.inputs.feedback.len(), 2);
            assert!(
                history
                    .inputs
                    .feedback
                    .iter()
                    .all(|sample| sample.sent_id.0 >= 2 && sample.received && sample.newly_acked)
            );
            assert!(history.inputs.fresh_network_feedback);
            if mode == PacketFeedbackKind::TransportWide {
                assert_eq!(history.highest_twcc_acked, Some(first + 3));
                assert_eq!(
                    history.inputs.feedback.last().unwrap().receiver_arrival,
                    Some(ReceiverTime::Micros(1_000))
                );
            }
            history.clear_controller_inputs();
            history.process_feedback(report(4, 30));
            assert_eq!(history.counters.received, 2);
            assert!(history.inputs.feedback.is_empty());
            assert!(!history.inputs.fresh_network_feedback);
        }
    }
}

#[test]
fn future_twcc_feedback_cannot_advance_ack_edge() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(6);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    let before_bif = history.inputs.bytes_in_flight;
    history.process_feedback(FeedbackBatch {
        received_at: at(now + Duration::from_millis(20)),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Twcc {
            media_ssrc: 7,
            base_sequence: 3,
            reference_time: 0,
            feedback_count: 1,
            statuses: vec![
                TwccStatus::Received { delta_250us: 1 },
                TwccStatus::Received { delta_250us: 1 },
            ]
            .into(),
        },
    });
    assert_eq!(history.inputs.bytes_in_flight, before_bif);
    assert!(history.inputs.feedback.is_empty());
    assert!(history.highest_twcc_acked.is_none());
    assert!(history.counters.unknown_feedback >= 2);
}

#[test]
fn future_rfc8888_feedback_cannot_advance_ack_edge() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(7);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(rfc_context(now, epoch, sequence)).unwrap();
    }
    let before_bif = history.inputs.bytes_in_flight;
    history.process_feedback(FeedbackBatch {
        received_at: at(now + Duration::from_millis(20)),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Rfc8888 {
            reports: vec![crate::rtcp::Rfc8888Report {
                ssrc: 7,
                begin_sequence: 3,
                report_count: 2,
                statuses: vec![
                    crate::rtcp::Rfc8888Status::Received {
                        ecn: 0,
                        arrival_offset: crate::rtcp::ArrivalOffset::Ticks(1),
                    },
                    crate::rtcp::Rfc8888Status::Received {
                        ecn: 0,
                        arrival_offset: crate::rtcp::ArrivalOffset::Ticks(1),
                    },
                ]
                .into(),
            }]
            .into(),
            report_timestamp: 1,
        },
    });
    assert_eq!(history.inputs.bytes_in_flight, before_bif);
    assert!(history.inputs.feedback.is_empty());
    assert!(history.ssrcs[0].highest_acked_sequence.is_none());
    assert!(history.counters.unknown_feedback >= 2);
}

#[test]
fn path_change_discards_pending_feedback_and_rebases_ssrc_history() {
    let now = Instant::now();
    let old_epoch = PathEpoch::from_value(20);
    let new_epoch = PathEpoch::from_value(21);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
    history.path_changed(old_epoch, true);
    history.commit(rfc_context(now, old_epoch, 10)).unwrap();
    history.emit(
        SentPacketId(0),
        false,
        false,
        Some(LossOrigin::Reordering),
        None,
        None,
    );
    assert!(!history.inputs.synthetic.is_empty());
    history.path_changed(new_epoch, true);
    assert!(history.inputs.synthetic.is_empty());
    assert!(history.inputs.timing.is_none());
    assert!(history.ssrcs[0].sent_ids.is_empty());
    history.commit(rfc_context(now, new_epoch, 500)).unwrap();
    assert_eq!(history.ssrcs[0].first_sequence, 500);
}

#[test]
fn terminal_rfc8888_receipt_cannot_replace_new_feedback_hold() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(36);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
    history.path_changed(epoch, true);
    for sequence in 1..=2 {
        history.commit(rfc_context(now, epoch, sequence)).unwrap();
    }
    let report = |received_at, timestamp, statuses: Vec<Rfc8888Status>| FeedbackBatch {
        received_at: at(received_at),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Rfc8888 {
            reports: vec![crate::rtcp::Rfc8888Report {
                ssrc: 7,
                begin_sequence: 1,
                report_count: u16::try_from(statuses.len()).unwrap(),
                statuses: statuses.into(),
            }]
            .into(),
            report_timestamp: timestamp,
        },
    };
    let received = |ticks| Rfc8888Status::Received {
        ecn: 0,
        arrival_offset: ArrivalOffset::Ticks(ticks),
    };
    history.process_feedback(report(
        now + Duration::from_millis(10),
        1,
        vec![received(1)],
    ));
    history.clear_controller_inputs();
    history.process_feedback(report(
        now + Duration::from_millis(20),
        2,
        vec![received(0), received(100)],
    ));
    assert_eq!(history.inputs.feedback.len(), 1);
    assert_eq!(
        history.inputs.timing.unwrap().feedback_hold,
        Some(Duration::from_micros(97_656))
    );
}

#[test]
fn feedback_timing_uses_only_the_current_accepted_report() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(37);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
    history.path_changed(epoch, true);
    let mut older = rfc_context(now, epoch, 1);
    older.rtp.as_mut().unwrap().ssrc = 8;
    history.commit(older).unwrap();
    history
        .commit(rfc_context(now + Duration::from_millis(10), epoch, 1))
        .unwrap();
    for (ssrc, received_at) in [(7, 20), (8, 100)] {
        history.process_feedback(FeedbackBatch {
            received_at: at(now + Duration::from_millis(received_at)),
            path_epoch: epoch,
            sender_ssrc: 9,
            report: FeedbackReport::Rfc8888 {
                reports: vec![crate::rtcp::Rfc8888Report {
                    ssrc,
                    begin_sequence: 1,
                    report_count: 1,
                    statuses: vec![Rfc8888Status::Received {
                        ecn: 0,
                        arrival_offset: ArrivalOffset::Ticks(0),
                    }]
                    .into(),
                }]
                .into(),
                report_timestamp: u32::try_from(received_at).unwrap(),
            },
        });
    }
    assert_eq!(
        history.inputs.timing.unwrap().newest_send_age,
        Some(Duration::from_millis(100))
    );
}

#[test]
fn all_missing_rfc8888_report_does_not_advance_either_ssrc() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(13);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
    history.path_changed(epoch, true);
    for ssrc in [7, 8] {
        for sequence in 1..=2 {
            let mut packet = rfc_context(now, epoch, sequence);
            packet.rtp.as_mut().unwrap().ssrc = ssrc;
            history.commit(packet).unwrap();
        }
    }
    history.process_feedback(FeedbackBatch {
        received_at: at(now + Duration::from_millis(10)),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Rfc8888 {
            reports: vec![crate::rtcp::Rfc8888Report {
                ssrc: 7,
                begin_sequence: 1,
                report_count: 2,
                statuses: vec![crate::rtcp::Rfc8888Status::NotReceived; 2].into(),
            }]
            .into(),
            report_timestamp: 1,
        },
    });
    assert_eq!(history.bytes_in_flight, 4_800);
    assert_eq!(history.inputs.feedback.len(), 2);
    assert!(
        history
            .inputs
            .feedback
            .iter()
            .all(|sample| sample.reported_lost_for_the_first_time
                && !sample.received
                && !sample.newly_acked
                && !sample.lost)
    );
    assert_eq!(history.ssrcs[0].highest_acked_sequence, None);
    assert_eq!(history.ssrcs[1].highest_acked_sequence, None);
    assert!(history.inputs.fresh_network_feedback);
}

#[test]
fn rfc8888_freshness_follows_report_time_not_status_count() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(34);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
    history.path_changed(epoch, true);
    history.commit(rfc_context(now, epoch, 1)).unwrap();
    for (timestamp, expected) in [(1, true), (1, false), (2, true)] {
        history.clear_controller_inputs();
        history.process_feedback(FeedbackBatch {
            received_at: at(now + Duration::from_millis(timestamp as u64 * 10)),
            path_epoch: epoch,
            sender_ssrc: 9,
            report: FeedbackReport::Rfc8888 {
                reports: vec![crate::rtcp::Rfc8888Report {
                    ssrc: 7,
                    begin_sequence: 1,
                    report_count: 1,
                    statuses: vec![Rfc8888Status::NotReceived].into(),
                }]
                .into(),
                report_timestamp: timestamp,
            },
        });
        assert_eq!(history.inputs.fresh_network_feedback, expected);
    }
}

#[test]
fn all_missing_twcc_report_does_not_advance_credit() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(12);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    history.process_feedback(batch(
        now + Duration::from_millis(10),
        epoch,
        vec![TwccStatus::NotReceived; 3],
    ));
    assert_eq!(history.bytes_in_flight, 3_600);
    assert_eq!(history.highest_twcc_acked, None);
    assert_eq!(history.inputs.feedback.len(), 3);
    assert!(
        history
            .inputs
            .feedback
            .iter()
            .all(|sample| !sample.reported_lost_for_the_first_time
                && !sample.received
                && !sample.newly_acked
                && !sample.lost)
    );
    assert!(history.inputs.fresh_network_feedback);
    history.clear_controller_inputs();
    history.process_feedback(batch(
        now + Duration::from_millis(20),
        epoch,
        vec![TwccStatus::NotReceived; 3],
    ));
    assert!(
        !history.inputs.fresh_network_feedback,
        "replayed report is not fresh"
    );
    history.process_feedback(FeedbackBatch {
        received_at: at(now + Duration::from_millis(21)),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Twcc {
            media_ssrc: 7,
            base_sequence: 1,
            reference_time: 0,
            feedback_count: 2,
            statuses: vec![TwccStatus::NotReceived; 3].into(),
        },
    });
    assert!(
        history.inputs.fresh_network_feedback,
        "new covering report is fresh"
    );
}

#[test]
fn all_missing_then_received_advances_credit_once_without_double_delivery() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(14);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    history.process_feedback(batch(
        now + Duration::from_millis(10),
        epoch,
        vec![TwccStatus::NotReceived; 3],
    ));
    assert_eq!(history.bytes_in_flight, 3_600);
    assert_eq!(history.missing.len(), 3);
    history.clear_controller_inputs();
    history.process_feedback(batch(
        now + Duration::from_millis(20),
        epoch,
        vec![
            TwccStatus::NotReceived,
            TwccStatus::Received { delta_250us: 1 },
            TwccStatus::NotReceived,
        ],
    ));
    assert_eq!(history.highest_twcc_acked, Some(2));
    assert_eq!(history.bytes_in_flight, 1_200);
    assert_eq!(
        history
            .inputs
            .feedback
            .iter()
            .filter(|item| item.newly_acked)
            .count(),
        2
    );
    assert_eq!(
        history
            .inputs
            .feedback
            .iter()
            .filter(|item| item.received)
            .count(),
        1
    );
    history.clear_controller_inputs();
    history.process_feedback(batch(
        now + Duration::from_millis(25),
        epoch,
        vec![
            TwccStatus::NotReceived,
            TwccStatus::Received { delta_250us: 1 },
        ],
    ));
    assert_eq!(history.bytes_in_flight, 1_200);
    assert!(history.inputs.feedback.is_empty());
}

#[test]
fn expiry_of_unreported_prefix_is_not_network_loss_alongside_fresh_receipts() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(16);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    let mut first = batch(
        now + Duration::from_millis(10),
        epoch,
        vec![TwccStatus::Received { delta_250us: 1 }],
    );
    if let FeedbackReport::Twcc { base_sequence, .. } = &mut first.report {
        *base_sequence = 3;
    }
    history.process_feedback(first);
    assert_eq!(history.bytes_in_flight, 0);
    history.clear_controller_inputs();
    let expired = now + SENT_HISTORY_MAX_AGE;
    history.expire(expired, MAX_EXPIRATIONS_PER_POLL);
    assert_eq!(history.inputs.synthetic.len(), 2);
    history.commit(context(expired, epoch, 4)).unwrap();
    let arrival = expired + Duration::from_millis(10);
    let mut next = batch(
        arrival,
        epoch,
        vec![TwccStatus::Received { delta_250us: 2 }],
    );
    if let FeedbackReport::Twcc {
        base_sequence,
        feedback_count,
        ..
    } = &mut next.report
    {
        *base_sequence = 4;
        *feedback_count = 2;
    }
    history.process_feedback(next);
    assert!(history.inputs.fresh_network_feedback);
    for sample in history
        .inputs
        .feedback
        .iter()
        .chain(&history.inputs.synthetic)
    {
        let sample = sample.controller_sample(arrival, now);
        assert!(!sample.lost && !sample.recovered);
    }
}

#[test]
fn synthetic_loss_never_evicts_network_evidence() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(16);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    history.commit(context(now, epoch, 1)).unwrap();
    history.emit(SentPacketId(0), true, true, None, None, None);
    for _ in 0..MAX_EXPIRATIONS_PER_POLL + 1 {
        history.emit(
            SentPacketId(0),
            false,
            false,
            Some(LossOrigin::Reordering),
            None,
            None,
        );
    }
    assert_eq!(history.inputs.feedback.len(), 1);
    assert!(history.inputs.feedback[0].received);
    assert_eq!(history.inputs.synthetic.len(), MAX_EXPIRATIONS_PER_POLL);
    assert!(!history.inputs.fresh_network_feedback);
}

#[test]
fn repeated_unavailable_path_preserves_pending_reset_evidence() {
    let epoch = PathEpoch::from_value(15);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, false);
    history.path_changed(epoch, false);
    assert_eq!(
        history.inputs.path_change,
        Some(PathChange {
            epoch,
            available: false
        })
    );
}

#[test]
fn unknown_twcc_gap_does_not_poison_report_cursors_or_accounting() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(11);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    history.commit(context(now, epoch, 1)).unwrap();
    history.commit(context(now, epoch, 3)).unwrap();
    let first = batch(
        now + Duration::from_millis(10),
        epoch,
        vec![TwccStatus::Received { delta_250us: 1 }],
    );
    history.process_feedback(first);
    history.clear_controller_inputs();
    let invalid = FeedbackBatch {
        received_at: at(now + Duration::from_millis(20)),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Twcc {
            media_ssrc: 7,
            base_sequence: 2,
            reference_time: 400,
            feedback_count: 200,
            statuses: vec![
                TwccStatus::NotReceived,
                TwccStatus::Received { delta_250us: 1 },
            ]
            .into(),
        },
    };
    let bytes = history.bytes_in_flight;
    let reference = history.twcc_reference;
    let count = history.twcc_feedback_count;
    history.process_feedback(invalid);
    assert_eq!(history.bytes_in_flight, bytes);
    assert_eq!(history.highest_twcc_acked, Some(1));
    assert_eq!(history.twcc_reference, reference);
    assert_eq!(history.twcc_feedback_count, count);
    assert!(!history.inputs.fresh_network_feedback);
    assert!(history.inputs.feedback.is_empty());
    assert!(history.inputs.timing.is_none());
}

#[test]
fn invalid_nonadvancing_report_does_not_mutate_feedback_baseline() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(11);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    history.commit(context(now, epoch, 1)).unwrap();
    history.commit(context(now, epoch, 3)).unwrap();
    history.process_feedback(batch(
        now + Duration::from_millis(10),
        epoch,
        vec![TwccStatus::Received { delta_250us: 1 }],
    ));
    history.clear_controller_inputs();
    let reference = history.twcc_reference;
    let count = history.twcc_feedback_count;
    history.process_feedback(FeedbackBatch {
        received_at: at(now + Duration::from_millis(20)),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Twcc {
            media_ssrc: 7,
            base_sequence: 2,
            reference_time: 400,
            feedback_count: 200,
            statuses: vec![TwccStatus::NotReceived].into(),
        },
    });
    assert_eq!(history.twcc_reference, reference);
    assert_eq!(history.twcc_feedback_count, count);
    assert!(history.inputs.timing.is_none());
    assert!(history.inputs.feedback.is_empty());
}

#[test]
fn all_missing_waits_for_higher_receipt_before_reorder_loss() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(31);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    history.process_feedback(batch(
        now + Duration::from_millis(10),
        epoch,
        vec![TwccStatus::NotReceived, TwccStatus::NotReceived],
    ));
    assert_eq!(history.confirm_losses(now + Duration::from_secs(1), 256), 2);
    assert_eq!(history.counters.not_received, 0);
    history.process_feedback(batch(
        now + Duration::from_millis(100),
        epoch,
        vec![
            TwccStatus::NotReceived,
            TwccStatus::NotReceived,
            TwccStatus::Received { delta_250us: 1 },
        ],
    ));
    assert_eq!(
        history.confirm_losses(now + Duration::from_millis(129), 256),
        2
    );
    assert_eq!(history.counters.not_received, 0);
    history.confirm_losses(now + Duration::from_millis(130), 256);
    assert_eq!(history.counters.not_received, 2);
}

#[test]
fn late_twcc_receipt_preserves_ledger_loss_without_native_loss_or_recovery() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(32);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    history.process_feedback(batch(
        now + Duration::from_millis(10),
        epoch,
        vec![
            TwccStatus::Received { delta_250us: 1 },
            TwccStatus::NotReceived,
            TwccStatus::Received { delta_250us: 1 },
        ],
    ));
    history.clear_controller_inputs();
    history.process_feedback(FeedbackBatch {
        received_at: at(now + Duration::from_millis(40)),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Twcc {
            media_ssrc: 7,
            base_sequence: 2,
            reference_time: 0,
            feedback_count: 2,
            statuses: vec![TwccStatus::Received { delta_250us: 1 }].into(),
        },
    });
    assert_eq!(
        history.entry(SentPacketId(1)).unwrap().acknowledgment,
        Acknowledgment::Lost
    );
    assert_eq!(history.counters.not_received, 1);
    assert_eq!(history.counters.received, 2);
    let recovery = history
        .inputs
        .feedback
        .iter()
        .find(|sample| sample.sent_id == SentPacketId(1) && sample.received)
        .unwrap();
    assert!(recovery.received);
    assert!(!recovery.newly_acked);
    let native = recovery.controller_sample(now + Duration::from_secs(1), now);
    assert!(!native.recovered);
    assert!(!native.lost);
    assert_eq!(native.received_at, Duration::from_millis(40));
    assert!(history.inputs.fresh_network_feedback);
    assert_eq!(history.reordering_window, INITIAL_REORDERING_WINDOW);
    history.clear_controller_inputs();
    history.process_feedback(FeedbackBatch {
        received_at: at(now + Duration::from_millis(80)),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Twcc {
            media_ssrc: 7,
            base_sequence: 2,
            reference_time: 0,
            feedback_count: 3,
            statuses: vec![TwccStatus::Received { delta_250us: 1 }].into(),
        },
    });
    assert_eq!(history.reordering_window, INITIAL_REORDERING_WINDOW);
    assert_eq!(history.counters.not_received, 1);
    assert_eq!(history.counters.received, 2);
    assert!(history.inputs.feedback.is_empty());
    assert!(!history.inputs.fresh_network_feedback);
}

#[test]
fn reorder_learning_cannot_reopen_another_packets_elapsed_deadline() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(35);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    history.process_feedback(batch(
        now + Duration::from_millis(10),
        epoch,
        vec![TwccStatus::NotReceived; 3],
    ));
    history.process_feedback(batch(
        now + Duration::from_millis(50),
        epoch,
        vec![
            TwccStatus::NotReceived,
            TwccStatus::NotReceived,
            TwccStatus::Received { delta_250us: 1 },
        ],
    ));
    history.clear_controller_inputs();
    history.process_feedback(batch(
        now + Duration::from_millis(100),
        epoch,
        vec![
            TwccStatus::Received { delta_250us: 1 },
            TwccStatus::Received { delta_250us: 1 },
        ],
    ));
    assert_eq!(history.counters.received, 1);
    assert_eq!(history.counters.not_received, 2);
    assert!(
        history
            .inputs
            .feedback
            .iter()
            .all(|sample| !sample.received)
    );
    assert!(!history.inputs.fresh_network_feedback);
}

#[test]
fn reorder_learning_preserves_elapsed_deadlines_when_loss_work_is_deferred() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(36);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    history.process_feedback(batch(
        now + Duration::from_millis(10),
        epoch,
        vec![TwccStatus::NotReceived; 3],
    ));
    history.process_feedback(batch(
        now + Duration::from_millis(50),
        epoch,
        vec![
            TwccStatus::NotReceived,
            TwccStatus::NotReceived,
            TwccStatus::Received { delta_250us: 1 },
        ],
    ));
    history.clear_controller_inputs();
    assert_eq!(
        history.confirm_losses(now + Duration::from_millis(80), 1),
        1
    );
    history.loss_work = MAX_EXPIRATIONS_PER_POLL;
    let receipt_at = now + Duration::from_millis(100);
    history.process_feedback(batch(
        receipt_at,
        epoch,
        vec![TwccStatus::Received { delta_250us: 1 }],
    ));
    assert_eq!(history.reordering_window, Duration::from_millis(90));
    assert_eq!(
        history.next_deadline(),
        Some(now + Duration::from_millis(80))
    );
    history.process_feedback(batch(
        receipt_at,
        epoch,
        vec![
            TwccStatus::NotReceived,
            TwccStatus::Received { delta_250us: 1 },
        ],
    ));
    assert_eq!(history.counters.received, 1);
    assert!(!history.inputs.fresh_network_feedback);
    history.clear_controller_inputs();
    history.confirm_losses(receipt_at, MAX_EXPIRATIONS_PER_POLL);
    assert_eq!(history.counters.not_received, 2);
}

#[test]
fn feedback_confirmation_shares_the_expiry_budget_and_defers_without_losing_evidence() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(34);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=512 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    let expired_at = now + SENT_HISTORY_MAX_AGE;
    assert_eq!(history.expire(expired_at, MAX_EXPIRATIONS_PER_POLL), 256);
    history.process_feedback(batch(
        expired_at,
        epoch,
        vec![TwccStatus::Received { delta_250us: 1 }; 512],
    ));
    assert_eq!(history.loss_work, MAX_EXPIRATIONS_PER_POLL);
    assert_eq!(history.inputs.synthetic.len(), MAX_EXPIRATIONS_PER_POLL);
    assert!(history.inputs.synthetic.iter().all(|sample| {
        sample.lost
            && sample.loss_origin == Some(LossOrigin::HistoryExpiry)
            && !sample.controller_sample(expired_at, now).lost
    }));
    assert!(history.inputs.feedback.is_empty());
    assert!(!history.inputs.fresh_network_feedback);
    assert!(!history.inputs.exhausted);
    assert_eq!(history.bytes_in_flight, 256 * 1_200);
    assert!(
        history
            .next_deadline()
            .is_some_and(|deadline| deadline <= expired_at)
    );
    history.clear_controller_inputs();
    assert_eq!(history.expire(expired_at, MAX_EXPIRATIONS_PER_POLL), 256);
    assert_eq!(history.inputs.synthetic.len(), MAX_EXPIRATIONS_PER_POLL);
    assert_eq!(history.bytes_in_flight, 0);
}

#[test]
fn rfc8888_sparse_block_expansion_signals_terminal_resource_exhaustion() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(34);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
    history.path_changed(epoch, true);
    for ssrc in 1..=3 {
        for sequence in 1..=4_096 {
            let mut commit = context(now, epoch, sequence);
            let rtp = commit.rtp.as_mut().unwrap();
            rtp.ssrc = ssrc;
            rtp.twcc_sequence = None;
            history.commit(commit).unwrap();
        }
    }
    history.process_feedback(FeedbackBatch {
        received_at: at(now + Duration::from_millis(10)),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Rfc8888 {
            report_timestamp: 65_536,
            reports: (1..=3)
                .map(|ssrc| crate::rtcp::Rfc8888Report {
                    ssrc,
                    begin_sequence: 4_096,
                    report_count: 1,
                    statuses: vec![Rfc8888Status::Received {
                        ecn: 0,
                        arrival_offset: ArrivalOffset::Ticks(1),
                    }]
                    .into(),
                })
                .collect(),
        },
    });
    assert!(history.inputs.exhausted);
    assert_eq!(history.inputs.feedback.len(), MAX_FEEDBACK_STATUSES);
    assert!(
        history
            .inputs
            .feedback
            .iter()
            .all(|sample| sample.sent_id.0 < MAX_FEEDBACK_STATUSES as u64)
    );
}

#[test]
fn terminal_late_receipt_cannot_refresh_replayed_report_covering_live_missing() {
    for mode in [
        PacketFeedbackKind::TransportWide,
        PacketFeedbackKind::Rfc8888,
    ] {
        let now = Instant::now();
        let epoch = PathEpoch::from_value(32);
        let mut history = SentHistory::new(mode);
        history.path_changed(epoch, true);
        for sequence in 1..=3 {
            let mut commit = context(now, epoch, sequence);
            if mode == PacketFeedbackKind::Rfc8888 {
                commit.rtp.as_mut().unwrap().twcc_sequence = None;
            }
            history.commit(commit).unwrap();
        }
        let report = |received_at, received_index| match mode {
            PacketFeedbackKind::TransportWide => batch(
                received_at,
                epoch,
                (0..3)
                    .map(|index| {
                        if index == received_index {
                            TwccStatus::Received { delta_250us: 1 }
                        } else {
                            TwccStatus::NotReceived
                        }
                    })
                    .collect(),
            ),
            PacketFeedbackKind::Rfc8888 => FeedbackBatch {
                received_at: at(received_at),
                path_epoch: epoch,
                sender_ssrc: 9,
                report: FeedbackReport::Rfc8888 {
                    report_timestamp: 65_536,
                    reports: vec![crate::rtcp::Rfc8888Report {
                        ssrc: 7,
                        begin_sequence: 1,
                        report_count: 3,
                        statuses: (0..3)
                            .map(|index| {
                                if index == received_index {
                                    Rfc8888Status::Received {
                                        ecn: 0,
                                        arrival_offset: ArrivalOffset::Ticks(1),
                                    }
                                } else {
                                    Rfc8888Status::NotReceived
                                }
                            })
                            .collect(),
                    }]
                    .into(),
                },
            },
        };
        history.process_feedback(report(now, 1));
        history.clear_controller_inputs();
        history.process_feedback(report(now + INITIAL_REORDERING_WINDOW, 0));
        assert!(!history.inputs.fresh_network_feedback, "{mode:?}");
        assert!(!history.inputs.feedback.iter().any(|sample| sample.received));
        assert!(matches!(
            history.entry(SentPacketId(0)).unwrap().acknowledgment,
            Acknowledgment::Lost
        ));
    }
}

#[test]
fn new_rfc8888_report_can_recover_native_loss_without_erasing_terminal_loss() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(32);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
    history.path_changed(epoch, true);
    history.commit(rfc_context(now, epoch, 1)).unwrap();
    history.set_missing(SentPacketId(0), now);
    history.anchor_missing(None, Some((7, 2)), now);
    history.clear_controller_inputs();
    history.process_feedback(FeedbackBatch {
        received_at: at(now + INITIAL_REORDERING_WINDOW),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Rfc8888 {
            report_timestamp: 65_536,
            reports: vec![crate::rtcp::Rfc8888Report {
                ssrc: 7,
                begin_sequence: 1,
                report_count: 1,
                statuses: vec![Rfc8888Status::Received {
                    ecn: 0,
                    arrival_offset: ArrivalOffset::Ticks(1),
                }]
                .into(),
            }]
            .into(),
        },
    });
    assert!(history.inputs.fresh_network_feedback);
    assert_eq!(
        history.inputs.timing.unwrap().feedback_hold,
        Some(Duration::from_micros(976))
    );
    assert_eq!(
        history.entry(SentPacketId(0)).unwrap().acknowledgment,
        Acknowledgment::Lost
    );
    assert_eq!(history.counters.not_received, 1);
    assert_eq!(history.counters.received, 0);
    let recovery = history
        .inputs
        .feedback
        .iter()
        .find(|sample| sample.recovered)
        .unwrap();
    assert!(recovery.received);
    // This report advances the physical ACK edge for the first time, independently
    // of the frozen terminal-loss classification.
    assert!(recovery.newly_acked);
}

#[test]
fn reordering_window_is_bounded_and_stays_monotonic_after_confirmed_loss() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(22);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
    history.path_changed(epoch, true);
    history.commit(rfc_context(now, epoch, 1)).unwrap();
    history.set_missing(SentPacketId(0), now);
    history.recover(
        Some(SentPacketId(0)),
        epoch,
        None,
        None,
        now + Duration::from_secs(2),
    );
    history.apply_reordering_window(now + Duration::from_secs(2));
    assert_eq!(history.reordering_window, MAX_REORDERING_WINDOW);

    history.commit(rfc_context(now, epoch, 2)).unwrap();
    history.set_missing(SentPacketId(1), now);
    assert_eq!(history.confirm_losses(now + MAX_REORDERING_WINDOW, 2), 2);
    assert_eq!(
        history.entry(SentPacketId(1)).unwrap().acknowledgment,
        Acknowledgment::Missing {
            since: now,
            higher_received_at: None
        }
    );
    history.anchor_missing(None, Some((7, 3)), now);
    history.confirm_losses(now + MAX_REORDERING_WINDOW, 2);
    assert_eq!(history.reordering_window, MAX_REORDERING_WINDOW);
}

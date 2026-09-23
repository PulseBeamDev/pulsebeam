use super::*;
use crate::{GlobalMediaTime, TimePoint, transport::PreparedRtpIdentity};

fn context(at: Instant, epoch: PathEpoch, sequence: u16) -> TransmitCommitContext {
    TransmitCommitContext {
        at,
        kind: DatagramKind::Rtp,
        wire_len: 1_200,
        path_epoch: Some(epoch),
        rtp: Some(PreparedRtpIdentity {
            ssrc: 7,
            sequence,
            twcc_sequence: Some(sequence),
            service: RtpService::Original,
        }),
    }
}

fn at(monotonic: Instant) -> TimePoint {
    TimePoint {
        monotonic,
        global: GlobalMediaTime::from_micros(1),
    }
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
    history.clear_controller_inputs();
    history.expire(now + Duration::from_millis(49), 256);
    assert!(history.inputs.synthetic.is_empty());
    history.expire(now + Duration::from_millis(50), 256);
    assert!(history.inputs.synthetic.iter().any(|sample| sample.lost));
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
    history.emit(SentPacketId(0), false, false, true, None, None);
    assert!(!history.inputs.synthetic.is_empty());
    history.path_changed(new_epoch, true);
    assert!(history.inputs.synthetic.is_empty());
    assert!(history.inputs.timing.is_none());
    assert!(history.ssrcs[0].sent_ids.is_empty());
    history.commit(rfc_context(now, new_epoch, 500)).unwrap();
    assert_eq!(history.ssrcs[0].first_sequence, 500);
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
    assert!(history.inputs.feedback.is_empty());
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
    assert!(history.inputs.feedback.is_empty());
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
fn synthetic_loss_never_evicts_network_evidence() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(16);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    history.commit(context(now, epoch, 1)).unwrap();
    history.emit(SentPacketId(0), true, true, false, None, None);
    for _ in 0..MAX_EXPIRATIONS_PER_POLL + 1 {
        history.emit(SentPacketId(0), false, false, true, None, None);
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
fn late_receipt_after_logical_deadline_cannot_erase_loss() {
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
    assert!(!history.inputs.feedback.iter().any(|sample| sample.received));
    assert_eq!(history.reordering_window, INITIAL_REORDERING_WINDOW);
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

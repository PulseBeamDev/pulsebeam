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
    assert!(!history.inputs.feedback.iter().any(|sample| sample.lost));
    history.expire(now + Duration::from_millis(50), 256);
    assert!(history.inputs.feedback.iter().any(|sample| sample.lost));
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
    assert!(!history.inputs.feedback.is_empty());
    history.path_changed(new_epoch, true);
    assert!(history.inputs.feedback.is_empty());
    assert!(history.inputs.timing.is_none());
    assert!(history.ssrcs[0].sent_ids.is_empty());
    history.commit(rfc_context(now, new_epoch, 500)).unwrap();
    assert_eq!(history.ssrcs[0].first_sequence, 500);
}

#[test]
fn reordering_window_is_bounded_and_decays_after_confirmed_loss() {
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
    history.confirm_losses(now + MAX_REORDERING_WINDOW, 2);
    assert!(history.reordering_window < MAX_REORDERING_WINDOW);
    assert!(history.reordering_window >= INITIAL_REORDERING_WINDOW);
}

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

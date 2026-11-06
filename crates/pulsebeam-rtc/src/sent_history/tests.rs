mod timing;

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
fn mixed_path_feedback_only_credits_the_committed_current_suffix_including_rollover() {
    for mode in [
        PacketFeedbackKind::TransportWide,
        PacketFeedbackKind::Rfc8888,
    ] {
        for rollover in [false, true] {
            let now = Instant::now();
            let old = PathEpoch::from_value(40);
            let current = PathEpoch::from_value(41);
            let mut history = SentHistory::new(mode, now);
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
            assert_eq!(history.controller_inputs().bytes_in_flight, 2_400);
            assert!(history.controller_inputs().feedback.is_empty());
            assert!(!history.controller_inputs().fresh_network_feedback);
            history.process_feedback(report(4, 20));
            assert_eq!(history.controller_inputs().bytes_in_flight, 0);
            assert_eq!(history.counters().received, 2);
            assert_eq!(history.controller_inputs().feedback.len(), 2);
            assert!(
                history
                    .controller_inputs()
                    .feedback
                    .iter()
                    .all(|sample| sample.sent_id.0 >= 2 && sample.received && sample.newly_acked)
            );
            assert!(history.controller_inputs().fresh_network_feedback);
            if mode == PacketFeedbackKind::TransportWide {
                assert_eq!(
                    history
                        .controller_inputs()
                        .feedback
                        .iter()
                        .last()
                        .unwrap()
                        .receiver_arrival,
                    Some(ReceiverTime::Micros(1_000))
                );
            }
            history.clear_controller_inputs();
            history.process_feedback(report(4, 30));
            assert_eq!(history.counters().received, 2);
            assert!(history.controller_inputs().feedback.is_empty());
            assert!(!history.controller_inputs().fresh_network_feedback);
        }
    }
}

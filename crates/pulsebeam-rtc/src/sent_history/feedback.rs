use super::*;
use pulsebeam_feedback::{IndependentReport, SendFeedback, SendReport, SendStatus};

impl SentHistory {
    pub(crate) fn process_feedback(&mut self, batch: FeedbackBatch) {
        if self.sender.observations().exhausted {
            return;
        }
        let epoch = Epoch(batch.path_epoch.value());
        let limit = self.sender.config().max_feedback_statuses;
        if let FeedbackReport::Rfc8888 { reports, .. } = &batch.report
            && reports.len() > limit
        {
            self.sender.reject_unmapped_feedback(epoch, reports.len());
            return;
        }
        let count = match &batch.report {
            FeedbackReport::Twcc { statuses, .. } => statuses.len(),
            FeedbackReport::Rfc8888 { reports, .. } => {
                reports.iter().fold(0usize, |total, report| {
                    total.saturating_add(report.statuses.len())
                })
            }
        };
        if self.sender.active_epoch() != Some(epoch) || count > limit {
            self.sender.reject_unmapped_feedback(epoch, count);
            return;
        }
        let received_at = batch.received_at.saturating_duration_since(self.origin);
        match batch.report {
            FeedbackReport::Twcc {
                base_sequence,
                reference_time,
                feedback_count,
                statuses,
                ..
            } => {
                let base = self.sender.newest_feedback_packet().map_or_else(
                    || Some(u64::from(base_sequence)),
                    |newest| unwrap_near(u64::from(base_sequence), newest.0, 1 << 16),
                );
                let reference = unambiguous_generation(
                    i64::from(reference_time),
                    self.sender
                        .receiver_reference_micros()
                        .map(|micros| micros / 64_000),
                    1 << 24,
                )
                .and_then(|reference| reference.checked_mul(64_000));
                let generation = unambiguous_generation(
                    i64::from(feedback_count),
                    self.sender.report_generation(),
                    1 << 8,
                );
                let (Some(base), Some(reference_micros), Some(generation)) =
                    (base, reference, generation)
                else {
                    self.sender.reject_unmapped_feedback(epoch, count);
                    return;
                };
                let mut arrival = reference_micros;
                let normalized: Vec<_> = statuses
                    .iter()
                    .map(|status| match status {
                        TwccStatus::NotReceived => SendStatus {
                            received: false,
                            arrival: None,
                            ecn: None,
                            hold: None,
                        },
                        TwccStatus::Received { delta_250us } => {
                            arrival =
                                arrival.saturating_add(i64::from(*delta_250us).saturating_mul(250));
                            SendStatus {
                                received: true,
                                arrival: Some(ReceiverTime::Micros(arrival)),
                                ecn: None,
                                hold: None,
                            }
                        }
                    })
                    .collect();
                self.sender.process_feedback(SendFeedback {
                    received_at,
                    path_epoch: epoch,
                    report: SendReport::Continuous {
                        base_sequence: PacketId(base),
                        reference_micros,
                        generation,
                        statuses: &normalized,
                    },
                });
            }
            FeedbackReport::Rfc8888 {
                reports,
                report_timestamp,
            } => {
                let mut normalized = Vec::with_capacity(reports.len());
                for report in &reports {
                    let space = SpaceId(u64::from(report.ssrc));
                    let anchor = self.sender.space_anchor(space);
                    let base = anchor.map_or_else(
                        || Some(u64::from(report.begin_sequence)),
                        |(newest, _)| {
                            unwrap_near(u64::from(report.begin_sequence), newest.0, 1 << 16)
                        },
                    );
                    let generation = unambiguous_generation(
                        i64::from(report_timestamp),
                        anchor.and_then(|(_, generation)| generation),
                        1 << 32,
                    );
                    let (Some(base), Some(generation)) = (base, generation) else {
                        self.sender
                            .reject_unmapped_feedback(epoch, report.statuses.len());
                        continue;
                    };
                    let report_micros = generation.saturating_mul(1_000_000) / 65_536;
                    let statuses: Vec<_> = report
                        .statuses
                        .iter()
                        .map(|status| match status {
                            Rfc8888Status::NotReceived => SendStatus {
                                received: false,
                                arrival: None,
                                ecn: None,
                                hold: None,
                            },
                            Rfc8888Status::Received {
                                ecn,
                                arrival_offset,
                            } => {
                                let (arrival, hold) = match arrival_offset {
                                    ArrivalOffset::Ticks(ticks) => {
                                        let micros =
                                            u64::from(*ticks).saturating_mul(1_000_000) / 1_024;
                                        (
                                            ReceiverTime::Micros(report_micros.saturating_sub(
                                                i64::try_from(micros).unwrap_or(i64::MAX),
                                            )),
                                            Some(Duration::from_micros(micros)),
                                        )
                                    }
                                    ArrivalOffset::OverRange => (ReceiverTime::OverRange, None),
                                    ArrivalOffset::Unavailable => (ReceiverTime::Unavailable, None),
                                };
                                SendStatus {
                                    received: true,
                                    arrival: Some(arrival),
                                    ecn: Some(ecn_mark(*ecn)),
                                    hold,
                                }
                            }
                        })
                        .collect();
                    normalized.push((space, PacketId(base), generation, statuses));
                }
                let reports: Vec<_> = normalized
                    .iter()
                    .map(|(space, base, generation, statuses)| IndependentReport {
                        space: *space,
                        base: *base,
                        generation: *generation,
                        statuses,
                    })
                    .collect();
                self.sender.process_feedback(SendFeedback {
                    received_at,
                    path_epoch: epoch,
                    report: SendReport::Independent { reports: &reports },
                });
            }
        }
    }
}

fn unambiguous_generation(value: i64, previous: Option<i64>, modulus: i64) -> Option<i64> {
    let value = unwrap_near_signed(value, previous, modulus);
    if previous.is_some_and(|previous| value.abs_diff(previous) == modulus.unsigned_abs() / 2) {
        None
    } else {
        Some(value)
    }
}

#![allow(
    clippy::cast_possible_truncation,
    reason = "bounded report lengths and wrapping RTCP wire counters retain their low bits"
)]

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use str0m::rtp_::rtcp::{
    Rtcp, Twcc,
    twcc::{Delta, PacketChunk},
};

use crate::{EcnCodepoint, PacketFeedbackKind};

const HISTORY: usize = 512;
const REPORT_STATUSES: u64 = 256;
const INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug)]
struct Receipt {
    ssrc: u32,
    sequence: u64,
    arrival: Instant,
    ecn: Option<EcnCodepoint>,
    pending: bool,
}

#[derive(Debug)]
pub(crate) struct ReceiveFeedback {
    mode: Option<PacketFeedbackKind>,
    origin: Instant,
    receipts: VecDeque<Receipt>,
    twcc_highest: Option<u64>,
    twcc_next: Option<u64>,
    feedback_count: u8,
    deadline: Option<Instant>,
}

impl ReceiveFeedback {
    pub(crate) fn new(mode: Option<PacketFeedbackKind>, origin: Instant) -> Self {
        Self {
            mode,
            origin,
            receipts: VecDeque::new(),
            twcc_highest: None,
            twcc_next: None,
            feedback_count: 0,
            deadline: None,
        }
    }

    pub(crate) fn observe(
        &mut self,
        now: Instant,
        ssrc: u32,
        sequence: u16,
        twcc: Option<u16>,
        ecn: Option<EcnCodepoint>,
    ) {
        let (ssrc, sequence) = match self.mode {
            Some(PacketFeedbackKind::TransportWide) => {
                let Some(sequence) = twcc else {
                    return;
                };
                (0, extend(self.twcc_highest, sequence))
            }
            Some(PacketFeedbackKind::Rfc8888) => {
                let highest = self
                    .receipts
                    .iter()
                    .filter(|r| r.ssrc == ssrc)
                    .map(|r| r.sequence)
                    .max();
                (ssrc, extend(highest, sequence))
            }
            None => return,
        };
        let mut index = if self.mode == Some(PacketFeedbackKind::TransportWide) {
            let highest = self
                .twcc_highest
                .map_or(sequence, |highest| highest.max(sequence));
            let floor = highest.saturating_sub(HISTORY.saturating_sub(1) as u64);
            if sequence < floor {
                return;
            }
            self.twcc_highest = Some(highest);
            while self.receipts.front().is_some_and(|r| r.sequence < floor) {
                self.receipts.pop_front();
            }
            let Err(index) = self
                .receipts
                .binary_search_by_key(&sequence, |r| r.sequence)
            else {
                return;
            };
            self.twcc_next = Some(
                self.twcc_next
                    .map_or(sequence, |next| next.min(sequence))
                    .max(floor),
            );
            index
        } else {
            if self
                .receipts
                .iter()
                .any(|r| r.ssrc == ssrc && r.sequence == sequence)
            {
                return;
            }
            self.receipts.len()
        };
        if self.receipts.len() == HISTORY {
            self.receipts.pop_front();
            index = index.saturating_sub(1);
        }
        self.receipts.insert(
            index,
            Receipt {
                ssrc,
                sequence,
                arrival: now,
                ecn,
                pending: true,
            },
        );
        self.deadline
            .get_or_insert_with(|| now.checked_add(INTERVAL).unwrap_or(now));
    }

    pub(crate) fn clear(&mut self) {
        self.receipts.clear();
        self.twcc_highest = None;
        self.twcc_next = None;
        self.deadline = None;
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(crate) fn poll(&mut self, now: Instant, sender_ssrc: u32) -> Option<Vec<u8>> {
        if now < self.deadline? {
            return None;
        }
        let packet = match self.mode? {
            PacketFeedbackKind::TransportWide => self.twcc(now, sender_ssrc),
            PacketFeedbackKind::Rfc8888 => self.rfc8888(now, sender_ssrc),
        };
        let pending = match self.mode? {
            PacketFeedbackKind::TransportWide => self
                .twcc_next
                .zip(self.twcc_highest)
                .is_some_and(|(next, highest)| next <= highest),
            PacketFeedbackKind::Rfc8888 => self.receipts.iter().any(|r| r.pending),
        };
        self.deadline = pending.then_some(now);
        packet
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "each report spans at most 256 sequences and each status vector contains at most seven symbols"
    )]
    fn twcc(&mut self, now: Instant, sender_ssrc: u32) -> Option<Vec<u8>> {
        let base = self.twcc_next?;
        let end = (base + REPORT_STATUSES).min(self.twcc_highest? + 1);
        let first_time = self
            .receipts
            .get(self.receipts.partition_point(|r| r.sequence < base))
            .filter(|r| r.sequence < end)
            .map_or(now, |r| r.arrival);
        let reference_ticks = self.ticks(first_time) / 256;
        let mut previous = reference_ticks * 256;
        let mut statuses = Vec::with_capacity(REPORT_STATUSES as usize);
        let mut deltas = VecDeque::new();
        for sequence in base..end {
            let receipt = self
                .receipts
                .binary_search_by_key(&sequence, |r| r.sequence)
                .ok()
                .and_then(|index| self.receipts.get(index));
            let status = if let Some(receipt) = receipt {
                let ticks = self.ticks(receipt.arrival);
                let Ok(delta) = i16::try_from(i128::from(ticks) - i128::from(previous)) else {
                    break;
                };
                previous = ticks;
                if let Ok(small) = u8::try_from(delta) {
                    deltas.push_back(Delta::Small(small));
                    1_u16
                } else {
                    deltas.push_back(Delta::Large(delta));
                    2_u16
                }
            } else {
                0
            };
            statuses.push(status);
        }
        let count = statuses.len() as u16;
        let chunks = statuses
            .chunks(7)
            .map(|statuses| {
                let bits = statuses
                    .iter()
                    .enumerate()
                    .fold(0, |bits, (index, status)| {
                        bits | (status << ((6 - index) * 2))
                    });
                PacketChunk::VectorDouble(bits, 7)
            })
            .collect();
        let report = Twcc {
            sender_ssrc: sender_ssrc.into(),
            ssrc: 0.into(),
            base_seq: base as u16,
            status_count: count,
            reference_time: reference_ticks as u32 & 0x00ff_ffff,
            feedback_count: self.feedback_count,
            chunks,
            delta: deltas,
        };
        let mut queue = VecDeque::from([Rtcp::Twcc(report)]);
        let mut bytes = vec![0; 1200];
        let len = Rtcp::write_packet(&mut queue, &mut bytes, |_| {});
        debug_assert!(queue.is_empty());
        bytes.truncate(len);
        let covered_end = base + u64::from(count);
        for receipt in &mut self.receipts {
            if (base..covered_end).contains(&receipt.sequence) {
                receipt.pending = false;
            }
        }
        self.feedback_count = self.feedback_count.wrapping_add(1);
        self.twcc_next = Some(covered_end);
        Some(bytes)
    }

    #[allow(
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        reason = "the report has at most 256 statuses and its fixed four-byte header is present before patching its length"
    )]
    fn rfc8888(&mut self, now: Instant, sender_ssrc: u32) -> Option<Vec<u8>> {
        let ssrc = self.receipts.iter().find(|r| r.pending)?.ssrc;
        let first = self
            .receipts
            .iter()
            .filter(|r| r.ssrc == ssrc && r.pending)
            .map(|r| r.sequence)
            .min()?;
        let preceding = self
            .receipts
            .iter()
            .filter(|r| r.ssrc == ssrc && !r.pending && r.sequence < first)
            .map(|r| r.sequence + 1)
            .max()
            .unwrap_or(first);
        let base = preceding.max(first.saturating_sub(REPORT_STATUSES - 1));
        let end = self
            .receipts
            .iter()
            .filter(|r| r.ssrc == ssrc && r.pending)
            .map(|r| r.sequence + 1)
            .max()?
            .min(base + REPORT_STATUSES);
        let count = (end - base) as u16;
        let mut bytes = vec![0x8b, 205, 0, 0];
        bytes.extend_from_slice(&sender_ssrc.to_be_bytes());
        bytes.extend_from_slice(&ssrc.to_be_bytes());
        bytes.extend_from_slice(&(base as u16).to_be_bytes());
        bytes.extend_from_slice(&count.to_be_bytes());
        for sequence in base..end {
            let status = self
                .receipts
                .iter()
                .find(|r| r.ssrc == ssrc && r.sequence == sequence)
                .map_or(0, |r| {
                    let offset = now
                        .saturating_duration_since(r.arrival)
                        .as_micros()
                        .saturating_mul(1024)
                        / 1_000_000;
                    let ecn = match r.ecn {
                        Some(EcnCodepoint::Ect1) => 1,
                        Some(EcnCodepoint::Ect0) => 2,
                        Some(EcnCodepoint::Ce) => 3,
                        _ => 0,
                    };
                    0x8000 | (ecn << 13) | offset.min(0x1ffe) as u16
                });
            bytes.extend_from_slice(&status.to_be_bytes());
        }
        if !count.is_multiple_of(2) {
            bytes.extend_from_slice(&[0, 0]);
        }
        let timestamp = (now
            .saturating_duration_since(self.origin)
            .as_micros()
            .saturating_mul(65536)
            / 1_000_000) as u32;
        bytes.extend_from_slice(&timestamp.to_be_bytes());
        let words = (bytes.len() / 4 - 1) as u16;
        bytes[2..4].copy_from_slice(&words.to_be_bytes());
        for receipt in &mut self.receipts {
            if receipt.ssrc == ssrc && (base..end).contains(&receipt.sequence) {
                receipt.pending = false;
            }
        }
        Some(bytes)
    }

    fn ticks(&self, at: Instant) -> u64 {
        (at.saturating_duration_since(self.origin).as_micros() / 250) as u64
    }
}

fn extend(highest: Option<u64>, sequence: u16) -> u64 {
    let Some(highest) = highest else {
        return u64::from(sequence) + 65536;
    };
    highest.saturating_add_signed(i64::from(i16::from_be_bytes(
        sequence.wrapping_sub(highest as u16).to_be_bytes(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        rtcp::{self, ArrivalOffset, FeedbackReport, Rfc8888Status, TwccStatus},
        transport::PathEpoch,
    };

    fn at(start: Instant, millis: u64) -> Instant {
        start.checked_add(Duration::from_millis(millis)).unwrap()
    }

    fn report(receiver: &mut ReceiveFeedback, now: Instant) -> FeedbackReport {
        let bytes = receiver.poll(now, 9).expect("due report");
        assert!(bytes.len() <= 1200);
        let mut parsed = rtcp::parse(&bytes, now, PathEpoch::from_value(1), receiver.mode)
            .expect("valid wire RTCP");
        assert_eq!(parsed.feedback.len(), 1);
        let feedback = parsed.feedback.pop().unwrap();
        assert_eq!(feedback.sender_ssrc, 9);
        feedback.report
    }

    #[test]
    fn twcc_tracks_one_transport_space_with_wrap_reorder_loss_and_late_arrival() {
        let start = Instant::now();
        let mut receiver = ReceiveFeedback::new(Some(PacketFeedbackKind::TransportWide), start);
        receiver.observe(start, 7, 99, Some(u16::MAX - 1), None);
        receiver.observe(at(start, 1), 8, 50, Some(0), None);
        receiver.observe(at(start, 2), 9, 1, Some(u16::MAX), None);
        receiver.observe(at(start, 3), 9, 1, Some(u16::MAX), None);
        assert!(receiver.poll(at(start, 49), 9).is_none());
        let FeedbackReport::Twcc {
            base_sequence,
            feedback_count,
            statuses,
            ..
        } = report(&mut receiver, at(start, 50))
        else {
            panic!("TWCC");
        };
        assert_eq!(base_sequence, u16::MAX - 1);
        assert_eq!(feedback_count, 0);
        assert_eq!(
            &*statuses,
            &[
                TwccStatus::Received { delta_250us: 0 },
                TwccStatus::Received { delta_250us: 8 },
                TwccStatus::Received { delta_250us: -4 }
            ]
        );
        receiver.observe(at(start, 51), 8, 50, Some(0), None);
        assert_eq!(receiver.deadline(), None);
        receiver.observe(at(start, 100), 7, 100, Some(3), None);
        let FeedbackReport::Twcc {
            base_sequence,
            statuses,
            ..
        } = report(&mut receiver, at(start, 150))
        else {
            panic!("TWCC");
        };
        assert_eq!(base_sequence, 1);
        assert_eq!(
            &*statuses,
            &[
                TwccStatus::NotReceived,
                TwccStatus::NotReceived,
                TwccStatus::Received { delta_250us: 144 }
            ]
        );
        receiver.observe(at(start, 160), 7, 98, Some(1), None);
        let FeedbackReport::Twcc {
            base_sequence,
            statuses,
            ..
        } = report(&mut receiver, at(start, 210))
        else {
            panic!("TWCC");
        };
        assert_eq!(base_sequence, 1);
        assert_eq!(
            &*statuses,
            &[
                TwccStatus::Received { delta_250us: 128 },
                TwccStatus::NotReceived,
                TwccStatus::Received { delta_250us: -240 }
            ]
        );
        assert!(receiver.poll(at(start, 211), 9).is_none());
    }

    #[test]
    fn twcc_bounds_retained_receipts_report_work_and_unrepresentable_time_deltas() {
        let start = Instant::now();
        let mut receiver = ReceiveFeedback::new(Some(PacketFeedbackKind::TransportWide), start);
        for sequence in 0..600 {
            receiver.observe(start, 7, sequence, Some(sequence), None);
            assert!(receiver.receipts.len() <= HISTORY);
        }
        let mut reported = 0;
        while receiver.deadline().is_some() {
            let FeedbackReport::Twcc { statuses, .. } = report(&mut receiver, at(start, 50)) else {
                panic!("TWCC");
            };
            assert!(statuses.len() <= REPORT_STATUSES as usize);
            reported += statuses.len();
        }
        assert_eq!(reported, HISTORY);
        receiver.clear();
        receiver.observe(start, 7, 10, Some(10), None);
        receiver.observe(at(start, 30_000), 7, 11, Some(11), None);
        for expected in [10, 11] {
            let FeedbackReport::Twcc {
                base_sequence,
                statuses,
                ..
            } = report(&mut receiver, at(start, 30_050))
            else {
                panic!("TWCC");
            };
            assert_eq!(base_sequence, expected);
            assert_eq!(statuses.len(), 1);
        }
        assert_eq!(receiver.deadline(), None);
    }

    #[test]
    fn rfc8888_reports_original_and_repair_spaces_with_arrival_hold_and_ecn() {
        let start = Instant::now();
        let mut receiver = ReceiveFeedback::new(Some(PacketFeedbackKind::Rfc8888), start);
        receiver.observe(start, 7, u16::MAX, None, Some(EcnCodepoint::Ce));
        receiver.observe(at(start, 1), 8, 5, None, None);
        receiver.observe(at(start, 2), 7, 1, None, Some(EcnCodepoint::Ect1));
        receiver.observe(at(start, 3), 7, 0, None, Some(EcnCodepoint::Ect0));
        receiver.observe(at(start, 4), 9, 15, None, None);
        let FeedbackReport::Rfc8888 {
            reports,
            report_timestamp,
        } = report(&mut receiver, at(start, 50))
        else {
            panic!("RFC8888");
        };
        assert_eq!(report_timestamp, 3276);
        let [stream] = &*reports else {
            panic!("one SSRC block");
        };
        assert_eq!(stream.ssrc, 7);
        assert_eq!(stream.begin_sequence, u16::MAX);
        assert_eq!(stream.report_count, 3);
        assert_eq!(
            &*stream.statuses,
            &[
                Rfc8888Status::Received {
                    ecn: 3,
                    arrival_offset: ArrivalOffset::Ticks(51)
                },
                Rfc8888Status::Received {
                    ecn: 2,
                    arrival_offset: ArrivalOffset::Ticks(48)
                },
                Rfc8888Status::Received {
                    ecn: 1,
                    arrival_offset: ArrivalOffset::Ticks(49)
                },
            ]
        );
        for ssrc in [8, 9] {
            let FeedbackReport::Rfc8888 { reports, .. } = report(&mut receiver, at(start, 50))
            else {
                panic!("RFC8888");
            };
            assert_eq!(reports.first().unwrap().ssrc, ssrc);
            assert_eq!(reports.first().unwrap().report_count, 1);
        }
        assert_eq!(receiver.deadline(), None);
        receiver.observe(at(start, 60), 8, 7, None, None);
        let FeedbackReport::Rfc8888 { reports, .. } = report(&mut receiver, at(start, 9_000))
        else {
            panic!("RFC8888");
        };
        assert_eq!(reports.first().unwrap().begin_sequence, 6);
        assert_eq!(
            &*reports.first().unwrap().statuses,
            &[
                Rfc8888Status::NotReceived,
                Rfc8888Status::Received {
                    ecn: 0,
                    arrival_offset: ArrivalOffset::OverRange
                }
            ]
        );
    }
}

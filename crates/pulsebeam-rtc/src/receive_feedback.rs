#![allow(
    clippy::cast_possible_truncation,
    reason = "bounded report lengths and wrapping RTCP wire counters retain their low bits"
)]

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use pulsebeam_feedback::{
    Ecn, Epoch, PacketId, Receipt, Receiver, ReceiverConfig, ReportOrder, ReportWindow, SpaceId,
};
use str0m::rtp_::rtcp::{
    Rtcp, Twcc,
    twcc::{Delta, PacketChunk},
};

use crate::{EcnCodepoint, PacketFeedbackKind};

const HISTORY: usize = 512;
const REPORT_STATUSES: usize = 256;
const INTERVAL: Duration = Duration::from_millis(50);
const EPOCH: Epoch = Epoch(0);
const TRANSPORT: SpaceId = SpaceId(0);

fn config(mode: PacketFeedbackKind) -> ReceiverConfig {
    ReceiverConfig {
        capacity: HISTORY,
        max_report_statuses: REPORT_STATUSES,
        interval: INTERVAL,
        order: match mode {
            PacketFeedbackKind::TransportWide => ReportOrder::Continuous(TRANSPORT),
            PacketFeedbackKind::Rfc8888 => ReportOrder::Independent,
        },
    }
}

#[derive(Debug)]
pub(crate) struct ReceiveFeedback {
    mode: Option<PacketFeedbackKind>,
    origin: Instant,
    receiver: Option<Receiver>,
    feedback_count: u8,
}

impl ReceiveFeedback {
    pub(crate) fn new(mode: Option<PacketFeedbackKind>, origin: Instant) -> Self {
        Self {
            mode,
            origin,
            receiver: None,
            feedback_count: 0,
        }
    }

    pub(crate) fn storage_bytes(&self) -> usize {
        self.receiver.as_ref().map_or(0, Receiver::storage_bytes)
    }

    pub(crate) fn reserve(&mut self, limit: usize) -> Option<usize> {
        match self.mode {
            Some(mode) => self.set_profile(mode, limit),
            None => Some(0),
        }
    }

    pub(crate) fn set_profile(
        &mut self,
        profile: PacketFeedbackKind,
        limit: usize,
    ) -> Option<usize> {
        if self.mode.is_some_and(|mode| mode != profile) {
            return None;
        }
        let bytes = if let Some(receiver) = &mut self.receiver {
            receiver.reserve(limit).ok()?
        } else {
            let mut receiver = Receiver::new(config(profile), EPOCH).ok()?;
            let bytes = receiver.reserve(limit).ok()?;
            self.receiver = Some(receiver);
            bytes
        };
        self.mode = Some(profile);
        Some(bytes)
    }

    pub(crate) fn observe(
        &mut self,
        now: Instant,
        ssrc: u32,
        sequence: u16,
        twcc: Option<u16>,
        ecn: Option<EcnCodepoint>,
    ) {
        let Some(receiver) = &mut self.receiver else {
            return;
        };
        let (space, sequence) = match self.mode {
            Some(PacketFeedbackKind::TransportWide) => {
                let Some(sequence) = twcc else {
                    return;
                };
                (TRANSPORT, sequence)
            }
            Some(PacketFeedbackKind::Rfc8888) => (SpaceId(u64::from(ssrc)), sequence),
            None => return,
        };
        let sequence = extend(receiver.highest(space).map(|id| id.0), sequence);
        let _ = receiver.observe(
            EPOCH,
            Receipt {
                space,
                packet: PacketId(sequence),
                arrived_at: now.saturating_duration_since(self.origin),
                ecn: ecn.map(|mark| match mark {
                    EcnCodepoint::NotEct => Ecn::NotEct,
                    EcnCodepoint::Ect1 => Ecn::Ect1,
                    EcnCodepoint::Ect0 => Ecn::Ect0,
                    EcnCodepoint::Ce => Ecn::Ce,
                }),
                tag: u64::from(ssrc),
            },
        );
    }

    pub(crate) fn clear(&mut self) {
        if let Some(receiver) = &mut self.receiver {
            receiver.clear(EPOCH);
        }
    }

    pub(crate) fn abort(&mut self) {
        if let Some(receiver) = &mut self.receiver {
            receiver.abort();
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.origin.checked_add(self.receiver.as_ref()?.deadline()?)
    }

    pub(crate) fn poll(&mut self, now: Instant, sender_ssrc: u32) -> Option<Vec<u8>> {
        let elapsed = now.saturating_duration_since(self.origin);
        let report = self.receiver.as_ref()?.report(elapsed)?;
        let encoded = match self.mode? {
            PacketFeedbackKind::TransportWide => self.twcc(elapsed, sender_ssrc, report),
            PacketFeedbackKind::Rfc8888 => self.rfc8888(elapsed, sender_ssrc, report),
        };
        let receiver = self.receiver.as_mut()?;
        let Some((bytes, count)) = encoded else {
            let _ = receiver.defer(elapsed, elapsed.saturating_add(INTERVAL));
            return None;
        };
        receiver.accept(report, count, elapsed).ok()?;
        if self.mode == Some(PacketFeedbackKind::TransportWide) {
            self.feedback_count = self.feedback_count.wrapping_add(1);
        }
        Some(bytes)
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "each report spans at most 256 sequences and each status vector contains at most seven symbols"
    )]
    fn twcc(
        &self,
        now: Duration,
        sender_ssrc: u32,
        window: ReportWindow,
    ) -> Option<(Vec<u8>, usize)> {
        let receiver = self.receiver.as_ref()?;
        let base = window.base.0;
        let end = base.checked_add(u64::try_from(window.count).ok()?)?;
        let first = receiver.first_receipt(window)?;
        // libwebrtc dispatches TWCC only for a registered media SSRC, including
        // loss-only slices whose next receipt lies beyond this bounded window.
        let media_ssrc = u32::try_from(first.tag).ok()?;
        let first_time = if first.packet.0 < end {
            first.arrived_at
        } else {
            now
        };
        let reference_ticks = ticks(first_time) / 256;
        let mut previous = reference_ticks * 256;
        let mut statuses = Vec::with_capacity(REPORT_STATUSES);
        let mut deltas = VecDeque::new();
        for sequence in base..end {
            let receipt = receiver.receipt(window.space, PacketId(sequence));
            let status = if let Some(receipt) = receipt {
                let ticks = ticks(receipt.arrived_at);
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
        let count = statuses.len();
        if count == 0 {
            return None;
        }
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
            ssrc: media_ssrc.into(),
            base_seq: base as u16,
            status_count: u16::try_from(count).ok()?,
            reference_time: reference_ticks as u32 & 0x00ff_ffff,
            feedback_count: self.feedback_count,
            chunks,
            delta: deltas,
        };
        let mut queue = VecDeque::from([Rtcp::Twcc(report)]);
        let mut bytes = vec![0; 1200];
        let len = Rtcp::write_packet(&mut queue, &mut bytes, |_| {});
        if !queue.is_empty() || len == 0 {
            return None;
        }
        bytes.truncate(len);
        let serialized = bytes.get(14..16)?;
        let serialized = usize::from(u16::from_be_bytes(serialized.try_into().ok()?));
        if serialized == 0 || serialized > count {
            return None;
        }
        Some((bytes, serialized))
    }

    #[allow(
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        reason = "the report has at most 256 statuses and its fixed four-byte header is present before patching its length"
    )]
    fn rfc8888(
        &self,
        now: Duration,
        sender_ssrc: u32,
        window: ReportWindow,
    ) -> Option<(Vec<u8>, usize)> {
        let receiver = self.receiver.as_ref()?;
        let ssrc = u32::try_from(window.space.0).ok()?;
        let base = window.base.0;
        let count = u16::try_from(window.count).ok()?;
        let end = base.checked_add(u64::from(count))?;
        let mut bytes = vec![0x8b, 205, 0, 0];
        bytes.extend_from_slice(&sender_ssrc.to_be_bytes());
        bytes.extend_from_slice(&ssrc.to_be_bytes());
        bytes.extend_from_slice(&(base as u16).to_be_bytes());
        bytes.extend_from_slice(&count.to_be_bytes());
        for sequence in base..end {
            let status = receiver
                .receipt(window.space, PacketId(sequence))
                .map_or(0, |receipt| {
                    let offset = now
                        .saturating_sub(receipt.arrived_at)
                        .as_micros()
                        .saturating_mul(1024)
                        / 1_000_000;
                    let ecn = match receipt.ecn {
                        Some(Ecn::Ect1) => 1,
                        Some(Ecn::Ect0) => 2,
                        Some(Ecn::Ce) => 3,
                        _ => 0,
                    };
                    0x8000 | (ecn << 13) | offset.min(0x1ffe) as u16
                });
            bytes.extend_from_slice(&status.to_be_bytes());
        }
        if !count.is_multiple_of(2) {
            bytes.extend_from_slice(&[0, 0]);
        }
        let timestamp = (now.as_micros().saturating_mul(65536) / 1_000_000) as u32;
        bytes.extend_from_slice(&timestamp.to_be_bytes());
        let words = (bytes.len() / 4 - 1) as u16;
        bytes[2..4].copy_from_slice(&words.to_be_bytes());
        Some((bytes, usize::from(count)))
    }
}

fn ticks(at: Duration) -> u64 {
    (at.as_micros() / 250) as u64
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
        let bytes = rtcp::compound(bytes, 9, b"feedback-reporter").unwrap();
        assert_eq!(&bytes[..4], &[0x80, 201, 0, 1]);
        let mut parsed = rtcp::parse(&bytes, now, PathEpoch::from_value(1), receiver.mode)
            .expect("valid wire RTCP");
        assert_eq!(parsed.feedback.len(), 1);
        assert_eq!(
            parsed.lifecycle,
            vec![rtcp::LifecycleFact::Cname {
                ssrc: 9,
                cname: "feedback-reporter".into()
            }]
        );
        let feedback = parsed.feedback.pop().unwrap();
        assert_eq!(feedback.sender_ssrc, 9);
        feedback.report
    }

    #[test]
    fn twcc_tracks_one_transport_space_with_wrap_reorder_loss_and_late_arrival() {
        let start = Instant::now();
        let mut receiver = ReceiveFeedback::new(Some(PacketFeedbackKind::TransportWide), start);
        receiver.reserve(usize::MAX).unwrap();
        receiver.observe(start, 7, 99, Some(u16::MAX - 1), None);
        receiver.observe(at(start, 1), 8, 50, Some(0), None);
        receiver.observe(at(start, 2), 9, 1, Some(u16::MAX), None);
        receiver.observe(at(start, 3), 9, 1, Some(u16::MAX), None);
        assert!(receiver.poll(at(start, 49), 9).is_none());
        let FeedbackReport::Twcc {
            media_ssrc,
            base_sequence,
            feedback_count,
            statuses,
            ..
        } = report(&mut receiver, at(start, 50))
        else {
            panic!("TWCC");
        };
        assert_eq!(
            media_ssrc, 7,
            "anchor one actual stream in transport feedback"
        );
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
        let mut receiver = ReceiveFeedback::new(None, start);
        assert_eq!(receiver.reserve(0), Some(0));
        receiver.observe(start, 0, 0, Some(0), None);
        assert!(
            receiver.receiver.as_ref().is_none_or(Receiver::is_empty)
                && receiver.deadline().is_none()
        );
        let required = config(PacketFeedbackKind::TransportWide)
            .minimum_storage_bytes()
            .unwrap();
        assert_eq!(
            receiver.set_profile(PacketFeedbackKind::TransportWide, required - 1),
            None
        );
        assert_eq!(receiver.mode, None);
        assert_eq!(receiver.storage_bytes(), 0);
        assert_eq!(
            receiver.set_profile(PacketFeedbackKind::TransportWide, required),
            Some(required)
        );
        receiver.observe(start, 0, 0, None, None);
        assert!(receiver.receiver.as_ref().unwrap().is_empty() && receiver.deadline().is_none());
        for sequence in 0..600 {
            receiver.observe(start, 0, sequence, Some(sequence), None);
            assert!(receiver.receiver.as_ref().unwrap().len() <= HISTORY);
            assert_eq!(receiver.storage_bytes(), required);
        }
        let mut reported = 0;
        while receiver.deadline().is_some() {
            let FeedbackReport::Twcc { statuses, .. } = report(&mut receiver, at(start, 50)) else {
                panic!("TWCC");
            };
            assert!(statuses.len() <= REPORT_STATUSES);
            reported += statuses.len();
        }
        assert_eq!(reported, HISTORY);
        let deadline = receiver.deadline();
        assert_eq!(receiver.reserve(required - 1), None);
        assert_eq!(receiver.storage_bytes(), required);
        assert_eq!(receiver.deadline(), deadline);
        receiver.clear();
        assert_eq!(receiver.storage_bytes(), required);
        receiver.observe(start, 11, 10, Some(10), None);
        receiver.observe(at(start, 30_000), 12, 11, Some(11), None);
        for (expected_sequence, expected_ssrc) in [(10, 11), (11, 12)] {
            let FeedbackReport::Twcc {
                media_ssrc,
                base_sequence,
                statuses,
                ..
            } = report(&mut receiver, at(start, 30_050))
            else {
                panic!("TWCC");
            };
            assert_eq!(
                media_ssrc, expected_ssrc,
                "clear must retire the old source"
            );
            assert_eq!(base_sequence, expected_sequence);
            assert_eq!(statuses.len(), 1);
        }
        assert_eq!(receiver.deadline(), None);
        assert_eq!(receiver.storage_bytes(), required);
        receiver.abort();
        assert_eq!(receiver.storage_bytes(), 0);
        assert_eq!(receiver.deadline(), None);
    }

    #[test]
    fn twcc_anchors_loss_only_ranges_without_splitting_transport_space() {
        let start = Instant::now();
        let mut receiver = ReceiveFeedback::new(Some(PacketFeedbackKind::TransportWide), start);
        receiver.reserve(usize::MAX).unwrap();
        receiver.observe(start, 7, 10, Some(0), None);
        let FeedbackReport::Twcc { media_ssrc, .. } = report(&mut receiver, at(start, 50)) else {
            panic!("TWCC");
        };
        assert_eq!(media_ssrc, 7);
        receiver.observe(at(start, 60), 8, 20, Some(300), None);
        let FeedbackReport::Twcc {
            media_ssrc,
            base_sequence,
            statuses,
            ..
        } = report(&mut receiver, at(start, 110))
        else {
            panic!("TWCC");
        };
        assert_eq!(
            media_ssrc, 8,
            "even a loss-only slice uses an observed source"
        );
        assert_eq!(base_sequence, 1);
        assert_eq!(statuses.len(), REPORT_STATUSES);
        assert!(statuses.iter().all(|s| *s == TwccStatus::NotReceived));
        let FeedbackReport::Twcc {
            media_ssrc,
            base_sequence,
            statuses,
            ..
        } = report(&mut receiver, at(start, 110))
        else {
            panic!("TWCC");
        };
        assert_eq!(media_ssrc, 8);
        assert_eq!(base_sequence, 257);
        assert_eq!(statuses.len(), 44);
        assert!(matches!(statuses.last(), Some(TwccStatus::Received { .. })));
        assert_eq!(receiver.deadline(), None);
    }

    #[test]
    fn rfc8888_reports_original_and_repair_spaces_with_arrival_hold_and_ecn() {
        let start = Instant::now();
        let mut receiver = ReceiveFeedback::new(Some(PacketFeedbackKind::Rfc8888), start);
        let required = receiver.reserve(usize::MAX).unwrap();
        receiver.observe(start, 0, u16::MAX, None, Some(EcnCodepoint::Ce));
        receiver.observe(at(start, 1), 8, 5, None, None);
        receiver.observe(at(start, 2), 0, 1, None, Some(EcnCodepoint::Ect1));
        receiver.observe(at(start, 3), 0, 0, None, Some(EcnCodepoint::Ect0));
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
        assert_eq!(stream.ssrc, 0);
        assert_eq!(receiver.storage_bytes(), required);
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

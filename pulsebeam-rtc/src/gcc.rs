use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

use crate::{CompoundRtcpView, SendId};

const TWCC_PACKET_TYPE: u8 = 205;
const TWCC_FORMAT: u8 = 15;
const MIN_BITRATE_BPS: u64 = 30_000;
const MAX_BITRATE_BPS: u64 = 50_000_000;
const INITIAL_BITRATE_BPS: u64 = 300_000;
const OUTAGE: Duration = Duration::from_secs(2);
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CongestionEstimate {
    bitrate_bps: u64,
    application_limited: bool,
}

impl CongestionEstimate {
    pub const fn bitrate_bps(self) -> u64 {
        self.bitrate_bps
    }

    pub const fn application_limited(self) -> bool {
        self.application_limited
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EgressCongestion {
    send_id: SendId,
    transport_sequence: u16,
}

impl EgressCongestion {
    pub const fn send_id(self) -> SendId {
        self.send_id
    }

    pub const fn transport_sequence(self) -> u16 {
        self.transport_sequence
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeDecision {
    target_bitrate_bps: u64,
    packet_count: u8,
}

impl ProbeDecision {
    pub const fn target_bitrate_bps(self) -> u64 {
        self.target_bitrate_bps
    }

    pub const fn packet_count(self) -> u8 {
        self.packet_count
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GccOutcome {
    estimate: CongestionEstimate,
    acknowledged: usize,
    lost: usize,
    probe: Option<ProbeDecision>,
}

impl GccOutcome {
    pub const fn estimate(self) -> CongestionEstimate {
        self.estimate
    }

    pub const fn acknowledged(self) -> usize {
        self.acknowledged
    }

    pub const fn lost(self) -> usize {
        self.lost
    }

    pub const fn probe(self) -> Option<ProbeDecision> {
        self.probe
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TwccStatus {
    sequence: u16,
    received_at: Option<Duration>,
}

impl TwccStatus {
    pub const fn sequence(self) -> u16 {
        self.sequence
    }

    pub const fn received_at(self) -> Option<Duration> {
        self.received_at
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TwccFeedback {
    statuses: Box<[TwccStatus]>,
}

impl TwccFeedback {
    pub fn statuses(&self) -> &[TwccStatus] {
        &self.statuses
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GccError {
    #[error("TWCC feedback is malformed")]
    MalformedTwcc,
    #[error("send identity {0:?} is already tracked")]
    DuplicateSend(SendId),
    #[error("send identity {0:?} is not tracked")]
    UnknownSend(SendId),
    #[error("send identity {0:?} already has a departure timestamp")]
    DuplicateDeparture(SendId),
}

#[derive(Clone, Copy, Debug)]
struct SendRecord {
    transport_sequence: u16,
    bytes: usize,
    departed_at: Option<Instant>,
    acknowledged: bool,
}

pub struct Gcc {
    next_transport_sequence: u16,
    records: HashMap<SendId, SendRecord>,
    sequence_index: HashMap<u16, SendId>,
    history_order: VecDeque<SendId>,
    history_capacity: usize,
    bitrate_bps: u64,
    last_departure: Option<Instant>,
    last_feedback: Option<Instant>,
    last_probe: Option<Instant>,
    application_limited: bool,
}

impl Gcc {
    pub fn new(history_capacity: usize) -> Self {
        Self::with_initial_bitrate(history_capacity, INITIAL_BITRATE_BPS)
    }

    pub fn with_initial_bitrate(history_capacity: usize, initial_bitrate_bps: u64) -> Self {
        let history_capacity = history_capacity.max(1);
        Self {
            next_transport_sequence: 0,
            records: HashMap::with_capacity(history_capacity),
            sequence_index: HashMap::with_capacity(history_capacity),
            history_order: VecDeque::with_capacity(history_capacity),
            history_capacity,
            bitrate_bps: initial_bitrate_bps.clamp(MIN_BITRATE_BPS, MAX_BITRATE_BPS),
            last_departure: None,
            last_feedback: None,
            last_probe: None,
            application_limited: true,
        }
    }

    pub fn estimate(&self, now: Instant) -> CongestionEstimate {
        CongestionEstimate {
            bitrate_bps: self.bitrate_bps,
            application_limited: self.application_limited
                || self
                    .last_departure
                    .is_none_or(|departure| now.saturating_duration_since(departure) > OUTAGE),
        }
    }

    pub fn assign(&mut self, send_id: SendId, bytes: usize) -> Result<EgressCongestion, GccError> {
        if self.records.contains_key(&send_id) {
            return Err(GccError::DuplicateSend(send_id));
        }
        while self.history_order.len() >= self.history_capacity {
            let Some(expired) = self.history_order.pop_front() else {
                break;
            };
            if let Some(record) = self.records.remove(&expired) {
                self.sequence_index.remove(&record.transport_sequence);
            }
        }
        let sequence = self.next_transport_sequence;
        self.next_transport_sequence = self.next_transport_sequence.wrapping_add(1);
        let record = SendRecord {
            transport_sequence: sequence,
            bytes,
            departed_at: None,
            acknowledged: false,
        };
        self.records.insert(send_id, record);
        self.sequence_index.insert(sequence, send_id);
        self.history_order.push_back(send_id);
        Ok(EgressCongestion {
            send_id,
            transport_sequence: sequence,
        })
    }

    pub fn record_departure(&mut self, send_id: SendId, now: Instant) -> Result<(), GccError> {
        let record = self
            .records
            .get_mut(&send_id)
            .ok_or(GccError::UnknownSend(send_id))?;
        if record.departed_at.is_some() {
            return Err(GccError::DuplicateDeparture(send_id));
        }
        record.departed_at = Some(now);
        self.last_departure = Some(now);
        self.application_limited = false;
        Ok(())
    }

    pub fn process_feedback(&mut self, now: Instant, feedback: &TwccFeedback) -> GccOutcome {
        let mut acknowledged = Vec::new();
        let mut lost = 0usize;
        for status in feedback.statuses() {
            let Some(send_id) = self.sequence_index.get(&status.sequence()).copied() else {
                continue;
            };
            let Some(record) = self.records.get_mut(&send_id) else {
                continue;
            };
            let Some(departed_at) = record.departed_at else {
                continue;
            };
            if record.acknowledged {
                continue;
            }
            record.acknowledged = true;
            if let Some(received_at) = status.received_at() {
                acknowledged.push((departed_at, received_at, record.bytes));
            } else {
                lost = lost.saturating_add(1);
            }
        }
        let acknowledged_count = acknowledged.len();
        let total = acknowledged_count.saturating_add(lost);
        let congested = lost.saturating_mul(2) > total;
        if total > 0 {
            let first_feedback = self.last_feedback.is_none();
            self.last_feedback = Some(now);
            self.update_estimate(&acknowledged, lost, congested, first_feedback);
        }
        let probe = self.maybe_probe(now, congested);
        GccOutcome {
            estimate: self.estimate(now),
            acknowledged: acknowledged_count,
            lost,
            probe,
        }
    }

    pub fn process_rtcp(
        &mut self,
        now: Instant,
        rtcp: &CompoundRtcpView<'_>,
    ) -> Result<Vec<GccOutcome>, GccError> {
        let feedback = parse_twcc(rtcp)?;
        Ok(feedback
            .iter()
            .map(|feedback| self.process_feedback(now, feedback))
            .collect())
    }

    pub fn handle_timeout(&mut self, now: Instant) -> Option<ProbeDecision> {
        if self
            .last_feedback
            .is_some_and(|feedback| now.saturating_duration_since(feedback) > OUTAGE)
        {
            self.bitrate_bps = self
                .bitrate_bps
                .saturating_mul(4)
                .saturating_div(5)
                .max(MIN_BITRATE_BPS);
            self.application_limited = true;
        }
        self.maybe_probe(now, false)
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "the saturating throughput calculation accepts only a nonzero interval"
    )]
    fn update_estimate(
        &mut self,
        acknowledged: &[(Instant, Duration, usize)],
        lost: usize,
        congested: bool,
        first_feedback: bool,
    ) {
        if acknowledged.len() >= 2 {
            let Some(first) = acknowledged.first() else {
                return;
            };
            let Some(last) = acknowledged.last() else {
                return;
            };
            let departed = last.0.saturating_duration_since(first.0);
            let received = last.1.saturating_sub(first.1);
            let bytes: usize = acknowledged.iter().map(|sample| sample.2).sum();
            let interval = departed.max(received);
            if !interval.is_zero() {
                let throughput = u64::try_from(bytes)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(8)
                    .saturating_mul(1_000_000)
                    .saturating_div(
                        u64::try_from(interval.as_micros())
                            .unwrap_or(u64::MAX)
                            .max(1),
                    );
                if first_feedback && lost == 0 && !congested {
                    self.bitrate_bps = throughput;
                } else if received > departed.saturating_add(Duration::from_millis(15)) {
                    self.bitrate_bps = self.bitrate_bps.saturating_mul(85).saturating_div(100);
                } else if !self.application_limited {
                    let increased = self.bitrate_bps.saturating_mul(105).saturating_div(100);
                    self.bitrate_bps = increased.min(throughput.max(self.bitrate_bps));
                }
            }
        }
        let total = acknowledged.len().saturating_add(lost);
        if total > 0 && congested {
            self.bitrate_bps = self.bitrate_bps.saturating_mul(85).saturating_div(100);
        }
        self.bitrate_bps = self.bitrate_bps.clamp(MIN_BITRATE_BPS, MAX_BITRATE_BPS);
    }

    fn maybe_probe(&mut self, now: Instant, congested: bool) -> Option<ProbeDecision> {
        if self.application_limited || congested {
            return None;
        }
        if self
            .last_probe
            .is_some_and(|probe| now.saturating_duration_since(probe) < PROBE_INTERVAL)
        {
            return None;
        }
        self.last_probe = Some(now);
        Some(ProbeDecision {
            target_bitrate_bps: self.bitrate_bps.saturating_mul(2).min(MAX_BITRATE_BPS),
            packet_count: 5,
        })
    }
}

pub fn parse_twcc(rtcp: &CompoundRtcpView<'_>) -> Result<Vec<TwccFeedback>, GccError> {
    let mut feedback = Vec::new();
    for packet in rtcp.packets() {
        if packet.packet_type() == TWCC_PACKET_TYPE && packet.report_count() == TWCC_FORMAT {
            feedback.push(parse_twcc_packet(packet.bytes())?);
        }
    }
    Ok(feedback)
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_wrap,
    reason = "each TWCC field is obtained from a bounds-checked structural slice"
)]
fn parse_twcc_packet(bytes: &[u8]) -> Result<TwccFeedback, GccError> {
    let fixed = bytes.get(..20).ok_or(GccError::MalformedTwcc)?;
    let base_sequence = u16::from_be_bytes([fixed[12], fixed[13]]);
    let status_count = usize::from(u16::from_be_bytes([fixed[14], fixed[15]]));
    let reference_time = u32::from_be_bytes([0, fixed[16], fixed[17], fixed[18]]);
    let mut offset = 20usize;
    let mut symbols = Vec::with_capacity(status_count);
    while symbols.len() < status_count {
        let chunk = bytes
            .get(offset..offset.saturating_add(2))
            .ok_or(GccError::MalformedTwcc)?;
        offset = offset.saturating_add(2);
        let chunk = u16::from_be_bytes([chunk[0], chunk[1]]);
        if chunk & 0x8000 == 0 {
            let symbol = ((chunk >> 13) & 0x03) as u8;
            let run = usize::from(chunk & 0x1fff);
            if run == 0 || symbols.len().saturating_add(run) > status_count {
                return Err(GccError::MalformedTwcc);
            }
            symbols.extend(std::iter::repeat_n(symbol, run));
        } else if chunk & 0x4000 == 0 {
            for shift in (0..14).rev() {
                if symbols.len() == status_count {
                    break;
                }
                symbols.push(((chunk >> shift) & 1) as u8);
            }
        } else {
            for shift in (0..7).rev() {
                if symbols.len() == status_count {
                    break;
                }
                symbols.push(((chunk >> (shift * 2)) & 3) as u8);
            }
        }
    }
    let mut received_at = Duration::from_micros(u64::from(reference_time).saturating_mul(64_000));
    let mut statuses = Vec::with_capacity(status_count);
    for (index, symbol) in symbols.into_iter().enumerate() {
        let sequence =
            base_sequence.wrapping_add(u16::try_from(index).map_err(|_| GccError::MalformedTwcc)?);
        let received = match symbol {
            0 => None,
            1 => {
                let delta = *bytes.get(offset).ok_or(GccError::MalformedTwcc)? as i8;
                offset = offset.saturating_add(1);
                received_at = apply_delta(received_at, i64::from(delta).saturating_mul(250));
                Some(received_at)
            }
            2 => {
                let delta = bytes
                    .get(offset..offset.saturating_add(2))
                    .ok_or(GccError::MalformedTwcc)?;
                offset = offset.saturating_add(2);
                let delta = i16::from_be_bytes([delta[0], delta[1]]);
                received_at = apply_delta(received_at, i64::from(delta).saturating_mul(250));
                Some(received_at)
            }
            _ => return Err(GccError::MalformedTwcc),
        };
        statuses.push(TwccStatus {
            sequence,
            received_at: received,
        });
    }
    if offset > bytes.len() {
        return Err(GccError::MalformedTwcc);
    }
    Ok(TwccFeedback {
        statuses: statuses.into_boxed_slice(),
    })
}

fn apply_delta(at: Duration, delta_micros: i64) -> Duration {
    if delta_micros >= 0 {
        at.saturating_add(Duration::from_micros(delta_micros as u64))
    } else {
        at.saturating_sub(Duration::from_micros(delta_micros.unsigned_abs()))
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use super::*;
    use crate::{IngressPacket, PacketId, PacketProvenance, TransportMetadata, TransportProtocol};

    fn feedback(statuses: &[(u16, Option<Duration>)]) -> TwccFeedback {
        TwccFeedback {
            statuses: statuses
                .iter()
                .map(|(sequence, received_at)| TwccStatus {
                    sequence: *sequence,
                    received_at: *received_at,
                })
                .collect(),
        }
    }

    #[test]
    fn gcc_uses_authoritative_departures_and_ignores_duplicates() {
        let now = Instant::now();
        let mut gcc = Gcc::new(8);
        let first = gcc.assign(SendId::new(1), 1200).expect("first send");
        let second = gcc.assign(SendId::new(2), 1200).expect("second send");
        gcc.record_departure(first.send_id(), now)
            .expect("first departure");
        gcc.record_departure(second.send_id(), now + Duration::from_millis(10))
            .expect("second departure");
        let report = feedback(&[
            (first.transport_sequence(), Some(Duration::from_millis(1))),
            (second.transport_sequence(), Some(Duration::from_millis(11))),
        ]);

        let first_outcome = gcc.process_feedback(now + Duration::from_millis(20), &report);
        let duplicate = gcc.process_feedback(now + Duration::from_millis(30), &report);

        assert_eq!(first_outcome.acknowledged(), 2);
        assert_eq!(duplicate.acknowledged(), 0);
        assert_eq!(duplicate.lost(), 0);
    }

    #[test]
    fn gcc_reduces_on_loss_and_recovers_after_outage() {
        let now = Instant::now();
        let mut gcc = Gcc::new(8);
        let first = gcc.assign(SendId::new(1), 1200).expect("send");
        gcc.record_departure(first.send_id(), now)
            .expect("departure");
        let before = gcc.estimate(now).bitrate_bps();
        let outcome = gcc.process_feedback(
            now + Duration::from_millis(10),
            &feedback(&[(first.transport_sequence(), None)]),
        );

        assert!(outcome.estimate().bitrate_bps() < before);
        gcc.handle_timeout(now + OUTAGE + Duration::from_secs(1));
        assert!(
            gcc.estimate(now + OUTAGE + Duration::from_secs(1))
                .application_limited()
        );
    }

    #[test]
    fn gcc_accepts_a_policy_selected_initial_bitrate() {
        let now = Instant::now();
        let gcc = Gcc::with_initial_bitrate(8, 2_000_000);

        assert_eq!(gcc.estimate(now).bitrate_bps(), 2_000_000);
    }

    #[test]
    fn gcc_promotes_its_first_clean_throughput_sample() {
        let now = Instant::now();
        let mut gcc = Gcc::new(8);
        let first = gcc.assign(SendId::new(1), 1200).expect("first send");
        let second = gcc.assign(SendId::new(2), 1200).expect("second send");
        gcc.record_departure(first.send_id(), now)
            .expect("first departure");
        gcc.record_departure(second.send_id(), now + Duration::from_millis(10))
            .expect("second departure");

        let outcome = gcc.process_feedback(
            now + Duration::from_millis(20),
            &feedback(&[
                (first.transport_sequence(), Some(Duration::from_millis(1))),
                (second.transport_sequence(), Some(Duration::from_millis(11))),
            ]),
        );

        assert!(outcome.estimate().bitrate_bps() > INITIAL_BITRATE_BPS);
    }

    #[test]
    fn gcc_does_not_treat_a_reordered_feedback_gap_as_congestion() {
        let now = Instant::now();
        let mut gcc = Gcc::new(8);
        let mut sequences = Vec::new();
        for index in 0..5 {
            let send = gcc
                .assign(SendId::new(index), 1200)
                .expect("unique send identity");
            gcc.record_departure(
                send.send_id(),
                now + Duration::from_millis(u64::from(index) * 10),
            )
            .expect("authoritative departure");
            sequences.push(send.transport_sequence());
        }

        let outcome = gcc.process_feedback(
            now + Duration::from_millis(60),
            &feedback(&[
                (sequences[0], Some(Duration::from_millis(1))),
                (sequences[1], Some(Duration::from_millis(11))),
                (sequences[2], None),
                (sequences[3], Some(Duration::from_millis(31))),
                (sequences[4], Some(Duration::from_millis(41))),
            ]),
        );

        assert_eq!(outcome.lost(), 1);
        assert!(outcome.estimate().bitrate_bps() >= INITIAL_BITRATE_BPS);
        assert!(outcome.probe().is_some());
    }

    #[test]
    fn gcc_parses_twcc_from_structural_rtcp_view() {
        let source = SocketAddr::from(([127, 0, 0, 1], 5000));
        let destination = SocketAddr::from(([127, 0, 0, 1], 6000));
        let bytes = [
            0x8f, 205, 0, 5, 0, 0, 0, 1, 0, 0, 0, 2, 0, 7, 0, 2, 0, 0, 0, 0, 0x20, 0x02, 1, 2,
        ];
        let packet = IngressPacket::new(
            &bytes,
            PacketProvenance::new(
                Instant::now(),
                TransportMetadata::new(TransportProtocol::Udp, source, destination),
                PacketId::new(1),
            ),
        )
        .parse()
        .expect("RTCP packet");
        let crate::PacketView::Rtcp(rtcp) = packet else {
            panic!("RTCP packet");
        };

        let parsed = parse_twcc(&rtcp).expect("TWCC feedback");

        assert_eq!(parsed[0].statuses()[0].sequence(), 7);
        assert_eq!(parsed[0].statuses()[1].sequence(), 8);
    }

    #[test]
    fn gcc_handles_capacity_change_reordered_and_unrecorded_feedback() {
        let now = Instant::now();
        let mut gcc = Gcc::new(8);
        let first = gcc.assign(SendId::new(1), 1200).expect("first send");
        let second = gcc.assign(SendId::new(2), 1200).expect("second send");
        let pending = gcc.assign(SendId::new(3), 1200).expect("pending send");
        gcc.record_departure(first.send_id(), now)
            .expect("first departure");
        gcc.record_departure(second.send_id(), now + Duration::from_millis(10))
            .expect("second departure");
        let increased = gcc.process_feedback(
            now + Duration::from_millis(20),
            &feedback(&[
                (second.transport_sequence(), Some(Duration::from_millis(11))),
                (first.transport_sequence(), Some(Duration::from_millis(1))),
                (
                    pending.transport_sequence(),
                    Some(Duration::from_millis(12)),
                ),
                (u16::MAX, Some(Duration::from_millis(12))),
            ]),
        );
        let before_delay = increased.estimate().bitrate_bps();
        let third = gcc.assign(SendId::new(4), 1200).expect("third send");
        let fourth = gcc.assign(SendId::new(5), 1200).expect("fourth send");
        gcc.record_departure(third.send_id(), now + Duration::from_millis(30))
            .expect("third departure");
        gcc.record_departure(fourth.send_id(), now + Duration::from_millis(40))
            .expect("fourth departure");
        let delayed = gcc.process_feedback(
            now + Duration::from_millis(100),
            &feedback(&[
                (third.transport_sequence(), Some(Duration::from_millis(30))),
                (fourth.transport_sequence(), Some(Duration::from_millis(90))),
            ]),
        );

        assert_eq!(increased.acknowledged(), 2);
        assert!(increased.probe().is_some());
        assert!(delayed.estimate().bitrate_bps() < before_delay);
    }
}

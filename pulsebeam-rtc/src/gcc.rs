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
const TWCC_FEEDBACK_INTERVAL: Duration = Duration::from_millis(50);
const TWCC_HISTORY_CAPACITY: usize = 8192;
const TWCC_MAX_STATUS_COUNT: usize = 512;

pub(crate) struct TwccReceiver {
    epoch: Instant,
    base_sequence: Option<u64>,
    highest_sequence: Option<u64>,
    received: VecDeque<Option<Instant>>,
    next_feedback: Option<Instant>,
    feedback_count: u8,
    media_ssrc: Option<u32>,
    symbols: Vec<u8>,
    deltas: Vec<i16>,
    encoded: Vec<u8>,
}

impl TwccReceiver {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            epoch: now,
            base_sequence: None,
            highest_sequence: None,
            received: VecDeque::with_capacity(TWCC_HISTORY_CAPACITY),
            next_feedback: None,
            feedback_count: 0,
            media_ssrc: None,
            symbols: Vec::with_capacity(TWCC_MAX_STATUS_COUNT),
            deltas: Vec::with_capacity(TWCC_MAX_STATUS_COUNT),
            encoded: Vec::with_capacity(1200),
        }
    }

    pub(crate) fn observe(&mut self, sequence: u16, received_at: Instant, media_ssrc: u32) {
        self.media_ssrc = Some(media_ssrc);
        let sequence = self.extend_sequence(sequence);
        let Some(base) = self.base_sequence else {
            self.base_sequence = Some(sequence);
            self.highest_sequence = Some(sequence);
            self.received.push_back(Some(received_at));
            self.next_feedback = received_at.checked_add(TWCC_FEEDBACK_INTERVAL);
            return;
        };
        if sequence < base {
            return;
        }
        let offset = sequence.saturating_sub(base);
        let Ok(offset) = usize::try_from(offset) else {
            self.reset(sequence, received_at);
            return;
        };
        if offset >= TWCC_HISTORY_CAPACITY {
            self.reset(sequence, received_at);
            return;
        }
        while self.received.len() <= offset {
            self.received.push_back(None);
        }
        let Some(slot) = self.received.get_mut(offset) else {
            debug_assert!(
                false,
                "the bounded TWCC receive history indexes its packet range"
            );
            return;
        };
        if slot.is_some() {
            return;
        }
        *slot = Some(received_at);
        self.highest_sequence = Some(self.highest_sequence.unwrap_or(sequence).max(sequence));
        if self.next_feedback.is_none() {
            self.next_feedback = received_at.checked_add(TWCC_FEEDBACK_INTERVAL);
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.next_feedback
    }

    pub(crate) fn build_feedback(&mut self, now: Instant, sender_ssrc: u32) -> Option<&[u8]> {
        if self.next_feedback.is_some_and(|deadline| now < deadline) {
            return None;
        }
        let first_received = self.received.iter().position(Option::is_some)?;
        if first_received > 0 {
            self.discard(first_received);
        }
        let count = self.received.len().min(TWCC_MAX_STATUS_COUNT);
        if count == 0 {
            self.next_feedback = None;
            return None;
        }
        let first_at = self.received.front().and_then(|received| *received)?;
        let reference_ticks = self.micros_since_epoch(first_at).saturating_div(64_000);
        let reference_at = self.epoch.checked_add(Duration::from_micros(
            reference_ticks.saturating_mul(64_000),
        ))?;
        self.symbols.clear();
        self.deltas.clear();
        let mut previous = reference_at;
        for received_at in self.received.iter().take(count) {
            let Some(received_at) = received_at else {
                self.symbols.push(0);
                continue;
            };
            let delta = signed_delta_250us(*received_at, previous)?;
            let symbol = if (0..=255).contains(&delta) { 1 } else { 2 };
            self.symbols.push(symbol);
            self.deltas.push(delta);
            previous = *received_at;
        }
        self.encoded.clear();
        self.encoded.resize(20, 0);
        self.encoded[0] = 0x8f;
        self.encoded[1] = TWCC_PACKET_TYPE;
        self.encoded[4..8].copy_from_slice(&sender_ssrc.to_be_bytes());
        self.encoded[8..12].copy_from_slice(&self.media_ssrc?.to_be_bytes());
        let base = self.base_sequence?;
        self.encoded[12..14].copy_from_slice(&(base as u16).to_be_bytes());
        self.encoded[14..16].copy_from_slice(
            &u16::try_from(count)
                .expect("bounded TWCC feedback status count fits a u16")
                .to_be_bytes(),
        );
        self.encoded[16] = u8::try_from((reference_ticks >> 16) & 0xff).ok()?;
        self.encoded[17] = u8::try_from((reference_ticks >> 8) & 0xff).ok()?;
        self.encoded[18] = u8::try_from(reference_ticks & 0xff).ok()?;
        self.encoded[19] = self.feedback_count;
        self.feedback_count = self.feedback_count.wrapping_add(1);
        for symbols in self.symbols.chunks(7) {
            let mut chunk = 0xc000u16;
            for (index, symbol) in symbols.iter().enumerate() {
                let shift = 12u32.saturating_sub(u32::try_from(index).ok()?.saturating_mul(2));
                chunk |= u16::from(*symbol) << shift;
            }
            self.encoded.extend_from_slice(&chunk.to_be_bytes());
        }
        let mut deltas = self.deltas.iter();
        for symbol in &self.symbols {
            match *symbol {
                0 => {}
                1 => self.encoded.push(u8::try_from(*deltas.next()?).ok()?),
                2 => self
                    .encoded
                    .extend_from_slice(&deltas.next()?.to_be_bytes()),
                _ => {
                    debug_assert!(false, "TWCC status symbols are generated locally");
                    return None;
                }
            }
        }
        debug_assert!(deltas.next().is_none());
        while !self.encoded.len().is_multiple_of(4) {
            self.encoded.push(0);
        }
        let words = self.encoded.len().checked_div(4)?;
        let length = u16::try_from(words.checked_sub(1)?).ok()?;
        self.encoded[2..4].copy_from_slice(&length.to_be_bytes());
        self.discard(count);
        self.next_feedback = if self.received.is_empty() {
            None
        } else {
            now.checked_add(TWCC_FEEDBACK_INTERVAL)
        };
        Some(&self.encoded)
    }

    fn reset(&mut self, sequence: u64, received_at: Instant) {
        self.base_sequence = Some(sequence);
        self.highest_sequence = Some(sequence);
        self.received.clear();
        self.received.push_back(Some(received_at));
        self.next_feedback = received_at.checked_add(TWCC_FEEDBACK_INTERVAL);
    }

    fn discard(&mut self, count: usize) {
        let count = count.min(self.received.len());
        self.received.drain(..count);
        self.base_sequence = self
            .base_sequence
            .map(|base| base.saturating_add(u64::try_from(count).unwrap_or(u64::MAX)));
        if self.received.is_empty() {
            self.base_sequence = None;
            self.highest_sequence = None;
            self.media_ssrc = None;
        }
    }

    fn extend_sequence(&self, sequence: u16) -> u64 {
        let Some(highest) = self.highest_sequence else {
            return u64::from(sequence);
        };
        let highest_low = highest as u16;
        let rollover = highest >> 16;
        let rollover = if sequence < highest_low
            && highest_low.wrapping_sub(sequence) > (u16::MAX / 2)
        {
            rollover.saturating_add(1)
        } else if sequence > highest_low && sequence.wrapping_sub(highest_low) > (u16::MAX / 2) {
            rollover.saturating_sub(1)
        } else {
            rollover
        };
        (rollover << 16) | u64::from(sequence)
    }

    fn micros_since_epoch(&self, instant: Instant) -> u64 {
        u64::try_from(instant.saturating_duration_since(self.epoch).as_micros()).unwrap_or(u64::MAX)
    }
}

fn signed_delta_250us(received_at: Instant, previous: Instant) -> Option<i16> {
    let micros = if received_at >= previous {
        i64::try_from(received_at.duration_since(previous).as_micros()).ok()?
    } else {
        -i64::try_from(previous.duration_since(received_at).as_micros()).ok()?
    };
    let units = micros / 250;
    i16::try_from(units).ok()
}

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
    last_outage_decay: Option<Instant>,
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
            last_outage_decay: None,
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
            self.last_outage_decay = None;
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

    pub fn next_deadline(&self) -> Option<Instant> {
        let feedback = self.last_feedback?;
        let base = self.last_outage_decay.unwrap_or(feedback);
        base.checked_add(OUTAGE)
    }

    pub fn handle_timeout(&mut self, now: Instant) -> Option<GccOutcome> {
        let feedback = self.last_feedback?;
        let first_decay = feedback.checked_add(OUTAGE).unwrap_or(feedback);
        let due = self
            .last_outage_decay
            .and_then(|decay| decay.checked_add(OUTAGE))
            .unwrap_or(first_decay);
        if now < due {
            return None;
        }
        self.bitrate_bps = self
            .bitrate_bps
            .saturating_mul(4)
            .saturating_div(5)
            .max(MIN_BITRATE_BPS);
        self.application_limited = true;
        self.last_outage_decay = Some(now);
        Some(GccOutcome {
            estimate: self.estimate(now),
            acknowledged: 0,
            lost: 0,
            probe: None,
        })
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
                } else if received > departed.saturating_add(Duration::from_millis(50)) {
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
    fn gcc_schedules_one_decay_per_feedback_outage() {
        let now = Instant::now();
        let mut gcc = Gcc::new(8);
        let send = gcc.assign(SendId::new(1), 1200).expect("send");
        gcc.record_departure(send.send_id(), now)
            .expect("departure");
        let feedback_at = now + Duration::from_millis(10);
        gcc.process_feedback(
            feedback_at,
            &feedback(&[(send.transport_sequence(), Some(Duration::from_millis(1)))]),
        );

        let first_deadline = feedback_at + OUTAGE;
        assert_eq!(gcc.next_deadline(), Some(first_deadline));
        assert!(
            gcc.handle_timeout(first_deadline - Duration::from_nanos(1))
                .is_none()
        );

        let first = gcc.handle_timeout(first_deadline).expect("outage decay");
        assert!(first.estimate().application_limited());
        assert!(first.estimate().bitrate_bps() < INITIAL_BITRATE_BPS);
        assert_eq!(gcc.next_deadline(), Some(first_deadline + OUTAGE));
        assert!(
            gcc.handle_timeout(first_deadline + Duration::from_millis(1))
                .is_none()
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
            gcc.record_departure(send.send_id(), now + Duration::from_millis(index * 10))
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
    fn twcc_receiver_reports_reordered_packets_and_loss() {
        let now = Instant::now();
        let mut receiver = TwccReceiver::new(now);
        receiver.observe(20, now, 9);
        receiver.observe(22, now + Duration::from_millis(10), 9);
        receiver.observe(21, now + Duration::from_millis(5), 9);
        let bytes = receiver
            .build_feedback(now + TWCC_FEEDBACK_INTERVAL, 7)
            .expect("due TWCC feedback")
            .to_vec();
        let source = SocketAddr::from(([127, 0, 0, 1], 5000));
        let destination = SocketAddr::from(([127, 0, 0, 1], 6000));
        let packet = IngressPacket::new(
            &bytes,
            PacketProvenance::new(
                now,
                TransportMetadata::new(TransportProtocol::Udp, source, destination),
                PacketId::new(1),
            ),
        )
        .parse()
        .expect("generated RTCP");
        let crate::PacketView::Rtcp(rtcp) = packet else {
            panic!("generated TWCC must be RTCP");
        };
        let report = parse_twcc(&rtcp).expect("generated TWCC is structurally valid");
        let statuses = report[0].statuses();

        assert_eq!(statuses.len(), 3);
        assert_eq!(statuses[0].sequence(), 20);
        assert_eq!(statuses[1].sequence(), 21);
        assert_eq!(statuses[2].sequence(), 22);
        assert!(statuses.iter().all(|status| status.received_at().is_some()));

        receiver.observe(23, now + Duration::from_millis(60), 9);
        receiver.observe(25, now + Duration::from_millis(70), 9);
        let bytes = receiver
            .build_feedback(now + Duration::from_millis(110), 7)
            .expect("second TWCC feedback")
            .to_vec();
        let packet = IngressPacket::new(
            &bytes,
            PacketProvenance::new(
                now,
                TransportMetadata::new(TransportProtocol::Udp, source, destination),
                PacketId::new(2),
            ),
        )
        .parse()
        .expect("generated RTCP");
        let crate::PacketView::Rtcp(rtcp) = packet else {
            panic!("generated TWCC must be RTCP");
        };
        let report = parse_twcc(&rtcp).expect("generated TWCC is structurally valid");
        let statuses = report[0].statuses();

        assert_eq!(statuses.len(), 3);
        assert_eq!(statuses[0].sequence(), 23);
        assert!(statuses[0].received_at().is_some());
        assert_eq!(statuses[1].sequence(), 24);
        assert!(statuses[1].received_at().is_none());
        assert_eq!(statuses[2].sequence(), 25);
        assert!(statuses[2].received_at().is_some());
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
                (
                    fourth.transport_sequence(),
                    Some(Duration::from_millis(100)),
                ),
            ]),
        );

        assert_eq!(increased.acknowledged(), 2);
        assert!(increased.probe().is_some());
        assert!(delayed.estimate().bitrate_bps() < before_delay);
    }
}

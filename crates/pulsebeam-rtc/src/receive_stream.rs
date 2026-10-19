#![allow(
    clippy::cast_possible_truncation,
    reason = "RTP sequence cycles and RTCP wire counters deliberately retain their low bits"
)]

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const REORDER_WINDOW: usize = 256;
const NACK_DELAY: Duration = Duration::from_millis(10);
const NACK_INTERVAL: Duration = Duration::from_millis(50);
const REPORT_INTERVAL: Duration = Duration::from_secs(1);

struct Missing {
    sequence: u64,
    next: Instant,
    attempts: u8,
}

/// Primary RTP receive state, shared by originals and normalized RTX.
pub(crate) struct ReceiveStream {
    base: Option<u64>,
    highest: u64,
    seen: [u64; REORDER_WINDOW],
    received: u64,
    missing: VecDeque<Missing>,
    nack: bool,
    clock_rate: u32,
    previous_arrival: Option<(Instant, u32)>,
    jitter_q4: u64,
    next_report: Option<Instant>,
    expected_prior: u64,
    received_prior: u64,
    last_sr: Option<(u32, Instant)>,
}

impl ReceiveStream {
    pub(crate) fn new(clock_rate: u32, nack: bool) -> Self {
        Self {
            base: None,
            highest: 0,
            seen: [u64::MAX; REORDER_WINDOW],
            received: 0,
            missing: VecDeque::new(),
            nack,
            clock_rate,
            previous_arrival: None,
            jitter_q4: 0,
            next_report: None,
            expected_prior: 0,
            received_prior: 0,
            last_sr: None,
        }
    }

    pub(crate) fn accept(
        &mut self,
        sequence: u16,
        timestamp: u32,
        arrival: Instant,
        repair: bool,
    ) -> bool {
        let extended = if self.base.is_some() {
            let delta =
                i16::from_be_bytes(sequence.wrapping_sub(self.highest as u16).to_be_bytes());
            let Some(extended) = self.highest.checked_add_signed(i64::from(delta)) else {
                return false;
            };
            extended
        } else {
            u64::from(sequence)
        };
        if self.highest.saturating_sub(extended) >= REORDER_WINDOW as u64 {
            return false;
        }
        let index = usize::from(sequence) % REORDER_WINDOW;
        if self.seen.get(index) == Some(&extended) {
            return false;
        }
        if let Some(slot) = self.seen.get_mut(index) {
            *slot = extended;
        }
        self.missing.retain(|missing| {
            missing.sequence != extended
                && extended.saturating_sub(missing.sequence) < REORDER_WINDOW as u64
        });
        if self.base.is_some() && extended > self.highest && self.nack {
            let first = self
                .highest
                .saturating_add(1)
                .max(extended.saturating_sub(REORDER_WINDOW as u64));
            for sequence in first..extended {
                if self.missing.len() == REORDER_WINDOW {
                    self.missing.pop_front();
                }
                self.missing.push_back(Missing {
                    sequence,
                    next: arrival.checked_add(NACK_DELAY).unwrap_or(arrival),
                    attempts: 0,
                });
            }
        }
        if !repair && (self.base.is_none() || extended > self.highest) {
            if let Some((previous, previous_timestamp)) = self.previous_arrival {
                let elapsed = arrival
                    .saturating_duration_since(previous)
                    .as_micros()
                    .saturating_mul(u128::from(self.clock_rate))
                    / 1_000_000;
                let elapsed = i128::try_from(elapsed).unwrap_or(i128::MAX);
                let ticks =
                    i32::from_be_bytes(timestamp.wrapping_sub(previous_timestamp).to_be_bytes());
                let difference = elapsed.saturating_sub(i128::from(ticks)).unsigned_abs();
                let difference = u64::try_from(difference).unwrap_or(u64::MAX);
                self.jitter_q4 = self
                    .jitter_q4
                    .saturating_add(difference)
                    .saturating_sub(self.jitter_q4.saturating_add(8) / 16);
            }
            self.previous_arrival = Some((arrival, timestamp));
        }
        self.base = Some(self.base.map_or(extended, |base| base.min(extended)));
        self.highest = self.highest.max(extended);
        self.received = self.received.saturating_add(1);
        self.next_report
            .get_or_insert_with(|| arrival.checked_add(REPORT_INTERVAL).unwrap_or(arrival));
        true
    }

    pub(crate) const fn received(&self) -> u64 {
        self.received
    }

    pub(crate) fn sender_report(&mut self, seconds: u32, fraction: u32, arrival: Instant) {
        self.last_sr = Some(((seconds << 16) | (fraction >> 16), arrival));
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.missing
            .iter()
            .map(|missing| missing.next)
            .chain(self.next_report)
            .min()
    }

    pub(crate) fn poll_control(
        &mut self,
        now: Instant,
        receiver_ssrc: u32,
        source_ssrc: u32,
    ) -> Option<Vec<u8>> {
        if let Some(index) = self.missing.iter().position(|missing| missing.next <= now) {
            let missing = self.missing.get_mut(index)?;
            let sequence = missing.sequence as u16;
            missing.attempts = missing.attempts.saturating_add(1);
            missing.next = now.checked_add(NACK_INTERVAL).unwrap_or(now);
            if missing.attempts >= 5 {
                self.missing.remove(index);
            }
            let mut packet = vec![0x81, 205, 0, 3];
            packet.extend_from_slice(&receiver_ssrc.to_be_bytes());
            packet.extend_from_slice(&source_ssrc.to_be_bytes());
            packet.extend_from_slice(&sequence.to_be_bytes());
            packet.extend_from_slice(&0_u16.to_be_bytes());
            return Some(packet);
        }
        if self.next_report.is_none_or(|deadline| deadline > now) {
            return None;
        }
        self.next_report = Some(now.checked_add(REPORT_INTERVAL).unwrap_or(now));
        let expected = self.highest.saturating_sub(self.base?).saturating_add(1);
        let lost = expected.saturating_sub(self.received).min(0x7f_ffff);
        let interval_expected = expected.saturating_sub(self.expected_prior);
        let interval_lost =
            interval_expected.saturating_sub(self.received.saturating_sub(self.received_prior));
        self.expected_prior = expected;
        self.received_prior = self.received;
        let fraction = interval_lost
            .saturating_mul(256)
            .checked_div(interval_expected)
            .unwrap_or(0)
            .min(255) as u8;
        let (last_sr, delay) = self.last_sr.map_or((0, 0), |(stamp, arrival)| {
            let ticks = now
                .saturating_duration_since(arrival)
                .as_micros()
                .saturating_mul(65_536)
                / 1_000_000;
            (stamp, u32::try_from(ticks).unwrap_or(u32::MAX))
        });
        let mut packet = vec![0x81, 201, 0, 7];
        packet.extend_from_slice(&receiver_ssrc.to_be_bytes());
        packet.extend_from_slice(&source_ssrc.to_be_bytes());
        packet.push(fraction);
        let [_, high, middle, low] = (lost as u32).to_be_bytes();
        packet.extend_from_slice(&[high, middle, low]);
        packet.extend_from_slice(&(self.highest as u32).to_be_bytes());
        packet.extend_from_slice(
            &u32::try_from(self.jitter_q4 / 16)
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        packet.extend_from_slice(&last_sr.to_be_bytes());
        packet.extend_from_slice(&delay.to_be_bytes());
        packet.extend_from_slice(&[0x81, 202, 0, 3]);
        packet.extend_from_slice(&receiver_ssrc.to_be_bytes());
        packet.extend_from_slice(&[1, 3, b'r', b't', b'c', 0, 0, 0]);
        Some(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::RtcpCompound;

    #[test]
    fn reorder_repair_and_wrap_share_one_bounded_primary_history() {
        let now = Instant::now();
        let mut stream = ReceiveStream::new(90_000, true);
        assert!(stream.accept(65_534, 0, now, false));
        assert!(stream.accept(1, 3_000, now + Duration::from_millis(33), false));
        assert!(stream.accept(0, 2_000, now + Duration::from_millis(34), false));
        let nack = stream
            .poll_control(now + Duration::from_millis(50), 9, 7)
            .unwrap();
        let nack = RtcpCompound::parse(&nack)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .nack()
            .unwrap()
            .unwrap();
        assert_eq!(nack.media_ssrc(), 7);
        assert_eq!(nack.pairs().collect::<Vec<_>>(), vec![(65_535, 0)]);
        assert!(stream.accept(65_535, 1_000, now + Duration::from_millis(51), true));
        assert!(!stream.accept(65_535, 1_000, now + Duration::from_millis(52), false));
        assert!(!stream.accept(1, 3_000, now + Duration::from_millis(52), true));
        assert_eq!(stream.received(), 4);
        assert!(
            stream
                .poll_control(now + Duration::from_millis(200), 9, 7)
                .is_none()
        );
        stream.sender_report(12, 0x3456_7890, now);
        let report = stream.poll_control(now + REPORT_INTERVAL, 9, 7).unwrap();
        let report = RtcpCompound::parse(&report)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .receiver_report()
            .unwrap()
            .unwrap();
        let block = report.reports().next().unwrap();
        assert_eq!(block.highest_sequence(), 65_537);
        assert_eq!(block.cumulative_lost(), 0);
        assert_eq!(block.last_sender_report(), 0x000c_3456);
        assert_eq!(block.delay_since_last_sender_report(), 65_536);
    }

    #[test]
    fn gaps_retries_and_ancient_duplicates_remain_bounded() {
        let now = Instant::now();
        let mut stream = ReceiveStream::new(48_000, true);
        assert!(stream.accept(1, 0, now, false));
        assert!(stream.accept(20_000, 0, now, false));
        assert_eq!(stream.missing.len(), REORDER_WINDOW);
        assert!(!stream.accept(1, 0, now, false));
        for retry in 0..5 {
            let time = now + NACK_DELAY + NACK_INTERVAL * retry;
            for _ in 0..REORDER_WINDOW {
                assert!(stream.poll_control(time, 9, 7).is_some());
            }
        }
        assert!(stream.missing.is_empty());
        assert!(
            stream
                .poll_control(now + Duration::from_millis(500), 9, 7)
                .is_none()
        );
    }
}

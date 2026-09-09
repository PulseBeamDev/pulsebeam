#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    reason = "the bounded RTP sequence/timestamp unwrap deliberately uses wrapping wire arithmetic"
)]

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::GlobalMediaTime;

pub(crate) const REORDER_WINDOW: i64 = 2_048;
const MAX_RESIDUAL_US: i128 = 100_000;
const MAX_NTP_REGRESSION_US: i128 = 1_000;
const STALE_AFTER: Duration = Duration::from_secs(10);
const DISCONTINUITY_US: i128 = 500_000;
const MAX_SEGMENTS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClockWarning {
    Synchronized,
    Stale,
    Discontinuous,
}

#[derive(Clone, Copy)]
struct Segment {
    rtp: i64,
    global: i128,
}

#[derive(Clone, Copy)]
struct Report {
    rtp: i64,
    arrival: Instant,
}

pub(crate) struct ClockMapper {
    rate: i64,
    first_timestamp: u32,
    frontier_sequence: i64,
    frontier_rtp: i64,
    segments: VecDeque<Segment>,
    reports: VecDeque<Report>,
    last_sr: Option<Instant>,
    last_ntp: Option<i128>,
    synchronized: bool,
    collapsed_segments: u64,
}

impl ClockMapper {
    pub(crate) fn new(global: GlobalMediaTime, sequence: u16, timestamp: u32, rate: u32) -> Self {
        Self {
            rate: i64::from(rate),
            first_timestamp: timestamp,
            frontier_sequence: i64::from(sequence),
            frontier_rtp: 0,
            segments: [Segment {
                rtp: 0,
                global: i128::from(global.as_micros()),
            }]
            .into(),
            reports: VecDeque::new(),
            last_sr: None,
            last_ntp: None,
            synchronized: false,
            collapsed_segments: 0,
        }
    }

    pub(crate) fn map(
        &mut self,
        sequence: u16,
        rtp: u32,
        now: Instant,
    ) -> Option<(GlobalMediaTime, Option<ClockWarning>)> {
        let sequence = self.frontier_sequence
            + i64::from(i16::from_be_bytes(
                sequence
                    .wrapping_sub(self.frontier_sequence as u16)
                    .to_be_bytes(),
            ));
        if sequence < self.frontier_sequence - REORDER_WINDOW {
            return None;
        }
        let rtp = self.unwrap_rtp(rtp);
        if sequence > self.frontier_sequence {
            self.frontier_sequence = sequence;
            self.frontier_rtp = rtp;
        }
        let warning = self.stale_if_needed(now);
        self.map_unwrapped(rtp).map(|time| (time, warning))
    }

    pub(crate) fn relation_at(&self, rtp: u32, ntp: i128) -> Option<i128> {
        self.map_unwrapped_signed(self.unwrap_rtp(rtp))?
            .checked_sub(ntp)
    }

    pub(crate) fn observe_sender_report(
        &mut self,
        rtp: u32,
        ntp: i128,
        arrival: Instant,
        smoothed_rtt: Option<Duration>,
        group_offset: i128,
    ) -> Option<ClockWarning> {
        let rtp = self.unwrap_rtp(rtp);
        if self
            .last_ntp
            .is_some_and(|previous| ntp < previous - MAX_NTP_REGRESSION_US)
        {
            return self.start_segment(rtp, self.next_global(rtp));
        }
        let predicted = self.map_unwrapped_signed(rtp)?;
        let target = ntp.checked_add(group_offset)?;
        let residual = target.checked_sub(predicted)?;
        self.last_ntp = Some(ntp);
        self.last_sr = Some(arrival);
        if residual.unsigned_abs() > DISCONTINUITY_US.unsigned_abs() {
            return self.start_segment(rtp, self.next_global(rtp).max(target));
        }

        self.reports.push_back(Report { rtp, arrival });
        while self.reports.len() > 3 {
            self.reports.pop_front();
        }
        let correction = residual.clamp(-MAX_RESIDUAL_US, MAX_RESIDUAL_US);
        if correction != 0 {
            let oldest_rtp = self.reports.front().map_or(rtp, |report| report.rtp);
            let max_slew = i128::from((rtp - oldest_rtp).unsigned_abs())
                .saturating_mul(1_000)
                .checked_div(1_000_000)
                .unwrap_or(0)
                .max(1);
            if let Some(segment) = self.segments.back_mut() {
                segment.global = segment
                    .global
                    .saturating_add(correction.clamp(-max_slew, max_slew));
            }
        }

        let was_synchronized = self.synchronized;
        self.synchronized = self.reports.len() == 3
            && smoothed_rtt.is_some_and(|rtt| {
                self.reports
                    .back()
                    .zip(self.reports.front())
                    .is_some_and(|(last, first)| last.arrival.duration_since(first.arrival) >= rtt)
            });
        (!was_synchronized && self.synchronized).then_some(ClockWarning::Synchronized)
    }

    fn stale_if_needed(&mut self, now: Instant) -> Option<ClockWarning> {
        if self.synchronized
            && self
                .last_sr
                .is_some_and(|last| now.duration_since(last) >= STALE_AFTER)
        {
            self.synchronized = false;
            self.reports.clear();
            return Some(ClockWarning::Stale);
        }
        None
    }

    fn start_segment(&mut self, rtp: i64, global: i128) -> Option<ClockWarning> {
        self.segments.push_back(Segment { rtp, global });
        if self.segments.len() > MAX_SEGMENTS {
            self.segments.pop_front();
            self.collapsed_segments = self.collapsed_segments.saturating_add(1);
        }
        self.reports.clear();
        self.synchronized = false;
        Some(ClockWarning::Discontinuous)
    }

    fn unwrap_rtp(&self, rtp: u32) -> i64 {
        self.frontier_rtp
            + i64::from(i32::from_be_bytes(
                rtp.wrapping_sub(self.wrapped_frontier_rtp()).to_be_bytes(),
            ))
    }

    fn wrapped_frontier_rtp(&self) -> u32 {
        self.first_timestamp.wrapping_add(self.frontier_rtp as u32)
    }

    fn next_global(&self, rtp: i64) -> i128 {
        self.map_unwrapped_signed(rtp).unwrap_or(0)
    }

    fn map_unwrapped(&self, rtp: i64) -> Option<GlobalMediaTime> {
        u64::try_from(self.map_unwrapped_signed(rtp)?)
            .ok()
            .map(GlobalMediaTime::from_micros)
    }

    fn map_unwrapped_signed(&self, rtp: i64) -> Option<i128> {
        let segment = self.segments.back()?;
        i128::from(rtp.checked_sub(segment.rtp)?)
            .checked_mul(1_000_000)?
            .checked_div(i128::from(self.rate))?
            .checked_add(segment.global)
    }
}

pub(crate) fn ntp_micros(seconds: u32, fraction: u32) -> Option<i128> {
    i128::from(seconds).checked_mul(1_000_000)?.checked_add(
        i128::from(fraction)
            .checked_mul(1_000_000)?
            .checked_div(1_i128 << 32)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapper() -> ClockMapper {
        ClockMapper::new(GlobalMediaTime::from_micros(1_000), 10, 90_000, 90_000)
    }

    #[test]
    fn reorder_window_and_wrap_are_bounded() {
        let now = Instant::now();
        let mut mapper = mapper();
        assert_eq!(
            mapper.map(11, 180_000, now).unwrap().0.as_micros(),
            1_001_000
        );
        assert_eq!(mapper.map(10, 90_000, now).unwrap().0.as_micros(), 1_000);
        assert!(mapper.map(10_u16.wrapping_sub(2_049), 0, now).is_none());
    }

    #[test]
    fn three_reports_need_an_rtt_span() {
        let now = Instant::now();
        let mut mapper = mapper();
        let offset = mapper.relation_at(90_000, 5).unwrap();
        for second in 0..3_u64 {
            let warning = mapper.observe_sender_report(
                90_000 + u32::try_from(second).unwrap() * 90_000,
                5 + i128::from(second) * 1_000_000,
                now + Duration::from_millis(second * 50),
                Some(Duration::from_millis(100)),
                offset,
            );
            assert_eq!(warning, (second == 2).then_some(ClockWarning::Synchronized));
        }
    }

    #[test]
    fn stale_transition_is_exactly_ten_seconds() {
        let now = Instant::now();
        let mut mapper = mapper();
        let offset = mapper.relation_at(90_000, 5).unwrap();
        for second in 0..3_u64 {
            let _ = mapper.observe_sender_report(
                90_000 + u32::try_from(second).unwrap() * 90_000,
                5 + i128::from(second) * 1_000_000,
                now + Duration::from_secs(second),
                Some(Duration::ZERO),
                offset,
            );
        }
        assert_eq!(
            mapper
                .map(14, 360_000, now + Duration::from_secs(12))
                .unwrap()
                .1,
            Some(ClockWarning::Stale)
        );
        assert_eq!(
            mapper
                .map(15, 450_000, now + Duration::from_secs(13))
                .unwrap()
                .1,
            None
        );
    }
}

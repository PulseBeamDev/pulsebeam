use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::SenderReport;

pub const MAX_SENDER_REPORT_FUTURE: Duration = Duration::from_secs(2);
pub const MAX_SENDER_REPORT_AGE: Duration = Duration::from_secs(5);
pub const MAX_SENDER_REPORT_SAMPLE_INTERVAL: Duration = Duration::from_millis(100);
pub const MAX_SENDER_REPORT_RATE_ERROR_NUMERATOR: u128 = 120;
pub const MAX_SENDER_REPORT_RATE_ERROR_DENOMINATOR: u128 = 100;
pub const MAX_SENDER_REPORT_SLEW: Duration = Duration::from_millis(5);
pub const MAX_DISCONTINUITY: Duration = Duration::from_millis(250);
pub const DISCONTINUITY_CONFIRMATIONS: u8 = 3;

const NTP_UNIX_EPOCH_SECONDS: i128 = 2_208_988_800;
const NTP_ERA_SECONDS: i128 = 1_i128 << 32;
const MIN_RATE_PERCENT: u128 = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockError {
    BeforeAnchor,
    BeforeEpoch,
    Overflow,
    NonMonotonic,
    InvalidRate,
    InvalidSenderReport,
}

impl std::fmt::Display for ClockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::BeforeAnchor => "time precedes the clock anchor",
            Self::BeforeEpoch => "time precedes the Unix epoch",
            Self::Overflow => "time conversion overflowed",
            Self::NonMonotonic => "monotonic time moved backwards",
            Self::InvalidRate => "media clock rate is invalid",
            Self::InvalidSenderReport => "sender report is invalid",
        })
    }
}

impl std::error::Error for ClockError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockAnchor {
    monotonic: Instant,
    wall: SystemTime,
}

impl ClockAnchor {
    pub fn new(monotonic: Instant, wall: SystemTime) -> Result<Self, ClockError> {
        if wall.duration_since(UNIX_EPOCH).is_err() {
            return Err(ClockError::BeforeEpoch);
        }
        Ok(Self { monotonic, wall })
    }

    pub const fn monotonic(self) -> Instant {
        self.monotonic
    }

    pub const fn wall(self) -> SystemTime {
        self.wall
    }

    pub fn project(&self, at: Instant) -> Result<SystemTime, ClockError> {
        let elapsed = at
            .checked_duration_since(self.monotonic)
            .ok_or(ClockError::BeforeAnchor)?;
        let wall = self.wall.checked_add(elapsed).ok_or(ClockError::Overflow)?;
        debug_assert!(wall >= self.wall);
        Ok(wall)
    }

    pub fn deadline(&self, now: Instant, wall: SystemTime) -> Result<Instant, ClockError> {
        self.ensure_now(now)?;
        let elapsed = wall
            .duration_since(self.wall)
            .map_err(|_| ClockError::BeforeAnchor)?;
        let deadline = self
            .monotonic
            .checked_add(elapsed)
            .ok_or(ClockError::Overflow)?;
        debug_assert!(deadline >= self.monotonic);
        Ok(deadline)
    }

    pub fn deadline_after(&self, now: Instant, delay: Duration) -> Result<Instant, ClockError> {
        self.ensure_now(now)?;
        now.checked_add(delay).ok_or(ClockError::Overflow)
    }

    fn ensure_now(&self, now: Instant) -> Result<Duration, ClockError> {
        now.checked_duration_since(self.monotonic)
            .ok_or(ClockError::BeforeAnchor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SenderReportRejection {
    WrongSsrc,
    Duplicate,
    Stale,
    Future,
    NetworkDelay,
    ImplausibleRate,
    DiscontinuityPending,
    ArithmeticOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SenderReportDecision {
    Candidate,
    Accepted,
    Rejected(SenderReportRejection),
    EpochChanged { epoch: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedMediaTime {
    playout_time: SystemTime,
    epoch: u64,
    provisional: bool,
}

impl MappedMediaTime {
    pub const fn playout_time(self) -> SystemTime {
        self.playout_time
    }

    pub const fn epoch(self) -> u64 {
        self.epoch
    }

    pub const fn provisional(self) -> bool {
        self.provisional
    }
}

#[derive(Clone, Copy, Debug)]
struct Discontinuity {
    offset: i128,
    rtp_timestamp: u32,
    client_wall: SystemTime,
    arrival: Instant,
    confirmations: u8,
}

#[derive(Debug)]
pub struct RtpClockMapper {
    anchor: ClockAnchor,
    ssrc: u32,
    clock_rate: u32,
    epoch: u64,
    base_rtp: Option<u32>,
    base_client_wall: Option<SystemTime>,
    offset_nanos: i128,
    latest_sr: Option<(u32, SystemTime, Instant)>,
    rate_rtp_delta: u128,
    rate_wall_nanos: u128,
    last_now: Option<Instant>,
    newest_rtp: Option<u32>,
    newest_output: Option<SystemTime>,
    discontinuity: Option<Discontinuity>,
}

impl RtpClockMapper {
    pub fn new(anchor: ClockAnchor, ssrc: u32, clock_rate: u32) -> Result<Self, ClockError> {
        if clock_rate == 0 {
            return Err(ClockError::InvalidRate);
        }
        Ok(Self {
            anchor,
            ssrc,
            clock_rate,
            epoch: 0,
            base_rtp: None,
            base_client_wall: None,
            offset_nanos: 0,
            latest_sr: None,
            rate_rtp_delta: u128::from(clock_rate),
            rate_wall_nanos: 1_000_000_000,
            last_now: None,
            newest_rtp: None,
            newest_output: None,
            discontinuity: None,
        })
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub const fn anchor(&self) -> ClockAnchor {
        self.anchor
    }

    pub const fn ssrc(&self) -> u32 {
        self.ssrc
    }

    pub const fn clock_rate(&self) -> u32 {
        self.clock_rate
    }

    pub const fn synchronized(&self) -> bool {
        self.base_rtp.is_some()
    }

    pub fn observe_sender_report(
        &mut self,
        report: &SenderReport<'_>,
        arrival: Instant,
    ) -> Result<SenderReportDecision, ClockError> {
        self.observe_sender_report_parts(
            report.sender_ssrc(),
            report.rtp_timestamp(),
            report.ntp_seconds(),
            report.ntp_fraction(),
            arrival,
        )
    }

    pub fn observe_sender_report_parts(
        &mut self,
        sender_ssrc: u32,
        rtp_timestamp: u32,
        ntp_seconds: u32,
        ntp_fraction: u32,
        arrival: Instant,
    ) -> Result<SenderReportDecision, ClockError> {
        self.ensure_monotonic(arrival)?;
        if sender_ssrc != self.ssrc {
            return Ok(SenderReportDecision::Rejected(
                SenderReportRejection::WrongSsrc,
            ));
        }
        let arrival_wall = self.anchor.project(arrival)?;
        let client_wall = ntp_to_system_time(ntp_seconds, ntp_fraction, arrival_wall)?;
        let offset = signed_nanos(arrival_wall, client_wall)?;
        if offset < 0 && offset.unsigned_abs() > MAX_SENDER_REPORT_FUTURE.as_nanos() {
            return Ok(SenderReportDecision::Rejected(
                SenderReportRejection::Future,
            ));
        }
        if offset >= 0
            && u128::try_from(offset).map_err(|_| ClockError::Overflow)?
                > MAX_SENDER_REPORT_AGE.as_nanos()
        {
            return Ok(SenderReportDecision::Rejected(
                SenderReportRejection::NetworkDelay,
            ));
        }

        let Some((previous_rtp, previous_client_wall, previous_arrival)) = self.latest_sr else {
            self.offset_nanos = offset;
            self.latest_sr = Some((rtp_timestamp, client_wall, arrival));
            return Ok(SenderReportDecision::Candidate);
        };

        if rtp_timestamp == previous_rtp && client_wall == previous_client_wall {
            return Ok(SenderReportDecision::Rejected(
                SenderReportRejection::Duplicate,
            ));
        }
        if arrival < previous_arrival {
            return Ok(SenderReportDecision::Rejected(SenderReportRejection::Stale));
        }

        let difference = offset.saturating_sub(self.offset_nanos);
        let large_offset = abs_i128(difference) > MAX_DISCONTINUITY.as_nanos();
        let (progress_rtp, progress_wall, progress_arrival) =
            self.discontinuity.filter(|_| large_offset).map_or(
                (previous_rtp, previous_client_wall, previous_arrival),
                |pending| (pending.rtp_timestamp, pending.client_wall, pending.arrival),
            );
        let rtp_delta = i64::from(rtp_timestamp.wrapping_sub(progress_rtp).cast_signed());
        let wall_delta = signed_nanos(client_wall, progress_wall)?;
        if wall_delta <= 0 || rtp_delta < 0 || arrival <= progress_arrival {
            return Ok(SenderReportDecision::Rejected(SenderReportRejection::Stale));
        }
        if !large_offset
            && arrival.duration_since(previous_arrival) < MAX_SENDER_REPORT_SAMPLE_INTERVAL
        {
            return Ok(SenderReportDecision::Rejected(
                SenderReportRejection::Duplicate,
            ));
        }
        if large_offset && rtp_delta == 0 {
            return Ok(SenderReportDecision::Rejected(
                SenderReportRejection::ImplausibleRate,
            ));
        }
        if rtp_delta > 0 && (!large_offset || self.discontinuity.is_some()) {
            let credible = rate_is_credible(
                u128::try_from(rtp_delta).map_err(|_| ClockError::Overflow)?,
                u128::try_from(wall_delta).map_err(|_| ClockError::Overflow)?,
                self.clock_rate,
            );
            match credible {
                Ok(true) => {}
                Ok(false) => {
                    return Ok(SenderReportDecision::Rejected(
                        SenderReportRejection::ImplausibleRate,
                    ));
                }
                Err(()) => {
                    return Ok(SenderReportDecision::Rejected(
                        SenderReportRejection::ArithmeticOverflow,
                    ));
                }
            }
        }

        if large_offset {
            let same = self.discontinuity.is_some_and(|pending| {
                abs_i128(offset.saturating_sub(pending.offset)) <= MAX_DISCONTINUITY.as_nanos()
                    && pending.rtp_timestamp != rtp_timestamp
            });
            let confirmations = self.discontinuity.map_or(1, |pending| {
                pending.confirmations.saturating_add(u8::from(same))
            });
            if confirmations < DISCONTINUITY_CONFIRMATIONS {
                self.discontinuity = Some(Discontinuity {
                    offset,
                    rtp_timestamp,
                    client_wall,
                    arrival,
                    confirmations,
                });
                return Ok(SenderReportDecision::Rejected(
                    SenderReportRejection::DiscontinuityPending,
                ));
            }
            self.epoch = self.epoch.checked_add(1).ok_or(ClockError::Overflow)?;
            self.base_rtp = Some(rtp_timestamp);
            self.base_client_wall = Some(client_wall);
            self.offset_nanos = offset;
            if rtp_delta > 0 {
                self.rate_rtp_delta =
                    u128::try_from(rtp_delta).map_err(|_| ClockError::Overflow)?;
                self.rate_wall_nanos =
                    u128::try_from(wall_delta).map_err(|_| ClockError::Overflow)?;
            }
            self.latest_sr = Some((rtp_timestamp, client_wall, arrival));
            self.discontinuity = None;
            self.newest_rtp = None;
            return Ok(SenderReportDecision::EpochChanged { epoch: self.epoch });
        }

        if self.base_rtp.is_none() {
            self.base_rtp = Some(previous_rtp);
            self.base_client_wall = Some(previous_client_wall);
            self.offset_nanos = offset;
        } else {
            self.offset_nanos = slew(self.offset_nanos, offset, MAX_SENDER_REPORT_SLEW);
        }
        if rtp_delta > 0 {
            self.rate_rtp_delta = u128::try_from(rtp_delta).map_err(|_| ClockError::Overflow)?;
            self.rate_wall_nanos = u128::try_from(wall_delta).map_err(|_| ClockError::Overflow)?;
        }
        self.latest_sr = Some((rtp_timestamp, client_wall, arrival));
        self.discontinuity = None;
        Ok(SenderReportDecision::Accepted)
    }

    pub fn map_packet(
        &mut self,
        rtp_timestamp: u32,
        arrival: Instant,
    ) -> Result<MappedMediaTime, ClockError> {
        self.ensure_monotonic(arrival)?;
        let arrival_wall = self.anchor.project(arrival)?;
        let (mut output, provisional) = match (self.base_rtp, self.base_client_wall) {
            (Some(base_rtp), Some(base_wall)) => {
                let ticks = i64::from(rtp_timestamp.wrapping_sub(base_rtp).cast_signed());
                let elapsed = ticks_to_nanos(ticks, self.rate_rtp_delta, self.rate_wall_nanos)?;
                let mapped = add_signed_nanos(base_wall, elapsed, self.offset_nanos)?;
                (mapped, false)
            }
            _ => (arrival_wall, true),
        };
        let newer = self
            .newest_rtp
            .is_none_or(|frontier| rtp_timestamp.wrapping_sub(frontier).cast_signed() > 0);
        if newer {
            if let Some(last) = self.newest_output
                && output < last
            {
                output = last;
            }
            self.newest_rtp = Some(rtp_timestamp);
            self.newest_output = Some(output);
        }
        debug_assert!(
            self.newest_output
                .is_none_or(|newest| newest >= output || !newer)
        );
        Ok(MappedMediaTime {
            playout_time: output,
            epoch: self.epoch,
            provisional,
        })
    }

    fn ensure_monotonic(&mut self, now: Instant) -> Result<(), ClockError> {
        if self.last_now.is_some_and(|last| now < last) {
            return Err(ClockError::NonMonotonic);
        }
        self.last_now = Some(now);
        Ok(())
    }
}

fn abs_i128(value: i128) -> u128 {
    value.unsigned_abs()
}

fn signed_nanos(later: SystemTime, earlier: SystemTime) -> Result<i128, ClockError> {
    if let Ok(delta) = later.duration_since(earlier) {
        return i128::try_from(delta.as_nanos()).map_err(|_| ClockError::Overflow);
    }
    let delta = earlier
        .duration_since(later)
        .map_err(|_| ClockError::Overflow)?;
    let nanos = i128::try_from(delta.as_nanos()).map_err(|_| ClockError::Overflow)?;
    nanos.checked_neg().ok_or(ClockError::Overflow)
}

fn add_signed_nanos(
    base: SystemTime,
    elapsed: i128,
    offset_nanos: i128,
) -> Result<SystemTime, ClockError> {
    let total = elapsed
        .checked_add(offset_nanos)
        .ok_or(ClockError::Overflow)?;
    if total >= 0 {
        base.checked_add(Duration::from_nanos(
            u64::try_from(total).map_err(|_| ClockError::Overflow)?,
        ))
        .ok_or(ClockError::Overflow)
    } else {
        base.checked_sub(Duration::from_nanos(
            u64::try_from(total.unsigned_abs()).map_err(|_| ClockError::Overflow)?,
        ))
        .ok_or(ClockError::BeforeEpoch)
    }
}

fn slew(current: i128, target: i128, max_step: Duration) -> i128 {
    let max_step = i128::try_from(max_step.as_nanos()).unwrap_or(i128::MAX);
    let delta = target.saturating_sub(current);
    if delta.unsigned_abs() <= u128::try_from(max_step).unwrap_or(u128::MAX) {
        target
    } else if delta > 0 {
        current.saturating_add(max_step)
    } else {
        current.saturating_sub(max_step)
    }
}

fn ticks_to_nanos(
    ticks: i64,
    rate_rtp_delta: u128,
    rate_wall_nanos: u128,
) -> Result<i128, ClockError> {
    debug_assert_ne!(rate_rtp_delta, 0);
    let magnitude = u128::from(ticks.unsigned_abs());
    let nanos = magnitude
        .checked_mul(rate_wall_nanos)
        .and_then(|value| value.checked_div(rate_rtp_delta))
        .ok_or(ClockError::Overflow)?;
    let nanos = i128::try_from(nanos).map_err(|_| ClockError::Overflow)?;
    if ticks < 0 {
        nanos.checked_neg().ok_or(ClockError::Overflow)
    } else {
        Ok(nanos)
    }
}

fn rate_is_credible(rtp_delta: u128, wall_nanos: u128, clock_rate: u32) -> Result<bool, ()> {
    debug_assert_ne!(rtp_delta, 0);
    debug_assert_ne!(wall_nanos, 0);
    let expected = wall_nanos.checked_mul(u128::from(clock_rate)).ok_or(())?;
    let actual = rtp_delta.checked_mul(1_000_000_000).ok_or(())?;
    let actual_scaled = actual
        .checked_mul(MAX_SENDER_REPORT_RATE_ERROR_DENOMINATOR)
        .ok_or(())?;
    let minimum = expected.checked_mul(MIN_RATE_PERCENT).ok_or(())?;
    let maximum = expected
        .checked_mul(MAX_SENDER_REPORT_RATE_ERROR_NUMERATOR)
        .ok_or(())?;
    Ok(actual_scaled >= minimum && actual_scaled <= maximum)
}

fn ntp_to_system_time(
    seconds: u32,
    fraction: u32,
    reference: SystemTime,
) -> Result<SystemTime, ClockError> {
    let reference_seconds = i128::from(
        reference
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ClockError::BeforeEpoch)?
            .as_secs(),
    )
    .checked_add(NTP_UNIX_EPOCH_SECONDS)
    .ok_or(ClockError::Overflow)?;
    let era = reference_seconds
        .checked_sub(i128::from(seconds))
        .ok_or(ClockError::Overflow)?
        .div_euclid(NTP_ERA_SECONDS);
    let fraction_nanos = u128::from(fraction)
        .checked_mul(1_000_000_000)
        .ok_or(ClockError::Overflow)?
        >> 32;
    let fraction_nanos = u32::try_from(fraction_nanos).map_err(|_| ClockError::Overflow)?;
    let mut best = None;
    for candidate_era in [
        era.checked_sub(1).ok_or(ClockError::Overflow)?,
        era,
        era.checked_add(1).ok_or(ClockError::Overflow)?,
    ] {
        let ntp_seconds = i128::from(seconds)
            .checked_add(
                candidate_era
                    .checked_mul(NTP_ERA_SECONDS)
                    .ok_or(ClockError::Overflow)?,
            )
            .ok_or(ClockError::Overflow)?;
        let unix_seconds = ntp_seconds
            .checked_sub(NTP_UNIX_EPOCH_SECONDS)
            .ok_or(ClockError::Overflow)?;
        if unix_seconds < 0 {
            continue;
        }
        let candidate = UNIX_EPOCH
            .checked_add(Duration::from_secs(
                u64::try_from(unix_seconds).map_err(|_| ClockError::Overflow)?,
            ))
            .and_then(|value| value.checked_add(Duration::from_nanos(u64::from(fraction_nanos))))
            .ok_or(ClockError::Overflow)?;
        let distance = abs_i128(signed_nanos(candidate, reference)?);
        if best.is_none_or(|(_, best_distance)| distance < best_distance) {
            best = Some((candidate, distance));
        }
    }
    best.map(|(candidate, _)| candidate)
        .ok_or(ClockError::BeforeEpoch)
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    reason = "test fixtures use bounded, checked timeline values"
)]
mod tests {
    use super::*;

    fn anchor() -> ClockAnchor {
        ClockAnchor::new(Instant::now(), UNIX_EPOCH + Duration::from_secs(1_000)).unwrap()
    }

    fn ntp(time: SystemTime) -> (u32, u32) {
        let seconds = time.duration_since(UNIX_EPOCH).unwrap();
        let ntp_seconds = seconds.as_secs() + NTP_UNIX_EPOCH_SECONDS as u64;
        let fraction = ((u128::from(seconds.subsec_nanos()) << 32) / 1_000_000_000) as u32;
        (ntp_seconds as u32, fraction)
    }

    #[test]
    fn anchor_projects_and_inverts_without_wall_resampling() {
        let mono = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(42);
        let anchor = ClockAnchor::new(mono, wall).unwrap();
        let later = mono + Duration::from_millis(250);
        let projected = anchor.project(later).unwrap();
        assert_eq!(projected, wall + Duration::from_millis(250));
        assert_eq!(anchor.deadline(mono, projected).unwrap(), later);
    }

    #[test]
    fn anchor_rejects_invalid_direction_and_overflow() {
        let mono = Instant::now();
        let anchor = ClockAnchor::new(mono, UNIX_EPOCH + Duration::from_secs(42)).unwrap();
        assert_eq!(
            anchor.project(mono - Duration::from_nanos(1)),
            Err(ClockError::BeforeAnchor)
        );
        assert_eq!(
            anchor.deadline(mono, UNIX_EPOCH),
            Err(ClockError::BeforeAnchor)
        );
        assert_eq!(
            ClockAnchor::new(mono, UNIX_EPOCH - Duration::from_nanos(1)),
            Err(ClockError::BeforeEpoch)
        );
    }

    #[test]
    fn synchronized_anchors_transfer_wall_time() {
        let first =
            ClockAnchor::new(Instant::now(), UNIX_EPOCH + Duration::from_secs(100)).unwrap();
        let second_mono = first.monotonic() + Duration::from_secs(10);
        let second = ClockAnchor::new(second_mono, UNIX_EPOCH + Duration::from_secs(110)).unwrap();
        let playout = first
            .project(first.monotonic() + Duration::from_secs(16))
            .unwrap();
        assert_eq!(
            second.deadline(second_mono, playout).unwrap(),
            second_mono + Duration::from_secs(6)
        );
    }

    #[test]
    fn provisional_mapping_is_arrival_anchored_and_monotonic() {
        let anchor = anchor();
        let now = anchor.monotonic() + Duration::from_secs(1);
        let mut mapper = RtpClockMapper::new(anchor, 7, 90_000).unwrap();
        let first = mapper.map_packet(10, now).unwrap();
        let second = mapper
            .map_packet(9, now + Duration::from_millis(1))
            .unwrap();
        assert!(first.provisional());
        assert_eq!(
            second.playout_time(),
            anchor.project(now + Duration::from_millis(1)).unwrap()
        );
    }

    #[test]
    fn sender_report_establishes_mapping_without_adopting_absolute_time() {
        let anchor = anchor();
        let now = anchor.monotonic() + Duration::from_secs(1);
        let wall = anchor.project(now).unwrap();
        let (seconds, fraction) = ntp(wall - Duration::from_millis(50));
        let mut mapper = RtpClockMapper::new(anchor, 7, 90_000).unwrap();
        assert_eq!(
            mapper
                .observe_sender_report_parts(7, 1000, seconds, fraction, now)
                .unwrap(),
            SenderReportDecision::Candidate
        );
        let second_wall = wall + Duration::from_secs(1) - Duration::from_millis(50);
        let second = ntp(second_wall);
        assert_eq!(
            mapper
                .observe_sender_report_parts(
                    7,
                    91_000,
                    second.0,
                    second.1,
                    now + Duration::from_secs(1)
                )
                .unwrap(),
            SenderReportDecision::Accepted
        );
        let mapped = mapper
            .map_packet(1000 + 90_000, now + Duration::from_secs(1))
            .unwrap();
        assert_eq!(mapped.playout_time(), wall + Duration::from_secs(1));
        assert!(!mapped.provisional());
    }

    #[test]
    fn accepted_sender_report_rate_is_used_for_future_mapping() {
        let anchor = anchor();
        let first_now = anchor.monotonic() + Duration::from_secs(1);
        let first_wall = anchor.project(first_now).unwrap();
        let first = ntp(first_wall - Duration::from_millis(50));
        let mut mapper = RtpClockMapper::new(anchor, 7, 90_000).unwrap();
        mapper
            .observe_sender_report_parts(7, 0, first.0, first.1, first_now)
            .unwrap();

        let second_now = first_now + Duration::from_secs(1);
        let second_wall = first_wall + Duration::from_secs(1) - Duration::from_millis(50);
        let second = ntp(second_wall);
        assert_eq!(
            mapper
                .observe_sender_report_parts(7, 90_100, second.0, second.1, second_now)
                .unwrap(),
            SenderReportDecision::Accepted
        );
        let mapped = mapper
            .map_packet(180_200, second_now + Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            mapped.playout_time(),
            first_wall.checked_add(Duration::from_secs(2)).unwrap()
        );
    }

    #[test]
    fn hostile_reports_are_rejected_and_small_changes_slew() {
        let anchor = anchor();
        let first_now = anchor.monotonic() + Duration::from_secs(1);
        let first_wall = anchor.project(first_now).unwrap();
        let first_ntp = ntp(first_wall - Duration::from_millis(50));
        let mut mapper = RtpClockMapper::new(anchor, 7, 90_000).unwrap();
        mapper
            .observe_sender_report_parts(7, 0, first_ntp.0, first_ntp.1, first_now)
            .unwrap();
        let duplicate = mapper
            .observe_sender_report_parts(7, 0, first_ntp.0, first_ntp.1, first_now)
            .unwrap();
        assert_eq!(
            duplicate,
            SenderReportDecision::Rejected(SenderReportRejection::Duplicate)
        );
        let delayed_duplicate = mapper
            .observe_sender_report_parts(
                7,
                0,
                first_ntp.0,
                first_ntp.1,
                first_now + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(
            delayed_duplicate,
            SenderReportDecision::Rejected(SenderReportRejection::Duplicate)
        );
        let future = ntp(first_wall + Duration::from_secs(5));
        assert_eq!(
            mapper
                .observe_sender_report_parts(
                    7,
                    0,
                    future.0,
                    future.1,
                    first_now + Duration::from_secs(1)
                )
                .unwrap(),
            SenderReportDecision::Rejected(SenderReportRejection::Future)
        );
        let good = ntp(first_wall + Duration::from_secs(1) - Duration::from_millis(45));
        assert_eq!(
            mapper
                .observe_sender_report_parts(
                    7,
                    90_000,
                    good.0,
                    good.1,
                    first_now + Duration::from_secs(1)
                )
                .unwrap(),
            SenderReportDecision::Accepted
        );
    }

    #[test]
    fn sustained_discontinuity_starts_a_new_epoch_without_backward_output() {
        let anchor = anchor();
        let first_now = anchor.monotonic() + Duration::from_secs(1);
        let first_wall = anchor.project(first_now).unwrap();
        let first_ntp = ntp(first_wall - Duration::from_millis(50));
        let mut mapper = RtpClockMapper::new(anchor, 7, 90_000).unwrap();
        mapper
            .observe_sender_report_parts(7, 0, first_ntp.0, first_ntp.1, first_now)
            .unwrap();
        let mut result = SenderReportDecision::Accepted;
        for n in 1..=DISCONTINUITY_CONFIRMATIONS {
            let at = first_now + Duration::from_secs(u64::from(n) + 1);
            let wall = first_wall + Duration::from_secs(u64::from(n));
            let ntp = ntp(wall);
            result = mapper
                .observe_sender_report_parts(7, u32::from(n) * 90_000, ntp.0, ntp.1, at)
                .unwrap();
        }
        assert_eq!(result, SenderReportDecision::EpochChanged { epoch: 1 });
        let mapped = mapper
            .map_packet(400_000, first_now + Duration::from_secs(8))
            .unwrap();
        assert_eq!(mapped.epoch(), 1);
        assert!(mapped.playout_time() >= first_wall);
    }

    #[test]
    fn inconsistent_discontinuity_candidates_never_confirm() {
        let anchor = anchor();
        let first_now = anchor.monotonic() + Duration::from_secs(1);
        let first_wall = anchor.project(first_now).unwrap();
        let first = ntp(first_wall - Duration::from_millis(50));
        let mut mapper = RtpClockMapper::new(anchor, 7, 90_000).unwrap();
        mapper
            .observe_sender_report_parts(7, 0, first.0, first.1, first_now)
            .unwrap();
        let candidates = [
            (350_u64, 2_300_u64, 90_000_u32),
            (750, 3_700, 180_000),
            (1_150, 5_100, 270_000),
        ];
        for (offset_ms, arrival_ms, rtp_timestamp) in candidates {
            let arrival = first_now + Duration::from_millis(arrival_ms - 1_000);
            let client = first_wall + Duration::from_millis(arrival_ms - 1_000 - offset_ms);
            let report = ntp(client);
            let decision = mapper
                .observe_sender_report_parts(7, rtp_timestamp, report.0, report.1, arrival)
                .unwrap();
            assert_eq!(
                decision,
                SenderReportDecision::Rejected(SenderReportRejection::DiscontinuityPending)
            );
            assert_eq!(mapper.epoch(), 0);
        }
    }

    #[test]
    fn stable_large_offset_reacquires_without_old_epoch_rate() {
        let anchor = anchor();
        let first_now = anchor.monotonic() + Duration::from_secs(1);
        let first_wall = anchor.project(first_now).unwrap();
        let first = ntp(first_wall - Duration::from_millis(50));
        let mut mapper = RtpClockMapper::new(anchor, 7, 90_000).unwrap();
        mapper
            .observe_sender_report_parts(7, 0, first.0, first.1, first_now)
            .unwrap();

        let established_wall = first_wall + Duration::from_secs(1) - Duration::from_millis(50);
        let established = ntp(established_wall);
        assert_eq!(
            mapper
                .observe_sender_report_parts(
                    7,
                    90_000,
                    established.0,
                    established.1,
                    first_now + Duration::from_secs(1),
                )
                .unwrap(),
            SenderReportDecision::Accepted
        );

        let mut decisions = Vec::new();
        for (rtp_timestamp, elapsed_seconds) in [(180_000_u32, 2_u64), (270_000, 3), (360_000, 4)] {
            let arrival = first_now + Duration::from_secs(elapsed_seconds);
            let client_wall =
                first_wall + Duration::from_secs(elapsed_seconds) - Duration::from_millis(350);
            let report = ntp(client_wall);
            decisions.push(
                mapper
                    .observe_sender_report_parts(7, rtp_timestamp, report.0, report.1, arrival)
                    .unwrap(),
            );
        }
        assert_eq!(
            decisions,
            [
                SenderReportDecision::Rejected(SenderReportRejection::DiscontinuityPending),
                SenderReportDecision::Rejected(SenderReportRejection::DiscontinuityPending),
                SenderReportDecision::EpochChanged { epoch: 1 },
            ]
        );
    }

    #[test]
    fn reordered_rtp_keeps_its_source_mapping_without_moving_frontier() {
        let anchor = anchor();
        let first_now = anchor.monotonic() + Duration::from_secs(1);
        let first_wall = anchor.project(first_now).unwrap();
        let first = ntp(first_wall - Duration::from_millis(50));
        let mut mapper = RtpClockMapper::new(anchor, 7, 90_000).unwrap();
        mapper
            .observe_sender_report_parts(7, 1_000, first.0, first.1, first_now)
            .unwrap();
        let second_wall = first_wall + Duration::from_secs(1) - Duration::from_millis(50);
        let second = ntp(second_wall);
        mapper
            .observe_sender_report_parts(
                7,
                91_000,
                second.0,
                second.1,
                first_now + Duration::from_secs(1),
            )
            .unwrap();
        let newest = mapper
            .map_packet(91_000, first_now + Duration::from_secs(1))
            .unwrap();
        let reordered = mapper
            .map_packet(1_000, first_now + Duration::from_secs(2))
            .unwrap();
        let future = mapper
            .map_packet(181_000, first_now + Duration::from_secs(3))
            .unwrap();
        assert!(reordered.playout_time() < newest.playout_time());
        assert_eq!(reordered.playout_time(), first_wall);
        assert!(future.playout_time() > newest.playout_time());
    }

    #[test]
    fn wrong_ssrc_still_advances_authoritative_monotonic_time() {
        let anchor = anchor();
        let mut mapper = RtpClockMapper::new(anchor, 7, 90_000).unwrap();
        let far = anchor.monotonic() + Duration::from_secs(10);
        assert_eq!(
            mapper.observe_sender_report_parts(8, 0, 0, 0, far).unwrap(),
            SenderReportDecision::Rejected(SenderReportRejection::WrongSsrc)
        );
        assert_eq!(
            mapper.observe_sender_report_parts(
                7,
                0,
                0,
                0,
                anchor.monotonic() + Duration::from_secs(2)
            ),
            Err(ClockError::NonMonotonic)
        );
    }

    #[test]
    fn rtp_and_ntp_wrap_are_resolved() {
        let anchor = anchor();
        let now = anchor.monotonic() + Duration::from_secs(1);
        let wall = anchor.project(now).unwrap();
        let first = ntp(wall - Duration::from_millis(50));
        let mut mapper = RtpClockMapper::new(anchor, 7, 90_000).unwrap();
        mapper
            .observe_sender_report_parts(7, u32::MAX - 44_999, first.0, first.1, now)
            .unwrap();
        let later_wall = wall + Duration::from_secs(1) - Duration::from_millis(50);
        let later = ntp(later_wall);
        assert_eq!(
            mapper
                .observe_sender_report_parts(
                    7,
                    45_000,
                    later.0,
                    later.1,
                    now + Duration::from_secs(1)
                )
                .unwrap(),
            SenderReportDecision::Accepted
        );
        let newest = mapper
            .map_packet(45_000, now + Duration::from_secs(1))
            .unwrap();
        let reordered = mapper
            .map_packet(u32::MAX, now + Duration::from_secs(2))
            .unwrap();
        assert!(reordered.playout_time() < newest.playout_time());
    }

    #[test]
    fn ntp_era_wrap_is_resolved_against_the_supplied_anchor() {
        let era_seconds = u64::try_from(NTP_ERA_SECONDS).unwrap();
        let anchor_wall = UNIX_EPOCH
            .checked_add(Duration::from_secs(era_seconds - 2))
            .unwrap();
        let anchor = ClockAnchor::new(Instant::now(), anchor_wall).unwrap();
        let now = anchor.monotonic() + Duration::from_secs(1);
        let report_wall = anchor.project(now).unwrap() - Duration::from_millis(20);
        let report = ntp(report_wall);
        let mut mapper = RtpClockMapper::new(anchor, 7, 90_000).unwrap();
        assert_eq!(
            mapper
                .observe_sender_report_parts(7, 10, report.0, report.1, now)
                .unwrap(),
            SenderReportDecision::Candidate
        );
        let second_report_wall = report_wall + Duration::from_secs(1);
        let second_report = ntp(second_report_wall);
        assert_eq!(
            mapper
                .observe_sender_report_parts(
                    7,
                    90_010,
                    second_report.0,
                    second_report.1,
                    now + Duration::from_secs(1)
                )
                .unwrap(),
            SenderReportDecision::Accepted
        );
        let mapped = mapper
            .map_packet(90_010, now + Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            mapped.playout_time(),
            anchor.project(now).unwrap() + Duration::from_secs(1)
        );
    }

    #[test]
    fn mapper_rejects_backward_monotonic_input() {
        let anchor = anchor();
        let mut mapper = RtpClockMapper::new(anchor, 7, 90_000).unwrap();
        let later = anchor.monotonic() + Duration::from_secs(2);
        mapper.map_packet(1, later).unwrap();
        assert_eq!(
            mapper.map_packet(2, later - Duration::from_nanos(1)),
            Err(ClockError::NonMonotonic)
        );
    }
}

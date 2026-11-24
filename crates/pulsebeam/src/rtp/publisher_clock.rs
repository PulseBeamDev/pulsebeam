//! Clock evidence scoped to authenticated publisher admission. An SR describes
//! the publisher's NTP coordinate, not the receiver's global reference.

use std::collections::{HashMap, VecDeque};

use pulsebeam_clock::{
    Error,
    domain::{
        ClockDomain, Config, LocalOrigin, Projection, ReferenceState, RtpSample, SenderReport,
    },
    time::{Binding, ClockRate, GlobalInstant, MonotonicTime, RtpBinding, SourceTime},
};
use pulsebeam_runtime::time::Instant;

const NS_PER_SECOND: i128 = 1_000_000_000;
const NTP_ERA_SECONDS: i128 = 4_294_967_296;
const NTP_TO_UNIX_SECONDS: i128 = 2_208_988_800;
const EVIDENCE_AGE_NS: u64 = 10_000_000_000;
const REORDER_NS: u64 = 2_000_000_000;
const ASSIGNED_SAMPLES: usize = 256;
// A report clock can be offset from UTC, but a far-off epoch doesn't qualify
// an initial era. Forward using arrival cadence if it cannot be unfolded.
const ERA_BOOTSTRAP_WINDOW_SECONDS: u128 = 86_400 * 365;

const CONFIG: Config = Config {
    track_capacity: 1,
    nominal_delay_ns: 0,
    max_past_ns: 30_000_000_000,
    max_future_ns: REORDER_NS,
    observation_max_age_ns: EVIDENCE_AGE_NS,
    max_drift_ppm: 2_000,
    max_exchange_rtt_ns: 1_000_000_000,
    min_fit_span_ns: 100_000_000,
    lock_uncertainty_ns: 5_000_000,
    reorder_ns: REORDER_NS,
    history_capacity: 8,
    slew_ppm: 1_000,
    max_phase_error_ns: REORDER_NS,
};

#[derive(Clone, Copy)]
struct TimedTick {
    tick: i128,
    at: MonotonicTime,
}

pub(crate) struct PublisherClock {
    origin: Instant,
    global_origin: GlobalInstant,
    domain: ClockDomain,
    encodings: Box<[EncodingClock]>,
    reference: Option<(u64, ReferenceState)>,
}

struct EncodingClock {
    track: RtpBinding,
    primary_ssrc: u32,
    frontier: Option<TimedTick>,
    pending_report: Option<pulsebeam_rtc::SenderReport>,
    last_report: Option<(MonotonicTime, SourceTime)>,
    assigned: HashMap<u32, u32>,
    assigned_order: VecDeque<(Instant, u32)>,
    latest_assignment: Option<(Instant, u32, u32)>,
}

impl PublisherClock {
    pub(crate) fn admit(
        binding: Binding,
        primary_ssrc: u32,
        rate: ClockRate,
        now: Instant,
        reference: &crate::clock::reference::Estimate,
        deferred: Option<pulsebeam_rtc::SenderReport>,
    ) -> Result<Self, Error> {
        use pulsebeam_clock::time::GlobalDuration;
        let earlier = deferred
            .filter(|report| report.ssrc == primary_ssrc)
            .and_then(|report| {
                let at = report.arrival.into();
                let age = u64::try_from(now.checked_duration_since(at)?.as_nanos()).ok()?;
                if age > EVIDENCE_AGE_NS {
                    return None;
                }
                let age = i64::try_from(age).ok()?;
                let global = reference
                    .global
                    .checked_add(GlobalDuration::from_nanos(age.saturating_neg()))
                    .ok()?;
                Some((report, at, global))
            });
        // The first packet can already be in the receive queue when discovery
        // completes. Reserve only the admission/repair window, not an old era.
        let slack = pulsebeam_runtime::time::Duration::from_millis(120);
        let (origin, global) =
            earlier.map_or((now, reference.global), |(_, at, global)| (at, global));
        let origin = origin.checked_sub(slack).ok_or(Error::Overflow)?;
        let global = global.checked_add(GlobalDuration::from_nanos(-120_000_000))?;
        let mut clock =
            Self::with_verified_members(binding, &[(primary_ssrc, rate)], origin, global)?;
        clock.sync_host_reference(now, Some(reference));
        if let Some((report, _, _)) = earlier {
            let _ = clock.report(report, now);
        }
        Ok(clock)
    }

    /// Membership is a caller-verified assertion about one source reference.
    /// Neither a common transport nor an RTP/RTCP identifier proves it.
    fn with_verified_members(
        binding: Binding,
        members: &[(u32, ClockRate)],
        admitted_at: Instant,
        global_origin: GlobalInstant,
    ) -> Result<Self, Error> {
        let track_capacity = u32::try_from(members.len()).map_err(|_| Error::OutOfWindow)?;
        let mut domain = ClockDomain::new(
            Config {
                track_capacity,
                ..CONFIG
            },
            binding,
            LocalOrigin {
                at: MonotonicTime::from_nanos(0),
                global: global_origin,
            },
        )?;
        let mut encodings = Vec::with_capacity(members.len());
        for (slot, &(primary_ssrc, rate)) in members.iter().enumerate() {
            if encodings
                .iter()
                .any(|encoding: &EncodingClock| encoding.primary_ssrc == primary_ssrc)
            {
                return Err(Error::Invalid);
            }
            encodings.push(EncodingClock {
                track: domain
                    .bind_track(u32::try_from(slot).map_err(|_| Error::OutOfWindow)?, rate)?,
                primary_ssrc,
                frontier: None,
                pending_report: None,
                last_report: None,
                assigned: HashMap::with_capacity(ASSIGNED_SAMPLES),
                assigned_order: VecDeque::with_capacity(ASSIGNED_SAMPLES),
                latest_assignment: None,
            });
        }
        Ok(Self {
            origin: admitted_at,
            global_origin,
            domain,
            encodings: encodings.into_boxed_slice(),
            reference: None,
        })
    }

    fn at(&self, instant: Instant) -> Result<MonotonicTime, Error> {
        let age = instant
            .checked_duration_since(self.origin)
            .ok_or(Error::Stale)?;
        Ok(MonotonicTime::from_nanos(
            u64::try_from(age.as_nanos()).map_err(|_| Error::Overflow)?,
        ))
    }

    pub(crate) fn sync_host_reference(
        &mut self,
        now: Instant,
        reference: Option<&crate::clock::reference::Estimate>,
    ) {
        let generation = reference.map_or_else(
            || self.reference.map_or(0, |(generation, _)| generation),
            |reference| reference.evidence_generation,
        );
        let locked = reference.is_some_and(|reference| reference.uncertainty_ns.is_some());
        let _ = self.host_reference(now, generation, locked);
    }

    fn host_reference(&mut self, now: Instant, generation: u64, locked: bool) -> Result<(), Error> {
        let state = match self.reference {
            Some((prior, _)) if prior != generation => ReferenceState::Stepped,
            _ if locked => ReferenceState::Usable,
            _ => ReferenceState::Lost,
        };
        if self.reference == Some((generation, state)) {
            return Ok(());
        }
        let at = self.at(now)?;
        self.domain
            .set_reference_state(self.domain.binding(), at, state)?;
        self.reference = Some((generation, state));
        Ok(())
    }

    /// Assign once per source sample. The bounded sample cache is independent
    /// of sequence identity: several packets and repairs can name one sample.
    pub(crate) fn assign(&mut self, slot: usize, raw: u32, arrival: Instant, now: Instant) -> u32 {
        let Some(track) = self.encodings.get_mut(slot) else {
            return 0;
        };
        let retention = pulsebeam_runtime::time::Duration::from_nanos(REORDER_NS);
        while let Some(&(assigned_at, oldest)) = track.assigned_order.front() {
            if now.saturating_duration_since(assigned_at) <= retention {
                break;
            }
            track.assigned_order.pop_front();
            track.assigned.remove(&oldest);
        }
        if let Some(timestamp) = track.assigned.get(&raw) {
            return *timestamp;
        }
        let recovered = self
            .media(slot, raw, arrival, now)
            .ok()
            .map(|time| time.rtp.0);
        let Some(track) = self.encodings.get_mut(slot) else {
            return 0;
        };
        let timestamp = recovered.unwrap_or_else(|| {
            if let Some((previous_at, previous_raw, previous_timestamp)) = track.latest_assignment {
                // An ambiguous or physically implausible source jump cannot
                // move the admitted cadence by half an RTP wrap.
                let delta = raw.wrapping_sub(previous_raw);
                let age = arrival.saturating_duration_since(previous_at).as_nanos();
                let expected = age
                    .saturating_mul(u128::from(track.track.rate.hz()))
                    .div_euclid(1_000_000_000);
                let tolerance = u128::from(REORDER_NS)
                    .saturating_mul(u128::from(track.track.rate.hz()))
                    .div_euclid(1_000_000_000);
                if u128::from(delta).abs_diff(expected) <= tolerance {
                    previous_timestamp.wrapping_add(delta)
                } else {
                    let cadence =
                        u32::try_from(expected.rem_euclid(1u128 << 32)).unwrap_or_default();
                    previous_timestamp.wrapping_add(cadence)
                }
            } else {
                use pulsebeam_clock::time::GlobalDuration;
                let signed_age = if let Some(age) = arrival.checked_duration_since(self.origin) {
                    i64::try_from(age.as_nanos()).unwrap_or(i64::MAX)
                } else {
                    i64::try_from(self.origin.saturating_duration_since(arrival).as_nanos())
                        .unwrap_or(i64::MAX)
                        .saturating_neg()
                };
                let arrival_time = self
                    .global_origin
                    .checked_add(GlobalDuration::from_nanos(signed_age))
                    .unwrap_or(self.global_origin);
                track
                    .track
                    .rate
                    .timestamp(arrival_time)
                    .map_or(0, |timestamp| timestamp.0)
            }
        });
        if track.assigned_order.len() == ASSIGNED_SAMPLES
            && let Some((_, oldest)) = track.assigned_order.pop_front()
        {
            track.assigned.remove(&oldest);
        }
        track.assigned_order.push_back((now, raw));
        track.assigned.insert(raw, timestamp);
        if track
            .latest_assignment
            .is_none_or(|(previous, _, _)| arrival >= previous)
        {
            track.latest_assignment = Some((arrival, raw, timestamp));
        }
        timestamp
    }

    fn media(
        &mut self,
        slot: usize,
        residue: u32,
        received_at: Instant,
        now: Instant,
    ) -> Result<Projection, Error> {
        let at = self.at(received_at)?;
        let measured = self.at(now)?;
        if measured.elapsed_since(at)? > EVIDENCE_AGE_NS {
            return Err(Error::Stale);
        }
        let track = self.encodings.get_mut(slot).ok_or(Error::WrongBinding)?;
        let tick = match track.frontier {
            Some(frontier) => unwrap_rtp(residue, at, frontier, track.track.rate)?,
            None => i128::from(residue),
        };
        let sample = RtpSample {
            binding: track.track,
            tick,
        };
        if track.frontier.is_none() {
            self.domain.bootstrap_track(sample, at, measured)?;
        }
        let projection = self.domain.project_rtp(sample, measured)?;
        if track.frontier.is_none_or(|prior| tick > prior.tick) {
            track.frontier = Some(TimedTick { tick, at });
        }
        if let Some(report) = track.pending_report.take() {
            let _ = self.report(report, now);
        }
        Ok(projection)
    }

    pub(crate) fn report(
        &mut self,
        report: pulsebeam_rtc::SenderReport,
        now: Instant,
    ) -> Result<(), Error> {
        let at = self.at(report.arrival.into())?;
        let now = self.at(now)?;
        if now.elapsed_since(at)? > EVIDENCE_AGE_NS {
            return Err(Error::Stale);
        }
        let track = self
            .encodings
            .iter_mut()
            .find(|encoding| encoding.primary_ssrc == report.ssrc)
            .ok_or(Error::WrongBinding)?;
        let source = Self::unfold_report(
            self.global_origin,
            track.last_report,
            at,
            report.ntp_seconds,
            report.ntp_fraction,
        )?;
        let Some(frontier) = track.frontier else {
            if track
                .pending_report
                .is_some_and(|pending| report.arrival <= pending.arrival)
            {
                return Err(Error::Stale);
            }
            track.pending_report = Some(report);
            return Ok(());
        };
        let tick = unwrap_rtp(report.rtp_timestamp, at, frontier, track.track.rate)?;
        // The exact sampling phase within the RTP tick is not observable.
        let uncertainty_ns = 1_000_000_000u64
            .div_ceil(u64::from(track.track.rate.hz()))
            .saturating_add(1);
        self.domain.observe_sender_report(
            SenderReport {
                sample: RtpSample {
                    binding: track.track,
                    tick,
                },
                source,
                uncertainty_ns,
                received_at: at,
            },
            frontier.tick,
            now,
        )?;
        track.last_report = Some((at, source));
        Ok(())
    }

    fn unfold_report(
        global_origin: GlobalInstant,
        last_report: Option<(MonotonicTime, SourceTime)>,
        at: MonotonicTime,
        seconds: u32,
        fraction: u32,
    ) -> Result<SourceTime, Error> {
        let (pivot, window) = if let Some((previous, source)) = last_report {
            let age = at.elapsed_since(previous)?;
            if age == 0 {
                return Err(Error::Stale);
            }
            let pivot = i128::from(source.as_nanos())
                .saturating_add(i128::from(age))
                .saturating_add(NTP_TO_UNIX_SECONDS.saturating_mul(NS_PER_SECOND));
            (pivot, 30)
        } else {
            let pivot = i128::from(global_origin.as_nanos())
                .saturating_add(i128::from(at.as_nanos()))
                .saturating_add(NTP_TO_UNIX_SECONDS.saturating_mul(NS_PER_SECOND));
            (pivot, ERA_BOOTSTRAP_WINDOW_SECONDS)
        };
        let extended = nearest_residue(
            i128::from(seconds),
            pivot.div_euclid(NS_PER_SECOND),
            NTP_ERA_SECONDS,
        )?;
        if extended.abs_diff(pivot.div_euclid(NS_PER_SECOND)) > window {
            return Err(Error::OutOfWindow);
        }
        let ns = extended
            .saturating_sub(NTP_TO_UNIX_SECONDS)
            .saturating_mul(NS_PER_SECOND)
            .saturating_add(
                i128::from(fraction)
                    .saturating_mul(NS_PER_SECOND)
                    .div_euclid(NTP_ERA_SECONDS),
            );
        Ok(SourceTime::from_nanos(
            i64::try_from(ns).map_err(|_| Error::Overflow)?,
        ))
    }
}

fn nearest_residue(residue: i128, pivot: i128, modulus: i128) -> Result<i128, Error> {
    let half = modulus.div_euclid(2);
    let delta = residue
        .checked_sub(pivot)
        .ok_or(Error::Overflow)?
        .rem_euclid(modulus);
    if delta == half {
        return Err(Error::Ambiguous);
    }
    let delta = if delta > half {
        delta.saturating_sub(modulus)
    } else {
        delta
    };
    pivot.checked_add(delta).ok_or(Error::Overflow)
}

fn unwrap_rtp(
    raw: u32,
    at: MonotonicTime,
    frontier: TimedTick,
    rate: ClockRate,
) -> Result<i128, Error> {
    let elapsed = i128::from(at.as_nanos()).saturating_sub(i128::from(frontier.at.as_nanos()));
    let hz = i128::from(rate.hz());
    let predicted = elapsed.saturating_mul(hz).div_euclid(NS_PER_SECOND);
    let allowance_ns = elapsed
        .unsigned_abs()
        .saturating_mul(u128::from(CONFIG.max_drift_ppm))
        .div_euclid(1_000_000)
        .saturating_add(u128::from(REORDER_NS));
    let allowance = allowance_ns
        .saturating_mul(u128::from(rate.hz()))
        .div_euclid(1_000_000_000);
    if allowance >= 2_147_483_648 {
        return Err(Error::Ambiguous);
    }
    let pivot = frontier
        .tick
        .checked_add(predicted)
        .ok_or(Error::Overflow)?;
    let tick = nearest_residue(i128::from(raw), pivot, NTP_ERA_SECONDS)?;
    if tick.abs_diff(pivot) > allowance {
        return Err(Error::OutOfWindow);
    }
    Ok(tick)
}

#[cfg(test)]
mod tests;

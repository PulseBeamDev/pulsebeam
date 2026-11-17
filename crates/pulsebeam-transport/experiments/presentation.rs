//! Pure offered-timeline-v1 measurement. Inputs are validated decoder-ready facts.
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct CaptureId {
    pub source: u64,
    pub lifetime: u64,
    pub epoch: u64,
    pub capture: u64,
}
impl CaptureId {
    fn scope(self) -> (u64, u64, u64) {
        (self.source, self.lifetime, self.epoch)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Capture {
    pub id: CaptureId,
    pub at_ns: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Layer {
    Q,
    H,
    F,
}

#[derive(Clone, Copy, Debug)]
pub struct Ready {
    pub id: CaptureId,
    pub layer: Layer,
    pub at_ns: u64,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub horizon_ns: u64,
    pub cutoff_ns: u64,
    pub tick_ns: u64,
    pub deadline_ns: u64,
    pub freeze_threshold_ns: u64,
    pub max_ticks: u64,
    pub evidence_complete: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeStatus {
    Timely,
    Late,
    Missing,
    Unknown,
}

pub fn decode_status(
    capture_ns: u64,
    ready_ns: Option<u64>,
    deadline_ns: u64,
    evidence_complete: bool,
) -> DecodeStatus {
    match ready_ns {
        Some(time) if time.saturating_sub(capture_ns) <= deadline_ns => DecodeStatus::Timely,
        Some(_) => DecodeStatus::Late,
        None if evidence_complete => DecodeStatus::Missing,
        None => DecodeStatus::Unknown,
    }
}

/// Indices refer to the caller's offered/ready slices, including for pixel scoring.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Tick {
    pub at_ns: u64,
    pub duration_ns: u64,
    pub reference: usize,
    pub ready: Option<usize>,
    pub held: bool,
    pub age_ns: Option<u64>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub ticks: Vec<Tick>,
    pub decode: Vec<DecodeStatus>,
    pub qualified: bool,
    pub startup_blank_ns: u64,
    pub blank_ns: u64,
    pub presented_ns: u64,
    pub held_ns: u64,
    /// Entire held runs which reach the threshold, including pre-threshold time.
    pub freeze_ns: u64,
    pub longest_freeze_ns: u64,
    pub freezes: u64,
    pub age_ns_times_duration_ns: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Invalid {
    Policy,
    TickBudget,
    CaptureOrder,
    ReusedScope,
    UnknownCapture,
    ObservationTime,
    DuplicateSequence,
    Overflow,
}

impl Report {
    fn finish_hold(&mut self, held: u64, threshold: u64) -> Result<(), Invalid> {
        if held >= threshold {
            self.freeze_ns = self.freeze_ns.checked_add(held).ok_or(Invalid::Overflow)?;
            self.longest_freeze_ns = self.longest_freeze_ns.max(held);
            self.freezes = self.freezes.checked_add(1).ok_or(Invalid::Overflow)?;
        }
        Ok(())
    }
}

#[allow(
    clippy::indexing_slicing,
    reason = "indices originate in validated nonempty capture and observation slices"
)]
pub fn measure(captures: &[Capture], ready: &[Ready], policy: Policy) -> Result<Report, Invalid> {
    if policy.horizon_ns == 0
        || policy.cutoff_ns < policy.horizon_ns
        || policy.tick_ns == 0
        || policy.freeze_threshold_ns == 0
    {
        return Err(Invalid::Policy);
    }
    let tick_count = policy.horizon_ns.div_ceil(policy.tick_ns);
    if tick_count > policy.max_ticks {
        return Err(Invalid::TickBudget);
    }
    if captures.first().map(|capture| capture.at_ns) != Some(0) {
        return Err(Invalid::CaptureOrder);
    }
    let mut indices = BTreeMap::new();
    let mut scopes = BTreeSet::new();
    let mut previous: Option<&Capture> = None;
    for (index, capture) in captures.iter().enumerate() {
        if capture.at_ns >= policy.horizon_ns
            || previous.is_some_and(|last| {
                capture.at_ns <= last.at_ns || capture.id.capture <= last.id.capture
            })
        {
            return Err(Invalid::CaptureOrder);
        }
        if previous.is_none_or(|last| last.id.scope() != capture.id.scope())
            && !scopes.insert(capture.id.scope())
        {
            return Err(Invalid::ReusedScope);
        }
        if indices.insert(capture.id, index).is_some() {
            return Err(Invalid::CaptureOrder);
        }
        previous = Some(capture);
    }
    let mut first = vec![None; captures.len()];
    let mut sequences = BTreeSet::new();
    let mut arrivals = Vec::with_capacity(ready.len());
    for (index, observation) in ready.iter().enumerate() {
        let &capture = indices
            .get(&observation.id)
            .ok_or(Invalid::UnknownCapture)?;
        if observation.at_ns < captures[capture].at_ns || observation.at_ns > policy.cutoff_ns {
            return Err(Invalid::ObservationTime);
        }
        if !sequences.insert(observation.sequence) {
            return Err(Invalid::DuplicateSequence);
        }
        let time = &mut first[capture];
        *time = Some(time.map_or(observation.at_ns, |old: u64| old.min(observation.at_ns)));
        arrivals.push((observation.at_ns, observation.sequence, index, capture));
    }
    arrivals.sort_unstable();
    let decode = captures
        .iter()
        .zip(first)
        .map(|(capture, time)| {
            decode_status(
                capture.at_ns,
                time,
                policy.deadline_ns,
                policy.evidence_complete,
            )
        })
        .collect();
    let mut report = Report {
        decode,
        qualified: policy.evidence_complete,
        ..Report::default()
    };
    let mut offered = 0usize;
    let mut arrival = 0usize;
    let mut selected: Option<(usize, usize)> = None;
    let mut previous_display: Option<CaptureId> = None;
    let mut ever_displayed = false;
    let mut held_run = 0u64;
    let mut tick = 0u64;
    while tick < policy.horizon_ns {
        while let Some(next) = captures.get(offered.checked_add(1).ok_or(Invalid::Overflow)?) {
            if next.at_ns > tick {
                break;
            }
            offered = offered.checked_add(1).ok_or(Invalid::Overflow)?;
        }
        let scope = captures[offered].id.scope();
        if selected.is_some_and(|(_, capture)| captures[capture].id.scope() != scope) {
            selected = None;
        }
        while let Some(&(at, _, observation, capture)) = arrivals.get(arrival) {
            if at > tick {
                break;
            }
            if captures[capture].id.scope() == scope
                && selected.is_none_or(|(old, old_capture)| {
                    (capture, ready[observation].layer) > (old_capture, ready[old].layer)
                })
            {
                selected = Some((observation, capture));
            }
            arrival = arrival.checked_add(1).ok_or(Invalid::Overflow)?;
        }
        let duration = policy.tick_ns.min(
            policy
                .horizon_ns
                .checked_sub(tick)
                .ok_or(Invalid::Overflow)?,
        );
        let displayed = selected.map(|(_, capture)| captures[capture].id);
        let held = displayed.is_some() && displayed == previous_display;
        if held {
            held_run = held_run.checked_add(duration).ok_or(Invalid::Overflow)?;
            report.held_ns = report
                .held_ns
                .checked_add(duration)
                .ok_or(Invalid::Overflow)?;
        } else {
            report.finish_hold(held_run, policy.freeze_threshold_ns)?;
            held_run = 0;
        }
        let age = if let Some((_, capture)) = selected {
            let age = tick
                .checked_sub(captures[capture].at_ns)
                .ok_or(Invalid::ObservationTime)?;
            report.presented_ns = report
                .presented_ns
                .checked_add(duration)
                .ok_or(Invalid::Overflow)?;
            report.age_ns_times_duration_ns = report
                .age_ns_times_duration_ns
                .checked_add(
                    u128::from(age)
                        .checked_mul(u128::from(duration))
                        .ok_or(Invalid::Overflow)?,
                )
                .ok_or(Invalid::Overflow)?;
            ever_displayed = true;
            Some(age)
        } else {
            report.blank_ns = report
                .blank_ns
                .checked_add(duration)
                .ok_or(Invalid::Overflow)?;
            if !ever_displayed {
                report.startup_blank_ns = report
                    .startup_blank_ns
                    .checked_add(duration)
                    .ok_or(Invalid::Overflow)?;
            }
            None
        };
        report.ticks.push(Tick {
            at_ns: tick,
            duration_ns: duration,
            reference: offered,
            ready: selected.map(|(observation, _)| observation),
            held,
            age_ns: age,
        });
        previous_display = displayed;
        tick = tick.checked_add(duration).ok_or(Invalid::Overflow)?;
    }
    report.finish_hold(held_run, policy.freeze_threshold_ns)?;
    Ok(report)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "small authored event sequences with asserted exact outcomes"
)]
mod tests {
    use super::*;

    fn captures(count: u64) -> Vec<Capture> {
        (0..count)
            .map(|capture| Capture {
                id: CaptureId {
                    source: 0,
                    lifetime: 0,
                    epoch: 0,
                    capture,
                },
                at_ns: capture * 100,
            })
            .collect()
    }
    fn ready(capture: Capture, layer: Layer, at_ns: u64, sequence: u64) -> Ready {
        Ready {
            id: capture.id,
            layer,
            at_ns,
            sequence,
        }
    }
    fn policy(horizon_ns: u64) -> Policy {
        Policy {
            horizon_ns,
            cutoff_ns: 600,
            tick_ns: 100,
            deadline_ns: 100,
            freeze_threshold_ns: 200,
            max_ticks: 10,
            evidence_complete: true,
        }
    }

    #[test]
    fn startup_interior_missing_late_reorder_and_drain_keep_the_offered_denominator() {
        let c = captures(5);
        let outputs = [
            ready(c[2], Layer::Q, 250, 2),
            ready(c[0], Layer::Q, 50, 0),
            ready(c[1], Layer::Q, 350, 1),
            ready(c[4], Layer::F, 550, 3),
        ];
        let r = measure(&c, &outputs, policy(500)).unwrap();
        assert_eq!(
            r.decode,
            [
                DecodeStatus::Timely,
                DecodeStatus::Late,
                DecodeStatus::Timely,
                DecodeStatus::Missing,
                DecodeStatus::Late
            ]
        );
        assert!(r.qualified);
        assert_eq!(
            (r.startup_blank_ns, r.blank_ns, r.presented_ns, r.held_ns),
            (100, 100, 400, 200)
        );
        assert_eq!(
            r.ticks.iter().map(|t| t.ready).collect::<Vec<_>>(),
            [None, Some(1), Some(1), Some(0), Some(0)]
        );
        assert_eq!(r.ticks.last().unwrap().age_ns, Some(200));
        assert_eq!(r.ticks.last().unwrap().reference, 4);
        assert_eq!(r.freezes, 0);
    }

    #[test]
    fn upgrades_and_duplicates_do_not_reset_age_freeze_or_multiply_captures() {
        let c = captures(1);
        let outputs = [
            ready(c[0], Layer::Q, 0, 0),
            ready(c[0], Layer::H, 150, 1),
            ready(c[0], Layer::F, 200, 2),
            ready(c[0], Layer::Q, 250, 3),
            ready(c[0], Layer::F, 350, 4),
        ];
        let r = measure(&c, &outputs, policy(450)).unwrap();
        assert_eq!(r.decode, [DecodeStatus::Timely]);
        assert_eq!((r.blank_ns, r.presented_ns, r.held_ns), (0, 450, 350));
        assert_eq!((r.freezes, r.freeze_ns, r.longest_freeze_ns), (1, 350, 350));
        assert_eq!(r.age_ns_times_duration_ns, 80_000);
        assert_eq!(
            r.ticks.iter().map(|t| t.ready).collect::<Vec<_>>(),
            [Some(0), Some(0), Some(2), Some(2), Some(2)]
        );
        assert_eq!(r.ticks.last().unwrap().duration_ns, 50);
    }

    #[test]
    fn reset_fences_held_and_late_old_output_without_erasing_old_captures() {
        let mut c = captures(3);
        c[2].id.epoch = 1;
        let outputs = [
            ready(c[0], Layer::F, 0, 0),
            ready(c[1], Layer::F, 300, 1),
            ready(c[2], Layer::Q, 350, 2),
        ];
        let r = measure(&c, &outputs, policy(500)).unwrap();
        assert_eq!(
            r.ticks.iter().map(|t| t.ready).collect::<Vec<_>>(),
            [Some(0), Some(0), None, None, Some(2)]
        );
        assert_eq!((r.startup_blank_ns, r.blank_ns, r.held_ns), (0, 200, 100));
        assert_eq!(
            r.decode,
            [DecodeStatus::Timely, DecodeStatus::Late, DecodeStatus::Late]
        );
        let mut replacement = c.clone();
        replacement[2].id.lifetime = 1;
        replacement[2].id.source = 9;
        let outputs = [
            outputs[0],
            outputs[1],
            ready(replacement[2], Layer::Q, 350, 2),
        ];
        assert_eq!(
            measure(&replacement, &outputs, policy(500)).unwrap().ticks,
            r.ticks
        );
    }

    #[test]
    fn blank_tail_unknown_evidence_and_budget_are_explicit() {
        let c = captures(3);
        let r = measure(&c, &[], policy(250)).unwrap();
        assert_eq!(
            (r.startup_blank_ns, r.blank_ns, r.presented_ns, r.freezes),
            (250, 250, 0, 0)
        );
        assert_eq!(r.decode, [DecodeStatus::Missing; 3]);
        let incomplete = measure(
            &c,
            &[],
            Policy {
                evidence_complete: false,
                ..policy(250)
            },
        )
        .unwrap();
        assert!(!incomplete.qualified);
        assert_eq!(incomplete.decode, [DecodeStatus::Unknown; 3]);
        assert_eq!(
            measure(
                &c,
                &[],
                Policy {
                    max_ticks: 2,
                    ..policy(250)
                }
            )
            .unwrap_err(),
            Invalid::TickBudget
        );
        assert_eq!(
            measure(
                &c,
                &[],
                Policy {
                    tick_ns: 0,
                    ..policy(250)
                }
            )
            .unwrap_err(),
            Invalid::Policy
        );
    }

    #[test]
    fn invalid_identity_time_sequence_and_capture_order_fail_closed() {
        let c = captures(3);
        assert_eq!(
            measure(&c, &[ready(c[1], Layer::Q, 99, 0)], policy(300)).unwrap_err(),
            Invalid::ObservationTime
        );
        assert_eq!(
            measure(&c, &[ready(c[0], Layer::Q, 601, 0)], policy(300)).unwrap_err(),
            Invalid::ObservationTime
        );
        assert_eq!(
            measure(&c, &[ready(captures(4)[3], Layer::Q, 400, 0)], policy(300)).unwrap_err(),
            Invalid::UnknownCapture
        );
        assert_eq!(
            measure(
                &c,
                &[ready(c[0], Layer::Q, 0, 0), ready(c[1], Layer::Q, 100, 0)],
                policy(300)
            )
            .unwrap_err(),
            Invalid::DuplicateSequence
        );
        assert_eq!(
            measure(&c[1..], &[], policy(300)).unwrap_err(),
            Invalid::CaptureOrder
        );
        let mut reused = c;
        reused[1].id.epoch = 1;
        assert_eq!(
            measure(&reused, &[], policy(300)).unwrap_err(),
            Invalid::ReusedScope
        );
    }
}

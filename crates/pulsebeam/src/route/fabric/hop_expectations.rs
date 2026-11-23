use std::collections::BTreeMap;

use pulsebeam_runtime::time::{Duration, Instant};

use super::local::{DataExpectation, PropagationOutcome, RepairFragment};

const LIMIT: usize = 64;
const WINDOW: u64 = 512;
const RETRY: Duration = Duration::from_millis(100);

pub(crate) trait Path: Clone {
    fn matches(&self, other: &Self) -> bool;
    fn deadline(&self) -> Instant;
}

pub(crate) struct ExpectedUnits<P, V> {
    entries: BTreeMap<u64, Unit<P, V>>,
}

impl<P, V> Default for ExpectedUnits<P, V> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
}

struct Unit<P, V> {
    parent: P,
    data: DataExpectation,
    expires: Instant,
    value: V,
}

impl<P: Copy + Eq, V> ExpectedUnits<P, V> {
    pub(crate) fn admit(
        &mut self,
        now: Instant,
        parent: P,
        data: DataExpectation,
        expires: Instant,
        value: impl FnOnce() -> V,
    ) -> bool {
        let seq = data.unit_seq;
        if self.entries.contains_key(&seq) {
            return self.contains(parent, data, now);
        }
        let high = self
            .entries
            .last_key_value()
            .map_or(seq, |(&high, _)| high.max(seq));
        let floor = high.saturating_sub(WINDOW - 1);
        self.entries
            .retain(|&key, unit| key >= floor && now < unit.expires);
        if self.entries.len() >= LIMIT || now >= expires || seq < floor {
            return false;
        }
        self.entries.insert(
            seq,
            Unit {
                parent,
                data,
                expires,
                value: value(),
            },
        );
        true
    }

    pub(crate) fn contains(&self, parent: P, data: DataExpectation, now: Instant) -> bool {
        self.entries.get(&data.unit_seq).is_some_and(|entry| {
            entry.parent == parent && entry.data == data && now < entry.expires
        })
    }

    pub(crate) fn parent(&self, fragment: RepairFragment, now: Instant) -> Option<P> {
        let entry = self.entries.get(&fragment.unit_seq)?;
        (fragment.index < entry.data.fragment_count && now < entry.expires).then_some(entry.parent)
    }
}

pub(crate) struct HopExpectations<P, R> {
    units: ExpectedUnits<P, Vec<Hop<R>>>,
}

impl<P, R> Default for HopExpectations<P, R> {
    fn default() -> Self {
        Self {
            units: ExpectedUnits::default(),
        }
    }
}

struct Hop<R> {
    route: R,
    expires: Instant,
    attempt: Option<Instant>,
    attempted: bool,
    outcome: Option<PropagationOutcome>,
}

#[derive(Debug)]
pub(crate) struct Notice<R> {
    pub route: Box<R>,
    pub unit: DataExpectation,
    pub expires: Instant,
}

#[derive(Debug)]
pub(crate) enum Step<R> {
    Send(Notice<R>),
    Settled(PropagationOutcome),
}

impl<P: Copy + Eq, R: Path> HopExpectations<P, R> {
    pub(crate) fn seed<'a>(
        &mut self,
        now: Instant,
        parent: P,
        data: DataExpectation,
        expires: Instant,
        routes: impl Iterator<Item = &'a R>,
    ) -> bool
    where
        R: 'a,
    {
        self.units.admit(now, parent, data, expires, || {
            routes
                .map(|route| Hop {
                    route: route.clone(),
                    expires: expires.min(route.deadline()),
                    attempt: Some(now),
                    attempted: false,
                    outcome: None,
                })
                .collect()
        })
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.units
            .entries
            .values()
            .flat_map(|unit| unit.value.iter().filter_map(|hop| hop.attempt))
            .min()
    }

    pub(crate) fn tick(
        &mut self,
        now: Instant,
        is_live: impl FnOnce(&R) -> bool,
    ) -> Option<Step<R>> {
        let (unit, index, _) = self
            .units
            .entries
            .values_mut()
            .filter_map(|unit| {
                let (index, due) = unit
                    .value
                    .iter()
                    .enumerate()
                    .filter_map(|(index, hop)| Some((index, hop.attempt?)))
                    .min_by_key(|(_, due)| *due)?;
                (now >= due).then_some((unit, index, due))
            })
            .min_by_key(|(unit, _, due)| (*due, unit.data.unit_seq))?;
        let hop = unit.value.get_mut(index)?;
        let outcome = if now >= hop.expires {
            PropagationOutcome::Unknown
        } else if !is_live(&hop.route) {
            hop.attempt = Some(hop.expires);
            return None;
        } else {
            hop.attempted = true;
            hop.attempt = Some(
                now.checked_add(RETRY)
                    .unwrap_or(hop.expires)
                    .min(hop.expires),
            );
            return Some(Step::Send(Notice {
                route: Box::new(hop.route.clone()),
                unit: unit.data,
                expires: hop.expires,
            }));
        };
        hop.attempt = None;
        hop.outcome = Some(outcome);
        Some(Step::Settled(outcome))
    }

    pub(crate) fn observe(
        &mut self,
        now: Instant,
        route: &R,
        data: DataExpectation,
        outcome: PropagationOutcome,
    ) -> bool {
        let Some(unit) = self
            .units
            .entries
            .get_mut(&data.unit_seq)
            .filter(|unit| unit.data == data)
        else {
            return false;
        };
        let Some(hop) = unit.value.iter_mut().find(|hop| {
            hop.route.matches(route) && hop.attempt.is_some() && hop.attempted && now < hop.expires
        }) else {
            return false;
        };
        hop.attempt = None;
        hop.outcome = Some(outcome);
        true
    }

    pub(crate) fn confirmed_window(
        &self,
        route: &R,
        fragment: RepairFragment,
        now: Instant,
    ) -> Option<(Instant, u16)> {
        let unit = self.units.entries.get(&fragment.unit_seq)?;
        if fragment.index >= unit.data.fragment_count {
            return None;
        }
        unit.value
            .iter()
            .find(|hop| {
                hop.route.matches(route)
                    && hop.outcome == Some(PropagationOutcome::Confirmed)
                    && now < hop.expires
            })
            .map(|hop| (hop.expires, unit.data.fragment_count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Route(Instant, u8);

    impl Path for Route {
        fn matches(&self, other: &Self) -> bool {
            self.1 == other.1
        }
        fn deadline(&self) -> Instant {
            self.0
        }
    }

    #[test]
    fn replay_does_not_add_or_confirm_a_replacement_hop() {
        let now = Instant::now();
        let expires = now + Duration::from_secs(1);
        let original = Route(expires, 1);
        let replacement = Route(expires, 2);
        let data = DataExpectation {
            unit_seq: 12,
            fragment_count: 2,
        };
        let fragment = RepairFragment {
            unit_seq: data.unit_seq,
            index: 1,
        };
        let mut publisher = HopExpectations::default();
        assert!(publisher.seed(now, (), data, expires, std::iter::once(&original)));
        assert!(!publisher.observe(now, &original, data, PropagationOutcome::Confirmed));
        assert!(publisher.seed(now, (), data, expires, std::iter::once(&replacement)));
        assert!(
            matches!(publisher.tick(now, |_| true), Some(Step::Send(notice)) if notice.route.matches(&original))
        );
        assert!(!publisher.observe(now, &replacement, data, PropagationOutcome::Confirmed));
        assert!(publisher.observe(now, &original, data, PropagationOutcome::Confirmed));
        assert_eq!(
            publisher.confirmed_window(&original, fragment, now),
            Some((expires, 2))
        );
        assert_eq!(
            publisher.confirmed_window(&replacement, fragment, now),
            None
        );
        assert!(publisher.deadline().is_none());
    }
}

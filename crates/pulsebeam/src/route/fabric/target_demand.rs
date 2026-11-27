use std::num::NonZeroU64;

use pulsebeam_core::dd::evidence::{ProjectionContext, TargetSet};
use pulsebeam_runtime::time::{Duration, Instant};

pub(crate) const MAX_LEASE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Projection {
    Whole,
    Targets {
        context: ProjectionContext,
        targets: TargetSet,
    },
}

impl Projection {
    pub(crate) fn targets(context: ProjectionContext, targets: TargetSet) -> Option<Self> {
        (targets.mask() & !context.active_targets == 0)
            .then_some(Self::Targets { context, targets })
    }

    pub(crate) fn union(self, other: Self) -> Self {
        match (self, other) {
            (
                Self::Targets { context, targets },
                Self::Targets {
                    context: other_context,
                    targets: other_targets,
                },
            ) if context == other_context => Self::Targets {
                context,
                targets: targets.union(other_targets),
            },
            _ => Self::Whole,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Update {
    Lease {
        projection: Projection,
        expires: Instant,
    },
    Withdraw,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct VersionedUpdate {
    pub generation: NonZeroU64,
    pub update: Update,
}

/// State of one authorized, directed live route. Replacing the route replaces
/// this state; renewing, withdrawing or expiring its demand does not.
#[derive(Default)]
pub(crate) struct Demand {
    generation: Option<NonZeroU64>,
    current: Option<(Projection, Instant)>,
}

impl Demand {
    pub(crate) fn apply(
        &mut self,
        now: Instant,
        route_expires: Instant,
        generation: NonZeroU64,
        update: Update,
    ) -> bool {
        if now >= route_expires
            || self
                .generation
                .is_some_and(|previous| generation <= previous)
        {
            return false;
        }
        let current = match update {
            Update::Lease {
                projection,
                expires,
            } if now < expires
                && expires <= route_expires
                && expires.saturating_duration_since(now) <= MAX_LEASE =>
            {
                Some((projection, expires))
            }
            Update::Lease { .. } => return false,
            Update::Withdraw => None,
        };
        self.generation = Some(generation);
        self.current = current;
        true
    }

    pub(crate) fn live(&self, now: Instant) -> Option<Projection> {
        let (projection, expires) = self.current?;
        (now < expires).then_some(projection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unions_only_comparable_evidence_and_respects_whole_demand() {
        let context = ProjectionContext {
            attachment: 1,
            active_targets: 0b111,
        };
        let make = |context, mask| Projection::targets(context, TargetSet::new(mask).unwrap());
        let base = make(context, 0b100).unwrap();
        let other = make(context, 0b001).unwrap();
        assert_eq!(base.union(other), make(context, 0b101).unwrap());
        assert_eq!(base.union(Projection::Whole), Projection::Whole);
        let replacement = ProjectionContext {
            attachment: 9,
            ..context
        };
        assert_eq!(
            base.union(make(replacement, 0b001).unwrap()),
            Projection::Whole
        );
        assert_eq!(make(context, 0b1000), None);
    }

    #[test]
    fn expiration_and_withdrawal_do_not_reopen_an_old_generation() {
        let mut demand = Demand::default();
        let now = Instant::now();
        let lease_expires = now + MAX_LEASE;
        let route_expires = now + MAX_LEASE * 3;
        let generation = |n| NonZeroU64::new(n).unwrap();
        let lease = |expires| Update::Lease {
            projection: Projection::Whole,
            expires,
        };

        assert!(!demand.apply(now, route_expires, generation(10), lease(route_expires)));
        assert!(demand.apply(now, route_expires, generation(1), lease(lease_expires)));
        assert_eq!(demand.live(now), Some(Projection::Whole));
        assert_eq!(demand.live(lease_expires), None);
        let renewed = lease(lease_expires + MAX_LEASE);
        assert!(!demand.apply(lease_expires, route_expires, generation(1), renewed));
        assert!(demand.apply(lease_expires, route_expires, generation(2), renewed));
        assert!(demand.apply(
            lease_expires,
            route_expires,
            generation(3),
            Update::Withdraw
        ));
        assert!(!demand.apply(lease_expires, route_expires, generation(2), renewed));
        assert_eq!(demand.live(lease_expires), None);
        assert!(!demand.apply(
            route_expires,
            route_expires,
            generation(4),
            Update::Withdraw
        ));
    }
}

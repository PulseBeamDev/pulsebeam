use std::time::Duration;

use pulsebeam_link::BindingCutoff;
use pulsebeam_runtime::time::Instant;

use crate::rtp::monitor::{MAX_LADDER_TARGETS, MeasuredTargetLadder};

#[derive(Clone, Debug)]
pub(super) struct Lease {
    pub(super) cutoff: BindingCutoff,
    fresh_until: Instant,
    bps: u32,
    rising: Option<(Instant, u32)>,
}

impl Lease {
    pub(super) fn new(cutoff: BindingCutoff, bps: u32, expires: Instant) -> Self {
        Self {
            cutoff,
            fresh_until: expires,
            bps,
            rising: None,
        }
    }

    pub(super) fn applies(&self) -> bool {
        // A missing reply is not evidence that the hop can carry an upgrade.
        // Preserve the previous restriction until new feedback or a route fence.
        self.cutoff.is_live()
    }

    pub(super) fn refreshed(&self, bps: u32, expires: Instant, now: Instant) -> Self {
        let mut next = self.clone();
        next.fresh_until = expires;
        if bps <= self.bps {
            next.bps = bps;
            next.rising = None;
        } else {
            let (since, floor) = self
                .rising
                .filter(|_| now < self.fresh_until)
                .map_or((now, bps), |(since, floor)| (since, floor.min(bps)));
            if now.saturating_duration_since(since) >= Duration::from_millis(400) {
                next.bps = floor;
                next.rising = None;
            } else {
                next.rising = Some((since, floor));
            }
        }
        next
    }

    pub(super) fn restrict(&self, ladder: MeasuredTargetLadder) -> MeasuredTargetLadder {
        restrict(ladder, self.bps)
    }
}

fn restrict(mut ladder: MeasuredTargetLadder, bps: u32) -> MeasuredTargetLadder {
    let active = |index: usize| ladder.context.active_targets & (1 << index) != 0;
    let measured = || {
        (0..MAX_LADDER_TARGETS)
            .filter(|&index| active(index))
            .filter_map(|index| Some((index, ladder.target(index)?)))
    };
    let base = measured()
        .min_by_key(|&(_, geometry)| geometry)
        .map(|(index, _)| index);
    ladder.context.active_targets = measured()
        .filter(|&(index, (_, _, rate))| base == Some(index) || rate <= bps)
        .map(|(index, _)| index)
        .fold(0, |mask, index| mask | (1 << index));
    ladder
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::fabric::local::DecodeTargetRateContext;

    #[test]
    fn budget_filters_measured_cost_but_retains_base_by_geometry() {
        let ladder = MeasuredTargetLadder {
            context: DecodeTargetRateContext {
                structure_origin: 2,
                active_targets: 0b111,
            },
            through: 4,
            heights: [720, 180, 360],
            payload_bps: [800_000, 250_000, 460_000],
            fps: [30; 3],
        };
        assert_eq!(restrict(ladder, 200_000).context.active_targets, 0b010);
        assert_eq!(restrict(ladder, 500_000).context.active_targets, 0b110);
        assert_eq!(restrict(ladder, 820_000), ladder);
    }
}

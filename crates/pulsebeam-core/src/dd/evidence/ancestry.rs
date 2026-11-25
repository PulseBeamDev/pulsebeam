use std::collections::{BTreeMap, BTreeSet};

use arrayvec::ArrayVec;

use super::{Qualified, SEQUENCE_HISTORY};
use crate::dd::{DecodeTargetIndication, FrameDependencyTemplate, MAX_DECODE_TARGETS};

#[derive(Default)]
pub(super) struct Ancestry {
    packets: BTreeMap<u64, u64>,
    frames: BTreeMap<u64, AdmittedFrame>,
}

struct AdmittedFrame {
    start: Option<u64>,
    end: Option<u64>,
    initialized: bool,
    active_targets: u32,
    dependencies: FrameDependencyTemplate,
    chains: ArrayVec<u8, MAX_DECODE_TARGETS>,
}

impl Ancestry {
    pub(super) fn prune(&mut self, oldest_frame: u64, highest_sequence: u64) {
        while self
            .frames
            .first_key_value()
            .is_some_and(|(&frame, _)| frame < oldest_frame)
        {
            self.frames.pop_first();
        }
        while self
            .packets
            .first_key_value()
            .is_some_and(|(&sequence, _)| {
                highest_sequence.saturating_sub(sequence) >= SEQUENCE_HISTORY
            })
        {
            self.packets.pop_first();
        }
    }

    pub(super) fn record(
        &mut self,
        number: u64,
        sequence: u64,
        evidence: &Qualified,
        initialized: bool,
    ) {
        let descriptor = &evidence.descriptor;
        let frame = self.frames.entry(number).or_insert_with(|| AdmittedFrame {
            start: None,
            end: None,
            initialized: false,
            active_targets: evidence.active_targets,
            dependencies: descriptor.frame_dependencies.clone(),
            chains: evidence.target_chains.clone(),
        });
        frame.active_targets &= evidence.active_targets;
        if descriptor.start_of_frame {
            frame.start.get_or_insert(sequence);
            frame.initialized |= initialized;
        }
        if descriptor.end_of_frame {
            frame.end.get_or_insert(sequence);
        }
        self.packets.insert(sequence, number);
    }

    pub(super) fn supports_switch(&self, number: u64, evidence: &Qualified, target: usize) -> bool {
        let mut credited = BTreeSet::new();
        for (&ancestor, frame) in self.frames.range(..number) {
            if frame.active_targets & (1_u32 << target) == 0
                || frame
                    .dependencies
                    .dtis
                    .get(target)
                    .is_none_or(|dti| *dti == DecodeTargetIndication::NotPresent)
                || !self.complete(ancestor, frame)
            {
                continue;
            }
            if frame.initialized
                || (!frame.dependencies.frame_diffs.is_empty()
                    && proves_dependencies(
                        ancestor,
                        &frame.dependencies,
                        &frame.chains,
                        target,
                        &credited,
                    ))
            {
                credited.insert(ancestor);
            }
        }
        proves_dependencies(
            number,
            &evidence.descriptor.frame_dependencies,
            &evidence.target_chains,
            target,
            &credited,
        ) && !credited.is_empty()
    }

    fn complete(&self, number: u64, frame: &AdmittedFrame) -> bool {
        let (Some(start), Some(end)) = (frame.start, frame.end) else {
            return false;
        };
        let Some(width) = end.checked_sub(start).and_then(|span| span.checked_add(1)) else {
            return false;
        };
        width <= SEQUENCE_HISTORY
            && u64::try_from(
                self.packets
                    .range(start..=end)
                    .take_while(|(_, frame)| **frame == number)
                    .count(),
            )
            .ok()
                == Some(width)
    }
}

fn proves_dependencies(
    number: u64,
    dependencies: &FrameDependencyTemplate,
    chains: &[u8],
    target: usize,
    credited: &BTreeSet<u64>,
) -> bool {
    dependencies_satisfied(dependencies, chains, target, |difference| {
        number
            .checked_sub(u64::from(difference))
            .is_some_and(|parent| credited.contains(&parent))
    })
}

/// Frame and protecting-chain references must all be backed by credited ancestors.
pub fn dependencies_satisfied(
    dependencies: &FrameDependencyTemplate,
    chains: &[u8],
    target: usize,
    mut has: impl FnMut(u16) -> bool,
) -> bool {
    let chain_proven = if dependencies.chain_diffs.is_empty() {
        true
    } else {
        chains
            .get(target)
            .and_then(|chain| dependencies.chain_diffs.get(usize::from(*chain)))
            .is_some_and(|diff| *diff == 0 || has(u16::from(*diff)))
    };
    chain_proven && dependencies.frame_diffs.iter().copied().all(has)
}

use std::num::NonZeroU32;

use super::{DecodeTarget, FrameStatus, Qualified};
use crate::dd::{DecodeTargetIndication, MAX_DECODE_TARGETS};

/// A nonempty union of the dependency targets requested on one hop. Target
/// indices are labels, not evidence of a quality or dependency ordering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetSet(NonZeroU32);

impl TargetSet {
    pub fn new(mask: u32) -> Option<Self> {
        let mask = NonZeroU32::new(mask)?;
        let top = usize::try_from(mask.get().ilog2()).ok()?;
        (top < MAX_DECODE_TARGETS).then_some(Self(mask))
    }

    pub fn single(target: DecodeTarget) -> Self {
        let bit = NonZeroU32::MIN.get().rotate_left(u32::from(target.index()));
        Self(NonZeroU32::new(bit).unwrap_or(NonZeroU32::MIN))
    }

    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn mask(self) -> u32 {
        self.0.get()
    }
}

/// An exact source attachment observed by the requesting receiver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectionContext {
    pub attachment: u64,
    pub active_targets: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct ProjectionEvidence {
    origin: u64,
    attachment: u64,
    active: u32,
    absent: u32,
}

impl ProjectionEvidence {
    pub fn new(qualified: &Qualified, frame: FrameStatus) -> Option<Self> {
        let dd = &qualified.descriptor;
        if !matches!(frame, FrameStatus::Resolved(_))
            || dd.attached_structure.is_some()
            || dd.active_decode_targets.is_some()
        {
            return None;
        }
        let absent =
            dd.frame_dependencies
                .dtis
                .iter()
                .enumerate()
                .fold(0, |mask, (index, &dti)| {
                    if dti == DecodeTargetIndication::NotPresent {
                        mask | (1 << index)
                    } else {
                        mask
                    }
                })
                & qualified.active_targets;
        (absent != 0).then_some(Self {
            origin: qualified.structure_origin,
            attachment: qualified.structure_sequence,
            active: qualified.active_targets,
            absent,
        })
    }

    /// True only if qualified DD explicitly excludes the frame from every
    /// requested dependency target. The caller still needs to preserve source
    /// position and prove omissions to the receiving hop.
    pub fn excludes(self, observed: ProjectionContext, targets: TargetSet) -> bool {
        // A receiver may have joined at a later copy of an identical
        // attachment. Only match it within this proven contiguous run.
        (self.origin..=self.attachment).contains(&observed.attachment)
            && self.active == observed.active_targets
            && targets.mask() & !self.absent == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dd::{DependencyDescriptor, FrameDependencyTemplate};
    use DecodeTargetIndication::{NotPresent, Required, Switch};

    impl TargetSet {
        fn excludes(
            self,
            origin: u64,
            active: u32,
            qualified: &Qualified,
            frame: FrameStatus,
        ) -> bool {
            ProjectionEvidence::new(qualified, frame).is_some_and(|evidence| {
                evidence.excludes(
                    ProjectionContext {
                        attachment: origin,
                        active_targets: active,
                    },
                    self,
                )
            })
        }
    }

    #[test]
    fn combines_incomparable_targets_and_keeps_unknown_or_context_updates() {
        let first = TargetSet::single(DecodeTarget::new(0).unwrap());
        let third = TargetSet::single(DecodeTarget::new(2).unwrap());
        let union = first.union(third);
        assert_eq!(union, TargetSet::new(0b101).unwrap());
        assert!(TargetSet::new(0).is_none());

        let mut evidence = Qualified {
            descriptor: DependencyDescriptor {
                frame_dependencies: FrameDependencyTemplate {
                    dtis: [Required, Switch, NotPresent].into_iter().collect(),
                    ..Default::default()
                },
                ..Default::default()
            },
            active_targets: 0b111,
            structure_origin: 12,
            structure_sequence: 20,
            target_chains: Default::default(),
        };
        let resolved = FrameStatus::Resolved(5);
        assert!(third.excludes(12, 0b111, &evidence, resolved));
        assert!(!union.excludes(12, 0b111, &evidence, resolved));
        evidence.descriptor.frame_dependencies.dtis =
            [NotPresent, Switch, Required].into_iter().collect();
        assert!(first.excludes(12, 0b111, &evidence, resolved));
        assert!(!union.excludes(12, 0b111, &evidence, resolved));
        evidence.descriptor.frame_dependencies.dtis[2] = NotPresent;
        assert!(union.excludes(12, 0b111, &evidence, resolved));
        assert!(union.excludes(20, 0b111, &evidence, resolved));
        assert!(!union.excludes(11, 0b111, &evidence, resolved));
        assert!(!union.excludes(21, 0b111, &evidence, resolved));
        evidence.structure_origin = 20;
        assert!(!union.excludes(12, 0b111, &evidence, resolved));
        evidence.structure_origin = 12;
        assert!(!union.excludes(12, 0b101, &evidence, resolved));
        assert!(!union.excludes(12, 0b111, &evidence, FrameStatus::Unresolved));
        evidence.descriptor.active_decode_targets = Some(0b111);
        assert!(!union.excludes(12, 0b111, &evidence, resolved));
    }
}

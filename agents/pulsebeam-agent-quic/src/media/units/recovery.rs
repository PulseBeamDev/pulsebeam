use std::collections::BTreeMap;

use pulsebeam_core::dd::{
    DecodeTargetIndication::{Discardable, NotPresent},
    TemplateDependencyStructure,
    evidence::{FRAME_HISTORY, dependencies_satisfied},
};

use super::{Access, Dependency, Event, Unit};

/// Recovery for complete video units. Recording and standalone depacketization
/// do not use this gate.
pub struct VideoRecovery<S> {
    source: S,
    dd: Option<Box<DescriptorRecovery>>,
    awaiting_entry: bool,
    refresh_taken: bool,
}

#[derive(Debug)]
pub enum Recovery<S> {
    Deliver(Event<S>),
    Withheld(Unit<S>),
    Retired,
}

impl<S: Copy + Eq> VideoRecovery<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            dd: None,
            awaiting_entry: true,
            refresh_taken: false,
        }
    }

    pub fn apply(&mut self, event: Event<S>) -> Recovery<S> {
        match event {
            Event::Discontinuity(gap) if gap.source == self.source => {
                self.lose_context();
                Recovery::Deliver(Event::Discontinuity(gap))
            }
            Event::Unit(unit) if unit.source == self.source => {
                if !matches!(unit.dependency, Dependency::Absent) {
                    self.dd.get_or_insert_with(Box::default);
                }
                if let Some(dd) = &mut self.dd {
                    match dd.admit(&unit) {
                        Admission::Deliver => {}
                        Admission::Omit => return Recovery::Withheld(unit),
                        Admission::NeedsEntry => {
                            self.mark_loss();
                            return Recovery::Withheld(unit);
                        }
                    }
                } else if self.awaiting_entry && unit.access == Access::DependentVideo {
                    return Recovery::Withheld(unit);
                }
                self.awaiting_entry = false;
                Recovery::Deliver(Event::Unit(unit))
            }
            _ => Recovery::Retired,
        }
    }

    /// Fence dependent video after application-side loss, such as overflow.
    pub fn mark_loss(&mut self) {
        if let Some(dd) = &mut self.dd {
            dd.clear_history();
        }
        if !self.awaiting_entry {
            self.awaiting_entry = true;
            self.refresh_taken = false;
        }
    }

    pub(crate) fn lose_context(&mut self) {
        self.mark_loss();
        if let Some(dd) = &mut self.dd {
            dd.structure = None;
        }
    }

    /// Return one generation-scoped refresh request per recovery interval.
    /// Retain it on transport backpressure; no depacketizer sends feedback.
    pub fn take_refresh(&mut self) -> Option<S> {
        if !self.awaiting_entry || self.refresh_taken {
            return None;
        }
        self.refresh_taken = true;
        Some(self.source)
    }
}

#[derive(Default)]
struct DescriptorRecovery {
    structure: Option<Box<TemplateDependencyStructure>>,
    active: u32,
    initialized: u32,
    newest: Option<(u16, u64)>,
    credited: BTreeMap<u64, u32>,
}

enum Admission {
    Deliver,
    Omit,
    NeedsEntry,
}

impl DescriptorRecovery {
    fn clear_history(&mut self) {
        self.initialized = 0;
        self.credited.clear();
    }

    fn forget_targets(&mut self, targets: u32) {
        if targets != 0 {
            self.initialized &= !targets;
            self.credited.retain(|_, mask| {
                *mask &= !targets;
                *mask != 0
            });
        }
    }

    fn admit<S>(&mut self, unit: &Unit<S>) -> Admission {
        let Dependency::Parsed(dd) = &unit.dependency else {
            self.structure = None;
            return Admission::NeedsEntry;
        };
        if !dd.start_of_frame {
            self.structure = None;
            return Admission::NeedsEntry;
        }
        let number = match self.newest {
            Some((previous, high)) => {
                let distance = dd.frame_number.wrapping_sub(previous);
                if !(1..(1 << 15)).contains(&distance) {
                    return Admission::NeedsEntry;
                }
                let Some(number) = high.checked_add(u64::from(distance)) else {
                    return Admission::NeedsEntry;
                };
                number
            }
            None => u64::from(dd.frame_number),
        };
        self.newest = Some((dd.frame_number, number));
        if let Some(structure) = &dd.attached_structure
            && self.structure.as_ref() != Some(structure)
        {
            self.clear_history();
            self.structure = Some(structure.clone());
        }
        let Some(all) = self
            .structure
            .as_deref()
            .map(TemplateDependencyStructure::all_decode_targets_bitmask)
        else {
            return Admission::NeedsEntry;
        };
        let default_active = if dd.attached_structure.is_some() {
            all
        } else {
            self.active
        };
        let active = dd.active_decode_targets.unwrap_or(default_active) & all;
        self.forget_targets(self.active & !active);
        self.active = active;
        let Some(structure) = &self.structure else {
            return Admission::NeedsEntry;
        };
        while self
            .credited
            .first_key_value()
            .is_some_and(|(&frame, _)| number.saturating_sub(frame) >= FRAME_HISTORY)
        {
            self.credited.pop_first();
        }

        let deps = &dd.frame_dependencies;
        let root = unit.access == Access::IndependentVideo && deps.frame_diffs.is_empty();
        let mut proven = 0;
        let mut required = false;
        for (target, &dti) in deps.dtis.iter().enumerate() {
            let bit = 1 << target;
            if self.active & bit == 0 || dti == NotPresent {
                continue;
            }
            required |= dti != Discardable;
            if (root || (self.initialized & bit != 0 && !deps.frame_diffs.is_empty()))
                && dependencies_satisfied(
                    deps,
                    &structure.decode_target_protected_by,
                    target,
                    |diff| {
                        !root
                            && number.checked_sub(u64::from(diff)).is_some_and(|parent| {
                                self.credited
                                    .get(&parent)
                                    .is_some_and(|mask| mask & bit != 0)
                            })
                    },
                )
            {
                proven |= bit;
            }
        }
        if proven != 0 {
            if root {
                self.forget_targets(proven);
            }
            self.credited.insert(number, proven);
            self.initialized |= proven;
            Admission::Deliver
        } else if required {
            Admission::NeedsEntry
        } else {
            Admission::Omit
        }
    }
}

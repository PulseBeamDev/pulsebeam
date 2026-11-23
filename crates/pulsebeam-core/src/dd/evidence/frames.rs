//! Bounded frame consistency evidence, independent of any branch decision.

use std::collections::BTreeMap;

use arrayvec::ArrayVec;

use super::{MAX_DECODE_TARGETS, Qualified};
use crate::dd::FrameDependencyTemplate;

pub const FRAME_HISTORY: u64 = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameStatus {
    Resolved(u64),
    Unresolved,
    Contradictory(u64),
}

#[derive(Default)]
pub struct Frames {
    high: Option<u64>,
    frames: BTreeMap<u64, Frame>,
}

struct Frame {
    dependencies: FrameDependencyTemplate,
    target_chains: ArrayVec<u8, MAX_DECODE_TARGETS>,
    first_seen: u64,
    last_seen: u64,
    start: Option<u64>,
    end: Option<u64>,
    contradictory: bool,
}

impl Frames {
    pub fn with_anchor(anchor: u64) -> Self {
        Self {
            high: Some(anchor),
            frames: BTreeMap::new(),
        }
    }

    pub fn observe(&mut self, sequence: u64, evidence: &Qualified) -> FrameStatus {
        let raw = u64::from(evidence.descriptor.frame_number);
        let Some(number) = self.high.map_or(Some(raw), |high| extend_frame(high, raw)) else {
            return FrameStatus::Unresolved;
        };
        let high = self.high.map_or(number, |high| high.max(number));
        let oldest = high.saturating_sub(FRAME_HISTORY.saturating_sub(1));
        if number < oldest {
            return FrameStatus::Unresolved;
        }
        self.high = Some(high);
        while self
            .frames
            .first_key_value()
            .is_some_and(|(&frame, _)| frame < oldest)
        {
            self.frames.pop_first();
        }
        let descriptor = &evidence.descriptor;
        let frame = self.frames.entry(number).or_insert_with(|| Frame {
            dependencies: descriptor.frame_dependencies.clone(),
            target_chains: evidence.target_chains.clone(),
            first_seen: sequence,
            last_seen: sequence,
            start: None,
            end: None,
            contradictory: false,
        });
        frame.first_seen = frame.first_seen.min(sequence);
        frame.last_seen = frame.last_seen.max(sequence);
        if descriptor.start_of_frame {
            frame.start.get_or_insert(sequence);
        }
        if descriptor.end_of_frame {
            frame.end.get_or_insert(sequence);
        }
        frame.contradictory |= frame.dependencies != descriptor.frame_dependencies
            || frame.target_chains != evidence.target_chains
            || frame.start.is_some_and(|start| {
                start != frame.first_seen || (sequence == start) != descriptor.start_of_frame
            })
            || frame.end.is_some_and(|end| {
                end != frame.last_seen || (sequence == end) != descriptor.end_of_frame
            });
        if frame.contradictory {
            FrameStatus::Contradictory(number)
        } else {
            FrameStatus::Resolved(number)
        }
    }

    pub fn is_consistent(&self, number: u64) -> bool {
        self.frames
            .get(&number)
            .is_some_and(|frame| !frame.contradictory)
    }
}

fn extend_frame(anchor: u64, raw: u64) -> Option<u64> {
    const CYCLE: u64 = 1 << 16;
    let ahead = raw.wrapping_sub(anchor) & u64::from(u16::MAX);
    match ahead.cmp(&(CYCLE / 2)) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Less => anchor.checked_add(ahead),
        std::cmp::Ordering::Greater => anchor.checked_sub(CYCLE.saturating_sub(ahead)),
    }
}

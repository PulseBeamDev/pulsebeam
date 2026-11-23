//! Source-sequence evidence for dynamic Dependency Descriptors.

use std::collections::VecDeque;

use super::{DdReadError, DependencyDescriptor, DependencyDescriptorReader, MAX_DECODE_TARGETS};

mod ancestry;
mod frames;
mod selection;
pub use frames::{FRAME_HISTORY, FrameStatus, Frames};
pub use selection::{Decision, DecodeTarget, Enqueue, FilteredBranch};

const SEQUENCE_HISTORY: u64 = 4096;
const CONTEXT_HISTORY: usize = 16;

/// With no proven context, a reference to an unattached structure is unresolved,
/// not malformed. A self-contained descriptor must still pass the full parser.
pub fn parse_independent(raw: &[u8]) -> Result<Option<DependencyDescriptor>, DdReadError> {
    parse(&DependencyDescriptorReader::new(), raw)
}

fn parse(
    reader: &DependencyDescriptorReader,
    raw: &[u8],
) -> Result<Option<DependencyDescriptor>, DdReadError> {
    match reader.parse(raw) {
        Ok(descriptor) => Ok(Some(descriptor)),
        Err(DdReadError::NoStructure) => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
pub struct Qualified {
    pub descriptor: DependencyDescriptor,
    pub active_targets: u32,
    /// Exact source position that installed the applicable template structure.
    pub structure_sequence: u64,
    pub target_chains: arrayvec::ArrayVec<u8, MAX_DECODE_TARGETS>,
}

struct Interval {
    first: u64,
    last: u64,
    structure_sequence: u64,
    reader: DependencyDescriptorReader,
}

/// A conservative forward context for one installed source incarnation. An
/// out-of-order observation never changes the forward template or active mask.
#[derive(Default)]
pub struct SourceContext {
    frontier: Option<u64>,
    reader: DependencyDescriptorReader,
    context_start: Option<u64>,
    structure_sequence: Option<u64>,
    history: VecDeque<Interval>,
}

impl SourceContext {
    /// Advance over authenticated positions known not to carry a descriptor.
    /// A missing or mixed range cannot stand in for that evidence.
    pub fn bridge_padding_before(
        &mut self,
        sequence: u64,
        covers: impl FnOnce(std::ops::Range<u64>) -> bool,
    ) {
        if let Some(next) = self.frontier.and_then(|frontier| frontier.checked_add(1))
            && next < sequence
            && covers(next..sequence)
        {
            self.frontier = sequence.checked_sub(1);
        }
    }

    /// Requalify an immutable packet for an output reservation without
    /// advancing or restoring the forward context.
    pub fn qualify(
        &self,
        sequence: u64,
        raw: Option<&[u8]>,
    ) -> Result<Option<Qualified>, DdReadError> {
        let newer = self.frontier.is_none_or(|frontier| sequence > frontier);
        let contiguous =
            self.frontier.and_then(|frontier| frontier.checked_add(1)) == Some(sequence);
        if self
            .frontier
            .is_some_and(|high| high.saturating_sub(sequence) >= SEQUENCE_HISTORY)
        {
            return Ok(None);
        }
        let context = if contiguous
            || (!newer && self.context_start.is_some_and(|start| sequence >= start))
        {
            self.structure_sequence
                .map(|structure| (&self.reader, structure))
        } else {
            self.history
                .iter()
                .rev()
                .find(|interval| (interval.first..=interval.last).contains(&sequence))
                .map(|interval| (&interval.reader, interval.structure_sequence))
        };
        let parsed = match (context, raw) {
            (Some((reader, _)), Some(raw)) => parse(reader, raw)?,
            (None, Some(raw)) => parse_independent(raw)?,
            (_, None) => None,
        };
        Ok(parsed.and_then(|descriptor| {
            let attached = descriptor.attached_structure.as_deref();
            let structure =
                attached.or_else(|| context.and_then(|(reader, _)| reader.structure()))?;
            let structure_sequence = if attached.is_some() {
                sequence
            } else {
                context?.1
            };
            let active_targets = descriptor.active_decode_targets.unwrap_or_else(|| {
                if attached.is_some() {
                    structure.all_decode_targets_bitmask()
                } else {
                    context.map_or(0, |(reader, _)| reader.active_decode_targets())
                }
            });
            let target_chains = structure.decode_target_protected_by.clone();
            Some(Qualified {
                descriptor,
                active_targets,
                structure_sequence,
                target_chains,
            })
        }))
    }

    /// Observe a source position after its identity and metadata are frozen.
    /// Missing DD breaks continuity; late observations never change context.
    pub fn observe(
        &mut self,
        sequence: u64,
        raw: Option<&[u8]>,
    ) -> Result<Option<Qualified>, DdReadError> {
        let qualified = self.qualify(sequence, raw)?;
        let contiguous =
            self.frontier.and_then(|frontier| frontier.checked_add(1)) == Some(sequence);
        if self.frontier.is_none_or(|frontier| sequence > frontier) {
            let reset = !contiguous || qualified.is_none();
            let replaced = qualified
                .as_ref()
                .is_some_and(|q| q.descriptor.attached_structure.is_some());
            let mask_changed = qualified
                .as_ref()
                .is_some_and(|q| q.active_targets != self.reader.active_decode_targets());
            if reset || replaced || mask_changed {
                if let (Some(first), Some(last), Some(structure_sequence)) = (
                    self.context_start.take(),
                    self.frontier,
                    self.structure_sequence,
                ) {
                    let reader = if reset || replaced {
                        std::mem::take(&mut self.reader)
                    } else {
                        self.reader.clone()
                    };
                    if self.history.len() == CONTEXT_HISTORY {
                        self.history.pop_front();
                    }
                    self.history.push_back(Interval {
                        first,
                        last,
                        structure_sequence,
                        reader,
                    });
                }
                if reset {
                    self.reader.reset();
                    self.structure_sequence = None;
                }
            }
            self.frontier = Some(sequence);
            while self
                .history
                .front()
                .is_some_and(|interval| sequence.saturating_sub(interval.last) >= SEQUENCE_HISTORY)
            {
                self.history.pop_front();
            }
            if let Some(qualified) = &qualified {
                self.context_start.get_or_insert(sequence);
                self.structure_sequence = Some(qualified.structure_sequence);
                if reset || replaced || mask_changed {
                    self.reader.commit(&qualified.descriptor);
                }
            }
        }
        Ok(qualified)
    }
}

#[cfg(test)]
mod tests;

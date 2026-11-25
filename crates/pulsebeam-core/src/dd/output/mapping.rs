use std::time::Instant;

use super::{Retention, TemplateInterval, TemplateIntervals, templates::Reservation};
use crate::dd::{
    DependencyDescriptorWriter, MAX_DD_LEN, TemplateDependencyStructure, evidence::Qualified,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Whole,
    /// A single active target, as decided by the receiving branch.
    Filtered(u8),
}

pub struct Evidence<'a> {
    pub qualified: &'a Qualified,
    pub frame: u64,
}

pub struct Packet<'a, S> {
    pub source: S,
    pub output_sequence: u64,
    pub mode: Mode,
    /// The branch has qualified entry or a target switch for this packet.
    pub entry: bool,
    pub dd: Option<Evidence<'a>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rejected {
    Bootstrap,
    Stale,
    InvalidTarget,
    TemplateSpace,
    Unrepresentable,
    Admission,
}

struct Epoch<S> {
    source: S,
    input: u64,
    output: u64,
}

struct Mapping {
    first: u64,
    structure_sequence: u64,
    structure_origin: u64,
    interval: TemplateInterval,
    source_offset: u8,
    mode: Mode,
    active_mask: u32,
}

struct PreparedMapping<S> {
    epoch: Option<Epoch<S>>,
    mapping: Mapping,
    structure: Option<Box<TemplateDependencyStructure>>,
    reservation: Reservation,
    frame: u64,
}

/// A staged rewrite. Dropping it does not advance the writer or template leases.
pub struct Prepared<S> {
    revision: u64,
    sequence: u64,
    bytes: [u8; MAX_DD_LEN],
    len: usize,
    mapping: Option<PreparedMapping<S>>,
}

impl<S> Prepared<S> {
    pub fn descriptor(&self) -> Option<&[u8]> {
        if self.len == 0 {
            None
        } else {
            self.bytes.get(..self.len)
        }
    }
}

/// DD wire state for one receiving SSRC. The caller owns RTP indexes and path
/// preflight; it must accept the encoded bytes into that same transaction.
pub struct OutputMapper<S> {
    epoch: Option<Epoch<S>>,
    mapping: Option<Mapping>,
    initial_frame: u16,
    frame_high: Option<u64>,
    packet_high: Option<u64>,
    writer: DependencyDescriptorWriter,
    intervals: TemplateIntervals,
    revision: u64,
}

impl<S: Copy + Eq> OutputMapper<S> {
    pub fn new(initial_frame: u16) -> Self {
        Self {
            epoch: None,
            mapping: None,
            initial_frame,
            frame_high: None,
            packet_high: None,
            writer: DependencyDescriptorWriter::new(),
            intervals: TemplateIntervals::default(),
            revision: 0,
        }
    }

    /// Invalidate receiver context without releasing still-live template IDs.
    pub fn lose_context(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.mapping = None;
        self.writer.reset();
    }

    /// Successful enqueue is the only event that advances the DD mapping.
    /// Unresolved whole-source media can still be admitted without DD.
    pub fn offer(
        &mut self,
        now: Instant,
        retention: impl Into<Retention>,
        packet: Packet<'_, S>,
        admit: impl FnOnce(Option<&[u8]>) -> bool,
    ) -> Result<(), Rejected> {
        let proposal = self.prepare(now, retention, packet)?;
        if !admit(proposal.descriptor()) {
            return Err(Rejected::Admission);
        }
        self.commit(proposal)
    }

    /// Prepare without publishing. Hold the proposal until final admission,
    /// or discard it if the source is fenced.
    pub fn prepare(
        &self,
        now: Instant,
        retention: impl Into<Retention>,
        packet: Packet<'_, S>,
    ) -> Result<Prepared<S>, Rejected> {
        if self.revision == u64::MAX {
            return Err(Rejected::Stale);
        }
        let Packet {
            source,
            output_sequence,
            mode,
            entry,
            dd,
        } = packet;
        let late = self.packet_high.is_some_and(|high| output_sequence <= high);
        let same_source = self
            .epoch
            .as_ref()
            .is_some_and(|epoch| epoch.source == source);
        let applicable = |mapping: &Mapping, dd: &Evidence<'_>| {
            same_source
                && mapping.structure_origin == dd.qualified.structure_origin
                && mapping.structure_sequence <= dd.qualified.structure_sequence
        };
        let Some(dd) = dd else {
            return self.without_dd(mode, output_sequence);
        };
        let qualified = dd.qualified;
        let descriptor = &qualified.descriptor;
        let mask = active_mask(mode, qualified)?;
        let current = self
            .mapping
            .as_ref()
            .filter(|mapping| applicable(mapping, &dd));
        if late
            && current.is_none_or(|mapping| {
                output_sequence < mapping.first
                    || mapping.mode != mode
                    || mapping.active_mask != mask
                    || descriptor.attached_structure.is_some()
            })
        {
            return Err(Rejected::Stale);
        }
        let new_mapping = current.is_none();
        let mode_change = current.is_some_and(|mapping| mapping.mode != mode);
        if matches!(mode, Mode::Filtered(_)) && (new_mapping || mode_change) && !entry {
            return Err(Rejected::Bootstrap);
        }
        if new_mapping && descriptor.attached_structure.is_none() {
            return self.without_dd(mode, output_sequence);
        }
        let new_epoch = !same_source;
        let (anchor_in, anchor_out) = match self.epoch.as_ref().filter(|_| same_source) {
            Some(epoch) => (epoch.input, epoch.output),
            None => (
                dd.frame,
                match self.frame_high {
                    Some(high) => high.checked_add(1).ok_or(Rejected::Unrepresentable)?,
                    None => u64::from(self.initial_frame),
                },
            ),
        };
        let frame = if dd.frame >= anchor_in {
            anchor_out.checked_add(dd.frame.saturating_sub(anchor_in))
        } else {
            anchor_out.checked_sub(anchor_in.saturating_sub(dd.frame))
        }
        .ok_or(Rejected::Unrepresentable)?;
        let frame_number =
            u16::try_from(frame & u64::from(u16::MAX)).map_err(|_| Rejected::Unrepresentable)?;
        let mut rewritten = descriptor.clone();
        rewritten.frame_number = frame_number;
        // The decoder inherits the committed mask until a mode or structure
        // change. Repeating it on mandatory-only packets can widen RTP padding.
        let emit_mask = match mode {
            Mode::Filtered(_) => {
                new_mapping || mode_change || descriptor.attached_structure.is_some()
            }
            Mode::Whole => {
                descriptor.active_decode_targets.is_some()
                    || current.is_some_and(|mapping| mapping.active_mask != mask)
            }
        };
        rewritten.active_decode_targets = emit_mask.then_some(mask);
        let source_offset = descriptor
            .attached_structure
            .as_ref()
            .map(|structure| structure.template_id_offset)
            .or_else(|| current.map(|mapping| mapping.source_offset))
            .ok_or(Rejected::Bootstrap)?;
        let retention = retention.into();
        let reservation = if let Some(structure) = descriptor.attached_structure.as_deref() {
            self.intervals.prepare(now, retention, structure)
        } else {
            current.and_then(|mapping| {
                self.intervals
                    .prepare_extend(mapping.interval, now, retention)
            })
        }
        .ok_or(Rejected::TemplateSpace)?;
        let interval = reservation.interval;
        rewritten.template_id = interval
            .map(source_offset, descriptor.template_id)
            .ok_or(Rejected::Unrepresentable)?;
        if let Some(structure) = &mut rewritten.attached_structure {
            structure.template_id_offset = interval.offset();
        }
        let mut bytes = [0; MAX_DD_LEN];
        let len = self
            .writer
            .encode(&rewritten, &mut bytes)
            .map_err(|_| Rejected::Unrepresentable)?;
        let first = if new_mapping || mode_change {
            output_sequence
        } else {
            current.map_or(output_sequence, |mapping| mapping.first)
        };
        Ok(Prepared {
            revision: self.revision,
            sequence: output_sequence,
            bytes,
            len,
            mapping: Some(PreparedMapping {
                epoch: new_epoch.then_some(Epoch {
                    source,
                    input: anchor_in,
                    output: anchor_out,
                }),
                mapping: Mapping {
                    first,
                    structure_sequence: if descriptor.attached_structure.is_some() {
                        qualified.structure_sequence
                    } else {
                        current.ok_or(Rejected::Bootstrap)?.structure_sequence
                    },
                    structure_origin: qualified.structure_origin,
                    interval,
                    source_offset,
                    mode,
                    active_mask: mask,
                },
                structure: rewritten.attached_structure,
                reservation,
                frame,
            }),
        })
    }

    pub fn is_current(&self, proposal: &Prepared<S>) -> bool {
        self.revision == proposal.revision
    }

    pub fn commit(&mut self, proposal: Prepared<S>) -> Result<(), Rejected> {
        if !self.is_current(&proposal) {
            return Err(Rejected::Stale);
        }
        if let Some(next) = proposal.mapping {
            if !self.intervals.commit(next.reservation) {
                return Err(Rejected::TemplateSpace);
            }
            if let Some(epoch) = next.epoch {
                self.epoch = Some(epoch);
            }
            self.writer.commit_structure(next.structure);
            self.mapping = Some(next.mapping);
            self.frame_high = Some(
                self.frame_high
                    .map_or(next.frame, |high| high.max(next.frame)),
            );
        } else if self.packet_high.is_none_or(|high| proposal.sequence > high) {
            self.mapping = None;
            self.writer.reset();
        }
        self.note_packet(proposal.sequence);
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    fn without_dd(&self, mode: Mode, sequence: u64) -> Result<Prepared<S>, Rejected> {
        if mode != Mode::Whole {
            return Err(Rejected::Bootstrap);
        }
        Ok(Prepared {
            revision: self.revision,
            sequence,
            bytes: [0; MAX_DD_LEN],
            len: 0,
            mapping: None,
        })
    }

    fn note_packet(&mut self, sequence: u64) {
        self.packet_high = Some(self.packet_high.map_or(sequence, |high| high.max(sequence)));
    }
}

#[cfg(test)]
mod tests;

fn active_mask(mode: Mode, qualified: &Qualified) -> Result<u32, Rejected> {
    match mode {
        Mode::Whole => Ok(qualified.active_targets),
        Mode::Filtered(target) => {
            let mask = 1_u32
                .checked_shl(u32::from(target))
                .ok_or(Rejected::InvalidTarget)?;
            (qualified.active_targets & mask != 0
                && qualified
                    .descriptor
                    .is_in_decode_target(usize::from(target)))
            .then_some(mask)
            .ok_or(Rejected::InvalidTarget)
        }
    }
}

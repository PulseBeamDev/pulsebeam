use std::time::Instant;

use super::{Retention, TemplateInterval, TemplateIntervals};
use crate::dd::{DependencyDescriptorWriter, MAX_DD_LEN, evidence::Qualified};

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
    interval: TemplateInterval,
    source_offset: u8,
    mode: Mode,
    active_mask: u32,
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
        }
    }

    /// Invalidate receiver context without releasing still-live template IDs.
    pub fn lose_context(&mut self) {
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
            same_source && mapping.structure_sequence == dd.qualified.structure_sequence
        };
        let Some(dd) = dd else {
            return self.without_dd(mode, output_sequence, admit);
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
            return self.without_dd(mode, output_sequence, admit);
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
        let mut result = Err(Rejected::TemplateSpace);
        let writer = &self.writer;
        let encode = |interval: TemplateInterval| {
            result = (|| {
                rewritten.template_id = interval
                    .map(source_offset, descriptor.template_id)
                    .ok_or(Rejected::Unrepresentable)?;
                if let Some(structure) = &mut rewritten.attached_structure {
                    structure.template_id_offset = interval.offset();
                }
                let mut bytes = [0; MAX_DD_LEN];
                let len = writer
                    .encode(&rewritten, &mut bytes)
                    .map_err(|_| Rejected::Unrepresentable)?;
                if !admit(bytes.get(..len)) {
                    return Err(Rejected::Admission);
                }
                Ok(())
            })();
            result.is_ok()
        };
        let retention = retention.into();
        let interval = if let Some(structure) = descriptor.attached_structure.as_deref() {
            self.intervals.offer(now, retention, structure, encode)
        } else if let Some(mapping) = current {
            self.intervals
                .extend(mapping.interval, now, retention, || {
                    encode(mapping.interval)
                })
                .then_some(mapping.interval)
        } else {
            None
        };
        result?;
        let interval = interval.ok_or(Rejected::TemplateSpace)?;
        self.writer.commit(&rewritten);
        if new_epoch {
            self.epoch = Some(Epoch {
                source,
                input: anchor_in,
                output: anchor_out,
            });
        }
        let first = if new_mapping || mode_change {
            output_sequence
        } else {
            current.map_or(output_sequence, |mapping| mapping.first)
        };
        self.mapping = Some(Mapping {
            first,
            structure_sequence: qualified.structure_sequence,
            interval,
            source_offset,
            mode,
            active_mask: mask,
        });
        self.note_packet(output_sequence);
        self.frame_high = Some(self.frame_high.map_or(frame, |high| high.max(frame)));
        Ok(())
    }

    fn without_dd(
        &mut self,
        mode: Mode,
        sequence: u64,
        admit: impl FnOnce(Option<&[u8]>) -> bool,
    ) -> Result<(), Rejected> {
        if mode != Mode::Whole {
            return Err(Rejected::Bootstrap);
        }
        if !admit(None) {
            return Err(Rejected::Admission);
        }
        if self.packet_high.is_none_or(|high| sequence > high) {
            self.lose_context();
        }
        self.note_packet(sequence);
        Ok(())
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

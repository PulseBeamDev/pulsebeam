use std::time::Instant;

use crate::dd::{MAX_TEMPLATES, TemplateDependencyStructure};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TemplateInterval {
    offset: u8,
    len: u8,
}

impl TemplateInterval {
    pub fn offset(self) -> u8 {
        self.offset
    }

    /// Rewrite a reference relative to the source structure's first ID.
    pub fn map(self, source_offset: u8, template_id: u8) -> Option<u8> {
        let index = template_id.wrapping_sub(source_offset) & 63;
        (source_offset < 64 && template_id < 64 && index < self.len)
            .then_some(self.offset.wrapping_add(index) & 63)
    }

    fn occupied(self) -> u64 {
        (u64::MAX >> 64_u32.saturating_sub(u32::from(self.len))).rotate_left(u32::from(self.offset))
    }
}

#[derive(Clone, Copy)]
pub enum Retention {
    Until(Instant),
    /// Use when the adapter cannot prove when queued and repair work expires.
    SenderLifetime,
}

impl From<Instant> for Retention {
    fn from(until: Instant) -> Self {
        Self::Until(until)
    }
}

impl Retention {
    fn live(self, now: Instant) -> bool {
        match self {
            Self::Until(until) => until > now,
            Self::SenderLifetime => true,
        }
    }

    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Until(a), Self::Until(b)) => Self::Until(a.max(b)),
            _ => Self::SenderLifetime,
        }
    }
}

struct Lease {
    interval: TemplateInterval,
    structure: Box<TemplateDependencyStructure>,
    retention: Retention,
}

/// Owns the disjoint live template intervals in a receiver's six-bit space.
/// Each admission must cover the full reorder and repair lifetime of the
/// work it publishes, not just the lifetime of the current mapping.
#[derive(Default)]
pub struct TemplateIntervals {
    live: Vec<Lease>,
}

impl TemplateIntervals {
    pub(super) fn extend(
        &mut self,
        interval: TemplateInterval,
        now: Instant,
        retention: Retention,
        admit: impl FnOnce() -> bool,
    ) -> bool {
        if !retention.live(now) {
            return false;
        }
        let Some(lease) = self
            .live
            .iter_mut()
            .find(|lease| lease.interval == interval && lease.retention.live(now))
        else {
            return false;
        };
        if !admit() {
            return false;
        }
        lease.retention = lease.retention.merge(retention);
        true
    }

    /// Reserve and publish in one transaction. False from `admit` leaves
    /// the proposed interval and its expiry uncommitted.
    pub fn offer(
        &mut self,
        now: Instant,
        retention: impl Into<Retention>,
        structure: &TemplateDependencyStructure,
        admit: impl FnOnce(TemplateInterval) -> bool,
    ) -> Option<TemplateInterval> {
        const _: () = assert!(MAX_TEMPLATES == 64);
        let len = u8::try_from(structure.templates.len()).ok()?;
        let retention = retention.into();
        if len == 0 || len > 64 || structure.template_id_offset >= 64 || !retention.live(now) {
            return None;
        }
        self.live.retain(|lease| lease.retention.live(now));
        if let Some(lease) = self
            .live
            .iter_mut()
            .find(|lease| same_semantics(&lease.structure, structure))
        {
            if !admit(lease.interval) {
                return None;
            }
            lease.retention = lease.retention.merge(retention);
            return Some(lease.interval);
        }
        let occupied = self
            .live
            .iter()
            .fold(0, |mask, lease| mask | lease.interval.occupied());
        let interval = (0_u8..64)
            .map(|offset| TemplateInterval {
                offset: structure.template_id_offset.wrapping_add(offset) & 63,
                len,
            })
            .find(|candidate| candidate.occupied() & occupied == 0)?;
        if !admit(interval) {
            return None;
        }
        self.live.push(Lease {
            interval,
            structure: Box::new(structure.clone()),
            retention,
        });
        Some(interval)
    }
}

fn same_semantics(left: &TemplateDependencyStructure, right: &TemplateDependencyStructure) -> bool {
    left.decode_target_count == right.decode_target_count
        && left.chain_count == right.chain_count
        && left.decode_target_protected_by == right.decode_target_protected_by
        && left.templates == right.templates
        && left.resolutions == right.resolutions
}

#[cfg(test)]
mod tests;

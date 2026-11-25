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

pub(super) struct Reservation {
    pub(super) interval: TemplateInterval,
    prepared_at: Instant,
    retention: Retention,
    structure: Option<Box<TemplateDependencyStructure>>,
}

/// Owns the disjoint live template intervals in a receiver's six-bit space.
/// Each admission must cover the full reorder and repair lifetime of the
/// work it publishes, not just the lifetime of the current mapping.
#[derive(Default)]
pub struct TemplateIntervals {
    live: Vec<Lease>,
}

impl TemplateIntervals {
    pub(super) fn prepare_extend(
        &self,
        interval: TemplateInterval,
        now: Instant,
        retention: Retention,
    ) -> Option<Reservation> {
        if !retention.live(now) || !self.live.iter().any(|lease| lease.interval == interval) {
            return None;
        }
        Some(Reservation {
            interval,
            prepared_at: now,
            retention,
            structure: None,
        })
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
        let reservation = self.prepare(now, retention.into(), structure)?;
        let interval = reservation.interval;
        if !admit(interval) {
            return None;
        }
        self.commit(reservation).then_some(interval)
    }

    pub(super) fn prepare(
        &self,
        now: Instant,
        retention: Retention,
        structure: &TemplateDependencyStructure,
    ) -> Option<Reservation> {
        const _: () = assert!(MAX_TEMPLATES == 64);
        let len = u8::try_from(structure.templates.len()).ok()?;
        if len == 0 || len > 64 || structure.template_id_offset >= 64 || !retention.live(now) {
            return None;
        }
        let live = || self.live.iter().filter(|lease| lease.retention.live(now));
        if let Some(lease) = live().find(|lease| same_semantics(&lease.structure, structure)) {
            return self.prepare_extend(lease.interval, now, retention);
        }
        let occupied = live().fold(0, |mask, lease| mask | lease.interval.occupied());
        let interval = (0_u8..64)
            .map(|offset| TemplateInterval {
                offset: structure.template_id_offset.wrapping_add(offset) & 63,
                len,
            })
            .find(|candidate| candidate.occupied() & occupied == 0)?;
        Some(Reservation {
            interval,
            prepared_at: now,
            retention,
            structure: Some(Box::new(structure.clone())),
        })
    }

    pub(super) fn commit(&mut self, reservation: Reservation) -> bool {
        let Reservation {
            interval,
            prepared_at,
            retention,
            structure,
        } = reservation;
        // An expired repair lease can still be the current wire mapping.
        // Only a fresh allocation can retire or reuse that interval.
        self.live.retain(|lease| {
            lease.retention.live(prepared_at) || (structure.is_none() && lease.interval == interval)
        });
        if let Some(structure) = structure {
            if self
                .live
                .iter()
                .any(|lease| lease.interval.occupied() & interval.occupied() != 0)
            {
                return false;
            }
            self.live.push(Lease {
                interval,
                structure,
                retention,
            });
        } else if let Some(lease) = self
            .live
            .iter_mut()
            .find(|lease| lease.interval == interval)
        {
            lease.retention = lease.retention.merge(retention);
        } else {
            return false;
        }
        true
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

use std::collections::VecDeque;

use super::{MediaEgress, RETIREMENT_BYTES, RetiringSender};

pub(in crate::egress) struct RetirementReservation {
    buffer: Option<VecDeque<RetiringSender>>,
    slots: usize,
    runtime_bytes: usize,
}

pub(in crate::egress) fn retirement_bytes(queue: &VecDeque<RetiringSender>) -> usize {
    queue.capacity().saturating_mul(RETIREMENT_BYTES)
}

impl RetirementReservation {
    pub(in crate::egress) fn current(owner: &MediaEgress) -> Self {
        Self {
            buffer: None,
            slots: owner.retirement_slots,
            runtime_bytes: owner.runtime_bytes,
        }
    }
}

impl MediaEgress {
    pub(in crate::egress) fn prepare_retirement(
        &self,
        slots: usize,
        total: usize,
    ) -> Option<RetirementReservation> {
        let old = retirement_bytes(&self.retiring);
        let mut buffer = None;
        let runtime_bytes = if slots == 0 {
            total.checked_sub(old)?
        } else if slots <= self.retiring.capacity() {
            total
        } else {
            let capacity = self.retiring.capacity().checked_mul(2)?.max(slots);
            let minimum = capacity.checked_mul(RETIREMENT_BYTES)?.checked_sub(old)?;
            if total.checked_add(minimum)? > self.max_runtime_bytes {
                return None;
            }
            let mut expanded = VecDeque::new();
            expanded.try_reserve_exact(capacity).ok()?;
            let projected = total
                .checked_sub(old)?
                .checked_add(retirement_bytes(&expanded))?;
            buffer = Some(expanded);
            projected
        };
        (runtime_bytes <= self.max_runtime_bytes).then_some(RetirementReservation {
            buffer,
            slots,
            runtime_bytes,
        })
    }

    pub(in crate::egress) fn commit_retirement(&mut self, reservation: RetirementReservation) {
        if let Some(mut buffer) = reservation.buffer {
            buffer.append(&mut self.retiring);
            self.retiring = buffer;
        }
        self.retirement_slots = reservation.slots;
        if self.retirement_slots == 0 {
            debug_assert!(self.retiring.is_empty());
            self.retiring = VecDeque::new();
        }
        self.runtime_bytes = reservation.runtime_bytes;
    }

    pub(in crate::egress) fn enqueue_retirement(&mut self, job: RetiringSender) {
        debug_assert!(self.retiring.len() < self.retirement_slots);
        debug_assert!(self.retirement_slots <= self.retiring.capacity());
        self.retiring.push_back(job);
    }

    pub(in crate::egress) fn release_idle_retirement(&mut self) {
        if self.retirement_slots == 0 {
            debug_assert!(self.retiring.is_empty());
            self.runtime_bytes = self
                .runtime_bytes
                .saturating_sub(retirement_bytes(&self.retiring));
            self.retiring = VecDeque::new();
        }
    }

    pub(in crate::egress) fn release_retirement(&mut self, key: u64) {
        if let Some(sender) = self
            .senders
            .get_mut(&key)
            .filter(|sender| !sender.retirement_reserved)
        {
            sender.retirement_reserved = true;
        } else {
            self.retirement_slots = self.retirement_slots.saturating_sub(1);
        }
    }
}

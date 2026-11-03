use std::{
    collections::VecDeque,
    mem::{size_of, size_of_val},
};

use super::{IngressOwner, events};
use crate::rtcp::{FeedbackBatch, FeedbackReport, KeyframeRequest, RepairRequest};

pub(super) fn feedback_bytes(batch: &FeedbackBatch) -> Option<usize> {
    match &batch.report {
        FeedbackReport::Twcc { statuses, .. } => Some(size_of_val(statuses.as_ref())),
        FeedbackReport::Rfc8888 { reports, .. } => reports
            .iter()
            .try_fold(size_of_val(reports.as_ref()), |bytes, report| {
                bytes.checked_add(size_of_val(report.statuses.as_ref()))
            }),
    }
}

pub(super) struct Feedback {
    pub(super) batch: FeedbackBatch,
    pub(super) owned_bytes: usize,
}

fn backing_bytes<T>(queue: &VecDeque<T>) -> usize {
    queue.capacity() * size_of::<T>()
}

pub(super) fn release_empty<T>(queue: &mut VecDeque<T>) -> usize {
    if queue.is_empty() {
        let bytes = backing_bytes(queue);
        *queue = VecDeque::new();
        bytes
    } else {
        0
    }
}

fn minimum_capacity<T>(queue: &VecDeque<T>, additional: usize) -> Option<usize> {
    let required = queue.len().checked_add(additional)?;
    Some(if required == 0 {
        0
    } else {
        required.max(queue.capacity())
    })
}

struct Queue<T> {
    expanded: Option<VecDeque<T>>,
    pending: Vec<T>,
}

impl<T> Queue<T> {
    fn prepare(
        queue: &VecDeque<T>,
        pending: Vec<T>,
        total: &mut usize,
        limit: usize,
    ) -> Option<Self> {
        let capacity = minimum_capacity(queue, pending.len())?;
        if capacity == queue.capacity() {
            return Some(Self {
                expanded: None,
                pending,
            });
        }
        let fits = |actual: usize| {
            actual
                .checked_sub(capacity)?
                .checked_mul(size_of::<T>())?
                .checked_add(*total)
                .filter(|bytes| *bytes <= limit)
        };
        let doubled = queue
            .capacity()
            .checked_mul(2)
            .unwrap_or(capacity)
            .max(capacity);
        let desired = if capacity == 0 {
            0
        } else if fits(doubled).is_some() {
            doubled
        } else {
            capacity
        };
        let mut expanded = VecDeque::new();
        expanded.try_reserve_exact(desired).ok()?;
        *total = fits(expanded.capacity())?;
        Some(Self {
            expanded: Some(expanded),
            pending,
        })
    }

    fn commit(self, queue: &mut VecDeque<T>) {
        if let Some(mut expanded) = self.expanded {
            assert!(expanded.capacity() >= queue.len() + self.pending.len());
            expanded.append(queue);
            *queue = expanded;
        }
        assert!(queue.capacity() - queue.len() >= self.pending.len());
        queue.extend(self.pending);
    }
}

pub(super) struct Reservation {
    feedback: Queue<Feedback>,
    repair: Queue<RepairRequest>,
    keyframes: Queue<KeyframeRequest>,
    events: events::Reservation,
    runtime_bytes: usize,
}

impl Reservation {
    pub(super) fn commit(self, owner: &mut IngressOwner) {
        self.events.commit(&mut owner.events);
        self.feedback.commit(&mut owner.feedback);
        self.repair.commit(&mut owner.repair);
        self.keyframes.commit(&mut owner.keyframes);
        owner.runtime_bytes = self.runtime_bytes;
    }
}

impl IngressOwner {
    pub(super) fn control_backing_bytes(&self) -> usize {
        backing_bytes(&self.feedback) + backing_bytes(&self.repair) + backing_bytes(&self.keyframes)
    }

    pub(super) fn prepare_control(
        &self,
        feedback: Vec<FeedbackBatch>,
        repair: Vec<RepairRequest>,
        keyframes: Vec<KeyframeRequest>,
        notices: usize,
    ) -> Option<Reservation> {
        let feedback: Vec<_> = feedback
            .into_iter()
            .map(|batch| {
                Some(Feedback {
                    owned_bytes: feedback_bytes(&batch)?,
                    batch,
                })
            })
            .collect::<Option<_>>()?;
        let owned = feedback
            .iter()
            .try_fold(0usize, |bytes, value| bytes.checked_add(value.owned_bytes))?;
        let minimum = minimum_capacity(&self.feedback, feedback.len())?
            .checked_mul(size_of::<Feedback>())?
            .checked_add(
                minimum_capacity(&self.repair, repair.len())?
                    .checked_mul(size_of::<RepairRequest>())?,
            )?
            .checked_add(
                minimum_capacity(&self.keyframes, keyframes.len())?
                    .checked_mul(size_of::<KeyframeRequest>())?,
            )?;
        let base = self
            .runtime_bytes
            .checked_sub(self.control_backing_bytes())?
            .checked_add(owned)?
            .checked_add(minimum)?;
        let events = self.prepare_events(base, self.runtime_reservations.len(), notices)?;
        let mut total = events.runtime_bytes;
        Some(Reservation {
            feedback: Queue::prepare(&self.feedback, feedback, &mut total, self.max_runtime_bytes)?,
            repair: Queue::prepare(&self.repair, repair, &mut total, self.max_runtime_bytes)?,
            keyframes: Queue::prepare(
                &self.keyframes,
                keyframes,
                &mut total,
                self.max_runtime_bytes,
            )?,
            events,
            runtime_bytes: total,
        })
    }
}

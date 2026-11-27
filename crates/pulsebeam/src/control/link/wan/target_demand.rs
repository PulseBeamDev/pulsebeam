use std::{
    future::Future,
    num::NonZeroU64,
    pin::Pin,
    task::{Context, Poll},
};

use pulsebeam_runtime::{
    sync::oneshot,
    time::{Duration, Instant},
};

use crate::route::fabric::target_demand::Projection;

const SAMPLE_INTERVAL: Duration = Duration::from_millis(150);
const SAMPLE_LIFETIME: Duration = Duration::from_millis(350);

pub(super) type Receipt = oneshot::Receiver<Option<Projection>>;

#[derive(Default)]
pub(super) struct ProjectionTx {
    pub selected: bool,
    pub selecting: Option<oneshot::Receiver<bool>>,
    next: Option<Instant>,
    generation: u64,
    last_budget: u64,
    targeted_since: Option<NonZeroU64>,
    sample: Option<Sample>,
}

struct Sample {
    expires: Instant,
    receipts: Vec<Receipt>,
    projection: Option<Projection>,
    attempted: bool,
}

impl ProjectionTx {
    pub(super) fn poll_delivery(&mut self, cx: &mut Context<'_>) -> bool {
        let Some(sample) = self.sample.as_mut().filter(|sample| !sample.attempted) else {
            return false;
        };
        sample.collect(|receipt| match Pin::new(receipt).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => Poll::Ready(result.unwrap_or(Some(Projection::Whole))),
        });
        self.delivery_ready()
    }

    pub(super) fn delivery_ready(&self) -> bool {
        self.sample
            .as_ref()
            .is_some_and(|sample| !sample.attempted && sample.receipts.is_empty())
    }

    pub(super) fn accepts_budget(&mut self, generation: NonZeroU64) -> bool {
        let generation = generation.get();
        if !self.selected
            || self
                .targeted_since
                .is_none_or(|since| generation < since.get())
            || generation <= self.last_budget
            || generation > self.generation
            || self.generation.saturating_sub(generation) > 1
        {
            return false;
        }
        self.last_budget = generation;
        true
    }

    pub(super) fn step(
        &mut self,
        now: Instant,
        sample: impl FnOnce() -> Vec<Receipt>,
        mut send: impl FnMut(NonZeroU64, Option<Projection>) -> bool,
    ) {
        if let Some(expired) = self.sample.as_mut().filter(|sample| now >= sample.expires) {
            expired.attempted = true;
            if !self.send_update(Some(Projection::Whole), &mut send) {
                return;
            }
            self.sample = None;
        }
        if let Some(pending) = &mut self.sample {
            pending.poll();
            if !pending.receipts.is_empty() {
                return;
            }
            let projection = pending.projection;
            // Backpressure retries on the regular ownership turn.
            pending.attempted = true;
            if self.send_update(projection, &mut send) {
                self.sample = None;
            }
            return;
        }
        if self.next.is_none_or(|next| now >= next) {
            self.next = now.checked_add(SAMPLE_INTERVAL);
            let Some(expires) = now.checked_add(SAMPLE_LIFETIME) else {
                return;
            };
            self.sample = Some(Sample {
                expires,
                receipts: sample(),
                projection: None,
                attempted: false,
            });
        }
    }

    fn send_update(
        &mut self,
        projection: Option<Projection>,
        send: impl FnOnce(NonZeroU64, Option<Projection>) -> bool,
    ) -> bool {
        let targeting = matches!(projection, Some(Projection::Targets { .. }));
        if !targeting && self.targeted_since.is_none() {
            return true;
        }
        let Some(generation) = self.generation.checked_add(1).and_then(NonZeroU64::new) else {
            return false;
        };
        if !send(generation, projection) {
            return false;
        }
        self.generation = generation.get();
        self.targeted_since = targeting.then_some(self.targeted_since.unwrap_or(generation));
        true
    }
}

impl Sample {
    fn poll(&mut self) {
        self.collect(|receipt| match receipt.try_recv() {
            Ok(result) => Poll::Ready(result),
            Err(oneshot::error::TryRecvError::Empty) => Poll::Pending,
            Err(oneshot::error::TryRecvError::Closed) => Poll::Ready(Some(Projection::Whole)),
        });
    }

    fn collect(&mut self, mut read: impl FnMut(&mut Receipt) -> Poll<Option<Projection>>) {
        self.receipts.retain_mut(|receipt| {
            let Poll::Ready(result) = read(receipt) else {
                return true;
            };
            if let Some(result) = result {
                self.projection = Some(
                    self.projection
                        .map_or(result, |previous| previous.union(result)),
                );
            }
            false
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_sample_wakes_early_but_backpressure_does_not_spin() {
        let now = Instant::now();
        let mut tx = ProjectionTx {
            selected: true,
            targeted_since: Some(NonZeroU64::MIN),
            generation: 1,
            ..Default::default()
        };
        let (reply, receipt) = oneshot::channel();
        tx.step(now, || vec![receipt], |_, _| unreachable!());
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(!tx.poll_delivery(&mut cx));
        reply.send(None).unwrap();
        assert!(tx.poll_delivery(&mut cx));
        tx.step(
            now,
            || unreachable!(),
            |generation, projection| {
                assert_eq!(generation.get(), 2);
                assert_eq!(projection, None);
                false
            },
        );
        assert!(!tx.poll_delivery(&mut cx));
        tx.step(
            now + SAMPLE_INTERVAL,
            || unreachable!(),
            |generation, _| {
                assert_eq!(generation.get(), 2);
                true
            },
        );
        assert!(tx.sample.is_none());
        assert!(tx.targeted_since.is_none());
    }
}

use std::cell::{Cell, RefCell};

use futures_channel::oneshot;

#[derive(Default)]
pub(super) struct LocalOperationGate {
    tail: RefCell<Option<oneshot::Receiver<()>>>,
    revision: Cell<Option<u64>>,
}

impl LocalOperationGate {
    pub(super) fn advance_revision(&self) -> Option<u64> {
        let next = self.revision.get().unwrap_or_default().checked_add(1)?;
        self.revision.set(Some(next));
        Some(next)
    }

    pub(super) fn revision(&self) -> Option<u64> {
        self.revision.get()
    }

    pub(super) fn clear_revision(&self) {
        self.revision.set(None);
    }

    pub(super) async fn enter(&self) -> LocalOperationPermit {
        let (done, next) = oneshot::channel();
        let previous = self.tail.replace(Some(next));
        if let Some(previous) = previous {
            let _ = previous.await;
        }
        LocalOperationPermit { done: Some(done) }
    }
}

pub(super) struct LocalOperationPermit {
    done: Option<oneshot::Sender<()>>,
}

impl Drop for LocalOperationPermit {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };

    #[test]
    fn fifo_is_per_slot_and_revision_changes_fence_queued_work() {
        let gate = LocalOperationGate::default();
        let mut context = Context::from_waker(Waker::noop());
        let first_revision = gate.advance_revision().unwrap();
        let mut first = pin!(gate.enter());
        let Poll::Ready(first_permit) = first.as_mut().poll(&mut context) else {
            panic!("first operation must enter immediately")
        };
        let queued_revision = gate.advance_revision().unwrap();
        let mut queued = pin!(gate.enter());
        assert!(queued.as_mut().poll(&mut context).is_pending());
        gate.advance_revision().unwrap();
        let replacement_revision = gate.advance_revision().unwrap();
        let mut replacement = pin!(gate.enter());
        assert!(replacement.as_mut().poll(&mut context).is_pending());

        let independent = LocalOperationGate::default();
        let mut other_slot = pin!(independent.enter());
        assert!(other_slot.as_mut().poll(&mut context).is_ready());

        assert_ne!(gate.revision(), Some(first_revision));
        drop(first_permit);
        let Poll::Ready(queued_permit) = queued.as_mut().poll(&mut context) else {
            panic!("queued operation must enter after its predecessor")
        };
        assert_ne!(gate.revision(), Some(queued_revision));
        assert!(replacement.as_mut().poll(&mut context).is_pending());
        drop(queued_permit);
        let Poll::Ready(replacement_permit) = replacement.as_mut().poll(&mut context) else {
            panic!("replacement must wait for every preceding operation")
        };
        assert_eq!(gate.revision(), Some(replacement_revision));
        gate.clear_revision();
        assert_ne!(gate.revision(), Some(replacement_revision));
        drop(replacement_permit);
    }

    #[test]
    fn exhausted_revision_preserves_the_last_fence_without_wrapping() {
        let gate = LocalOperationGate::default();
        gate.revision.set(Some(u64::MAX));
        assert_eq!(gate.advance_revision(), None);
        assert_eq!(gate.revision(), Some(u64::MAX));
    }
}

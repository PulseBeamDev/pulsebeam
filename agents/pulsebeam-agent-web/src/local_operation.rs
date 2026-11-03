use std::cell::RefCell;

use futures_channel::oneshot;

#[derive(Default)]
pub(super) struct LocalOperationGate {
    tail: RefCell<Option<oneshot::Receiver<()>>>,
}

impl LocalOperationGate {
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
    fn fifo_serializes_physical_operations_independently() {
        let gate = LocalOperationGate::default();
        let mut context = Context::from_waker(Waker::noop());
        let mut first = pin!(gate.enter());
        let Poll::Ready(first_permit) = first.as_mut().poll(&mut context) else {
            panic!("first operation must enter immediately")
        };
        let mut queued = pin!(gate.enter());
        assert!(queued.as_mut().poll(&mut context).is_pending());
        let mut replacement = pin!(gate.enter());
        assert!(replacement.as_mut().poll(&mut context).is_pending());

        let independent = LocalOperationGate::default();
        let mut other_slot = pin!(independent.enter());
        assert!(other_slot.as_mut().poll(&mut context).is_ready());

        drop(first_permit);
        let Poll::Ready(queued_permit) = queued.as_mut().poll(&mut context) else {
            panic!("queued operation must enter after its predecessor")
        };
        assert!(replacement.as_mut().poll(&mut context).is_pending());
        drop(queued_permit);
        assert!(replacement.as_mut().poll(&mut context).is_ready());
    }
}

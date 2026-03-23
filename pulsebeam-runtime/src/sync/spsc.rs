use crate::sync::Arc;
use crate::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use crate::sync::AtomicWaker;
use std::cell::UnsafeCell;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RecvError {
    Closed,
}

#[derive(Debug)]
struct Slot<T> {
    value: UnsafeCell<Option<T>>,
}

unsafe impl<T: Send> Send for Slot<T> {}
unsafe impl<T: Send> Sync for Slot<T> {}

#[derive(Debug)]
struct Ring<T> {
    capacity: usize,
    mask: usize,
    buffer: Vec<Slot<T>>,
    head: AtomicUsize,
    tail: AtomicUsize,
    waker: AtomicWaker,
    closed: AtomicBool,
}

impl<T> Ring<T> {
    fn new(mut capacity: usize) -> Arc<Self> {
        if capacity == 0 {
            capacity = 1;
        } else if !capacity.is_power_of_two() {
            let old = capacity;
            capacity = capacity.next_power_of_two();
            tracing::warn!("Capacity should be power of 2, using nearest: {old} -> {capacity}");
        }

        let buffer = (0..capacity)
            .map(|_| Slot {
                value: UnsafeCell::new(None),
            })
            .collect();

        Arc::new(Self {
            capacity,
            mask: capacity - 1,
            buffer,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            waker: AtomicWaker::new(),
            closed: AtomicBool::new(false),
        })
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub struct Sender<T> {
    ring: Arc<Ring<T>>,
}

#[derive(Debug)]
pub struct TrySendError<T>(pub T);

impl<T> Sender<T> {
    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        if self.ring.is_closed() {
            return Err(TrySendError(item));
        }

        let head = self.ring.head.load(Ordering::Acquire);
        let tail = self.ring.tail.load(Ordering::Acquire);

        if head.wrapping_sub(tail) >= self.ring.capacity {
            return Err(TrySendError(item));
        }

        let idx = head & self.ring.mask;
        unsafe {
            *self.ring.buffer[idx].value.get() = Some(item);
        }

        self.ring.head.store(head.wrapping_add(1), Ordering::Release);
        self.ring.waker.wake();
        Ok(())
    }

    pub fn fill_ratio(&self) -> f64 {
        let head = self.ring.head.load(Ordering::Acquire);
        let tail = self.ring.tail.load(Ordering::Acquire);
        (head.wrapping_sub(tail) as f64) / (self.ring.capacity as f64)
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            ring: self.ring.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.ring.closed.store(true, Ordering::Release);
        self.ring.waker.wake();
    }
}

#[derive(Debug)]
pub struct Receiver<T> {
    ring: Arc<Ring<T>>,
    tail: usize,
}

impl<T> Receiver<T> {
    pub fn try_recv(&mut self) -> Result<Option<T>, RecvError> {
        let head = self.ring.head.load(Ordering::Acquire);
        let tail = self.tail;

        if tail >= head {
            if self.ring.is_closed() {
                return Err(RecvError::Closed);
            }
            return Ok(None);
        }

        let idx = tail & self.ring.mask;
        let value = unsafe { (&mut *self.ring.buffer[idx].value.get()).take() };
        self.tail = tail.wrapping_add(1);
        self.ring.tail.store(self.tail, Ordering::Release);

        Ok(value)
    }

    pub async fn recv(&mut self) -> Result<T, RecvError> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        let head = self.ring.head.load(Ordering::Acquire);
        let tail = self.tail;

        if tail >= head {
            if self.ring.is_closed() {
                return Poll::Ready(Err(RecvError::Closed));
            }
            self.ring.waker.register(cx.waker());
            let head2 = self.ring.head.load(Ordering::Acquire);
            if tail < head2 {
                // item arrived after registration, continue to read recomputed.
            } else {
                return Poll::Pending;
            }
        }

        let idx = self.tail & self.ring.mask;
        let value = unsafe { (&mut *self.ring.buffer[idx].value.get()).take() };
        self.tail = self.tail.wrapping_add(1);
        self.ring.tail.store(self.tail, Ordering::Release);

        if let Some(item) = value {
            Poll::Ready(Ok(item))
        } else {
            // Race or incomplete write, try again.
            Poll::Pending
        }
    }
}

pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let ring = Ring::new(capacity);
    (
        Sender { ring: ring.clone() },
        Receiver { ring, tail: 0 },
    )
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            ring: self.ring.clone(),
            tail: self.tail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_test::task::panic_waker;
    use std::task::Context;

    #[test]
    fn send_and_receive() {
        let (tx, mut rx) = channel::<u64>(4);
        tx.try_send(10).unwrap();
        assert_eq!(rx.try_recv(), Ok(Some(10)));
    }

    #[test]
    fn empty_returns_none() {
        let (_tx, mut rx) = channel::<u64>(4);
        assert_eq!(rx.try_recv(), Ok(None));
    }

    #[test]
    fn closed_returns_error() {
        let (tx, mut rx) = channel::<u64>(4);
        drop(tx);
        assert_eq!(rx.try_recv(), Err(RecvError::Closed));

        let waker = panic_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(rx.poll_recv(&mut cx), Poll::Ready(Err(RecvError::Closed))));
    }
}

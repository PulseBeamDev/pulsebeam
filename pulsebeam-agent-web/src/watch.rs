//! Single-threaded latest-value channel.
//!
//! Intended for local async executors such as browser WASM.
//! This type is deliberately `!Send` and `!Sync`.
//!
//! Semantics:
//! - Single producer, single consumer.
//! - At most one value is buffered.
//! - `send()` replaces any unconsumed value.
//! - `recv().await` consumes the latest buffered value.
//! - Dropping the sender closes the channel.
//! - A buffered value is delivered before closure is observed.

use alloc::rc::Rc;
use core::{
    cell::RefCell,
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Rc::new(Shared {
        state: RefCell::new(State {
            value: None,
            waker: None,
            sender_alive: true,
            receiver_alive: true,
        }),
    });

    (
        Sender {
            shared: Rc::clone(&shared),
        },
        Receiver { shared },
    )
}

struct Shared<T> {
    state: RefCell<State<T>>,
}

struct State<T> {
    value: Option<T>,
    waker: Option<Waker>,
    sender_alive: bool,
    receiver_alive: bool,
}

pub struct Sender<T> {
    shared: Rc<Shared<T>>,
}

pub struct Receiver<T> {
    shared: Rc<Shared<T>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("channel closed")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SendError<T>(T);

impl<T> SendError<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("channel receiver dropped")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryRecvError {
    Empty,
    Closed,
}

impl<T> Sender<T> {
    /// Replace the currently buffered value.
    ///
    /// If the receiver has not yet consumed a previous value, that value is
    /// discarded and replaced by `value`.
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        let (old_value, waker) = {
            let mut state = self.shared.state.borrow_mut();

            if !state.receiver_alive {
                return Err(SendError(value));
            }

            let old_value = state.value.replace(value);
            let waker = state.waker.take();

            (old_value, waker)
        };

        // Important: don't run arbitrary T::drop() or Waker code while the
        // RefCell is borrowed. Either may theoretically re-enter user code.
        drop(old_value);

        if let Some(waker) = waker {
            waker.wake();
        }

        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        !self.shared.state.borrow().receiver_alive
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut state = self.shared.state.borrow_mut();
            state.sender_alive = false;
            state.waker.take()
        };

        // Never invoke a Waker while holding our RefCell borrow.
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> Receiver<T> {
    pub fn recv(&mut self) -> Recv<'_, T> {
        Recv {
            receiver: self,
            done: false,
        }
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let mut state = self.shared.state.borrow_mut();

        if let Some(value) = state.value.take() {
            return Ok(value);
        }

        if !state.sender_alive {
            return Err(TryRecvError::Closed);
        }

        Err(TryRecvError::Empty)
    }

    /// Returns true once the sender has been dropped.
    ///
    /// There may still be one buffered value available after this becomes
    /// true.
    pub fn is_closed(&self) -> bool {
        !self.shared.state.borrow().sender_alive
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let (value, waker) = {
            let mut state = self.shared.state.borrow_mut();

            state.receiver_alive = false;

            (state.value.take(), state.waker.take())
        };

        // As with send(), destructors run outside the RefCell borrow.
        drop(value);
        drop(waker);
    }
}

pub struct Recv<'a, T> {
    receiver: &'a mut Receiver<T>,
    done: bool,
}

impl<T> Future for Recv<'_, T> {
    type Output = Result<T, Closed>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.done {
            debug_assert!(!self.done, "Recv polled after completion");
            return Poll::Pending;
        }

        let this = &mut *self;

        let (result, old_waker) = {
            let mut state = this.receiver.shared.state.borrow_mut();

            if let Some(value) = state.value.take() {
                let old_waker = state.waker.take();

                (Some(Poll::Ready(Ok(value))), old_waker)
            } else if !state.sender_alive {
                let old_waker = state.waker.take();

                (Some(Poll::Ready(Err(Closed))), old_waker)
            } else {
                let replace_waker = match state.waker.as_ref() {
                    Some(waker) => !waker.will_wake(cx.waker()),
                    None => true,
                };

                let old_waker = if replace_waker {
                    state.waker.replace(cx.waker().clone())
                } else {
                    None
                };

                (None, old_waker)
            }
        };

        // A Waker is arbitrary user/executor code from our perspective.
        drop(old_waker);

        match result {
            Some(result) => {
                this.done = true;
                result
            }
            None => Poll::Pending,
        }
    }
}

impl<T> Drop for Recv<'_, T> {
    fn drop(&mut self) {
        if self.done {
            return;
        }

        // There can only be one Recv because it exclusively borrows Receiver,
        // so any registered waker belongs to this future.
        let waker = self.receiver.shared.state.borrow_mut().waker.take();

        drop(waker);
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::disallowed_types,
        reason = "Wake requires Arc and the test counter requires atomic visibility to its Waker"
    )]

    extern crate std;

    use super::*;
    use alloc::task::Wake;
    use core::{
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll, Waker},
    };
    use std::{boxed::Box, sync::Arc};

    #[derive(Default)]
    struct WakeCounter {
        wakes: AtomicUsize,
    }

    impl WakeCounter {
        fn count(&self) -> usize {
            self.wakes.load(Ordering::SeqCst)
        }
    }

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counter_waker() -> (Arc<WakeCounter>, Waker) {
        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&counter));

        (counter, waker)
    }

    fn poll<F: Future>(future: Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(waker))
    }

    #[test]
    fn recv_waits_until_value_is_sent() {
        let (tx, mut rx) = channel();

        let (counter, waker) = counter_waker();
        let mut recv = Box::pin(rx.recv());

        assert!(poll(recv.as_mut(), &waker).is_pending());
        assert_eq!(counter.count(), 0);

        tx.send(42).unwrap();

        assert_eq!(counter.count(), 1);
        assert_eq!(poll(recv.as_mut(), &waker), Poll::Ready(Ok(42)),);
    }

    #[test]
    fn send_before_poll_is_immediately_available() {
        let (tx, mut rx) = channel();

        tx.send(42).unwrap();

        let (_, waker) = counter_waker();
        let mut recv = Box::pin(rx.recv());

        assert_eq!(poll(recv.as_mut(), &waker), Poll::Ready(Ok(42)),);
    }

    #[test]
    fn multiple_sends_coalesce_to_latest_value() {
        let (tx, mut rx) = channel();

        tx.send(1).unwrap();
        tx.send(2).unwrap();
        tx.send(3).unwrap();

        assert_eq!(rx.try_recv(), Ok(3));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn multiple_sends_only_need_one_wakeup() {
        let (tx, mut rx) = channel();

        let (counter, waker) = counter_waker();
        let mut recv = Box::pin(rx.recv());

        assert!(poll(recv.as_mut(), &waker).is_pending());

        tx.send(1).unwrap();
        tx.send(2).unwrap();
        tx.send(3).unwrap();

        // The first send consumes the registered waker. Further sends merely
        // replace the already-buffered value; another wake is unnecessary.
        assert_eq!(counter.count(), 1);

        assert_eq!(poll(recv.as_mut(), &waker), Poll::Ready(Ok(3)),);
    }

    #[test]
    fn latest_waker_replaces_previous_waker() {
        let (tx, mut rx) = channel();

        let (first_counter, first_waker) = counter_waker();
        let (second_counter, second_waker) = counter_waker();

        let mut recv = Box::pin(rx.recv());

        assert!(poll(recv.as_mut(), &first_waker).is_pending());
        assert!(poll(recv.as_mut(), &second_waker).is_pending());

        tx.send(42).unwrap();

        assert_eq!(first_counter.count(), 0);
        assert_eq!(second_counter.count(), 1);

        assert_eq!(poll(recv.as_mut(), &second_waker), Poll::Ready(Ok(42)),);
    }

    #[test]
    fn dropping_sender_wakes_pending_receiver() {
        let (tx, mut rx) = channel::<u32>();

        let (counter, waker) = counter_waker();
        let mut recv = Box::pin(rx.recv());

        assert!(poll(recv.as_mut(), &waker).is_pending());

        drop(tx);

        assert_eq!(counter.count(), 1);
        assert_eq!(poll(recv.as_mut(), &waker), Poll::Ready(Err(Closed)),);
    }

    #[test]
    fn buffered_value_is_received_before_closed() {
        let (tx, mut rx) = channel();

        tx.send(42).unwrap();
        drop(tx);

        assert_eq!(rx.try_recv(), Ok(42));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    }

    #[test]
    fn send_returns_value_when_receiver_is_gone() {
        let (tx, rx) = channel();

        drop(rx);

        let error = tx.send(42).unwrap_err();

        assert_eq!(error.into_inner(), 42);
        assert!(tx.is_closed());
    }

    #[test]
    fn dropping_pending_recv_unregisters_waker() {
        let (tx, mut rx) = channel();

        let (counter, waker) = counter_waker();

        {
            let mut recv = Box::pin(rx.recv());

            assert!(poll(recv.as_mut(), &waker).is_pending());
        }

        // The abandoned receive future's task must not be woken.
        tx.send(42).unwrap();

        assert_eq!(counter.count(), 0);
        assert_eq!(rx.try_recv(), Ok(42));
    }

    #[test]
    fn try_recv_reports_empty_and_closed_distinctly() {
        let (tx, mut rx) = channel::<u32>();

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        drop(tx);

        assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
        assert!(rx.is_closed());
    }

    #[test]
    fn receiver_can_wait_for_multiple_updates() {
        let (tx, mut rx) = channel();

        tx.send(1).unwrap();
        assert_eq!(rx.try_recv(), Ok(1));

        tx.send(2).unwrap();
        assert_eq!(rx.try_recv(), Ok(2));

        tx.send(3).unwrap();
        assert_eq!(rx.try_recv(), Ok(3));
    }
}

//! Single-threaded, multi-producer FIFO channel.

use alloc::{collections::VecDeque, rc::Rc};
use core::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use spin::Mutex;

/// Creates a single-consumer, multi-producer FIFO channel.
///
/// `Sender` is cheaply clonable. Every sender and the single receiver share
/// the same channel state through `Rc`, making this suitable for a
/// single-threaded executor such as the browser/WASM runtime.
///
/// The channel becomes closed for the receiver once the last `Sender` is
/// dropped. It becomes closed for senders once the `Receiver` is dropped.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Rc::new(Shared {
        state: Mutex::new(State {
            values: VecDeque::new(),
            waker: None,
            senders_alive: true,
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

/// State shared by all producers and the single consumer.
///
/// `Shared` itself must not implement `Clone`. Cloning a sender clones the
/// surrounding `Rc`, which preserves one shared queue rather than copying
/// channel state.
struct Shared<T> {
    state: Mutex<State<T>>,
}

struct State<T> {
    values: VecDeque<T>,

    /// Waker for the currently pending `Receiver::recv`.
    ///
    /// There can only be one because the receiver is unique and `recv`
    /// borrows it mutably.
    waker: Option<Waker>,

    /// Whether at least one sender still exists.
    ///
    /// The actual sender count is represented by the `Rc` strong count.
    /// This boolean records the terminal transition after the final sender
    /// disappears.
    senders_alive: bool,

    /// Whether the unique receiver still exists.
    receiver_alive: bool,
}

/// Sending half of the channel.
///
/// Cloning a sender creates another producer for the same FIFO queue.
pub struct Sender<T> {
    shared: Rc<Shared<T>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Rc::clone(&self.shared),
        }
    }
}

/// Receiving half of the channel.
///
/// There is intentionally only one receiver.
pub struct Receiver<T> {
    shared: Rc<Shared<T>>,
}

/// Returned by `Receiver::recv` when all senders have been dropped and no
/// queued values remain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("channel closed")
    }
}

/// Returned by `Sender::send` when the receiver has already been dropped.
///
/// The unsent value is preserved and can be recovered with `into_inner`.
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

/// Error returned by `Receiver::try_recv`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryRecvError {
    /// No value is currently queued, but at least one sender still exists.
    Empty,

    /// No value is queued and all senders have been dropped.
    Closed,
}

impl<T> Sender<T> {
    /// Enqueues a value at the back of the FIFO queue.
    ///
    /// If the receiver is currently waiting in `recv`, it is woken after the
    /// value has been committed to the queue.
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        let waker = {
            let mut state = self.shared.state.lock();

            if !state.receiver_alive {
                return Err(SendError(value));
            }

            state.values.push_back(value);
            state.waker.take()
        };

        // Never invoke a waker while holding the channel lock. A wake may
        // synchronously cause the future to be polled again by some
        // executors.
        if let Some(waker) = waker {
            waker.wake();
        }

        Ok(())
    }

    /// Returns whether the receiver has been dropped.
    pub fn is_closed(&self) -> bool {
        !self.shared.state.lock().receiver_alive
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // While the receiver exists, Rc ownership looks like:
        //
        //     receiver + N senders
        //
        // During Drop of the final sender there are therefore exactly two
        // strong references:
        //
        //     receiver + this sender
        //
        // Other sender drops must leave the channel open.
        //
        // If the receiver has already disappeared, there is nobody left to
        // observe sender closure, so no state transition or wakeup is needed.
        if Rc::strong_count(&self.shared) != 2 {
            return;
        }

        let waker = {
            let mut state = self.shared.state.lock();

            if !state.receiver_alive {
                return;
            }

            state.senders_alive = false;
            state.waker.take()
        };

        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> Receiver<T> {
    /// Waits asynchronously for the next value.
    ///
    /// Returns `Err(Closed)` only when the queue is empty and all senders have
    /// been dropped. Values already queued before sender closure are still
    /// delivered first.
    pub fn recv(&mut self) -> Recv<'_, T> {
        Recv {
            receiver: self,
            done: false,
        }
    }

    /// Attempts to receive the next value without waiting.
    ///
    /// Queued values are always returned before `Closed`, even if the final
    /// sender has already been dropped.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let mut state = self.shared.state.lock();

        if let Some(value) = state.values.pop_front() {
            return Ok(value);
        }

        if state.senders_alive {
            Err(TryRecvError::Empty)
        } else {
            Err(TryRecvError::Closed)
        }
    }

    /// Returns whether all senders have been dropped.
    ///
    /// A closed receiver may still contain queued values. `try_recv` and
    /// `recv` will drain those values before reporting closure.
    pub fn is_closed(&self) -> bool {
        !self.shared.state.lock().senders_alive
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let (values, waker) = {
            let mut state = self.shared.state.lock();

            state.receiver_alive = false;

            (core::mem::take(&mut state.values), state.waker.take())
        };

        // Drop arbitrary T values and the waker outside the lock. Their Drop
        // implementations are not under our control.
        drop(values);
        drop(waker);
    }
}

/// Future returned by `Receiver::recv`.
///
/// The future borrows the receiver mutably, which guarantees there can be at
/// most one pending receive operation for this channel.
pub struct Recv<'a, T> {
    receiver: &'a mut Receiver<T>,
    done: bool,
}

impl<T> Future for Recv<'_, T> {
    type Output = Result<T, Closed>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        debug_assert!(!self.done, "Recv polled after completion");

        if self.done {
            // Futures are not required to behave meaningfully after returning
            // Ready. Returning Pending keeps release builds harmless if this
            // contract is violated.
            return Poll::Pending;
        }

        let this = &mut *self;

        let (result, old_waker) = {
            let mut state = this.receiver.shared.state.lock();

            if let Some(value) = state.values.pop_front() {
                (Some(Ok(value)), state.waker.take())
            } else if !state.senders_alive {
                (Some(Err(Closed)), state.waker.take())
            } else {
                // Avoid replacing an equivalent waker on every poll.
                let replace = state
                    .waker
                    .as_ref()
                    .is_none_or(|waker| !waker.will_wake(cx.waker()));

                let old_waker = if replace {
                    state.waker.replace(cx.waker().clone())
                } else {
                    None
                };

                (None, old_waker)
            }
        };

        // A Waker's Drop implementation is external code. Keep it outside the
        // spin mutex as well.
        drop(old_waker);

        match result {
            Some(result) => {
                this.done = true;
                Poll::Ready(result)
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

        // Since Recv holds `&mut Receiver`, no other receive future can have
        // installed a competing waker while this future exists.
        let waker = self.receiver.shared.state.lock().waker.take();
        drop(waker);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_and_receive_fifo() {
        let (sender, mut receiver) = channel();

        sender.send(1).unwrap();
        sender.send(2).unwrap();
        sender.send(3).unwrap();

        assert_eq!(receiver.try_recv(), Ok(1));
        assert_eq!(receiver.try_recv(), Ok(2));
        assert_eq!(receiver.try_recv(), Ok(3));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn cloned_senders_share_one_queue() {
        let (first, mut receiver) = channel();
        let second = first.clone();

        first.send(1).unwrap();
        second.send(2).unwrap();
        first.send(3).unwrap();

        assert_eq!(receiver.try_recv(), Ok(1));
        assert_eq!(receiver.try_recv(), Ok(2));
        assert_eq!(receiver.try_recv(), Ok(3));
    }

    #[test]
    fn dropping_one_sender_does_not_close_channel() {
        let (first, mut receiver) = channel();
        let second = first.clone();

        drop(first);

        assert!(!receiver.is_closed());
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));

        second.send(1).unwrap();
        assert_eq!(receiver.try_recv(), Ok(1));
    }

    #[test]
    fn dropping_last_sender_closes_channel() {
        let (sender, mut receiver) = channel::<u32>();

        assert!(!receiver.is_closed());

        drop(sender);

        assert!(receiver.is_closed());
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Closed));
    }

    #[test]
    fn queued_values_are_drained_before_closed() {
        let (sender, mut receiver) = channel();

        sender.send(1).unwrap();
        sender.send(2).unwrap();

        drop(sender);

        assert!(receiver.is_closed());

        assert_eq!(receiver.try_recv(), Ok(1));
        assert_eq!(receiver.try_recv(), Ok(2));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Closed));
    }

    #[test]
    fn send_fails_after_receiver_is_dropped() {
        let (sender, receiver) = channel();

        drop(receiver);

        let error = sender.send(42).unwrap_err();

        assert_eq!(error.into_inner(), 42);
        assert!(sender.is_closed());
    }

    #[test]
    fn sender_clone_does_not_require_t_clone() {
        struct NotClone;

        let (sender, _receiver) = channel::<NotClone>();

        let _another_sender = sender.clone();
    }
}

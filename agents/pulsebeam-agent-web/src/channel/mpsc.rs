use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::collections::VecDeque;
use std::rc::Rc;

use spin::Mutex;

pub struct Sender<T> {
    shared: Rc<Mutex<Shared<T>>>,
}

pub struct Receiver<T> {
    shared: Rc<Mutex<Shared<T>>>,
}

#[derive(Debug, PartialEq)]
pub enum SendError<T> {
    Closed(T),
}

#[derive(Debug, PartialEq)]
pub enum TrySendError<T> {
    Full(T),
    Closed(T),
}

struct Shared<T> {
    capacity: usize,
    queue: VecDeque<T>,
    sender_count: usize,
    receiver_present: bool,
    closed: bool,
    next_token: usize,
    waiting_senders: VecDeque<(usize, Waker)>,
    receiver_waker: Option<Waker>,
}

pub struct Send<T> {
    shared: Rc<Mutex<Shared<T>>>,
    token: usize,
    value: Option<T>,
    awaiting: bool,
}

impl<T> Send<T> {
    fn token(shared: &Rc<Mutex<Shared<T>>>) -> usize {
        let mut shared = shared.lock();
        let token = shared.next_token;
        shared.next_token = shared.next_token.saturating_add(1);
        token
    }
}

pub struct Recv<T> {
    shared: Rc<Mutex<Shared<T>>>,
}

pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    debug_assert!(capacity > 0, "channel capacity must be positive");
    let shared = Rc::new(Mutex::new(Shared {
        capacity,
        queue: VecDeque::new(),
        sender_count: 1,
        receiver_present: true,
        closed: false,
        next_token: 0,
        waiting_senders: VecDeque::new(),
        receiver_waker: None,
    }));
    (
        Sender {
            shared: Rc::clone(&shared),
        },
        Receiver { shared },
    )
}

impl<T> Sender<T> {
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        let mut shared = self.shared.lock();
        shared_invariants(&shared);

        if shared.closed || !shared.receiver_present {
            return Err(TrySendError::Closed(value));
        }
        if !shared.waiting_senders.is_empty() || shared.queue.len() >= shared.capacity {
            return Err(TrySendError::Full(value));
        }

        shared.queue.push_back(value);
        let mut wake_receiver = shared.receiver_waker.take();
        shared_invariants(&shared);

        if let Some(waker) = wake_receiver.take() {
            waker.wake();
        }

        Ok(())
    }

    pub fn send(&self, value: T) -> Send<T> {
        let token = Send::<T>::token(&self.shared);
        Send {
            shared: Rc::clone(&self.shared),
            token,
            value: Some(value),
            awaiting: false,
        }
    }

    pub fn sender_count(&self) -> usize {
        self.shared.lock().sender_count
    }

    fn close_sender(&self) {
        let mut wakers: Vec<Waker> = Vec::new();
        {
            let mut shared = self.shared.lock();
            debug_assert!(shared.sender_count > 0, "sender count underflow");
            shared.sender_count = shared.sender_count.saturating_sub(1);
            let final_sender = shared.sender_count == 0;
            if final_sender {
                shared.closed = true;
                while let Some((_, waker)) = shared.waiting_senders.pop_front() {
                    wakers.push(waker);
                }
                if let Some(waker) = shared.receiver_waker.take() {
                    wakers.push(waker);
                }
            }
            shared_invariants(&shared);
        }

        for waker in wakers {
            waker.wake();
        }
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        {
            let mut shared = self.shared.lock();
            shared.sender_count = shared.sender_count.saturating_add(1);
            shared_invariants(&shared);
        }
        Self {
            shared: Rc::clone(&self.shared),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.close_sender();
    }
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Option<T> {
        let mut wake_sender: Option<Waker> = None;
        let value = {
            let mut shared = self.shared.lock();
            shared_invariants(&shared);
            let value = shared.queue.pop_front();
            if value.is_some() {
                if let Some((_, waker)) = shared.waiting_senders.pop_front() {
                    wake_sender = Some(waker);
                }
            }
            shared_invariants(&shared);
            value
        };

        if let Some(waker) = wake_sender {
            waker.wake();
        }
        value
    }

    pub fn recv(&self) -> Recv<T> {
        Recv {
            shared: Rc::clone(&self.shared),
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut wakeers: Vec<Waker> = Vec::new();
        {
            let mut shared = self.shared.lock();
            if !shared.receiver_present {
                return;
            }
            shared.receiver_present = false;
            shared.closed = true;
            while let Some((_, waker)) = shared.waiting_senders.pop_front() {
                wakeers.push(waker);
            }
            if let Some(waker) = shared.receiver_waker.take() {
                wakeers.push(waker);
            }
            shared_invariants(&shared);
        }

        for waker in wakeers {
            waker.wake();
        }
    }
}

impl<T> Future for Send<T> {
    type Output = Result<(), SendError<T>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let value = match this.value.take() {
            Some(value) => value,
            None => return Poll::Pending,
        };
        let receiver_waker = {
            let mut shared = this.shared.lock();
            shared_invariants(&shared);

            if shared.closed || !shared.receiver_present {
                return Poll::Ready(Err(SendError::Closed(value)));
            }

            if shared.queue.len() >= shared.capacity {
                let mut index = 0;
                while index < shared.waiting_senders.len() {
                    if shared.waiting_senders[index].0 == this.token {
                        shared.waiting_senders[index].1 = cx.waker().clone();
                        this.value = Some(value);
                        this.awaiting = true;
                        return Poll::Pending;
                    }
                    index += 1;
                }

                shared
                    .waiting_senders
                    .push_back((this.token, cx.waker().clone()));
                this.value = Some(value);
                this.awaiting = true;
                shared_invariants(&shared);
                return Poll::Pending;
            }

            shared.queue.push_back(value);
            let receiver_waker = shared.receiver_waker.take();
            shared_invariants(&shared);
            this.awaiting = false;
            receiver_waker
        };

        if let Some(waker) = receiver_waker {
            waker.wake();
        }
        Poll::Ready(Ok(()))
    }
}

impl<T> Drop for Send<T> {
    fn drop(&mut self) {
        if !self.awaiting || self.value.is_none() {
            return;
        }
        let mut shared = self.shared.lock();
        shared_invariants(&shared);
        let mut index = 0;
        while index < shared.waiting_senders.len() {
            if shared.waiting_senders[index].0 == self.token {
                shared.waiting_senders.remove(index);
                break;
            }
            index += 1;
        }
        shared_invariants(&shared);
    }
}

impl<T> Future for Recv<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut wake_sender: Option<Waker> = None;

        {
            let mut shared = self.shared.lock();
            shared_invariants(&shared);

            if let Some(value) = shared.queue.pop_front() {
                if let Some((_, waker)) = shared.waiting_senders.pop_front() {
                    wake_sender = Some(waker);
                }
                shared_invariants(&shared);

                if let Some(waker) = wake_sender {
                    waker.wake();
                }

                return Poll::Ready(Some(value));
            }

            if shared.closed && shared.sender_count == 0 {
                return Poll::Ready(None);
            }

            shared.receiver_waker = Some(cx.waker().clone());
            shared_invariants(&shared);
        }

        Poll::Pending
    }
}

fn shared_invariants<T>(shared: &Shared<T>) {
    debug_assert!(shared.capacity > 0);
    debug_assert!(shared.queue.len() <= shared.capacity);
    debug_assert!(shared.sender_count > 0 || !shared.receiver_present || shared.closed);
}

#[cfg(test)]
mod tests {
    use core::pin::Pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    use super::{SendError, TrySendError, channel};

    fn noop_waker() -> Waker {
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(core::ptr::null(), &VTABLE)
        }
        fn wake(_: *const ()) {}
        fn wake_by_ref(_: *const ()) {}
        fn drop(_: *const ()) {}

        const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
        let raw = RawWaker::new(core::ptr::null(), &VTABLE);
        unsafe { Waker::from_raw(raw) }
    }

    fn poll<T: core::future::Future + Unpin>(future: &mut T) -> Poll<T::Output> {
        let binding = noop_waker();
        let mut context = Context::from_waker(&binding);
        let mut pinned = Pin::new(future);
        pinned.as_mut().poll(&mut context)
    }

    #[test]
    fn fifo_order_and_backpressure_with_try_send() {
        let (sender, receiver) = channel(2);
        let mut receiver_values = [0u8; 2];
        assert!(sender.try_send(1u8).is_ok());
        assert!(sender.try_send(2u8).is_ok());
        assert!(matches!(sender.try_send(3), Err(TrySendError::Full(3))));

        receiver_values[0] = receiver.try_recv().expect("first");
        receiver_values[1] = receiver.try_recv().expect("second");
        assert_eq!(receiver_values, [1, 2]);
        assert_eq!(receiver.try_recv(), None);
    }

    #[test]
    fn async_send_waits_when_full_and_can_be_canceled() {
        let (sender, receiver) = channel(1);
        assert!(sender.try_send(1).is_ok());

        let mut pending = Box::new(sender.send(2));
        assert!(poll(pending.as_mut()).is_pending());
        drop(pending);

        assert!(matches!(sender.try_send(2), Err(TrySendError::Full(2))));
        assert_eq!(receiver.try_recv(), Some(1));
        assert!(sender.try_send(2).is_ok());
        assert_eq!(receiver.try_recv(), Some(2));
    }

    #[test]
    fn final_sender_close_preserves_order_then_drains_then_closes() {
        let (sender, receiver) = channel(2);
        let sender_a = sender.clone();
        let sender_b = sender.clone();

        assert!(sender_a.try_send(1).is_ok());
        assert!(sender_b.try_send(2).is_ok());

        drop(sender_a);
        drop(sender_b);
        drop(sender);

        assert_eq!(receiver.try_recv(), Some(1));
        assert_eq!(receiver.try_recv(), Some(2));
        assert_eq!(receiver.try_recv(), None);
    }

    #[test]
    fn receiver_drop_rejects_waiting_senders() {
        let (sender, receiver) = channel(1);
        assert!(sender.try_send(1).is_ok());

        let mut pending = Box::new(sender.send(2));
        assert!(poll(pending.as_mut()).is_pending());
        drop(receiver);

        match poll(pending.as_mut()) {
            Poll::Ready(Err(SendError::Closed(2))) => {}
            other => panic!("expected closed send result, got {other:?}"),
        }
    }

    #[test]
    fn reentrant_wakeups_preserve_waiter_order() {
        let (sender, receiver) = channel(1);
        assert!(sender.try_send(1).is_ok());

        let mut first = Box::new(sender.send(2));
        let mut second = Box::new(sender.send(3));

        assert!(poll(first.as_mut()).is_pending());
        assert!(poll(second.as_mut()).is_pending());

        assert_eq!(receiver.try_recv(), Some(1));

        assert_eq!(poll(first.as_mut()), Poll::Ready(Ok(())));
        assert_eq!(poll(second.as_mut()), Poll::Pending);

        assert_eq!(receiver.try_recv(), Some(2));

        assert_eq!(poll(first.as_mut()), Poll::Pending);
        assert_eq!(poll(second.as_mut()), Poll::Ready(Ok(())));
        assert_eq!(receiver.try_recv(), Some(3));
    }

    #[test]
    fn sender_count_tracks_clones() {
        let (sender, _) = channel::<u8>(4);
        assert_eq!(sender.sender_count(), 1);
        let second = sender.clone();
        assert_eq!(sender.sender_count(), 2);
        let third = second.clone();
        assert_eq!(sender.sender_count(), 3);
        drop(second);
        assert_eq!(sender.sender_count(), 2);
        drop(third);
        assert_eq!(sender.sender_count(), 1);
    }

    #[test]
    fn no_lost_wakeup_after_async_send() {
        let (sender, receiver) = channel(1);
        assert!(sender.try_send(1).is_ok());

        let mut pending = Box::new(sender.send(2));

        assert!(poll(pending.as_mut()).is_pending());
        assert_eq!(receiver.try_recv(), Some(1));

        assert_eq!(poll(pending.as_mut()), Poll::Ready(Ok(())));
        assert_eq!(receiver.try_recv(), Some(2));
        drop(pending);
    }
}

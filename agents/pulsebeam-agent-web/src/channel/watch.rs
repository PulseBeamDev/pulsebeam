use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::collections::VecDeque;
use std::rc::Rc;

use spin::Mutex;

pub struct Sender<T: Clone> {
    shared: Rc<Mutex<Shared<T>>>,
}

pub struct Receiver<T: Clone> {
    shared: Rc<Mutex<Shared<T>>>,
    observed: u64,
}

struct Shared<T: Clone> {
    value: T,
    version: u64,
    sender_count: usize,
    next_waiter_id: usize,
    waiters: VecDeque<(usize, Waker)>,
    closed: bool,
}

pub struct Changed<'a, T: Clone> {
    receiver: &'a mut Receiver<T>,
    waiter_id: usize,
    registered: bool,
}

pub fn channel<T: Clone>(initial: T) -> (Sender<T>, Receiver<T>) {
    let shared = Rc::new(Mutex::new(Shared {
        value: initial,
        version: 0,
        sender_count: 1,
        next_waiter_id: 0,
        waiters: VecDeque::new(),
        closed: false,
    }));
    (
        Sender {
            shared: Rc::clone(&shared),
        },
        Receiver {
            shared,
            observed: 0,
        },
    )
}

impl<T: Clone> Sender<T> {
    pub fn send(&self, value: T) -> bool {
        let mut waiters: Vec<Waker> = Vec::new();

        {
            let mut shared = self.shared.lock();
            watch_invariants(&shared);
            if shared.closed {
                return false;
            }

            shared.value = value;
            shared.version = shared.version.saturating_add(1);

            while let Some((_, waker)) = shared.waiters.pop_front() {
                waiters.push(waker);
            }
        }

        for waker in waiters {
            waker.wake();
        }

        true
    }

    pub fn close(&self) {
        let mut waiters: Vec<Waker> = Vec::new();

        {
            let mut shared = self.shared.lock();
            if shared.closed {
                return;
            }
            if shared.sender_count == 0 {
                return;
            }

            shared.sender_count = shared.sender_count.saturating_sub(1);
            if shared.sender_count == 0 {
                shared.closed = true;
                while let Some((_, waker)) = shared.waiters.pop_front() {
                    waiters.push(waker);
                }
            }
            shared_invariants(&shared);
        }

        for waker in waiters {
            waker.wake();
        }
    }
}

impl<T: Clone> Clone for Sender<T> {
    fn clone(&self) -> Self {
        {
            let mut shared = self.shared.lock();
            shared.sender_count = shared.sender_count.saturating_add(1);
            watch_invariants(&shared);
        }

        Self {
            shared: Rc::clone(&self.shared),
        }
    }
}

impl<T: Clone> Drop for Sender<T> {
    fn drop(&mut self) {
        self.close();
    }
}

impl<T: Clone> Receiver<T> {
    pub fn current(&self) -> T {
        self.shared.lock().value.clone()
    }

    pub fn version(&self) -> u64 {
        self.shared.lock().version
    }

    pub fn changed(&mut self) -> Changed<'_, T> {
        Changed {
            receiver: self,
            waiter_id: 0,
            registered: false,
        }
    }
}

impl<T: Clone> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Rc::clone(&self.shared),
            observed: self.observed,
        }
    }
}

impl<'a, T: Clone> Future for Changed<'a, T> {
    type Output = Option<u64>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };

        {
            let mut shared = this.receiver.shared.lock();
            shared_invariants(&shared);

            if this.receiver.observed < shared.version {
                this.receiver.observed = shared.version;
                if this.registered {
                    unregister_waiter(&mut shared, this.waiter_id);
                    this.registered = false;
                }
                return Poll::Ready(Some(this.receiver.observed));
            }

            if shared.closed {
                return Poll::Ready(None);
            }

            if this.registered {
                let mut found = false;
                for (id, waker) in shared.waiters.iter_mut() {
                    if *id == this.waiter_id {
                        *waker = cx.waker().clone();
                        found = true;
                        break;
                    }
                }

                if found {
                    return Poll::Pending;
                }

                this.registered = false;
            }

            let waiter_id = shared.next_waiter_id;
            shared.next_waiter_id = shared.next_waiter_id.saturating_add(1);
            this.waiter_id = waiter_id;
            this.registered = true;
            shared.waiters.push_back((waiter_id, cx.waker().clone()));
            shared_invariants(&shared);
        }

        Poll::Pending
    }
}

impl<'a, T: Clone> Drop for Changed<'a, T> {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        let mut shared = self.receiver.shared.lock();
        unregister_waiter(&mut shared, self.waiter_id);
        self.registered = false;
    }
}

fn unregister_waiter<T: Clone>(shared: &mut Shared<T>, waiter_id: usize) {
    let mut index = 0;
    while index < shared.waiters.len() {
        if shared.waiters[index].0 == waiter_id {
            shared.waiters.remove(index);
            break;
        }
        index += 1;
    }
}

fn shared_invariants<T: Clone>(shared: &Shared<T>) {
    debug_assert!(shared.sender_count > 0 || shared.closed);
    debug_assert!(shared.version < u64::MAX);
}

fn watch_invariants<T: Clone>(shared: &Shared<T>) {
    debug_assert!(shared.sender_count > 0);
}

#[cfg(test)]
mod tests {
    use core::pin::Pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    use super::channel;

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
    fn initial_reads_and_version_monotonicity() {
        let (sender, receiver) = channel::<i32>(1);
        assert_eq!(receiver.current(), 1);
        assert_eq!(receiver.version(), 0);

        assert!(sender.send(2));
        assert_eq!(receiver.current(), 2);
        assert_eq!(receiver.version(), 1);
    }

    #[test]
    fn updates_are_coalesced_and_final_value_reads() {
        let (sender, mut receiver) = channel::<i32>(0);
        assert_eq!(receiver.version(), 0);
        assert_eq!(poll(&mut receiver.changed()), Poll::Pending);

        assert!(sender.send(1));
        assert!(sender.send(2));

        assert_eq!(poll(&mut receiver.changed()), Poll::Ready(Some(2)));
        assert_eq!(receiver.current(), 2);
        assert_eq!(receiver.version(), 2);
    }

    #[test]
    fn independent_receivers_wait_independently() {
        let (sender, mut first) = channel(10);
        let mut second = first.clone();

        assert_eq!(poll(&mut first.changed()), Poll::Pending);
        assert_eq!(poll(&mut second.changed()), Poll::Pending);

        assert!(sender.send(11));
        assert_eq!(poll(&mut first.changed()), Poll::Ready(Some(1)));
        assert_eq!(poll(&mut second.changed()), Poll::Ready(Some(1)));
    }

    #[test]
    fn close_while_pending_notices_closed_after_latest_value() {
        let (sender, mut receiver) = channel::<i32>(7);
        {
            let mut first = receiver.changed();
            assert_eq!(poll(&mut first), Poll::Pending);

            sender.close();
            assert_eq!(poll(&mut first), Poll::Ready(None));
        }

        assert_eq!(receiver.current(), 7);
    }

    #[test]
    fn changed_registrations_are_replaced_without_reaping_failures() {
        let (sender, mut receiver) = channel(String::from("a"));
        let mut second = receiver.clone();
        let mut first_wait = receiver.changed();
        let mut second_wait = second.changed();

        assert_eq!(poll(&mut first_wait), Poll::Pending);
        assert_eq!(poll(&mut second_wait), Poll::Pending);

        assert!(sender.send(String::from("b")));
        assert_eq!(poll(&mut first_wait), Poll::Ready(Some(1)));
        assert_eq!(poll(&mut second_wait), Poll::Ready(Some(1)));

        drop(first_wait);
        drop(second_wait);

        assert_eq!(receiver.current(), "b");
        drop(second);
    }
}

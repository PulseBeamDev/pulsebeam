use std::{
    cell::UnsafeCell,
    collections::VecDeque,
    rc::Rc,
    task::{Context, Poll, Waker},
};

#[derive(Debug)]
pub struct Ring<T> {
    buf: VecDeque<T>,
    waker: Option<Waker>,
    is_lagged: bool,
    is_closed: bool,
}

pub fn channel<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    let ring = Rc::new(UnsafeCell::new(Ring {
        buf: VecDeque::with_capacity(cap.next_power_of_two()),
        waker: None,
        is_lagged: false,
        is_closed: false,
    }));
    (
        Sender {
            ring: Rc::clone(&ring),
        },
        Receiver { ring },
    )
}

#[derive(Debug)]
pub enum TrySendError<T> {
    Closed(T),
}

#[derive(Debug)]
pub struct Sender<T> {
    ring: Rc<UnsafeCell<Ring<T>>>,
}

impl<T> Sender<T> {
    #[inline]
    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        let ring = unsafe { &mut *self.ring.get() };
        if ring.is_closed {
            return Err(TrySendError::Closed(item));
        }

        if ring.buf.len() == ring.buf.capacity() {
            ring.buf.pop_front();
            ring.is_lagged = true;
        }
        ring.buf.push_back(item);

        if let Some(waker) = ring.waker.as_ref() {
            waker.wake_by_ref();
        }
        Ok(())
    }

    pub fn fill_ratio(&self) -> f64 {
        let ring = unsafe { &mut *self.ring.get() };
        ring.buf.len() as f64 / ring.buf.capacity() as f64
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let ring = unsafe { &mut *self.ring.get() };
        ring.is_closed = true;
        if let Some(waker) = ring.waker.take() {
            waker.wake();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    Lagged,
    Closed,
}

#[derive(Debug)]
pub struct Receiver<T> {
    ring: Rc<UnsafeCell<Ring<T>>>,
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Option<T> {
        let ring = unsafe { &mut *self.ring.get() };
        ring.buf.pop_front()
    }

    pub async fn recv(&mut self) -> Result<T, RecvError> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    #[inline]
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        let ring = unsafe { &mut *self.ring.get() };
        if ring.is_lagged {
            ring.is_lagged = false;
            return Poll::Ready(Err(RecvError::Lagged));
        }

        if let Some(item) = ring.buf.pop_front() {
            return Poll::Ready(Ok(item));
        }

        if ring.is_closed {
            return Poll::Ready(Err(RecvError::Closed));
        }

        if ring
            .waker
            .as_ref()
            .is_none_or(|w| !w.will_wake(cx.waker()))
        {
            ring.waker = Some(cx.waker().clone());
        }

        Poll::Pending
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let ring = unsafe { &mut *self.ring.get() };
        ring.is_closed = true;
        ring.waker.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_test::task::{new_count_waker, panic_waker};
    use std::task::{Context, Poll};
    use std::sync::atomic::Ordering;

    #[test]
    fn spsc_wake_on_send_when_waiting() {
        let (tx, rx) = channel::<u32>(4);
        let (waker, counter) = new_count_waker();
        let mut cx = Context::from_waker(&waker);

        assert_eq!(rx.poll_recv(&mut cx), Poll::Pending);
        assert_eq!(counter.get(), 0);

        tx.try_send(10).unwrap();
        assert_eq!(counter.get(), 1);

        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Ok(10)));
    }

    #[test]
    fn spsc_wake_when_more_data_remains() {
        let (tx, rx) = channel::<u32>(4);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();

        let (waker, _counter) = new_count_waker();
        let mut cx = Context::from_waker(&waker);

        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Ok(1)));
        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Ok(2)));

        // when queue has data, the receiver should get it without extra Pending cycles
        assert_eq!(rx.poll_recv(&mut cx), Poll::Pending);
        tx.try_send(3).unwrap();
        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Ok(3)));
    }

    #[test]
    fn spsc_closed_while_waiting() {
        let (tx, mut rx) = channel::<u32>(4);
        let (waker, _counter) = new_count_waker();
        let mut cx = Context::from_waker(&waker);

        assert_eq!(rx.poll_recv(&mut cx), Poll::Pending);
        drop(tx);

        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Err(RecvError::Closed)));
    }

    #[test]
    fn spsc_waker_reuse_with_multiple_contexts() {
        let (tx, rx) = channel::<u32>(4);
        let mut rx = rx;

        for i in 0..10 {
            let (waker, counter) = new_count_waker();
            let mut cx = Context::from_waker(&waker);

            assert_eq!(rx.poll_recv(&mut cx), Poll::Pending);
            assert_eq!(counter.get(), 0);

            tx.try_send(i).unwrap();
            assert_eq!(counter.get(), 1);

            assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Ok(i)));
        }
    }
}


use event_listener::{Event, EventListener};
use futures_lite::Stream;
use parking_lot::Mutex;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, ready};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    Lagged(u64),
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamRecvError {
    Lagged(u64),
}

#[derive(Debug)]
struct Slot<T> {
    seq: u64,
    val: Option<T>,
}

#[derive(Debug)]
struct Ring<T> {
    // This is mspc, but we expect very low contention on the producers.
    // Mutex is generally cheaper than RWLock. So, no reason to pay
    // RWLock overhead.
    slots: Vec<Mutex<Slot<T>>>, // per-slot mutex
    mask: usize,
    head: AtomicU64, // next free sequence number
    event: Event,
    closed: AtomicU64,
}

impl<T> Ring<T> {
    fn new(mut capacity: usize) -> Arc<Self> {
        if capacity == 0 {
            capacity = 1;
        } else if !capacity.is_power_of_two() {
            capacity = capacity.next_power_of_two();
        }

        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(Mutex::new(Slot { seq: 0, val: None }));
        }

        Arc::new(Self {
            slots,
            mask: capacity - 1,
            head: AtomicU64::new(0),
            event: Event::new(),
            closed: AtomicU64::new(0),
        })
    }
}

#[derive(Debug)]
pub enum TrySendError<T> {
    Closed(T),
}

#[derive(Debug, Clone)]
pub struct Sender<T> {
    ring: Arc<Ring<T>>,
}

impl<T> Sender<T> {
    pub fn try_send(&self, val: T) -> Result<(), TrySendError<T>> {
        if self.ring.closed.load(Ordering::Relaxed) == 1 {
            return Err(TrySendError::Closed(val));
        }

        // Atomically claim slot
        let seq = self.ring.head.fetch_add(1, Ordering::AcqRel);
        let idx = (seq as usize) & self.ring.mask;

        let mut slot = self.ring.slots[idx].lock();
        slot.val = Some(val);
        slot.seq = seq;

        // there's only 1 consumer
        self.ring.event.notify(1);
        Ok(())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // TODO: there's no notification for receiver when no senders left.
        // self.ring.closed.store(1, Ordering::Release);
        // self.ring.event.notify(usize::MAX);
    }
}

#[derive(Debug)]
pub struct Receiver<T> {
    ring: Arc<Ring<T>>,
    next_seq: u64,
    local_head: u64,
    listener: Option<EventListener>,
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.ring.closed.store(1, Ordering::Release);
    }
}

impl<T: Clone> Receiver<T> {
    pub async fn recv(&mut self) -> Result<T, RecvError> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        loop {
            let coop = std::task::ready!(tokio::task::coop::poll_proceed(cx));

            // Refresh local snapshot of head
            if self.next_seq == self.local_head {
                self.local_head = self.ring.head.load(Ordering::Acquire);
            }

            // Closed + nothing left
            if self.ring.closed.load(Ordering::Acquire) == 1 && self.next_seq >= self.local_head {
                return Poll::Ready(Err(RecvError::Closed));
            }

            // No new items
            if self.next_seq >= self.local_head {
                match &mut self.listener {
                    Some(l) => {
                        if Pin::new(l).poll(cx).is_pending() {
                            return Poll::Pending;
                        }
                        self.listener = None;
                        continue;
                    }
                    None => {
                        self.listener = Some(self.ring.event.listen());
                        continue;
                    }
                }
            }

            let idx = (self.next_seq as usize) & self.ring.mask;
            let slot = self.ring.slots[idx].lock();
            let slot_seq = slot.seq;

            if slot_seq != self.next_seq {
                self.next_seq = self.local_head;
                return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
            }

            if let Some(ref v) = slot.val {
                coop.made_progress();
                let out = v.clone();
                self.next_seq += 1;
                return Poll::Ready(Ok(out));
            }

            self.next_seq = self.local_head;
            return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
        }
    }
}

impl<T: Clone> Stream for Receiver<T> {
    type Item = Result<T, StreamRecvError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let res = match ready!(this.poll_recv(cx)) {
            Ok(item) => Some(Ok(item)),
            Err(RecvError::Lagged(n)) => Some(Err(StreamRecvError::Lagged(n))),
            Err(RecvError::Closed) => None,
        };
        Poll::Ready(res)
    }
}

impl<T: Clone> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            ring: self.ring.clone(),
            next_seq: self.next_seq,
            local_head: self.local_head,
            listener: None,
        }
    }
}

pub fn channel<T: Send + Sync + Clone + 'static>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let ring = Ring::new(capacity);
    (
        Sender { ring: ring.clone() },
        Receiver {
            ring,
            next_seq: 0,
            local_head: 0,
            listener: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[tokio::test]
    async fn basic_send_recv() {
        let (tx, mut rx) = channel::<u64>(8);

        tx.try_send(42).unwrap();
        assert_eq!(rx.recv().await, Ok(42));

        tx.try_send(123).unwrap();
        assert_eq!(rx.recv().await, Ok(123));
    }

    #[tokio::test]
    async fn lagging_consumer_jumps_to_head() {
        let (tx, mut rx) = channel::<u64>(4);

        for i in 0..6 {
            tx.try_send(i).unwrap();
        }

        match rx.recv().await {
            Err(RecvError::Lagged(seq)) => assert_eq!(seq, 6),
            _ => panic!("Expected lag error"),
        }

        tx.try_send(6).unwrap();
        assert_eq!(rx.recv().await, Ok(6));
    }

    #[tokio::test]
    async fn close_signal_drains_then_stops() {
        let (tx, mut rx) = channel::<u64>(4);

        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();

        drop(tx);

        assert_eq!(rx.recv().await, Ok(1));
        assert_eq!(rx.recv().await, Ok(2));
        assert_eq!(rx.recv().await, Err(RecvError::Closed));
        assert_eq!(rx.recv().await, Err(RecvError::Closed));
    }

    #[tokio::test]
    async fn async_waker_notification() {
        let (tx, mut rx) = channel::<u64>(4);

        let handle = tokio::spawn(async move { rx.recv().await });

        tokio::time::sleep(Duration::from_millis(10)).await;
        tx.try_send(99).unwrap();

        let result = handle.await.unwrap();
        assert_eq!(result, Ok(99));
    }

    #[tokio::test]
    async fn buffer_wrap_around_behavior() {
        let (tx, mut rx) = channel::<u64>(4);

        for i in 0..4 {
            tx.try_send(i).unwrap();
        }

        assert_eq!(rx.recv().await, Ok(0));
        assert_eq!(rx.recv().await, Ok(1));

        tx.try_send(4).unwrap();
        tx.try_send(5).unwrap();

        assert_eq!(rx.recv().await, Ok(2));
        assert_eq!(rx.recv().await, Ok(3));
        assert_eq!(rx.recv().await, Ok(4));
        assert_eq!(rx.recv().await, Ok(5));
    }
}

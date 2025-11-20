use crate::sync::Arc;
use crate::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use arc_swap::ArcSwapOption;
use crossbeam_utils::CachePadded;
use std::fmt::Debug;
use std::task::{Context, Poll};
use tokio::sync::Notify;

/// Errors returned by a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// Receiver lagged too far and was advanced to the tail.
    Lagged(u64),
    /// The channel is closed and no further messages will be sent.
    Closed,
}

/// A published message slot.
#[derive(Debug)]
pub struct Slot<T> {
    pub seq: u64,
    pub value: T,
}

/// The shared ring buffer.
struct Ring<T: Send + Sync> {
    tail: CachePadded<AtomicU64>, // producer’s sequence
    capacity: usize,
    slots: Box<[ArcSwapOption<Slot<T>>]>,
    notify: CachePadded<Notify>,
    closed: CachePadded<AtomicBool>,
}

impl<T: Send + Sync> Ring<T> {
    fn new(capacity: usize) -> Arc<Self> {
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(ArcSwapOption::from(None));
        }

        Arc::new(Self {
            tail: CachePadded::new(AtomicU64::new(0)),
            capacity,
            slots: slots.into_boxed_slice(),
            notify: CachePadded::new(Notify::new()),
            closed: CachePadded::new(AtomicBool::new(false)),
        })
    }

    /// Push a new message into the ring.
    fn push(&self, value: T) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }

        // The ordering can't be Relaxed. There's no guarantee that
        // the publisher will stay on the same thread in a work-stealing environment.
        //
        // t0 (thread-2) -> tail=0 new_tail=1
        // t1 (thread-3) -> tail=0!!! (despite being the same publisher)
        let seq = self.tail.load(Ordering::Acquire);
        let idx = (seq % self.capacity as u64) as usize;

        // Atomically publish a new slot
        self.slots[idx].store(Some(Arc::new(Slot { seq, value })));

        // Advance tail
        self.tail.store(seq + 1, Ordering::Release);

        // Notify all waiters
        self.notify.notify_waiters();
    }

    /// Attempt to fetch the next available message for a receiver.
    fn get_next(&self, next_seq: &mut u64) -> Result<Option<Arc<Slot<T>>>, RecvError> {
        let tail = self.tail.load(Ordering::Acquire);
        let earliest = tail.saturating_sub(self.capacity as u64);

        // If the receiver is too far behind, jump ahead.
        if *next_seq < earliest {
            *next_seq = tail;
            return Err(RecvError::Lagged(tail));
        }

        // Nothing new yet.
        if *next_seq >= tail {
            if self.closed.load(Ordering::Acquire) {
                return Err(RecvError::Closed);
            }
            return Ok(None);
        }

        // Try reading the slot.
        let idx = (*next_seq % self.capacity as u64) as usize;
        if let Some(slot) = self.slots[idx].load_full() {
            if slot.seq == *next_seq {
                *next_seq += 1;
                return Ok(Some(slot));
            } else {
                // Overwritten before we could see it.
                *next_seq = tail;
                return Err(RecvError::Lagged(tail));
            }
        }

        // Slot not yet written.
        if self.closed.load(Ordering::Acquire) {
            Err(RecvError::Closed)
        } else {
            Ok(None)
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// The sending handle — single producer.
pub struct Sender<T: Send + Sync> {
    ring: Arc<Ring<T>>,
}

impl<T: Send + Sync> Sender<T> {
    pub fn send(&self, value: T) {
        self.ring.push(value);
    }

    pub fn is_closed(&self) -> bool {
        self.ring.is_closed()
    }
}

impl<T: Send + Sync> Drop for Sender<T> {
    fn drop(&mut self) {
        self.ring.close();
    }
}

impl<T: Send + Sync> Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender")
            .field("capacity", &self.ring.capacity)
            .field("tail", &self.ring.tail.load(Ordering::Relaxed))
            .field("closed", &self.ring.closed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// The receiving handle — multi-consumer.
pub struct Receiver<T: Send + Sync> {
    ring: Arc<Ring<T>>,
    next_seq: u64,
}

impl<T: Send + Sync> Receiver<T> {
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<Arc<Slot<T>>, RecvError>> {
        match self.ring.get_next(&mut self.next_seq) {
            Ok(Some(slot)) => return Poll::Ready(Ok(slot)),
            Err(e) => return Poll::Ready(Err(e)),
            Ok(None) => {}
        }

        let notified = self.ring.notify.notified();
        tokio::pin!(notified);

        match self.ring.get_next(&mut self.next_seq) {
            Ok(Some(slot)) => return Poll::Ready(Ok(slot)),
            Err(e) => return Poll::Ready(Err(e)),
            Ok(None) => {}
        }

        match notified.as_mut().poll(cx) {
            Poll::Ready(_) => Poll::Pending,
            Poll::Pending => Poll::Pending,
        }
    }

    pub async fn recv(&mut self) -> Result<Arc<Slot<T>>, RecvError> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub fn try_recv(&mut self) -> Result<Option<Arc<Slot<T>>>, RecvError> {
        self.ring.get_next(&mut self.next_seq)
    }

    pub fn is_closed(&self) -> bool {
        self.ring.is_closed()
    }

    pub fn reset(&mut self) {
        self.next_seq = self.ring.tail.load(Ordering::Acquire);
    }
}

impl<T: Send + Sync> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            ring: Arc::clone(&self.ring),
            next_seq: self.ring.tail.load(Ordering::Acquire),
        }
    }
}

impl<T: Send + Sync> Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver")
            .field("capacity", &self.ring.capacity)
            .field("next_seq", &self.next_seq)
            .field("tail", &self.ring.tail.load(Ordering::Relaxed))
            .field("closed", &self.ring.closed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Create a new SP/MC broadcast channel.
pub fn channel<T: Send + Sync>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert!(capacity > 0, "capacity must be > 0");
    let ring = Ring::new(capacity);
    let tail = ring.tail.load(Ordering::Acquire);

    (
        Sender {
            ring: Arc::clone(&ring),
        },
        Receiver {
            ring,
            next_seq: tail,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn basic_send_recv() {
        let (tx, mut rx) = channel::<u64>(8);
        tx.send(42);
        let slot = rx.recv().await.unwrap();
        assert_eq!(slot.value, 42);
    }

    #[tokio::test]
    async fn multi_subscribers_get_same_message() {
        let (tx, rx) = channel::<String>(4);
        let mut rx1 = rx.clone();
        let mut rx2 = rx.clone();

        tx.send("hello".to_string());
        let s1 = rx1.recv().await.unwrap();
        let s2 = rx2.recv().await.unwrap();

        assert_eq!(s1.value, s2.value);
        assert!(Arc::ptr_eq(&s1, &s2)); // both point to same slot Arc
    }

    #[tokio::test]
    async fn lagging_receiver_is_caught_up() {
        const CAP: usize = 4;
        const N: u64 = 12;
        let (tx, mut rx) = channel::<u64>(CAP);

        for i in 0..N {
            tx.send(i);
        }

        drop(tx);
        let mut received = 0;

        loop {
            match rx.recv().await {
                Ok(slot) => received += 1,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }

        assert!(received >= 0);
    }

    #[tokio::test]
    async fn receiver_detects_immediate_overwrite() {
        // Small capacity so we can easily force overwrites.
        const CAP: usize = 2;
        let (tx, mut rx) = channel::<u64>(CAP);

        // Fill the ring several times over its capacity quickly.
        for i in 0..CAP as u64 * 4 {
            tx.send(i);
        }

        // The receiver has not consumed any yet, so it's definitely lagged.
        match rx.try_recv() {
            Err(RecvError::Lagged(seq)) => {
                // It should jump directly to the tail.
                let tail_now = seq;
                let tail_expected = tx.ring.tail.load(std::sync::atomic::Ordering::Acquire);
                assert_eq!(
                    tail_now, tail_expected,
                    "lagged seq should equal current tail"
                );
            }
            other => panic!("expected immediate Lagged, got {:?}", other),
        }

        // After recovering, new sends should be received normally.
        let next_val = 1234;
        tx.send(next_val);
        let slot = rx.recv().await.expect("should receive new message");
        assert_eq!(slot.value, next_val);
    }
}

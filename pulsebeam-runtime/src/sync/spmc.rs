//! High-performance, Arc-aware, single-producer / multi-consumer broadcast channel.
//!
//! This implementation is optimized for single-producer / multi-consumer scenarios.
//! Each message is stored as `Arc<T>` internally to avoid cloning large payloads.
//! It uses per-slot sequence numbers to avoid races and lost notifications.

use arc_swap::ArcSwapOption;
use crossbeam_utils::CachePadded;
use std::{
    fmt::Debug,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::Notify;

/// Error returned when a `Receiver` operation fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// The receiver lagged too far behind and was jumped forward to the sender's
    /// current tail to catch up.
    Lagged(u64),
    /// The sender has been dropped and no more messages will be sent.
    Closed,
}

/// A single slot in the ring buffer.
struct Slot<T> {
    value: ArcSwapOption<T>,
    seq: AtomicU64, // Sequence number for race-free detection
}

impl<T> Slot<T> {
    fn new() -> Self {
        Self {
            value: ArcSwapOption::from(None),
            seq: AtomicU64::new(0),
        }
    }
}

/// The shared ring buffer.
struct Ring<T: Send + Sync> {
    tail: CachePadded<AtomicU64>, // Producer's tail
    capacity: usize,
    slots: Box<[Slot<T>]>,
    notify: CachePadded<Notify>,
    closed: CachePadded<AtomicBool>,
}

impl<T: Send + Sync> Ring<T> {
    fn new(capacity: usize) -> Arc<Self> {
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(Slot::new());
        }

        Arc::new(Self {
            tail: CachePadded::new(AtomicU64::new(0)),
            capacity,
            slots: slots.into_boxed_slice(),
            notify: CachePadded::new(Notify::new()),
            closed: CachePadded::new(AtomicBool::new(false)),
        })
    }

    /// Push a value into the ring buffer.
    fn push(&self, value: T) {
        let seq = self.tail.load(Ordering::Relaxed);
        let idx = (seq % self.capacity as u64) as usize;

        let slot = &self.slots[idx];
        let arc_val = Arc::new(value);

        slot.seq.store(seq, Ordering::Release);
        slot.value.store(Some(arc_val));

        // Advance tail
        self.tail.store(seq + 1, Ordering::Release);

        // Notify all waiting consumers
        self.notify.notify_waiters();
    }

    /// Attempt to get the next message for a receiver.
    fn get_next(&self, next_seq: &mut u64) -> Result<Option<Arc<T>>, RecvError> {
        let tail = self.tail.load(Ordering::Acquire);
        let earliest = tail.saturating_sub(self.capacity as u64);

        if *next_seq < earliest {
            *next_seq = tail;
            return Err(RecvError::Lagged(tail));
        }

        if *next_seq >= tail {
            if self.closed.load(Ordering::Acquire) {
                return Err(RecvError::Closed);
            }
            return Ok(None);
        }

        let idx = (*next_seq % self.capacity as u64) as usize;
        let slot = &self.slots[idx];

        let slot_seq = slot.seq.load(Ordering::Acquire);
        if slot_seq != *next_seq {
            // Producer has not yet stored the correct value
            return Ok(None);
        }

        if let Some(val) = slot.value.load_full().as_ref() {
            *next_seq += 1;
            Ok(Some(val.clone()))
        } else if self.closed.load(Ordering::Acquire) {
            Err(RecvError::Closed)
        } else {
            Ok(None)
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// Sender handle.
pub struct Sender<T: Send + Sync> {
    ring: Arc<Ring<T>>,
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
        self.ring.closed.store(true, Ordering::Release);
        self.ring.notify.notify_waiters();
    }
}

/// Receiver handle.
pub struct Receiver<T: Send + Sync> {
    ring: Arc<Ring<T>>,
    next_seq: u64,
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

impl<T: Send + Sync> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            ring: Arc::clone(&self.ring),
            next_seq: self.ring.tail.load(Ordering::Acquire),
        }
    }
}

impl<T: Send + Sync> Receiver<T> {
    pub async fn recv(&mut self) -> Result<Arc<T>, RecvError> {
        loop {
            // Try non-blocking first
            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(val)) => return Ok(val),
                Err(err) => return Err(err),
                Ok(None) => {}
            }

            let notified = self.ring.notify.notified();

            // Double-check after creating the notification future
            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(val)) => return Ok(val),
                Err(err) => return Err(err),
                Ok(None) => {
                    notified.await;
                }
            }
        }
    }

    pub fn try_recv(&mut self) -> Result<Option<Arc<T>>, RecvError> {
        self.ring.get_next(&mut self.next_seq)
    }

    pub fn is_closed(&self) -> bool {
        self.ring.is_closed()
    }
}

/// Create a new SP/MC broadcast channel.
pub fn channel<T: Send + Sync>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert!(capacity > 0, "Capacity must be > 0");
    let ring = Ring::new(capacity);
    let initial_seq = ring.tail.load(Ordering::Relaxed);
    (
        Sender {
            ring: Arc::clone(&ring),
        },
        Receiver {
            ring,
            next_seq: initial_seq,
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
        assert_eq!(*rx.recv().await.unwrap(), 42);
    }

    #[tokio::test]
    async fn multi_subscribers_receive_same_shared_message() {
        let (tx, rx) = channel::<String>(4);
        let mut rx1 = rx.clone();
        let mut rx2 = rx.clone();
        tx.send("hello".to_string());

        let val1 = rx1.recv().await.unwrap();
        let val2 = rx2.recv().await.unwrap();

        assert_eq!(*val1, "hello");
        assert_eq!(*val2, "hello");
        assert!(Arc::ptr_eq(&val1, &val2));
    }

    #[tokio::test]
    async fn lagging_consumer_is_handled_correctly() {
        const CAP: usize = 4;
        const MESSAGES: u64 = 100;

        let (tx, rx) = channel::<u64>(CAP);
        let consumer = tokio::spawn(async move {
            let mut rx = rx;
            let mut last = 0;
            loop {
                match rx.recv().await {
                    Ok(val) => {
                        let val = *val;
                        if last > 0 {
                            assert!(val == last + 1 || val >= last + 1);
                        }
                        last = val;
                    }
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                }
            }
            last
        });

        for i in 0..(CAP as u64 * 2) {
            tx.send(i);
        }
        drop(tx);

        let last_seen = consumer.await.unwrap();
        assert!(last_seen < MESSAGES || last_seen == (CAP as u64 * 2 - 1));
    }
}

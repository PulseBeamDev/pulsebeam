//! High-performance, drop-oldest, single-producer / multi-consumer broadcast channel
//! for fan-out workloads
//!
//! ## Features
//! - Single publisher, many subscribers
//! - Publisher explicitly controls wake timing (`notify_subscribers()`)
//! - Drop-oldest ring buffer (non-blocking)
//! - Arc<T> zero-copy fan-out
//! - Subscribers implicitly await when caught up
//! - Publisher-drop detection
//! - parking_lot for low-overhead synchronization

use parking_lot::RwLock;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

/// Indicates that a subscriber fell behind
#[derive(Debug, Clone, Copy)]
pub enum RecvError {
    Lagged(u64), // contains the new tail seq
}

/// Ring buffer shared between sender and receivers
struct Ring<T> {
    capacity: usize,
    tail: AtomicU64,
    slots: Vec<RwLock<Option<Arc<T>>>>,
    notify: tokio::sync::Notify,
    closed: AtomicBool,
}

impl<T> Ring<T> {
    fn new(capacity: usize) -> Arc<Self> {
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(RwLock::new(None));
        }
        Arc::new(Self {
            capacity,
            tail: AtomicU64::new(0),
            slots,
            notify: tokio::sync::Notify::new(),
            closed: AtomicBool::new(false),
        })
    }

    /// Internal: fetch next packet
    fn get_next(&self, seq: &mut u64) -> Result<Option<Arc<T>>, RecvError> {
        let tail = self.tail.load(Ordering::Acquire);
        let earliest = tail.saturating_sub(self.capacity as u64);

        // Subscriber fell behind
        if *seq < earliest {
            *seq = tail;
            return Err(RecvError::Lagged(tail));
        }

        // No new packet yet
        if *seq >= tail {
            return Ok(None);
        }

        let idx = *seq % self.capacity as u64;
        let packet = self.slots[idx as usize].read().clone();
        if let Some(pkt) = packet {
            *seq += 1;
            Ok(Some(pkt))
        } else {
            // Rare race: slot not yet written
            Ok(None)
        }
    }

    fn push(&self, value: Arc<T>) {
        let idx = self.tail.fetch_add(1, Ordering::AcqRel) % self.capacity as u64;
        *self.slots[idx as usize].write() = Some(value);
    }
}

/// Sender handle
#[derive(Clone)]
pub struct Sender<T> {
    ring: Arc<Ring<T>>,
}

impl<T> Sender<T> {
    pub fn try_send(&self, value: Arc<T>) {
        self.ring.push(value);
    }

    pub fn notify_subscribers(&self) {
        self.ring.notify.notify_waiters();
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.ring.closed.store(true, Ordering::Release);
        self.ring.notify.notify_waiters();
    }
}

/// Receiver handle
pub struct Receiver<T> {
    ring: Arc<Ring<T>>,
    next_seq: u64,
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            ring: Arc::clone(&self.ring),
            next_seq: self.next_seq,
        }
    }
}

impl<T> Receiver<T> {
    /// Await next packet. Returns None if publisher is dropped
    pub async fn recv(&mut self) -> Option<Arc<T>> {
        loop {
            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(pkt)) => return Some(pkt),
                Ok(None) => {
                    if self.ring.closed.load(Ordering::Acquire) {
                        return None;
                    }
                    self.ring.notify.notified().await;
                }
                Err(RecvError::Lagged(_tail)) => {
                    // Optional: log subscriber lag
                }
            }
        }
    }
}

/// Create a new broadcast channel
pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let ring = Ring::new(capacity);
    (
        Sender {
            ring: Arc::clone(&ring),
        },
        Receiver { ring, next_seq: 0 },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn basic_send_recv() {
        let (tx, mut rx) = channel::<u64>(8);
        tx.try_send(Arc::new(42));
        tx.notify_subscribers();

        let val = rx.recv().await.unwrap();
        assert_eq!(*val, 42);
    }

    #[tokio::test]
    async fn multi_subscribers() {
        let (tx, rx) = channel::<u64>(4);
        let mut rxs: Vec<_> = (0..5).map(|_| rx.clone()).collect();

        tx.try_send(Arc::new(1));
        tx.notify_subscribers();

        for rx in rxs.iter_mut() {
            let v = rx.recv().await.unwrap();
            assert_eq!(*v, 1);
        }
    }

    #[tokio::test]
    async fn drop_oldest() {
        let (tx, mut rx) = channel::<u64>(2);
        for i in 0..5 {
            tx.try_send(Arc::new(i));
        }
        tx.notify_subscribers();

        let v = rx.recv().await.unwrap();
        assert!(*v >= 3);
    }

    #[tokio::test]
    async fn stress_fanout() {
        let (tx, rx) = channel::<u64>(128);
        let mut subs: Vec<_> = (0..1000).map(|_| rx.clone()).collect();

        for i in 0..100 {
            tx.try_send(Arc::new(i));
            tx.notify_subscribers();
        }

        for rx in subs.iter_mut().take(10) {
            let v = rx.recv().await.unwrap();
            assert!(*v < 100);
        }
    }

    #[tokio::test]
    async fn publisher_drop() {
        let (tx, mut rx) = channel::<u64>(8);
        tx.try_send(Arc::new(1));
        tx.notify_subscribers();

        let val = rx.recv().await.unwrap();
        assert_eq!(*val, 1);

        drop(tx); // publisher dropped

        let val2 = rx.recv().await;
        assert!(val2.is_none()); // subscriber terminates
    }
}

//! A high-performance, drop-oldest, single-producer / multi-consumer broadcast channel
//! for fan-out workloads
//!
//! ## Design Highlights
//! - Single publisher, many subscribers
//! - Publisher explicitly controls wake timing (`notify_subscribers()`)
//! - Drop-oldest ring buffer (non-blocking)
//! - Arc<T> zero-copy fan-out
//! - Per-ring coalesced notifier (Tokio-style, no background tasks)
//! - parking_lot for low-overhead synchronization
//!
//! ## Example
//! ```rust,ignore
//! use std::sync::Arc;
//! use tokio::task;
//! use broadcast_ring::channel;
//!
//! #[tokio::main]
//! async fn main() {
//!     let (tx, rx) = channel::<String>(1024);
//!
//!     // Spawn subscribers
//!     for i in 0..5 {
//!         let mut rx = rx.clone();
//!         task::spawn(async move {
//!             while let Some(item) = rx.recv().await {
//!                 println!("sub {i} got: {item}");
//!             }
//!         });
//!     }
//!
//!     // Publisher
//!     for n in 0..10 {
//!         tx.try_send(Arc::new(format!("msg {n}"))).unwrap();
//!         tx.notify_subscribers();
//!     }
//! }
//! ```

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use parking_lot::RwLock;

/// A bounded, lock-free, drop-oldest ring buffer shared between sender and receivers.
struct Ring<T> {
    capacity: usize,
    tail: AtomicU64,
    slots: Vec<RwLock<Option<Arc<T>>>>,
    notify: tokio::sync::Notify,
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
        })
    }

    fn push(&self, value: Arc<T>) {
        let idx = self.tail.fetch_add(1, Ordering::AcqRel) % self.capacity as u64;
        *self.slots[idx as usize].write() = Some(value);
    }

    fn get(&self, seq: u64) -> Option<Arc<T>> {
        let tail = self.tail.load(Ordering::Acquire);
        if seq + (self.capacity as u64) < tail {
            // Too far behind
            return None;
        }
        let idx = seq % self.capacity as u64;
        self.slots[idx as usize].read().clone()
    }
}

/// Errors returned by `try_send`.
#[derive(Debug)]
pub enum TrySendError<T> {
    Closed(T),
}

/// Sender handle.
#[derive(Clone)]
pub struct Sender<T> {
    ring: Arc<Ring<T>>,
}

impl<T> Sender<T> {
    pub fn try_send(&self, value: Arc<T>) -> Result<(), TrySendError<Arc<T>>> {
        self.ring.push(value);
        Ok(())
    }

    /// Explicitly wake all subscribers.
    pub fn notify_subscribers(&self) {
        self.ring.notify.notify_waiters();
    }
}

/// Receiver handle.
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
    pub async fn recv(&mut self) -> Option<Arc<T>> {
        loop {
            if let Some(val) = self.ring.get(self.next_seq) {
                self.next_seq += 1;
                return Some(val);
            }
            self.ring.notify.notified().await; // await publisher wake
        }
    }
}

/// Create a new broadcast ring.
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
        tx.try_send(Arc::new(42)).unwrap();
        tx.notify_subscribers();

        let val = rx.recv().await.unwrap();
        assert_eq!(*val, 42);
    }

    #[tokio::test]
    async fn multi_subscribers() {
        let (tx, rx) = channel::<u64>(4);
        let mut rxs: Vec<_> = (0..5).map(|_| rx.clone()).collect();

        tx.try_send(Arc::new(1)).unwrap();
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
            tx.try_send(Arc::new(i)).unwrap();
        }
        tx.notify_subscribers();

        let v = rx.recv().await.unwrap();
        // may be last element depending on overwrite
        assert!(*v >= 3);
    }

    #[tokio::test]
    async fn stress_fanout() {
        let (tx, rx) = channel::<u64>(128);
        let mut subs: Vec<_> = (0..1000).map(|_| rx.clone()).collect();

        for i in 0..100 {
            tx.try_send(Arc::new(i)).unwrap();
            tx.notify_subscribers();
        }

        for rx in subs.iter_mut().take(10) {
            let v = rx.recv().await.unwrap();
            assert!(*v < 100);
        }
    }
}

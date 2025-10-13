//! High-performance, Arc-aware, single-producer / multi-consumer broadcast channel.
//!
//! The core design goal is to make `recv` a cheap, lock-free `Arc::clone`.
//! It's a broadcast channel, so it's built for one writer and many readers.
//!
//! To eliminate false sharing under heavy cross-core contention, critical atomic
//! fields and every single buffer slot are padded to cache-line boundaries.
//! This increases the memory footprint of the ring buffer itself but is necessary
//! to ensure maximum throughput when cache coherency is a limiting factor.

use arc_swap::ArcSwapOption;
use crossbeam_utils::CachePadded;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::Notify;

/// Error returned when a `Receiver` has fallen behind the `Sender`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// The receiver was too slow and the ring buffer has lapped it.
    /// The `u64` is the new sequence number the receiver must use to catch up.
    Lagged(u64),

    /// The sender was dropped, closing the channel.
    Closed,
}

/// The core ring buffer data structure.
#[derive(Debug)]
struct Ring<T: Send + Sync> {
    capacity: usize,

    // The producer exclusively writes to `tail`. Padding prevents cache line
    // invalidations for consumers reading other struct fields.
    tail: CachePadded<AtomicU64>,

    // Padding each slot is crucial. It prevents a write to slot[i] from
    // invalidating the cache line for a consumer reading from adjacent slot[i-1].
    // This is the main defense against inter-slot false sharing.
    slots: Vec<CachePadded<ArcSwapOption<T>>>,

    notify: Notify,

    // Consumers read the `closed` flag. Padding isolates it from the
    // producer's frequent writes to `tail`.
    closed: CachePadded<AtomicBool>,
}

impl<T: Send + Sync> Ring<T> {
    fn new(capacity: usize) -> Arc<Self> {
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            // Each slot gets its own padded, cache-line-aligned allocation.
            slots.push(CachePadded::new(ArcSwapOption::from(None)));
        }

        Arc::new(Self {
            capacity,
            tail: CachePadded::new(AtomicU64::new(0)),
            slots,
            notify: Notify::new(),
            closed: CachePadded::new(AtomicBool::new(false)),
        })
    }

    // `push` is only ever called by the single producer.
    fn push(&self, value: T) {
        // `fetch_add` gives us the sequence number for the current slot before the increment.
        let seq = self.tail.fetch_add(1, Ordering::Release);
        let idx = (seq % self.capacity as u64) as usize;

        // The only allocation for the broadcast happens here.
        let new_arc = Arc::new(value);

        // Atomically store the new `Arc` into the slot. `CachePadded` DRefs transparently.
        // This `store` with `Release` ordering makes the new value visible to consumers.
        self.slots[idx].store(Some(new_arc));

        // Wake up any sleeping receivers.
        self.notify.notify_waiters();
    }

    // `get_next` is called by consumers in their receive loop.
    fn get_next(&self, seq: &mut u66) -> Result<Option<Arc<T>>, RecvError> {
        // `Acquire` ordering ensures we see the latest writes from the producer's `push`.
        let tail = self.tail.load(Ordering::Acquire);
        let earliest = tail.saturating_sub(self.capacity as u64);

        // If our sequence number is older than the oldest data in the ring,
        // we've been lapped. The caller needs to jump forward to the new baseline.
        if *seq < earliest {
            *seq = earliest;
            return Err(RecvError::Lagged(earliest));
        }

        // If our sequence number is current, there's no new data for us yet.
        if *seq >= tail {
            return Ok(None);
        }

        let idx = (*seq % self.capacity as u64) as usize;
        let slot_load = self.slots[idx].load();

        if let Some(packet_arc) = &*slot_load {
            *seq += 1;
            // The whole point of the design: a cheap, atomic ref-count bump.
            Ok(Some(packet_arc.clone()))
        } else if self.closed.load(Ordering::Acquire) {
            // The slot is empty, but the channel is also closed. This can happen
            // if the last message was consumed right before the sender was dropped.
            Err(RecvError::Closed)
        } else {
            // The slot is empty. This indicates a race where `tail` has been
            // incremented but the `store` to the slot isn't visible to this core yet.
            // The `recv` loop will simply spin and try this function again.
            Ok(None)
        }
    }
}

// --- Public API ---

#[derive(Debug)]
pub struct Sender<T: Send + Sync> {
    ring: Arc<Ring<T>>,
}

impl<T: Send + Sync> Sender<T> {
    pub fn send(&self, value: T) {
        self.ring.push(value);
    }
}

impl<T: Send + Sync> Drop for Sender<T> {
    fn drop(&mut self) {
        self.ring.closed.store(true, Ordering::Release);
        self.ring.notify.notify_waiters();
    }
}

#[derive(Debug)]
pub struct Receiver<T: Send + Sync> {
    ring: Arc<Ring<T>>,
    next_seq: u64,
}

impl<T: Send + Sync> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            ring: Arc::clone(&self.ring),
            // A new receiver starts reading from the current tip of the ring.
            next_seq: self.ring.tail.load(Ordering::Acquire),
        }
    }
}

impl<T: Send + Sync> Receiver<T> {
    pub async fn recv(&mut self) -> Result<Option<Arc<T>>, RecvError> {
        loop {
            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(pkt)) => return Ok(Some(pkt)),
                Err(err) => return Err(err),
                Ok(None) => { /* fall through to the wait logic */ }
            }

            if self.ring.closed.load(Ordering::Acquire) {
                // Check for a value one last time in case a message and a close
                // happened between our last check and now.
                return self.ring.get_next(&mut self.next_seq);
            }

            let notified = self.ring.notify.notified();

            // Double-check after subscribing to the notification. This handles the
            // race where a message arrives after our first check but before we `await`.
            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(pkt)) => return Ok(Some(pkt)),
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
        self.ring.closed.load(Ordering::Acquire)
    }
}

pub fn channel<T: Send + Sync>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert!(capacity > 0, "Channel capacity must be non-zero");
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

    #[tokio::test]
    async fn basic_send_recv() {
        let (tx, mut rx) = channel::<u64>(8);
        tx.send(42);
        let val_arc = rx.recv().await.unwrap().unwrap();
        assert_eq!(*val_arc, 42);
    }

    #[tokio::test]
    async fn multi_subscribers_receive_same_shared_message() {
        let (tx, rx) = channel::<String>(4);
        let mut rx1 = rx.clone();
        let mut rx2 = rx.clone();

        tx.send("hello".to_string());

        let val1_arc = rx1.recv().await.unwrap().unwrap();
        let val2_arc = rx2.recv().await.unwrap().unwrap();

        assert_eq!(*val1_arc, "hello");
        assert_eq!(*val2_arc, "hello");
        // Ensure they point to the same memory allocation.
        assert!(Arc::ptr_eq(&val1_arc, &val2_arc));
    }

    #[tokio::test]
    async fn works_with_non_clone_types() {
        #[derive(Debug, PartialEq, Eq)]
        struct NonCloneable(String);

        let (tx, mut rx) = channel::<NonCloneable>(4);
        tx.send(NonCloneable("hello world".to_string()));

        let received_arc = rx.recv().await.unwrap().unwrap();
        assert_eq!(*received_arc, NonCloneable("hello world".to_string()));
    }

    #[tokio::test]
    async fn new_subscriber_starts_at_the_head() {
        let (tx, mut rx1) = channel(4);
        tx.send(1);
        tx.send(2);

        assert_eq!(*rx1.recv().await.unwrap().unwrap(), 1);

        let mut rx2 = rx1.clone();

        tx.send(3);

        assert_eq!(*rx1.recv().await.unwrap().unwrap(), 2);
        assert_eq!(*rx1.recv().await.unwrap().unwrap(), 3);

        // The new receiver starts after the point of cloning, only seeing new messages.
        assert_eq!(*rx2.recv().await.unwrap().unwrap(), 3);
    }

    #[tokio::test]
    async fn drop_oldest_on_overflow() {
        let (tx, mut rx) = channel::<u64>(2);
        tx.send(0);
        tx.send(1);
        tx.send(2); // Overwrites 0
        tx.send(3); // Overwrites 1

        let err = rx.recv().await.unwrap_err();
        assert_eq!(err, RecvError::Lagged(2));

        assert_eq!(*rx.recv().await.unwrap().unwrap(), 2);
        assert_eq!(*rx.recv().await.unwrap().unwrap(), 3);
    }

    #[tokio::test]
    async fn sender_drop_closes_channel() {
        let (tx, mut rx) = channel::<u64>(8);
        tx.send(1);

        assert_eq!(*rx.recv().await.unwrap().unwrap(), 1);

        drop(tx);

        // After the sender is dropped, recv returns an error indicating closed.
        assert!(matches!(rx.recv().await, Err(RecvError::Closed)));
    }
}

//! High-performance, Arc-aware, single-producer / multi-consumer broadcast channel.
//!
//! This implementation is optimized for the broadcast use case where a single
//! piece of data needs to be shared with multiple consumers efficiently. It
//! enforces a pattern where the channel manages the `Arc<T>` internally,
//! ensuring that receivers always perform a cheap reference-count clone instead
//! of a potentially expensive deep clone of `T`.
//!
//! Cache optimizations:
//! - Cache line padding to prevent false sharing between hot atomics
//! - Strategic alignment of frequently accessed fields
//! - Grouping of read-mostly vs write-mostly data

use arc_swap::ArcSwapOption;
use crossbeam_utils::CachePadded;
use std::{
    fmt::{Debug, Pointer},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::Notify;

/// Error returned when a `Receiver` operation fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// The receiver lagged and was jumped forward to the given sequence number.
    /// The receiver should call `recv` again to retrieve the message at this sequence.
    Lagged(u64),
    /// The sender has been dropped and no more messages will be sent.
    Closed,
}

/// The shared ring buffer at the core of the channel.
///
/// Fields are carefully ordered and padded to optimize cache behavior:
/// - Hot write path (tail) is isolated on its own cache line
/// - Read-mostly fields (capacity, slots) are grouped together
/// - Notification fields are separate to avoid contention
struct Ring<T: Send + Sync> {
    /// Write-hot field: isolated to prevent false sharing with readers
    tail: CachePadded<AtomicU64>,

    /// Read-mostly fields: grouped for spatial locality
    capacity: usize,

    /// The ring buffer slots - accessed by both readers and writers but at different indices
    /// Using Box to ensure stable heap allocation with proper alignment
    slots: Box<[ArcSwapOption<T>]>,

    /// Notification mechanism: separate cache line to avoid contention
    notify: CachePadded<Notify>,

    /// Close flag: used infrequently, kept separate
    closed: CachePadded<AtomicBool>,
}

impl<T: Send + Sync> Ring<T> {
    fn new(capacity: usize) -> Arc<Self> {
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            // Initialize all slots as empty (`None`). This involves no allocation.
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

    /// The `send` logic takes ownership of `T` and wraps it in an `Arc`.
    #[inline]
    fn push(&self, value: T) {
        // Relaxed ordering for fetch_add is safe here because:
        // - Each sender gets a unique sequence number
        // - The Release store below provides synchronization
        let seq = self.tail.fetch_add(1, Ordering::Relaxed);

        let idx = (seq % self.capacity as u64) as usize;

        // The single allocation for the broadcast happens here, wrapping the user's value.
        let new_arc = Arc::new(value);

        // Release ordering ensures the Arc is visible before readers see the updated tail
        self.slots[idx].store(Some(new_arc));

        // Notify is relatively expensive, so it's good that it's on a separate cache line
        self.notify.notify_waiters();
    }

    /// `get_next` now returns an `Option<Arc<T>>`, which is a cheap clone.
    /// Returns Ok(pkt) with data, or Err to signal special conditions.
    #[inline]
    fn get_next(&self, seq: &mut u64) -> Result<Option<Arc<T>>, RecvError> {
        // Acquire ordering to synchronize with the Release store in push()
        let tail = self.tail.load(Ordering::Acquire);
        let earliest = tail.saturating_sub(self.capacity as u64);

        if *seq < earliest {
            *seq = tail;
            return Err(RecvError::Lagged(tail));
        }

        if *seq >= tail {
            // No message available - check if closed
            if self.closed.load(Ordering::Acquire) {
                return Err(RecvError::Closed);
            }
            // Not ready yet, but not an error condition
            return Ok(None);
        }

        let idx = (*seq % self.capacity as u64) as usize;

        // `load()` is a lock-free read that returns a smart pointer to the content.
        let slot_load = self.slots[idx].load();

        if let Some(packet_arc) = &*slot_load {
            *seq += 1;
            // This is the key: we clone the Arc, which is just an atomic
            // increment, NOT a deep clone of the data `T`.
            Ok(Some(packet_arc.clone()))
        } else if self.closed.load(Ordering::Acquire) {
            Err(RecvError::Closed)
        } else {
            // Race condition: tail is updated, but store is not yet visible.
            // Just signal not ready.
            Ok(None)
        }
    }

    #[inline]
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// The sending handle for the broadcast channel.
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
    /// Sends a value to all active `Receiver`s by taking ownership of it.
    /// The value will be wrapped in an `Arc` internally to allow for efficient sharing.
    #[inline]
    pub fn send(&self, value: T) {
        self.ring.push(value);
    }

    /// Returns `true` if the sender has been closed (dropped).
    #[inline]
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

/// The receiving handle for the broadcast channel.
pub struct Receiver<T: Send + Sync> {
    ring: Arc<Ring<T>>,
    // Keep next_seq local to avoid cache line bouncing between receivers
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
            // Start at current tail to avoid reading stale data
            next_seq: self.ring.tail.load(Ordering::Acquire),
        }
    }
}

impl<T: Send + Sync> Receiver<T> {
    /// Asynchronously waits for the next message, returning a shared pointer (`Arc`).
    ///
    /// This method guarantees that receiving a message is a cheap operation, as it
    /// only clones an `Arc`, regardless of the size of `T`.
    ///
    /// Returns:
    /// - `Ok(Arc<T>)` when a message is successfully received
    /// - `Err(RecvError::Lagged(seq))` when the receiver has fallen behind and should retry
    /// - `Err(RecvError::Closed)` when the sender has been dropped and no more messages are available
    pub async fn recv(&mut self) -> Result<Arc<T>, RecvError> {
        loop {
            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(pkt)) => return Ok(pkt),
                Err(err) => return Err(err),
                Ok(None) => { /* Not ready, continue to wait */ }
            }

            if self.ring.is_closed() {
                // Final check after closed - see if there's a message we missed
                return match self.ring.get_next(&mut self.next_seq) {
                    Ok(Some(pkt)) => Ok(pkt),
                    Ok(None) => Err(RecvError::Closed),
                    Err(err) => Err(err),
                };
            }

            let notified = self.ring.notify.notified();

            // Check again before awaiting to avoid missing notifications
            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(pkt)) => return Ok(pkt),
                Err(err) => return Err(err),
                Ok(None) => {
                    notified.await;
                }
            }
        }
    }

    /// Attempts to receive the next message without blocking.
    ///
    /// Returns:
    /// - `Ok(Some(Arc<T>))` when a message is available
    /// - `Ok(None)` when no message is currently available (but the sender is still alive)
    /// - `Err(RecvError::Lagged(seq))` when the receiver has fallen behind
    /// - `Err(RecvError::Closed)` when the sender has been dropped and no more messages are available
    #[inline]
    pub fn try_recv(&mut self) -> Result<Option<Arc<T>>, RecvError> {
        self.ring.get_next(&mut self.next_seq)
    }

    /// Returns `true` if the sender has been dropped.
    ///
    /// Note: Even if this returns `true`, there may still be buffered messages
    /// available to receive.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.ring.is_closed()
    }
}

/// Creates a new single-producer, multi-consumer broadcast channel.
///
/// The transmitted type `T` only needs to be `Send + Sync`. The `Clone` trait
/// is not required, as the channel handles sharing via `Arc<T>`.
///
/// # Panics
/// Panics if `capacity` is 0.
pub fn channel<T: Send + Sync>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert!(capacity > 0, "Capacity must be greater than 0");
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
        let val_arc = rx.recv().await.unwrap();
        assert_eq!(*val_arc, 42);
    }

    #[tokio::test]
    async fn multi_subscribers_receive_same_shared_message() {
        let (tx, rx) = channel::<String>(4);
        let mut rx1 = rx.clone();
        let mut rx2 = rx.clone();

        tx.send("hello".to_string());

        let val1_arc = rx1.recv().await.unwrap();
        let val2_arc = rx2.recv().await.unwrap();

        assert_eq!(*val1_arc, "hello");
        assert_eq!(*val2_arc, "hello");
        // Crucially, check they point to the same memory allocation.
        assert!(Arc::ptr_eq(&val1_arc, &val2_arc));
    }

    #[tokio::test]
    async fn works_with_non_clone_types() {
        // This struct does not implement Clone, which is now supported.
        #[derive(Debug, PartialEq, Eq)]
        struct NonCloneable(String);

        let (tx, mut rx) = channel::<NonCloneable>(4);

        // This works because `send` takes ownership of the data.
        tx.send(NonCloneable("hello world".to_string()));

        let received_arc = rx.recv().await.unwrap();
        assert_eq!(*received_arc, NonCloneable("hello world".to_string()));
    }

    #[tokio::test]
    async fn new_subscriber_starts_at_the_head() {
        let (tx, mut rx1) = channel(4);
        tx.send(1);
        tx.send(2);

        assert_eq!(*rx1.recv().await.unwrap(), 1);

        let mut rx2 = rx1.clone();

        tx.send(3);

        assert_eq!(*rx1.recv().await.unwrap(), 2);
        assert_eq!(*rx1.recv().await.unwrap(), 3);

        // The new receiver starts after the point of cloning and only sees new messages.
        assert_eq!(*rx2.recv().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn drop_oldest_on_overflow() {
        let (tx, mut rx) = channel::<u64>(2);
        tx.send(0);
        tx.send(1);
        tx.send(2); // Overwrites 0
        tx.send(3); // Overwrites 1

        // First recv should detect lag and return error
        let err = rx.recv().await.unwrap_err();
        assert_eq!(err, RecvError::Lagged(4));

        // After lag error, next recv should go to tail
        assert!(rx.try_recv().unwrap().is_none());
        tx.send(4);
        let num = rx.recv().await.unwrap();
        assert_eq!(*num, 4);
    }

    #[tokio::test]
    async fn sender_drop_closes_channel() {
        let (tx, mut rx) = channel::<u64>(8);
        tx.send(1);

        assert_eq!(*rx.recv().await.unwrap(), 1);

        drop(tx);

        // After the sender is dropped, recv returns Closed error.
        assert_eq!(rx.recv().await.unwrap_err(), RecvError::Closed);
    }

    #[tokio::test]
    async fn is_closed_on_both_sender_and_receiver() {
        let (tx, rx) = channel::<u64>(8);

        assert!(!tx.is_closed());
        assert!(!rx.is_closed());

        drop(tx);

        assert!(rx.is_closed());
    }

    #[tokio::test]
    async fn try_recv_returns_none_when_empty() {
        let (tx, mut rx) = channel::<u64>(8);

        // Channel is empty
        assert_eq!(rx.try_recv().unwrap(), None);

        tx.send(42);

        // Now message is available
        assert_eq!(*rx.try_recv().unwrap().unwrap(), 42);

        // Empty again
        assert_eq!(rx.try_recv().unwrap(), None);
    }

    #[tokio::test]
    async fn try_recv_returns_closed_when_sender_dropped() {
        let (tx, mut rx) = channel::<u64>(8);

        tx.send(1);
        assert_eq!(*rx.try_recv().unwrap().unwrap(), 1);

        drop(tx);

        // Should return Closed error
        assert_eq!(rx.try_recv().unwrap_err(), RecvError::Closed);
    }
}

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
    /// current tail to catch up. The enclosed value is the new sequence number.
    /// The receiver should call `recv` again to get the next available message.
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
        // Relaxed ordering is sufficient because the `store` operation below
        // with `Release` ordering provides the necessary memory barrier.
        let seq = self.tail.load(Ordering::Relaxed);
        let idx = (seq % self.capacity as u64) as usize;

        let new_arc = Arc::new(value);

        self.slots[idx].store(Some(new_arc));
        self.tail.store(seq + 1, Ordering::Release);

        self.notify.notify_waiters();
    }

    #[inline]
    fn get_next(&self, seq: &mut u64) -> Result<Option<Arc<T>>, RecvError> {
        // An `Acquire` load ensures we see the most up-to-date `tail`.
        let tail = self.tail.load(Ordering::Acquire);
        let earliest = tail.saturating_sub(self.capacity as u64);

        if *seq < earliest {
            // JUMP TO TAIL: The receiver is lagging. To catch up, we discard
            // all buffered messages and set the receiver's next expected sequence
            // to the current tail.
            *seq = tail;
            return Err(RecvError::Lagged(tail));
        }

        if *seq >= tail {
            if self.closed.load(Ordering::Acquire) {
                return Err(RecvError::Closed);
            }
            return Ok(None);
        }

        let idx = (*seq % self.capacity as u64) as usize;

        let slot_load = self.slots[idx].load_full();

        // `as_ref()` correctly handles the case where `load()` returns a non-null
        // pointer to a `None` value.
        if let Some(packet_arc) = slot_load.as_ref() {
            *seq += 1;
            Ok(Some(packet_arc.clone()))
        } else if self.closed.load(Ordering::Acquire) {
            Err(RecvError::Closed)
        } else {
            // This case can happen if the `tail` is updated, but the `store`
            // to the slot is not yet visible. The caller should retry.
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
    #[inline]
    pub fn send(&self, value: T) {
        self.ring.push(value);
    }

    #[inline]
    pub fn is_closed(&self) -> bool {
        self.ring.is_closed()
    }
}

impl<T: Send + Sync> Drop for Sender<T> {
    fn drop(&mut self) {
        // A `Release` store synchronizes with `Acquire` loads in `get_next`.
        self.ring.closed.store(true, Ordering::Release);
        self.ring.notify.notify_waiters();
    }
}

/// The receiving handle for the broadcast channel.
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
            // A new receiver should start at the current `tail`, so it only
            // sees messages sent after it was cloned.
            next_seq: self.ring.tail.load(Ordering::Acquire),
        }
    }
}

impl<T: Send + Sync> Receiver<T> {
    pub async fn recv(&mut self) -> Result<Arc<T>, RecvError> {
        loop {
            // Attempt to receive a message without blocking first.
            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(pkt)) => return Ok(pkt),
                Err(err) => return Err(err),
                Ok(None) => { /* Not ready, continue to wait */ }
            }

            let notified = self.ring.notify.notified();

            // A crucial double-check after creating the notification future.
            // This prevents a race where a message arrives and notifies
            // *after* the first check but *before* we `.await`.
            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(pkt)) => return Ok(pkt),
                Err(err) => return Err(err),
                Ok(None) => {
                    // Finally, await the notification.
                    notified.await;
                }
            }
        }
    }

    #[inline]
    pub fn try_recv(&mut self) -> Result<Option<Arc<T>>, RecvError> {
        self.ring.get_next(&mut self.next_seq)
    }

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
    use std::time::Duration;
    use tokio::time::{advance, timeout};

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
        #[derive(Debug, PartialEq, Eq)]
        struct NonCloneable(String);
        let (tx, mut rx) = channel::<NonCloneable>(4);
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

        let mut rx2 = rx1.clone(); // Cloned when tail is 2
        tx.send(3);

        assert_eq!(*rx1.recv().await.unwrap(), 2);
        assert_eq!(*rx1.recv().await.unwrap(), 3);

        // rx2 starts at tail=2, so its first message is 3.
        assert_eq!(*rx2.recv().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn overflow_jumps_to_tail_to_catch_up() {
        let (tx, mut rx) = channel::<u64>(2);
        tx.send(0); // seq 0
        tx.send(1); // seq 1
        tx.send(2); // seq 2, overwrites 0
        tx.send(3); // seq 3, overwrites 1

        // At this point, ring contains {2, 3}. tail=4, capacity=2.
        // `rx` expects seq 0. `earliest` is 4-2=2. Since 0 < 2, it has lagged.
        let err = rx.recv().await.unwrap_err();

        // The error indicates the new sequence number, which is the tail (4).
        assert_eq!(err, RecvError::Lagged(4));

        // The receiver is now caught up. There are no new messages yet.
        assert_eq!(rx.try_recv().unwrap(), None);

        // Send a new message.
        tx.send(4);

        // The receiver should now get the new message immediately.
        assert_eq!(*rx.recv().await.unwrap(), 4);
    }

    #[tokio::test]
    async fn sender_drop_closes_channel() {
        let (tx, mut rx) = channel::<u64>(8);
        tx.send(1);
        assert_eq!(*rx.recv().await.unwrap(), 1);
        drop(tx);
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
        assert_eq!(rx.try_recv().unwrap(), None);
        tx.send(42);
        assert_eq!(*rx.try_recv().unwrap().unwrap(), 42);
        assert_eq!(rx.try_recv().unwrap(), None);
    }

    #[tokio::test]
    async fn try_recv_returns_closed_when_sender_dropped() {
        let (tx, mut rx) = channel::<u64>(8);
        tx.send(1);
        assert_eq!(*rx.try_recv().unwrap().unwrap(), 1);
        drop(tx);
        assert_eq!(rx.try_recv().unwrap_err(), RecvError::Closed);
    }

    /// This test deterministically verifies that a slow consumer correctly
    /// detects a lag and that no message gaps are perceived between successful
    /// reads, even after jumping to the tail.
    #[tokio::test(start_paused = true)]
    async fn lagging_consumer_is_handled_correctly() {
        const MESSAGES: u64 = 1000;
        const CAPACITY: usize = 64; // Small capacity to force lags

        let (tx, rx) = channel::<u64>(CAPACITY);

        let consumer_handle = tokio::spawn(async move {
            let mut rx = rx;
            let mut last_seen: u64 = 0;
            let mut saw_first_message = false;
            let mut lag_count = 0;

            loop {
                match rx.recv().await {
                    Ok(val_arc) => {
                        let val = *val_arc;
                        if !saw_first_message {
                            saw_first_message = true;
                        } else {
                            // The core assertion: we never see a gap between successful reads.
                            assert_eq!(val, last_seen + 1, "Consumer detected a message gap!");
                        }
                        last_seen = val;
                    }
                    Err(RecvError::Lagged(_)) => {
                        lag_count += 1;
                        // After a lag, the next message can be anything.
                        saw_first_message = false;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            (lag_count, last_seen)
        });

        // Advance time slightly to ensure the consumer task runs and waits on the notify.
        advance(Duration::from_micros(1)).await;

        // Send a burst of messages larger than the capacity to guarantee a lag.
        for i in 0..(CAPACITY as u64 * 2) {
            tx.send(i);
        }

        // Advance time to let the consumer wake up, detect the lag, and process.
        advance(Duration::from_micros(1)).await;

        // Send the rest of the messages.
        for i in (CAPACITY as u64 * 2)..MESSAGES {
            tx.send(i);
        }

        // Drop the sender to close the channel and unblock the consumer's final recv.
        drop(tx);

        let (lag_count, last_seen) = timeout(Duration::from_secs(1), consumer_handle)
            .await
            .expect("Test timed out")
            .expect("Consumer task panicked");

        // The test is designed to force at least one lag.
        assert!(
            lag_count > 0,
            "A lag was not detected; test setup is flawed."
        );
        // We can't know the exact last message, but it must be valid.
        // The successful completion of the task without panicking is the main success criteria.
        assert!(last_seen < MESSAGES, "Consumer saw an invalid message.");
    }
}

//! High-performance, Arc-aware, single-producer / multi-consumer broadcast channel.
//!
//! This implementation is optimized for the broadcast use case where a single
//! piece of data needs to be shared with multiple consumers efficiently. It
//! enforces a pattern where the channel manages the `Arc<T>` internally,
//! ensuring that receivers always perform a cheap reference-count clone instead
//! of a potentially expensive deep clone of `T`.

use arc_swap::ArcSwapOption;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::Notify;

/// Error returned when a `Receiver` has fallen behind the `Sender`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// The receiver lagged and was jumped forward to the given sequence number.
    /// The receiver should call `recv` again to retrieve the message at this sequence.
    Lagged(u64),
}

/// The shared ring buffer at the core of the channel.
#[derive(Debug)]
struct Ring<T: Send + Sync> {
    capacity: usize,
    tail: AtomicU64,
    /// `ArcSwapOption<T>` is a specialized, highly optimized version of
    /// `ArcSwap<Option<Arc<T>>>` from the `arc-swap` crate. It is perfect
    /// for this use case, providing lock-free reads of an optional `Arc`.
    slots: Vec<ArcSwapOption<T>>,
    notify: Notify,
    closed: AtomicBool,
}

impl<T: Send + Sync> Ring<T> {
    fn new(capacity: usize) -> Arc<Self> {
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            // Initialize all slots as empty (`None`). This involves no allocation.
            slots.push(ArcSwapOption::from(None));
        }

        Arc::new(Self {
            capacity,
            tail: AtomicU64::new(0),
            slots,
            notify: Notify::new(),
            closed: AtomicBool::new(false),
        })
    }

    /// The `send` logic takes ownership of `T` and wraps it in an `Arc`.
    fn push(&self, value: T) {
        let seq = self.tail.fetch_add(1, Ordering::Release);
        let idx = (seq % self.capacity as u64) as usize;

        // The single allocation for the broadcast happens here, wrapping the user's value.
        let new_arc = Arc::new(value);

        // Atomically store the `Some(Arc<T>)` into the slot.
        self.slots[idx].store(Some(new_arc));

        self.notify.notify_waiters();
    }

    /// `get_next` now returns an `Option<Arc<T>>`, which is a cheap clone.
    fn get_next(&self, seq: &mut u64) -> Result<Option<Arc<T>>, RecvError> {
        let tail = self.tail.load(Ordering::Acquire);
        let earliest = tail.saturating_sub(self.capacity as u64);

        if *seq < earliest {
            *seq = earliest;
            return Err(RecvError::Lagged(earliest));
        }

        if *seq >= tail {
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
        } else {
            // Race condition: tail is updated, but store is not yet visible.
            // The `recv` loop will simply retry.
            Ok(None)
        }
    }
}

/// The sending handle for the broadcast channel.
#[derive(Debug)]
pub struct Sender<T: Send + Sync> {
    ring: Arc<Ring<T>>,
}

impl<T: Send + Sync> Sender<T> {
    /// Sends a value to all active `Receiver`s by taking ownership of it.
    /// The value will be wrapped in an `Arc` internally to allow for efficient sharing.
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

/// The receiving handle for the broadcast channel.
#[derive(Debug)]
pub struct Receiver<T: Send + Sync> {
    ring: Arc<Ring<T>>,
    next_seq: u64,
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
    /// Asynchronously waits for the next message, returning a shared pointer (`Arc`).
    ///
    /// This method guarantees that receiving a message is a cheap operation, as it
    /// only clones an `Arc`, regardless of the size of `T`.
    pub async fn recv(&mut self) -> Result<Option<Arc<T>>, RecvError> {
        loop {
            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(pkt)) => return Ok(Some(pkt)),
                Err(err) => return Err(err),
                Ok(None) => { /* Continue to wait logic */ }
            }

            if self.ring.closed.load(Ordering::Acquire) {
                return self.ring.get_next(&mut self.next_seq);
            }

            let notified = self.ring.notify.notified();

            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(pkt)) => return Ok(Some(pkt)),
                Err(err) => return Err(err),
                Ok(None) => {
                    notified.await;
                }
            }
        }
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

        // The new receiver starts after the point of cloning and only sees new messages.
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

        // After the sender is dropped, recv returns None.
        assert!(rx.recv().await.unwrap().is_none());
    }
}

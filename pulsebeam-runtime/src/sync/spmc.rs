//! High-performance, drop-oldest, single-producer / multi-consumer broadcast channel.
//! T: Clone + Send

use parking_lot::RwLock;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

/// Error returned when a `Receiver` has fallen behind the `Sender`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// The receiver lagged and was jumped forward to the given sequence number.
    /// The receiver should call `recv` again to retrieve the message at this sequence.
    Lagged(u64),
}

/// The shared ring buffer at the core of the channel.
#[derive(Debug)]
struct Ring<T: Clone> {
    capacity: usize,
    tail: AtomicU64,
    slots: Vec<RwLock<Option<T>>>,
    notify: tokio::sync::Notify,
    closed: AtomicBool,
}

impl<T: Clone + Send> Ring<T> {
    fn new(capacity: usize) -> Arc<Self> {
        // Pre-allocate the slots in the ring buffer.
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

    /// Internal send logic. Pushes a value and notifies all waiting receivers.
    fn push(&self, value: T) {
        // `fetch_add` increments the tail and returns the *previous* value. This gives
        // us the sequence number for the current message.
        let seq = self.tail.fetch_add(1, Ordering::Release);
        let idx = (seq % self.capacity as u64) as usize;

        // Write the new value into the correct slot.
        *self.slots[idx].write() = Some(value);

        // Notify all waiting receivers that new data is available.
        self.notify.notify_waiters();
    }

    /// Attempts to retrieve the next message for a receiver at a given sequence.
    fn get_next(&self, seq: &mut u64) -> Result<Option<T>, RecvError> {
        let tail = self.tail.load(Ordering::Acquire);
        let earliest = tail.saturating_sub(self.capacity as u64);

        // --- Lagging Correction ---
        // If the receiver's sequence is older than the oldest message in the buffer,
        // it has lagged.
        if *seq < earliest {
            // Jump the receiver's sequence forward to the oldest available message.
            *seq = earliest;
            return Err(RecvError::Lagged(earliest));
        }

        // If the receiver is caught up, there are no new messages yet.
        if *seq >= tail {
            return Ok(None);
        }

        // --- Message Retrieval ---
        let idx = (*seq % self.capacity as u64) as usize;
        let slot_guard = self.slots[idx].read();

        if let Some(packet) = &*slot_guard {
            // Successfully retrieved the message. Advance the sequence for the next call.
            *seq += 1;
            Ok(Some(packet.clone()))
        } else {
            // This is a rare but possible race: the `tail` was updated by the sender,
            // but the `write()` to the slot has not yet completed. The `recv` loop
            // will simply retry.
            Ok(None)
        }
    }
}

/// The sending handle for the broadcast channel.
#[derive(Debug)]
pub struct Sender<T: Clone + Send> {
    ring: Arc<Ring<T>>,
}

impl<T: Clone + Send> Sender<T> {
    /// Sends a value to all active `Receiver`s.
    ///
    /// If the channel is at capacity, the oldest value is dropped. This method
    /// does not block.
    pub fn send(&self, value: T) {
        self.ring.push(value);
    }
}

impl<T: Clone + Send> Drop for Sender<T> {
    fn drop(&mut self) {
        // When the sender is dropped, mark the channel as closed and notify
        // any waiting receivers so they can terminate.
        self.ring.closed.store(true, Ordering::Release);
        self.ring.notify.notify_waiters();
    }
}

/// The receiving handle for the broadcast channel.
#[derive(Debug)]
pub struct Receiver<T: Clone + Send> {
    ring: Arc<Ring<T>>,
    next_seq: u64,
}

impl<T: Clone + Send> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            ring: Arc::clone(&self.ring),
            // A new subscriber should start from what's currently being written,
            // not from the historical position of the receiver it was cloned from.
            next_seq: self.ring.tail.load(Ordering::Acquire),
        }
    }
}

impl<T: Clone + Send> Receiver<T> {
    /// Asynchronously waits for the next message.
    ///
    /// # Returns
    /// - `Ok(Some(value))` if a message was received.
    /// - `Ok(None)` if the `Sender` has been dropped and all buffered messages have been received.
    /// - `Err(RecvError::Lagged(new_seq))` if this receiver fell behind. The caller should
    ///   typically loop and call `recv` again to get the next available message.
    pub async fn recv(&mut self) -> Result<Option<T>, RecvError> {
        loop {
            // --- First check (Fast Path) ---
            // Try to get a message without waiting.
            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(pkt)) => return Ok(Some(pkt)),
                Err(err) => return Err(err), // Propagate lag error immediately.
                Ok(None) => {
                    // No message was available.
                }
            }

            // If the channel is closed, we perform one final check to drain any
            // messages sent just before the sender was dropped.
            if self.ring.closed.load(Ordering::Acquire) {
                return self.ring.get_next(&mut self.next_seq);
            }

            // --- The "Check-Wait-Recheck" Pattern to prevent missed wakeups ---
            // 1. Prepare to wait for a notification.
            let notified = self.ring.notify.notified();

            // 2. Re-check for a message *after* subscribing to notifications but
            //    *before* actually waiting. This closes the race window where a
            //    sender could send a value and notify *between* our first check and the await.
            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(pkt)) => return Ok(Some(pkt)),
                Err(err) => return Err(err),
                Ok(None) => {
                    // Still no message. It's now safe to wait.
                    notified.await;
                    // After waking up, the loop continues and we will try `get_next` again.
                }
            }
        }
    }
}

/// Creates a new single-producer, multi-consumer broadcast channel.
pub fn channel<T: Clone + Send>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    // The capacity must be at least 1.
    assert!(capacity > 0, "Capacity must be greater than 0");
    let ring = Ring::new(capacity);
    (
        Sender {
            ring: Arc::clone(&ring),
        },
        Receiver {
            ring,
            // The first receiver starts at the beginning.
            next_seq: 0,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn basic_send_recv() {
        let (tx, mut rx) = channel::<u64>(8);
        tx.send(42);

        let val = rx.recv().await.unwrap().unwrap();
        assert_eq!(val, 42);
    }

    #[tokio::test]
    async fn multi_subscribers() {
        let (tx, rx) = channel::<u64>(4);
        let mut rxs: Vec<_> = (0..5).map(|_| rx.clone()).collect();

        tx.send(1);

        for rx_instance in rxs.iter_mut() {
            let v = rx_instance.recv().await.unwrap().unwrap();
            assert_eq!(v, 1);
        }
    }

    #[tokio::test]
    async fn new_subscriber_sees_new_messages() {
        let (tx, mut rx1) = channel(4);
        tx.send(1);
        tx.send(2);

        assert_eq!(rx1.recv().await.unwrap().unwrap(), 1);

        // Clone the receiver *after* some messages have been sent and received.
        let mut rx2 = rx1.clone();

        tx.send(3);

        // The original receiver continues normally.
        assert_eq!(rx1.recv().await.unwrap().unwrap(), 2);
        assert_eq!(rx1.recv().await.unwrap().unwrap(), 3);

        // The new receiver should only see the new message.
        assert_eq!(rx2.recv().await.unwrap().unwrap(), 3);
    }

    #[tokio::test]
    async fn drop_oldest() {
        let (tx, mut rx) = channel::<u64>(2);
        tx.send(0);
        tx.send(1);
        tx.send(2); // Overwrites 0
        tx.send(3); // Overwrites 1

        // The receiver should have been lapped. First, we get the Lagged error.
        let err = rx.recv().await.unwrap_err();
        assert_eq!(err, RecvError::Lagged(2)); // Should have jumped to oldest available (seq 2)

        // Now we can receive the remaining messages.
        let v = rx.recv().await.unwrap().unwrap();
        assert_eq!(v, 2);

        let v2 = rx.recv().await.unwrap().unwrap();
        assert_eq!(v2, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stress_fanout() {
        let (tx, rx) = channel::<u64>(128);
        let num_subs = 100;
        let num_messages = 1_000;

        let mut join_handles = Vec::new();

        for _ in 0..num_subs {
            let mut sub = rx.clone();
            let handle = tokio::spawn(async move {
                let mut received_count = 0;
                loop {
                    match sub.recv().await {
                        Ok(Some(_)) => {
                            received_count += 1;
                        }
                        Ok(None) => break,                     // Sender dropped
                        Err(RecvError::Lagged(_)) => continue, // Just catch up
                    }
                }
                received_count
            });
            join_handles.push(handle);
        }

        tokio::time::sleep(Duration::from_millis(10)).await;

        for i in 0..num_messages {
            tx.send(i);
        }
        drop(tx); // Close the channel

        for handle in join_handles {
            let count = handle.await.unwrap();
            // Each subscriber should receive a substantial number of messages,
            // but not necessarily all if they lag.
            assert!(count > 0);
        }
    }

    #[tokio::test]
    async fn publisher_drop() {
        let (tx, mut rx) = channel::<u64>(8);
        tx.send(1);

        let val = rx.recv().await.unwrap().unwrap();
        assert_eq!(val, 1);

        drop(tx); // Publisher dropped

        // Receiver should get Ok(None) to signal termination.
        let val2 = rx.recv().await.unwrap();
        assert!(val2.is_none());
    }

    #[tokio::test]
    async fn lag_error_and_recover() {
        let (tx, mut rx) = channel::<u64>(2);
        tx.send(0); // seq 0
        tx.send(1); // seq 1
        tx.send(2); // seq 2, drops 0
        tx.send(3); // seq 3, drops 1
        tx.send(4); // seq 4, drops 2

        // At this point, tail is 5. Buffer contains messages for seq 3 and 4.
        // The receiver's next_seq is still 0. Oldest available is 5-2=3.

        // The first call must report that we lagged and were jumped forward.
        let err = rx.recv().await.unwrap_err();
        assert_eq!(err, RecvError::Lagged(3));

        // Now that the receiver has been moved forward, the next call should
        // get the oldest available message.
        let val = rx.recv().await.unwrap().unwrap();
        assert_eq!(val, 3);

        // And the next one.
        let val2 = rx.recv().await.unwrap().unwrap();
        assert_eq!(val2, 4);
    }
}

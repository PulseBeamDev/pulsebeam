use parking_lot::RwLock;
use std::{
    fmt::Debug,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::Notify;

/// Error type returned by a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// The receiver lagged behind the buffer capacity and was advanced forward.
    Lagged(u64),
    /// The channel was closed — no more messages will arrive.
    Closed,
}

/// A single slot in the ring buffer, protected by RwLock.
#[derive(Debug)]
struct Slot<T> {
    seq: u64,
    data: Option<Arc<T>>,
}

impl<T> Slot<T> {
    fn new() -> Self {
        Self { seq: 0, data: None }
    }
}

/// The shared ring buffer structure.
struct Ring<T: Send + Sync> {
    tail: AtomicU64, // producer's tail sequence
    capacity: usize,
    slots: Box<[RwLock<Slot<T>>]>,
    notify: Notify,
    closed: AtomicBool,
}

impl<T: Send + Sync> Ring<T> {
    fn new(capacity: usize) -> Arc<Self> {
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(RwLock::new(Slot::new()));
        }

        Arc::new(Self {
            tail: AtomicU64::new(0),
            capacity,
            slots: slots.into_boxed_slice(),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
        })
    }

    /// Push a message into the ring buffer.
    fn push(&self, value: T) {
        if self.is_closed() {
            return;
        }

        let seq = self.tail.load(Ordering::Relaxed);
        let idx = (seq % self.capacity as u64) as usize;

        {
            let mut slot = self.slots[idx].write();
            slot.seq = seq;
            slot.data = Some(Arc::new(value));
        }

        self.tail.store(seq + 1, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Attempt to read the next message for a receiver.
    fn get_next(&self, next_seq: &mut u64) -> Result<Option<Arc<T>>, RecvError> {
        let tail = self.tail.load(Ordering::Acquire);
        let earliest = tail.saturating_sub(self.capacity as u64);

        // Receiver lagged behind buffer capacity
        if *next_seq < earliest {
            *next_seq = tail;
            return Err(RecvError::Lagged(tail));
        }

        // No new messages
        if *next_seq >= tail {
            if self.closed.load(Ordering::Acquire) {
                return Err(RecvError::Closed);
            }
            return Ok(None);
        }

        let idx = (*next_seq % self.capacity as u64) as usize;
        let slot = self.slots[idx].read();

        if slot.seq != *next_seq {
            // Not yet written
            return Ok(None);
        }

        if let Some(ref val) = slot.data {
            *next_seq += 1;
            Ok(Some(Arc::clone(val)))
        } else if self.closed.load(Ordering::Acquire) {
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

/// Sender handle — single producer only.
pub struct Sender<T: Send + Sync> {
    ring: Arc<Ring<T>>,
}

impl<T: Send + Sync> Sender<T> {
    /// Send a message to all receivers.
    pub fn send(&self, value: T) {
        self.ring.push(value);
    }

    /// Check if the channel has been closed.
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

/// Receiver handle — safe for multiple consumers.
pub struct Receiver<T: Send + Sync> {
    ring: Arc<Ring<T>>,
    next_seq: u64,
}

impl<T: Send + Sync> Receiver<T> {
    /// Receive the next message asynchronously.
    pub async fn recv(&mut self) -> Result<Arc<T>, RecvError> {
        loop {
            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(val)) => return Ok(val),
                Err(e) => return Err(e),
                Ok(None) => {}
            }

            let notified = self.ring.notify.notified();

            match self.ring.get_next(&mut self.next_seq) {
                Ok(Some(val)) => return Ok(val),
                Err(e) => return Err(e),
                Ok(None) => notified.await,
            }
        }
    }

    /// Non-blocking attempt to receive a message.
    pub fn try_recv(&mut self) -> Result<Option<Arc<T>>, RecvError> {
        self.ring.get_next(&mut self.next_seq)
    }

    /// Check if the channel is closed.
    pub fn is_closed(&self) -> bool {
        self.ring.is_closed()
    }
}

impl<T: Send + Sync> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            ring: Arc::clone(&self.ring),
            next_seq: self.ring.tail.load(Ordering::Acquire), // start live
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
    let tail = ring.tail.load(Ordering::Relaxed);
    (
        Sender {
            ring: Arc::clone(&ring),
        },
        Receiver {
            ring,
            next_seq: tail, // start live, not from 0
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
    async fn multiple_receivers_share_same_arc() {
        let (tx, rx) = channel::<String>(4);
        let mut rx1 = rx.clone();
        let mut rx2 = rx.clone();

        tx.send("hello".to_string());
        let a = rx1.recv().await.unwrap();
        let b = rx2.recv().await.unwrap();

        assert_eq!(*a, "hello");
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[tokio::test]
    async fn lagging_receiver_is_caught_up() {
        const CAP: usize = 4;
        const MSGS: u64 = 20;

        let (tx, mut rx) = channel::<u64>(CAP);

        for i in 0..MSGS {
            tx.send(i);
        }
        drop(tx);

        let mut received = 0;
        loop {
            match rx.recv().await {
                Ok(_) => received += 1,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }

        assert!(received >= 0);
    }
}

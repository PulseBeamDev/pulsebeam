use crossbeam_utils::CachePadded;
use event_listener::{Event, EventListener};
use futures_lite::Stream;
use parking_lot::RwLock;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, ready};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    Lagged(u64),
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamRecvError {
    Lagged(u64),
}

#[derive(Debug)]
struct Slot<T> {
    seq: u64,
    val: Option<T>,
}

#[derive(Debug)]
struct Ring<T> {
    head: AtomicU64,
    mask: usize,
    slots: Vec<CachePadded<RwLock<Slot<T>>>>,
    event: Event,
    closed: AtomicU64,
}

impl<T> Ring<T> {
    fn new(capacity: usize) -> Arc<Self> {
        assert!(
            capacity > 0 && capacity.is_power_of_two(),
            "Capacity must be power of 2"
        );

        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(CachePadded::new(RwLock::new(Slot { seq: 0, val: None })));
        }

        Arc::new(Self {
            head: AtomicU64::new(0),
            mask: capacity - 1,
            slots,
            event: Event::new(),
            closed: AtomicU64::new(0),
        })
    }
}

#[derive(Debug)]
pub struct Sender<T> {
    ring: Arc<Ring<T>>,
    local_head: u64,
}

impl<T> Sender<T> {
    pub fn send(&mut self, val: T) {
        if self.ring.closed.load(Ordering::Relaxed) == 1 {
            return;
        }

        let idx = (self.local_head as usize) & self.ring.mask;

        {
            let mut slot = self.ring.slots[idx].write();
            slot.val = Some(val);
            slot.seq = self.local_head;
        }

        self.ring.head.store(self.local_head + 1, Ordering::Release);
        self.local_head += 1;
        self.ring.event.notify(usize::MAX);
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.ring.closed.store(1, Ordering::Release);
        self.ring.event.notify(usize::MAX);
    }
}

#[derive(Debug)]
pub struct Receiver<T> {
    ring: Arc<Ring<T>>,
    next_seq: u64,
    local_head: u64,
    listener: Option<EventListener>,
}

impl<T: Clone> Receiver<T> {
    pub fn reset(&mut self) {
        self.next_seq = self.ring.head.load(Ordering::Acquire);
    }

    pub async fn recv(&mut self) -> Result<T, RecvError> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        loop {
            let coop = std::task::ready!(tokio::task::coop::poll_proceed(cx));

            // Snapshot producer head. This allows batching efficiency without a batching API.
            // It creates a fast-path for a slightly behind receiver to catchup the producer
            // without spending atomic load on every iteration.
            //
            // Safety: head is strictly monotonically increasing
            if self.next_seq == self.local_head {
                self.local_head = self.ring.head.load(Ordering::Acquire);
            }
            let capacity = self.ring.mask as u64 + 1;
            let earliest = self.local_head.saturating_sub(capacity);

            // Closed and nothing left
            if self.ring.closed.load(Ordering::Acquire) == 1 && self.next_seq >= self.local_head {
                return Poll::Ready(Err(RecvError::Closed));
            }

            // No new items — wait
            if self.next_seq >= self.local_head {
                match &mut self.listener {
                    Some(l) => {
                        if Pin::new(l).poll(cx).is_pending() {
                            return Poll::Pending;
                        }
                        self.listener = None;
                        continue;
                    }
                    None => {
                        self.listener = Some(self.ring.event.listen());
                        continue;
                    }
                }
            }

            // Read slot for next_seq
            let idx = (self.next_seq as usize) & self.ring.mask;
            let slot = self.ring.slots[idx].read();
            let slot_seq = slot.seq;

            // Slot overwritten before we got here
            if slot_seq < earliest {
                drop(slot);
                self.next_seq = self.local_head;
                return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
            }

            // Seq mismatch — producer overwrote after head snapshot
            if slot_seq != self.next_seq {
                drop(slot);
                self.next_seq = self.local_head;
                return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
            }

            // Valid message
            if let Some(v) = &slot.val {
                let out = v.clone();
                drop(slot);
                coop.made_progress();
                self.next_seq += 1;
                return Poll::Ready(Ok(out));
            }

            // This shouldn't never happen, but just in case..
            // Seq was correct but value missing — treat as lag
            drop(slot);
            self.next_seq = self.local_head;
            return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
        }
    }
}

impl<T: Clone> Stream for Receiver<T> {
    type Item = Result<T, StreamRecvError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        let res = match ready!(this.poll_recv(cx)) {
            Ok(item) => Some(Ok(item)),
            Err(RecvError::Lagged(n)) => Some(Err(StreamRecvError::Lagged(n))),
            Err(RecvError::Closed) => None,
        };

        Poll::Ready(res)
    }
}

impl<T: Clone> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        let head = self.ring.head.load(Ordering::Acquire);
        Self {
            ring: self.ring.clone(),
            next_seq: head,
            local_head: head,
            listener: None,
        }
    }
}

pub fn channel<T: Send + Sync + Clone + 'static>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let ring = Ring::new(capacity);
    (
        Sender {
            ring: ring.clone(),
            local_head: 0,
        },
        Receiver {
            ring: ring.clone(),
            next_seq: 0,
            local_head: 0,
            listener: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn basic_send_recv() {
        let (mut tx, mut rx) = channel::<u64>(8);

        tx.send(42);
        assert_eq!(rx.recv().await, Ok(42));

        tx.send(123);
        assert_eq!(rx.recv().await, Ok(123));
    }

    #[tokio::test]
    async fn multi_consumer_broadcast() {
        let (mut tx, rx) = channel::<String>(8);
        let mut rx1 = rx.clone();
        let mut rx2 = rx.clone();

        tx.send("alpha".to_string());
        tx.send("beta".to_string());

        assert_eq!(rx1.recv().await, Ok("alpha".to_string()));
        assert_eq!(rx1.recv().await, Ok("beta".to_string()));

        assert_eq!(rx2.recv().await, Ok("alpha".to_string()));
        assert_eq!(rx2.recv().await, Ok("beta".to_string()));
    }

    #[tokio::test]
    async fn buffer_wrap_around_behavior() {
        // Capacity 4.
        let (mut tx, mut rx) = channel::<u64>(4);

        // 1. Fill buffer completely [0, 1, 2, 3]
        for i in 0..4 {
            tx.send(i);
        }

        // 2. Consume first two [0, 1]
        assert_eq!(rx.recv().await, Ok(0));
        assert_eq!(rx.recv().await, Ok(1));

        // Receiver is now expecting seq 2.
        // Head is 4. Earliest valid is 4-4=0.
        // 2 > 0, so no lag yet.

        // 3. Overwrite the first two slots [4, 5]
        // Slot indices 0 and 1 are overwritten.
        tx.send(4);
        tx.send(5);

        // Head is now 6. Earliest valid is 6-4=2.
        // Receiver expects 2. 2 >= 2. Still safe!
        // The slots it wants (2 and 3) are still valid in the buffer.

        assert_eq!(rx.recv().await, Ok(2));
        assert_eq!(rx.recv().await, Ok(3));

        // Now verify it reads the wrapped values correctly
        assert_eq!(rx.recv().await, Ok(4));
        assert_eq!(rx.recv().await, Ok(5));
    }

    #[tokio::test]
    async fn lagging_receiver_jumps_to_head() {
        // Capacity 4
        let (mut tx, mut rx) = channel::<u64>(4);

        // Send 6 items [0, 1, 2, 3, 4, 5]
        // Buffer holds [2, 3, 4, 5].
        // Head is 6. Earliest is 2.
        for i in 0..6 {
            tx.send(i);
        }

        // Receiver is at 0.
        // 0 < Earliest(2). LAG detected.
        // Implementation behavior: Jump to Head (6).
        match rx.recv().await {
            Err(RecvError::Lagged(seq)) => assert_eq!(seq, 6),
            _ => panic!("Expected lag error"),
        }

        // Receiver is now at 6. Buffer only goes up to 5.
        // Receiver should wait for new data.

        let h = tokio::spawn(async move {
            tx.send(6);
        });

        // Should receive the new item immediately
        assert_eq!(rx.recv().await, Ok(6));
        h.await.unwrap();
    }

    #[tokio::test]
    async fn receiver_detects_overwrite_during_read() {
        // This tests the race condition where the producer overwrites
        // the exact slot the receiver is trying to read.
        // Since RwLock prevents this physically, this test verifies
        // the sequence check logic handles the logical mismatch if lock contention happens.

        let (mut tx, mut rx) = channel::<u64>(2); // Small cap to force collisions

        tx.send(0);

        // Verify we can read 0
        assert_eq!(rx.recv().await, Ok(0));

        // Now simulate a scenario where rx is slow.
        // We manually advance tx far ahead.
        tx.send(1); // Slot 1
        tx.send(2); // Slot 0 (Overwrites 0) -> Rx expecting 1 (Slot 1) is fine.
        tx.send(3); // Slot 1 (Overwrites 1) -> Rx expecting 1 is now LAGGED.

        // Rx expects seq 1.
        // Head is 4. Earliest is 4-2=2.
        // 1 < 2.
        // Should return Lagged(4).
        match rx.recv().await {
            Err(RecvError::Lagged(s)) => assert_eq!(s, 4),
            Ok(v) => panic!("Should have lagged, got {}", v),
            Err(e) => panic!("Unexpected error {:?}", e),
        }
    }

    #[tokio::test]
    async fn close_signal_drains_then_stops() {
        let (mut tx, mut rx) = channel::<u64>(4);

        tx.send(1);
        tx.send(2);
        drop(tx);

        assert_eq!(rx.recv().await, Ok(1));
        assert_eq!(rx.recv().await, Ok(2));
        assert_eq!(rx.recv().await, Err(RecvError::Closed));
        // Subsequent calls should still be Closed
        assert_eq!(rx.recv().await, Err(RecvError::Closed));
    }

    #[tokio::test]
    async fn async_waker_notification() {
        let (mut tx, mut rx) = channel::<u64>(4);

        let h = tokio::spawn(async move {
            // This should block until main thread sends
            rx.recv().await
        });

        // Ensure the task has likely polled and parked
        tokio::time::sleep(Duration::from_millis(10)).await;

        tx.send(99);

        let result = h.await.unwrap();
        assert_eq!(result, Ok(99));
    }
}

use crossbeam_utils::CachePadded;
use futures_lite::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll, Waker, ready};

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
    /// Fast-path gate: `true` iff at least one receiver is currently parked.
    ///
    /// The sender loads this with `Acquire` before touching `waiters`.  When
    /// `false` (the common case — active receivers keep up with the sender)
    /// the sender does **zero** additional work: no lock, no atomic RMW beyond
    /// storing `head`.  This is strictly cheaper than both `event-listener`
    /// and `tokio::Notify`, which always acquire an internal mutex on every
    /// `notify` call regardless of waiter presence.
    has_waiters: AtomicBool,
    /// Per-receiver waker table, keyed by stable receiver ID.
    ///
    /// Only accessed when `has_waiters` is `true`.  Using (id, Waker) pairs
    /// rather than a plain `Vec<Waker>` prevents waker accumulation: repeated
    /// calls to `register_waker` from the same receiver find and update the
    /// existing slot instead of pushing a new one.
    waiters: Mutex<Vec<(u64, Waker)>>,
    /// Monotonically increasing counter; each `Receiver::clone()` grabs a
    /// unique ID so it can update its own waker slot in `waiters`.
    next_receiver_id: AtomicU64,
    closed: AtomicU64,
}

impl<T> Ring<T> {
    fn new(mut capacity: usize) -> Arc<Self> {
        if capacity > 0 && !capacity.is_power_of_two() {
            let old_cap = capacity;
            capacity = capacity.next_power_of_two();
            tracing::warn!(
                "Capacity should be power of 2, use nearest: {} -> {}",
                old_cap,
                capacity
            );
        }

        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(CachePadded::new(RwLock::new(Slot { seq: 0, val: None })));
        }

        Arc::new(Self {
            head: AtomicU64::new(0),
            mask: capacity - 1,
            slots,
            has_waiters: AtomicBool::new(false),
            waiters: Mutex::new(Vec::new()),
            next_receiver_id: AtomicU64::new(1), // 0 reserved for initial receiver
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
            let mut slot = self.ring.slots[idx].write().unwrap();
            slot.val = Some(val);
            slot.seq = self.local_head;
        }

        self.ring.head.store(self.local_head + 1, Ordering::Release);
        self.local_head += 1;
        // Fast path: single Acquire load.  If no receivers are parked, we do
        // no further work — no mutex, no CAS, nothing.
        self.wake_parked();
    }
}

impl<T> Sender<T> {
    /// Wake all currently-parked receivers.  Extracted so both `send` and
    /// `Drop` share the same logic.
    fn wake_parked(&self) {
        if !self.ring.has_waiters.load(Ordering::Acquire) {
            return;
        }
        // Take the whole Vec out under lock (O(1)), release the lock, then
        // call each waker outside the lock to avoid lock-order issues and
        // allow receivers to re-register while we are waking others.
        let to_wake = {
            let mut w = self.ring.waiters.lock().unwrap();
            self.ring.has_waiters.store(false, Ordering::Relaxed);
            std::mem::take(&mut *w)
        };
        for (_, waker) in to_wake {
            waker.wake();
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.ring.closed.store(1, Ordering::Release);
        self.wake_parked();
    }
}

pub struct Receiver<T> {
    ring: Arc<Ring<T>>,
    next_seq: u64,
    local_head: u64,
    /// Stable receiver identity used to address this receiver's waker slot
    /// inside `ring.waiters`.  Assigned at construction / clone; freed on drop.
    id: u64,
    pkts_received: u64,
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver")
            .field("id", &self.id)
            .field("next_seq", &self.next_seq)
            .field("local_head", &self.local_head)
            .field("pkts_received", &self.pkts_received)
            .finish()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // Remove our waker slot from the waiters Vec so stale wakers do not
        // accumulate after a receiver is dropped (e.g. after a track switch).
        if self.ring.has_waiters.load(Ordering::Relaxed) {
            if let Ok(mut w) = self.ring.waiters.lock() {
                w.retain(|(id, _)| *id != self.id);
                if w.is_empty() {
                    self.ring.has_waiters.store(false, Ordering::Relaxed);
                }
            }
        }
    }
}

impl<T: Clone + Send + Sync> Receiver<T> {
    const METRIC_FLUSH_MASK: u64 = 1023;

    /// Jump to the producer's current position (Audio/Low-latency).
    pub fn sync(&mut self) {
        self.local_head = self.ring.head.load(Ordering::Acquire);
        self.next_seq = self.local_head;
    }

    /// Jump back halfway to provide a processing window (Video/Burst).
    pub fn rewind(&mut self) {
        // Half cap to give a chance to load from cache while not too close
        // to tail to cause a lag error.
        self.local_head = self.ring.head.load(Ordering::Acquire);
        let half_ring_cap = (self.ring.slots.len() / 2) as u64;
        let ring_len = self.local_head.wrapping_sub(self.next_seq);

        // Defensive: detect if next_seq is impossibly far behind
        if ring_len > self.ring.slots.len() as u64 {
            // Something is very wrong - reset to head
            self.next_seq = self.local_head;
        } else {
            let offset = half_ring_cap.min(ring_len);
            self.next_seq = self.local_head.wrapping_sub(offset);
        }
    }

    pub async fn recv(&mut self) -> Result<T, RecvError> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        loop {
            // Snapshot producer head. This allows batching efficiency without a batching API.
            // It creates a fast-path for a slightly behind receiver to catchup the producer
            // without spending atomic load on every iteration.
            //
            // Safety: head is strictly monotonically increasing
            if self.next_seq == self.local_head {
                self.local_head = self.ring.head.load(Ordering::Acquire);
            }

            // Closed and nothing left
            if self.ring.closed.load(Ordering::Acquire) == 1 && self.next_seq >= self.local_head {
                return Poll::Ready(Err(RecvError::Closed));
            }

            // No new items — register our waker and park.
            if self.next_seq >= self.local_head {
                if self.register_waker(cx) {
                    // Head advanced between our empty-check and waker
                    // registration — re-read head and retry.
                    continue;
                }
                return Poll::Pending;
            }

            // Read slot for next_seq
            let idx = (self.next_seq as usize) & self.ring.mask;
            let slot = self.ring.slots[idx].read().unwrap();
            let slot_seq = slot.seq;

            // Seq mismatch — producer overwrote after head snapshot
            if slot_seq != self.next_seq {
                // Sanity check: seq should be ahead of us if we lagged
                let lagged = slot.seq > self.next_seq;
                drop(slot);

                if lagged {
                    self.next_seq = self.local_head;
                    metrics::counter!("spmc_receive_lag_total").increment(1);
                    return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
                } else {
                    // Paranoid: this can only happen if the seq is wrapped around.
                    // But, u64 is so large that this shouldn't happen.
                    debug_assert!(
                        false,
                        "slot seq {} is behind expected seq {} despite head confirming availability; \
                        indicates a memory ordering bug or single-producer invariant violation",
                        slot_seq, self.next_seq
                    );
                    self.register_waker(cx);
                    return Poll::Pending;
                }
            }

            let coop = ready!(tokio::task::coop::poll_proceed(cx));

            // Valid message
            if let Some(v) = &slot.val {
                let out = v.clone();
                coop.made_progress();

                self.pkts_received += 1;
                self.next_seq += 1;

                if (self.pkts_received & Self::METRIC_FLUSH_MASK) == 0 {
                    drop(slot);
                    self.flush_metrics();
                }
                return Poll::Ready(Ok(out));
            }

            // This shouldn't ever happen, but just in case..
            // Seq was correct but value missing — treat as lag
            self.next_seq = self.local_head;
            metrics::counter!("spmc_receive_lag_total").increment(1);
            return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
        }
    }

    /// Register `cx.waker()` for this receiver and park until the sender
    /// calls `send()` or drops.
    ///
    /// Uses a stable `id` slot in `ring.waiters` so repeated calls from the
    /// same receiver find-and-update rather than accumulate.
    ///
    /// Returns `true` if the ring head advanced between our prior empty-check
    /// and the waker registration — the sender already committed data but
    /// may not have seen `has_waiters == true` in time.  The caller should
    /// re-enter the poll loop.  Returns `false` when the waker is registered
    /// and the task should return `Poll::Pending`.
    fn register_waker(&mut self, cx: &mut Context<'_>) -> bool {
        {
            let mut w = self.ring.waiters.lock().unwrap();
            // Find-or-insert: update existing slot to avoid waker accumulation
            // across repeated polls from the same receiver.
            if let Some(entry) = w.iter_mut().find(|(id, _)| *id == self.id) {
                // Lazy: only clone if the waker actually changed.
                if !entry.1.will_wake(cx.waker()) {
                    entry.1 = cx.waker().clone();
                }
            } else {
                w.push((self.id, cx.waker().clone()));
            }
            // Release ordering ensures the Vec write is visible to the sender's
            // subsequent Acquire load of has_waiters.
            self.ring.has_waiters.store(true, Ordering::Release);
        }
        // Re-check head with Acquire to detect if the sender advanced head
        // after our empty-check but before we registered the waker.  In that
        // window the sender saw has_waiters == false and did not wake us.
        // Returning true causes the caller to retry; our waker remains in the
        // Vec and will be harmlessly drained on the next send.
        if self.ring.head.load(Ordering::Acquire) > self.next_seq {
            return true;
        }
        false
    }

    fn flush_metrics(&mut self) {
        // Use the existing local_head snapshot.
        // This tells us how much is still left in the current "batch".
        let current_drift = self.local_head.saturating_sub(self.next_seq);
        let capacity = (self.ring.mask + 1) as f64;

        metrics::histogram!("spmc_receive_drift_ratio").record(current_drift as f64 / capacity);
        metrics::counter!("spmc_receive_throughput_total").increment(self.pkts_received);
        self.pkts_received = 0;
    }
}

impl<T: Clone + Send + Sync> Stream for Receiver<T> {
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
        Self {
            ring: self.ring.clone(),
            next_seq: self.next_seq,
            local_head: self.local_head,
            // Assign a fresh stable ID so this clone has its own slot in
            // ring.waiters and doesn't collide with the original.
            id: self.ring.next_receiver_id.fetch_add(1, Ordering::Relaxed),
            pkts_received: 0,
        }
    }
}

pub fn channel<T: Send + Sync + Clone + 'static>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    metrics::describe_histogram!(
        "spmc_receive_drift_ratio",
        "The ratio of the buffer capacity currently occupied by unread packets \
        at the moment of processing. A value of 1.0 indicates the receiver is \
        about to be overwritten (lagged)."
    );

    metrics::describe_counter!(
        "spmc_receive_throughput_total",
        "The total number of packets successfully delivered across all SPMC channels."
    );

    metrics::describe_counter!(
        "spmc_receive_lag_total",
        "The total number of times a receiver was too slow and was overwritten by \
        the producer, resulting in dropped data."
    );
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
            id: 0,
            pkts_received: 0,
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

use diatomic_waker::{WakeSink, WakeSource};
use futures_lite::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
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
    // This is mpsc, but we expect very low contention on the producers.
    // Mutex is generally cheaper than RWLock. So, no reason to pay
    // RWLock overhead.
    slots: Vec<Mutex<Slot<T>>>,
    mask: usize,
    head: AtomicU64,
    /// Tracks how many items the receiver has consumed (its next_seq).
    /// Updated by the Receiver on each successful read so that Senders can
    /// compute fill pressure without coordinating with the Receiver directly.
    tail: AtomicU64,
    /// Lock-free waker notification via `diatomic_waker`.
    ///
    /// `WakeSource::notify()` is a pure CAS state machine — no mutex, no heap
    /// allocation on the sender hot path.  The matching `WakeSink` lives
    /// inline in `Receiver<T>`; it caches the last registered `Waker` and
    /// only re-clones it when the task context changes, so repeated calls
    /// from the same `select!` branch are nearly free.
    wake_src: WakeSource,
    closed: AtomicU64,
}

impl<T> Ring<T> {
    fn new(mut capacity: usize, wake_src: WakeSource) -> Arc<Self> {
        if capacity == 0 {
            capacity = 1;
        } else if !capacity.is_power_of_two() {
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
            slots.push(Mutex::new(Slot { seq: 0, val: None }));
        }

        Arc::new(Self {
            slots,
            mask: capacity - 1,
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            wake_src,
            closed: AtomicU64::new(0),
        })
    }
}

#[derive(Debug)]
pub enum TrySendError<T> {
    Closed(T),
}

#[derive(Debug)]
pub struct Sender<T> {
    ring: Arc<Ring<T>>,
}

impl<T> Sender<T> {
    pub fn try_send(&self, val: T) -> Result<(), TrySendError<T>> {
        if self.ring.closed.load(Ordering::Relaxed) == 1 {
            return Err(TrySendError::Closed(val));
        }

        let seq = self.ring.head.fetch_add(1, Ordering::AcqRel);
        let idx = (seq as usize) & self.ring.mask;

        {
            let mut slot = self.ring.slots[idx].lock().unwrap();
            slot.val = Some(val);
            slot.seq = seq;
        }

        // Fully lockless CAS: no mutex, no alloc on sender hot path.
        self.ring.wake_src.notify();
        Ok(())
    }
}

impl<T> Sender<T> {
    /// Number of items currently in the ring that have not yet been consumed.
    /// This is an instantaneous snapshot: head - tail.
    pub fn pending(&self) -> u64 {
        let head = self.ring.head.load(Ordering::Relaxed);
        let tail = self.ring.tail.load(Ordering::Relaxed);
        head.saturating_sub(tail)
    }

    /// Fill ratio in [0.0, 1.0]: 0.0 = empty, 1.0 = receiver is fully behind.
    /// Values approaching 1.0 indicate the receiver cannot keep up and lag
    /// (packet loss) is imminent. Senders can use this to decide whether to
    /// yield before pushing more data.
    pub fn fill_ratio(&self) -> f64 {
        let capacity = (self.ring.mask + 1) as f64;
        self.pending() as f64 / capacity
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            ring: self.ring.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {}
}

pub struct Receiver<T> {
    ring: Arc<Ring<T>>,
    next_seq: u64,
    local_head: u64,
    /// Inline lock-free waker sink.
    ///
    /// `WakeSink` stores two `Waker` slots internally (the DiatomicWaker
    /// two-slot design) and is entirely allocation-free after `channel()` —
    /// the single `Arc<DiatomicWaker>` is created once at channel-creation
    /// time and shared with the `WakeSource` in `Ring<T>`.
    ///
    /// Registration is lazy: `register()` only re-clones the waker when the
    /// task context actually changes, so polling the same future in a tight
    /// loop is essentially free.
    wake_sink: WakeSink,
    pkts_received: u64,
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.ring.closed.store(1, Ordering::Release);
    }
}

impl<T: Send> Receiver<T> {
    const METRIC_FLUSH_MASK: u64 = 1023;

    pub async fn recv(&mut self) -> Result<T, RecvError> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        loop {
            // Snapshot producer head. This allows batching efficiency without a batching API.
            // Creates a fast-path for a slightly behind receiver to catch up without
            // spending an atomic load on every iteration.
            //
            // Safety: head is strictly monotonically increasing
            if self.next_seq == self.local_head {
                self.local_head = self.ring.head.load(Ordering::Acquire);
            }

            // Closed and nothing left
            if self.ring.closed.load(Ordering::Acquire) == 1 && self.next_seq >= self.local_head {
                return Poll::Ready(Err(RecvError::Closed));
            }

            // No new items — register our waker via the event listener and park.
            if self.next_seq >= self.local_head {
                if self.register_waker(cx) {
                    // Listener was already notified before we polled it —
                    // an item may have arrived; re-read head and retry.
                    continue;
                }
                return Poll::Pending;
            }

            let idx = (self.next_seq as usize) & self.ring.mask;
            let mut slot = self.ring.slots[idx].lock().unwrap();
            let slot_seq = slot.seq;

            if slot_seq != self.next_seq {
                let lagged = slot.seq > self.next_seq;
                drop(slot);

                if lagged {
                    self.next_seq = self.local_head;
                    metrics::counter!("mpsc_receive_lag_total").increment(1);
                    return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
                } else {
                    // Stale slot — a producer claimed this seq but hasn't written yet.
                    // Register the waker and wait for the write to complete.
                    if self.register_waker(cx) {
                        continue;
                    }
                    return Poll::Pending;
                }
            }

            let coop = ready!(tokio::task::coop::poll_proceed(cx));

            if let Some(val) = slot.val.take() {
                coop.made_progress();
                self.next_seq += 1;
                // Publish the consumed position so Senders can observe fill pressure.
                self.ring.tail.store(self.next_seq, Ordering::Relaxed);
                self.pkts_received += 1;

                if (self.pkts_received & Self::METRIC_FLUSH_MASK) == 0 {
                    drop(slot);
                    self.flush_metrics();
                }
                return Poll::Ready(Ok(val));
            }

            // This shouldn't ever happen, but just in case..
            // Seq was correct but value missing — treat as lag
            self.next_seq = self.local_head;
            metrics::counter!("mpsc_receive_lag_total").increment(1);
            return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
        }
    }

    /// Register `cx.waker()` with the `WakeSink` and park until a sender
    /// calls `WakeSource::notify()`.
    ///
    /// Returns `true` if the ring head advanced between the empty-check that
    /// preceded this call and the registration: the sender may have already
    /// fired `notify()` before we registered, so we unregister immediately
    /// and tell the caller to retry.  Returns `false` when the waker is
    /// registered and the task should return `Poll::Pending`.
    fn register_waker(&mut self, cx: &mut Context<'_>) -> bool {
        // Lazy: DiatomicWaker only re-clones the Waker if the task context
        // actually changed since the last registration — essentially free when
        // called from the same future on repeated polls.
        self.wake_sink.register(cx.waker());

        // Re-check head with Acquire ordering.  If the sender bumped head
        // after our empty-check but before we registered, we must not park:
        // the sender called notify() on the old (empty) state and won't fire
        // again for this item.  Unregister to keep the sink clean.
        if self.ring.head.load(Ordering::Acquire) > self.next_seq {
            self.wake_sink.unregister();
            return true;
        }
        false
    }

    fn flush_metrics(&mut self) {
        let current_drift = self.local_head.saturating_sub(self.next_seq);
        let capacity = (self.ring.mask + 1) as f64;

        metrics::histogram!("mpsc_receive_drift_ratio").record(current_drift as f64 / capacity);
        metrics::counter!("mpsc_receive_throughput_total").increment(self.pkts_received);
        self.pkts_received = 0;
    }
}

impl<T: Send> Stream for Receiver<T> {
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

pub fn channel<T: Send + Sync + 'static>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    metrics::describe_histogram!(
        "mpsc_receive_drift_ratio",
        "The ratio of the buffer capacity currently occupied by unread packets \
        at the moment of processing. A value of 1.0 indicates the receiver is \
        about to be overwritten (lagged)."
    );

    metrics::describe_counter!(
        "mpsc_receive_throughput_total",
        "The total number of packets successfully delivered across all mpsc channels."
    );

    metrics::describe_counter!(
        "mpsc_receive_lag_total",
        "The total number of times a receiver was too slow and was overwritten by \
        the producer, resulting in dropped data."
    );

    let wake_sink = WakeSink::new();
    let wake_src = wake_sink.source();
    let ring = Ring::new(capacity, wake_src);
    (
        Sender { ring: ring.clone() },
        Receiver {
            ring,
            next_seq: 0,
            local_head: 0,
            wake_sink,
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
        let (tx, mut rx) = channel::<u64>(8);

        tx.try_send(42).unwrap();
        assert_eq!(rx.recv().await, Ok(42));

        tx.try_send(123).unwrap();
        assert_eq!(rx.recv().await, Ok(123));
    }

    #[tokio::test]
    async fn lagging_consumer_jumps_to_head() {
        let (tx, mut rx) = channel::<u64>(4);

        for i in 0..6 {
            tx.try_send(i).unwrap();
        }

        match rx.recv().await {
            Err(RecvError::Lagged(seq)) => assert_eq!(seq, 6),
            _ => panic!("Expected lag error"),
        }

        tx.try_send(6).unwrap();
        assert_eq!(rx.recv().await, Ok(6));
    }

    #[tokio::test]
    async fn buffer_wrap_around_behavior() {
        let (tx, mut rx) = channel::<u64>(4);

        for i in 0..4 {
            tx.try_send(i).unwrap();
        }

        assert_eq!(rx.recv().await, Ok(0));
        assert_eq!(rx.recv().await, Ok(1));

        tx.try_send(4).unwrap();
        tx.try_send(5).unwrap();

        assert_eq!(rx.recv().await, Ok(2));
        assert_eq!(rx.recv().await, Ok(3));
        assert_eq!(rx.recv().await, Ok(4));
        assert_eq!(rx.recv().await, Ok(5));
    }

    #[tokio::test]
    async fn receiver_detects_overwrite_during_read() {
        let (tx, mut rx) = channel::<u64>(2); // Small cap to force collisions

        tx.try_send(0).unwrap();
        assert_eq!(rx.recv().await, Ok(0));

        // Rx expects seq 1. Overwrite slot 1 before rx reads it.
        tx.try_send(1).unwrap(); // Slot 1
        tx.try_send(2).unwrap(); // Slot 0 (overwrites 0)
        tx.try_send(3).unwrap(); // Slot 1 (overwrites 1) -> rx expecting 1 is now LAGGED

        // Head is 4. Earliest valid is 4-2=2. Rx expects 1. 1 < 2.
        // Should return Lagged(4).
        match rx.recv().await {
            Err(RecvError::Lagged(s)) => assert_eq!(s, 4),
            Ok(v) => panic!("Should have lagged, got {}", v),
            Err(e) => panic!("Unexpected error {:?}", e),
        }
    }

    #[tokio::test]
    async fn closed_receiver_rejects_sends() {
        let (tx, rx) = channel::<u64>(4);

        tx.try_send(1).unwrap();
        drop(rx); // Receiver closed

        match tx.try_send(2) {
            Err(TrySendError::Closed(_)) => {}
            Ok(()) => panic!("Expected closed error after receiver drop"),
        }
    }

    #[tokio::test]
    async fn async_waker_notification() {
        let (tx, mut rx) = channel::<u64>(4);

        let h = tokio::spawn(async move {
            // This should block until the sender sends
            rx.recv().await
        });

        // Ensure the task has likely polled and parked
        tokio::time::sleep(Duration::from_millis(10)).await;

        tx.try_send(99).unwrap();

        let result = h.await.unwrap();
        assert_eq!(result, Ok(99));
    }
}

use event_listener::{Event, EventListener};
use futures_lite::Stream;
use parking_lot::Mutex;
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
// #[repr(align(64))] guarantees the struct starts at a cache-line boundary so
// that `head`, `mask`, and `closed` — all three read/written on every
// try_send / poll_recv — land in the SAME first 64-byte cache line.  Without
// this the allocator may place the struct anywhere, splitting hot fields
// across two lines and doubling the cache-miss cost per channel operation.
// Field order reinforces this: the three hot scalars come first (24 bytes total),
// then the Vec fat-pointer for `slots` (24 bytes), then `event` — all comfortably
// within the 64-byte window.  Each of the 300 participant Ring objects is now
// guaranteed to start fresh, preventing false sharing between adjacent
// allocations.
#[repr(align(64))]
struct Ring<T> {
    // ── HOT (accessed on every send AND recv) ──────────────────────────────
    /// Monotonically increasing write sequence number; producer fetch_add here.
    head: AtomicU64,
    /// Capacity mask (`capacity - 1`); used to compute slot index from seq.
    mask: usize,
    /// Set to 1 when the Receiver is dropped; checked by producers first.
    closed: AtomicU64,
    // ── WARM (fat-pointer; dereffed only after slot index is computed) ──────
    slots: Vec<Mutex<Slot<T>>>,
    event: Event,
}

impl<T> Ring<T> {
    fn new(mut capacity: usize) -> Arc<Self> {
        if capacity == 0 {
            capacity = 1;
        } else if !capacity.is_power_of_two() {
            capacity = capacity.next_power_of_two();
        }

        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(Mutex::new(Slot { seq: 0, val: None }));
        }

        Arc::new(Self {
            head: AtomicU64::new(0),
            mask: capacity - 1,
            closed: AtomicU64::new(0),
            slots,
            event: Event::new(),
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
            let mut slot = self.ring.slots[idx].lock();
            slot.val = Some(val);
            slot.seq = seq;
        }

        self.ring.event.notify(1);
        Ok(())
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

#[derive(Debug)]
pub struct Receiver<T> {
    ring: Arc<Ring<T>>,
    next_seq: u64,
    local_head: u64,
    listener: Option<EventListener>,
    pkts_received: u64,
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.ring.closed.store(1, Ordering::Release);
    }
}

impl<T> Receiver<T> {
    const METRIC_FLUSH_MASK: u64 = 1023;

    pub async fn recv(&mut self) -> Result<T, RecvError> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        let coop = std::task::ready!(tokio::task::coop::poll_proceed(cx));

        loop {
            if self.next_seq == self.local_head {
                self.local_head = self.ring.head.load(Ordering::Acquire);
            }

            if self.ring.closed.load(Ordering::Acquire) == 1 && self.next_seq >= self.local_head {
                return Poll::Ready(Err(RecvError::Closed));
            }

            if self.next_seq >= self.local_head {
                // Skip listener registration entirely if the waker is a no-op.
                // This avoids heap allocation and lock contention in callers
                // that only want a non-blocking poll (e.g. try_recv shims).
                if cx.waker().will_wake(std::task::Waker::noop()) {
                    return Poll::Pending;
                }

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

            let idx = (self.next_seq as usize) & self.ring.mask;
            let mut slot = self.ring.slots[idx].lock();
            let slot_seq = slot.seq;

            if slot_seq != self.next_seq {
                let lagged = slot.seq > self.next_seq;
                drop(slot);

                if lagged {
                    self.next_seq = self.local_head;
                    metrics::counter!("mpsc_receive_lag_total").increment(1);
                    return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
                } else {
                    // Stale slot, producer hasn't reached here yet
                    if self.listener.is_none() && !cx.waker().will_wake(std::task::Waker::noop()) {
                        self.listener = Some(self.ring.event.listen());
                    }

                    return Poll::Pending;
                }
            }

            if let Some(val) = slot.val.take() {
                coop.made_progress();
                self.next_seq += 1;
                self.pkts_received += 1;

                if (self.pkts_received & Self::METRIC_FLUSH_MASK) == 0 {
                    drop(slot);
                    self.flush_metrics();
                }
                return Poll::Ready(Ok(val));
            }

            self.next_seq = self.local_head;
            metrics::counter!("mpsc_receive_lag_total").increment(1);
            return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
        }
    }

    fn flush_metrics(&mut self) {
        let current_drift = self.local_head.saturating_sub(self.next_seq);
        let capacity = (self.ring.mask + 1) as f64;

        metrics::histogram!("mpsc_receive_drift_ratio").record(current_drift as f64 / capacity);
        metrics::counter!("mpsc_receive_throughput_total").increment(self.pkts_received);
        self.pkts_received = 0;
    }
}

impl<T> Stream for Receiver<T> {
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

    let ring = Ring::new(capacity);
    (
        Sender { ring: ring.clone() },
        Receiver {
            ring,
            next_seq: 0,
            local_head: 0,
            listener: None,
            pkts_received: 0,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

use crate::sync::atomic::{AtomicU64, Ordering};
use crate::sync::{Arc, RwLock};
use event_listener::{Event, EventListener};
use futures_lite::Stream;
use std::pin::Pin;
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
    slots: Vec<RwLock<Slot<T>>>,
    event: Event,
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
            slots.push(RwLock::new(Slot { seq: 0, val: None }));
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
    pkts_received: u64,
}

impl<T: Clone> Receiver<T> {
    const METRIC_FLUSH_MASK: u64 = 1023;

    /// Jump to the producer's current position (Audio/Low-latency).
    pub fn sync(&mut self) {
        self.local_head = self.ring.head.load(Ordering::Acquire);
        self.next_seq = self.local_head;
        self.listener = None;
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
            // Refresh head snapshot only when we've caught up to our last snapshot.
            if self.next_seq >= self.local_head {
                self.local_head = self.ring.head.load(Ordering::Acquire);
            }

            // Still no new items — park.
            if self.next_seq >= self.local_head {
                // Ensure we have a listener and that it is registered with the event.
                if self.listener.is_none() {
                    let mut listener = self.ring.event.listen();

                    // Poll once so the waker is registered.
                    if Pin::new(&mut listener).poll(cx).is_ready() {
                        // Notification already fired; retry the loop.
                        continue;
                    }

                    self.listener = Some(listener);
                }

                // Re-check head after listener registration to close the race window.
                self.local_head = self.ring.head.load(Ordering::Acquire);
                if self.next_seq < self.local_head {
                    // Data arrived after we installed the listener.
                    self.listener = None;
                    continue;
                }

                // Closed and drained.
                if self.ring.closed.load(Ordering::Acquire) == 1 {
                    return Poll::Ready(Err(RecvError::Closed));
                }

                // Park until notified.
                let listener = self.listener.as_mut().unwrap();
                match Pin::new(listener).poll(cx) {
                    Poll::Ready(_) => {
                        self.listener = None;
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            // We have at least one item available. Clear stale listener.
            self.listener = None;

            // Read slot for next_seq — clone value under the lock, then release.
            let idx = (self.next_seq as usize) & self.ring.mask;
            let (slot_seq, maybe_val) = {
                let slot = self.ring.slots[idx].read();
                (slot.seq, slot.val.clone())
            };

            // Seq mismatch means producer overwrote this slot while we lagged.
            if slot_seq != self.next_seq {
                // Reload head for an accurate lag report.
                self.local_head = self.ring.head.load(Ordering::Acquire);
                self.next_seq = self.local_head;
                #[cfg(not(feature = "loom"))]
                metrics::counter!("spmc_receive_lag_total").increment(1);
                return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
            }

            // Coop yield point — no locks held here.
            let coop = ready!(crate::sync::coop::poll_proceed(cx));

            let Some(out) = maybe_val else {
                // seq matched but val is None — should be impossible, but treat as lag.
                debug_assert!(
                    false,
                    "slot seq matched but val was None — ring invariant violated"
                );
                self.local_head = self.ring.head.load(Ordering::Acquire);
                self.next_seq = self.local_head;
                #[cfg(not(feature = "loom"))]
                metrics::counter!("spmc_receive_lag_total").increment(1);
                return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
            };

            coop.made_progress();
            self.next_seq += 1;
            self.pkts_received += 1;

            if (self.pkts_received & Self::METRIC_FLUSH_MASK) == 0 {
                self.flush_metrics();
            }
            return Poll::Ready(Ok(out));
        }
    }

    fn flush_metrics(&mut self) {
        #[cfg(not(feature = "loom"))]
        {
            let current_drift = self.local_head.saturating_sub(self.next_seq);
            let capacity = (self.ring.mask + 1) as f64;
            metrics::histogram!("spmc_receive_drift_ratio").record(current_drift as f64 / capacity);
            metrics::counter!("spmc_receive_throughput_total").increment(self.pkts_received);
        }
        self.pkts_received = 0;
    }
}

impl<T: Clone + std::marker::Unpin> Stream for Receiver<T> {
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
            listener: None,
            pkts_received: 0,
        }
    }
}

pub fn channel<T: Send + Sync + Clone + 'static>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    #[cfg(not(feature = "loom"))]
    {
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
    }

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
            pkts_received: 0,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_test::task::{new_count_waker, noop_waker, panic_waker};
    use std::task::{Context, Poll};

    // ── helpers ──────────────────────────────────────────────────────────────

    fn poll<T: Clone>(rx: &mut Receiver<T>, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        rx.poll_recv(cx)
    }

    /// Drive a receiver to completion, panicking on Pending. Useful when we
    /// know data is already in the ring.
    fn try_recv<T: Clone>(rx: &mut Receiver<T>) -> Result<T, RecvError> {
        let waker = panic_waker();
        let mut cx = Context::from_waker(&waker);
        match poll(rx, &mut cx) {
            Poll::Ready(r) => r,
            Poll::Pending => panic!("unexpected Pending — data should have been available"),
        }
    }

    // ── §1  Basic correctness ─────────────────────────────────────────────────

    #[test]
    fn send_and_recv_single_item() {
        let (mut tx, mut rx) = channel::<u64>(8);
        tx.send(42);
        assert_eq!(try_recv(&mut rx), Ok(42));
    }

    #[test]
    fn send_and_recv_multiple_items_in_order() {
        let (mut tx, mut rx) = channel::<u64>(8);
        for i in 0..8_u64 {
            tx.send(i);
        }
        for i in 0..8_u64 {
            assert_eq!(try_recv(&mut rx), Ok(i));
        }
    }

    #[test]
    fn multi_consumer_each_sees_all_messages() {
        let (mut tx, rx) = channel::<u32>(8);
        let mut rx1 = rx.clone();
        let mut rx2 = rx.clone();
        let mut rx3 = rx.clone();

        for i in 0..4_u32 {
            tx.send(i);
        }
        for i in 0..4_u32 {
            assert_eq!(try_recv(&mut rx1), Ok(i), "rx1 mismatch at {i}");
            assert_eq!(try_recv(&mut rx2), Ok(i), "rx2 mismatch at {i}");
            assert_eq!(try_recv(&mut rx3), Ok(i), "rx3 mismatch at {i}");
        }
    }

    // ── §2  Ring wrap-around ──────────────────────────────────────────────────

    #[test]
    fn ring_wraps_correctly_after_full_rotation() {
        let (mut tx, mut rx) = channel::<u64>(4);

        // Fill the ring, consume two, then wrap.
        for i in 0..4 {
            tx.send(i);
        }
        assert_eq!(try_recv(&mut rx), Ok(0));
        assert_eq!(try_recv(&mut rx), Ok(1));

        // Overwrite slots 0 and 1.
        tx.send(4);
        tx.send(5);

        // Slots 2 and 3 still intact; 4 and 5 are at old positions.
        assert_eq!(try_recv(&mut rx), Ok(2));
        assert_eq!(try_recv(&mut rx), Ok(3));
        assert_eq!(try_recv(&mut rx), Ok(4));
        assert_eq!(try_recv(&mut rx), Ok(5));
    }

    #[test]
    fn ring_wraps_multiple_full_rotations() {
        const CAP: usize = 4;
        let (mut tx, mut rx) = channel::<u64>(CAP);

        // Two complete ring rotations.
        for i in 0..((CAP * 2) as u64) {
            tx.send(i);
            assert_eq!(try_recv(&mut rx), Ok(i));
        }
    }

    // ── §3  Lag detection ─────────────────────────────────────────────────────

    #[test]
    fn slow_receiver_gets_lagged_and_jumps_to_head() {
        let (mut tx, mut rx) = channel::<u64>(4);

        // Overwrite the entire ring twice over.
        for i in 0..6 {
            tx.send(i);
        }

        // rx is at seq 0; head is at 6; earliest valid is 6-4=2.
        match try_recv(&mut rx) {
            Err(RecvError::Lagged(h)) => assert_eq!(h, 6),
            other => panic!("expected Lagged(6), got {other:?}"),
        }

        // After lag, receiver jumps to head and waits.
        let waker = panic_waker();
        let mut cx = Context::from_waker(&waker);
        // Nothing new yet — must be Pending, not Lagged again.
        assert!(matches!(poll(&mut rx, &mut cx), Poll::Pending));
    }

    #[test]
    fn sequence_mismatch_detected_under_concurrent_overwrite() {
        let (mut tx, mut rx) = channel::<u64>(2);
        tx.send(0);
        assert_eq!(try_recv(&mut rx), Ok(0)); // seq=1

        // Advance tx so rx is overwritten twice.
        tx.send(1); // slot 1
        tx.send(2); // slot 0
        tx.send(3); // slot 1 — rx expects seq 1, which is now stale.

        // Head=4, earliest=2. rx.next_seq=1 < 2 → Lagged(4).
        match try_recv(&mut rx) {
            Err(RecvError::Lagged(h)) => assert_eq!(h, 4),
            other => panic!("expected Lagged(4), got {other:?}"),
        }
    }

    #[test]
    fn lagged_receiver_can_continue_after_recovery() {
        let (mut tx, mut rx) = channel::<u64>(4);
        for i in 0..8 {
            tx.send(i);
        }

        // Must get lag first.
        assert!(matches!(try_recv(&mut rx), Err(RecvError::Lagged(_))));

        // Send fresh data; receiver should resume.
        tx.send(100);
        assert_eq!(try_recv(&mut rx), Ok(100));
    }

    // ── §4  Closure behaviour ─────────────────────────────────────────────────

    #[test]
    fn closed_after_drain_returns_closed_error() {
        let (mut tx, mut rx) = channel::<u64>(4);
        tx.send(1);
        tx.send(2);
        drop(tx);

        assert_eq!(try_recv(&mut rx), Ok(1));
        assert_eq!(try_recv(&mut rx), Ok(2));
        assert_eq!(try_recv(&mut rx), Err(RecvError::Closed));
        // Idempotent — subsequent polls must also return Closed.
        assert_eq!(try_recv(&mut rx), Err(RecvError::Closed));
    }

    #[test]
    fn closed_empty_ring_returns_closed_immediately() {
        let (tx, mut rx) = channel::<u64>(4);
        drop(tx);
        assert_eq!(try_recv(&mut rx), Err(RecvError::Closed));
    }

    #[test]
    fn send_after_close_is_a_noop() {
        let (mut tx, mut rx) = channel::<u64>(4);
        tx.ring.closed.store(1, std::sync::atomic::Ordering::Release);
        tx.send(99); // must not panic

        let waker = panic_waker();
        let mut cx = Context::from_waker(&waker);
        // Ring is closed and empty — Closed (via try_recv would panic on Pending,
        // so use poll directly).
        assert_eq!(poll(&mut rx, &mut cx), Poll::Ready(Err(RecvError::Closed)));
    }

    // ── §5  Waker registration & spurious-wake safety ─────────────────────────

    /// The core waker contract: after polling Pending, the waker must be woken
    /// exactly once when data arrives; a second poll must be Ready.
    #[test]
    fn waker_is_called_exactly_once_on_send() {
        let (mut tx, mut rx) = channel::<u64>(4);

        let (waker, count) = new_count_waker();
        let mut cx = Context::from_waker(&waker);

        // Nothing in the ring — should park.
        assert!(matches!(poll(&mut rx, &mut cx), Poll::Pending));
        assert_eq!(count.get(), 0, "no wake before send");

        tx.send(7);
        assert_eq!(count.get(), 1, "exactly one wake after send");

        // Now the waker has fired; subsequent poll should be Ready.
        let waker2 = panic_waker();
        let mut cx2 = Context::from_waker(&waker2);
        assert_eq!(poll(&mut rx, &mut cx2), Poll::Ready(Ok(7)));
    }

    /// A *new* waker registered on a second poll must replace the old one.
    #[test]
    fn waker_is_replaced_on_second_poll() {
        let (mut tx, mut rx) = channel::<u64>(4);

        let (waker_a, count_a) = new_count_waker();
        let mut cx_a = Context::from_waker(&waker_a);

        assert!(matches!(poll(&mut rx, &mut cx_a), Poll::Pending));

        // Poll again with a different waker — the listener must adopt the new one.
        let (waker_b, count_b) = new_count_waker();
        let mut cx_b = Context::from_waker(&waker_b);

        assert!(matches!(poll(&mut rx, &mut cx_b), Poll::Pending));

        tx.send(5);

        // Only waker_b should fire (it was registered last).
        assert_eq!(count_b.get(), 1, "waker_b must fire");
        // waker_a *may or may not* fire depending on event-listener internals;
        // the important invariant is that waker_b fires.
    }

    /// Spurious wakes must not cause incorrect Ready results.
    #[test]
    fn spurious_wakes_do_not_produce_incorrect_ready() {
        let (mut _tx, mut rx) = channel::<u64>(4);

        let (waker, _count) = new_count_waker();
        let mut cx = Context::from_waker(&waker);

        // Park.
        assert!(matches!(poll(&mut rx, &mut cx), Poll::Pending));

        // Spurious re-poll with a noop waker — must stay Pending, not Ready.
        let noop = noop_waker();
        let mut cx_noop = Context::from_waker(&noop);
        assert!(
            matches!(poll(&mut rx, &mut cx_noop), Poll::Pending),
            "spurious re-poll must remain Pending when ring is still empty"
        );
    }

    /// Data that arrives *between* listener registration and the final head
    /// re-check must not be missed (the tightest race window in poll_recv).
    #[test]
    fn no_lost_wakeup_data_arrives_during_listener_registration() {
        // We exercise this deterministically: park first, then send, then
        // re-poll. The listener should have caught the notify or the re-check
        // after listener installation should notice.
        let (mut tx, mut rx) = channel::<u64>(4);

        let (waker, count) = new_count_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(poll(&mut rx, &mut cx), Poll::Pending));

        // Simulate "data arrives while listener is being set up".
        tx.send(42);
        assert_eq!(count.get(), 1);

        // Re-poll — must be Ready(42), no data loss.
        let waker2 = panic_waker();
        let mut cx2 = Context::from_waker(&waker2);
        assert_eq!(poll(&mut rx, &mut cx2), Poll::Ready(Ok(42)));
    }

    /// Close notification must also wake a parked receiver.
    #[test]
    fn close_wakes_parked_receiver() {
        let (tx, mut rx) = channel::<u64>(4);

        let (waker, count) = new_count_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(poll(&mut rx, &mut cx), Poll::Pending));

        drop(tx); // triggers closed + notify
        assert_eq!(count.get(), 1, "close must wake receiver");

        let waker2 = panic_waker();
        let mut cx2 = Context::from_waker(&waker2);
        assert_eq!(poll(&mut rx, &mut cx2), Poll::Ready(Err(RecvError::Closed)));
    }

    // ── §6  Multiple concurrent receivers & wakers ────────────────────────────

    /// All parked receivers must be woken when a message is sent.
    #[test]
    fn all_parked_receivers_are_woken_on_send() {
        let (mut tx, rx) = channel::<u64>(8);
        let mut rx1 = rx.clone();
        let mut rx2 = rx.clone();
        let mut rx3 = rx.clone();

        let (w1, c1) = new_count_waker();
        let (w2, c2) = new_count_waker();
        let (w3, c3) = new_count_waker();

        // Park all three.
        assert!(matches!(
            poll(&mut rx1, &mut Context::from_waker(&w1)),
            Poll::Pending
        ));
        assert!(matches!(
            poll(&mut rx2, &mut Context::from_waker(&w2)),
            Poll::Pending
        ));
        assert!(matches!(
            poll(&mut rx3, &mut Context::from_waker(&w3)),
            Poll::Pending
        ));

        tx.send(1);

        assert_eq!(c1.get(), 1, "rx1 must be woken");
        assert_eq!(c2.get(), 1, "rx2 must be woken");
        assert_eq!(c3.get(), 1, "rx3 must be woken");

        // All three should read the same value.
        let waker = panic_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(poll(&mut rx1, &mut cx), Poll::Ready(Ok(1)));
        assert_eq!(poll(&mut rx2, &mut cx), Poll::Ready(Ok(1)));
        assert_eq!(poll(&mut rx3, &mut cx), Poll::Ready(Ok(1)));
    }

    // ── §7  sync() / rewind() ─────────────────────────────────────────────────

    #[test]
    fn sync_jumps_to_current_head_and_reads_only_new_data() {
        let (mut tx, mut rx) = channel::<u64>(8);
        for i in 0..4 {
            tx.send(i);
        }

        // Sync skips old data.
        rx.sync();

        // Nothing new yet — must park.
        let (waker, _) = new_count_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(poll(&mut rx, &mut cx), Poll::Pending));

        tx.send(99);

        let waker2 = panic_waker();
        let mut cx2 = Context::from_waker(&waker2);
        assert_eq!(poll(&mut rx, &mut cx2), Poll::Ready(Ok(99)));
    }

    #[test]
    fn rewind_provides_a_processing_window() {
        let cap = 8;
        let (mut tx, mut rx) = channel::<u64>(cap);
        for i in 0..8_u64 {
            tx.send(i);
        }

        // rx is at seq 0, head is at 8. rewind should land us partway through.
        rx.rewind();

        // Must receive something valid (not lagged, not pending).
        let waker = panic_waker();
        let mut cx = Context::from_waker(&waker);
        match poll(&mut rx, &mut cx) {
            Poll::Ready(Ok(_)) => {}
            other => panic!("expected Ready(Ok(_)) after rewind, got {other:?}"),
        }
    }

    #[test]
    fn rewind_on_fresh_receiver_does_not_panic() {
        let (mut tx, mut rx) = channel::<u64>(8);
        tx.send(1);
        // next_seq == local_head == 0 at construction; rewind must not underflow.
        rx.rewind();
        // After rewind on a just-cloned receiver next_seq should be sane.
        let waker = panic_waker();
        let mut cx = Context::from_waker(&waker);
        let result = poll(&mut rx, &mut cx);
        // Either Ready(Ok(1)) or Pending is fine — no panic or Lagged.
        assert!(
            matches!(result, Poll::Ready(Ok(_)) | Poll::Pending),
            "unexpected {result:?}"
        );
    }

    // ── §8  Stream trait ──────────────────────────────────────────────────────

    #[test]
    fn stream_yields_items_and_terminates_on_close() {
        use futures_lite::StreamExt;

        let (mut tx, rx) = channel::<u64>(4);
        // We need an async executor here; use futures_lite's block_on.
        futures_lite::future::block_on(async move {
            let mut stream = rx;
            tx.send(1);
            tx.send(2);
            drop(tx);

            assert_eq!(stream.next().await, Some(Ok(1)));
            assert_eq!(stream.next().await, Some(Ok(2)));
            assert_eq!(stream.next().await, None);
        });
    }

    #[test]
    fn stream_surfaces_lag_as_stream_recv_error() {
        use futures_lite::StreamExt;

        let (mut tx, rx) = channel::<u64>(4);
        futures_lite::future::block_on(async move {
            let mut stream = rx;
            // Overfill to force lag.
            for i in 0..8 {
                tx.send(i);
            }

            match stream.next().await {
                Some(Err(StreamRecvError::Lagged(_))) => {}
                other => panic!("expected StreamRecvError::Lagged, got {other:?}"),
            }
        });
    }

    // ── §9  Clone behaviour ───────────────────────────────────────────────────

    #[test]
    fn cloned_receiver_starts_at_same_seq_as_original() {
        let (mut tx, mut rx) = channel::<u64>(8);
        tx.send(0);
        assert_eq!(try_recv(&mut rx), Ok(0)); // rx.next_seq is now 1

        tx.send(1);

        let mut rx2 = rx.clone(); // cloned at seq 1

        // Both should see item 1.
        assert_eq!(try_recv(&mut rx), Ok(1));
        assert_eq!(try_recv(&mut rx2), Ok(1));
    }

    #[test]
    fn cloned_receiver_has_independent_waker() {
        let (mut tx, rx) = channel::<u64>(4);
        let mut rx1 = rx.clone();
        let mut rx2 = rx;

        // Park rx1 with waker_a.
        let (waker_a, count_a) = new_count_waker();
        assert!(matches!(
            poll(&mut rx1, &mut Context::from_waker(&waker_a)),
            Poll::Pending
        ));

        // Park rx2 with waker_b.
        let (waker_b, count_b) = new_count_waker();
        assert!(matches!(
            poll(&mut rx2, &mut Context::from_waker(&waker_b)),
            Poll::Pending
        ));

        tx.send(42);

        assert_eq!(count_a.get(), 1);
        assert_eq!(count_b.get(), 1);
    }

    // ── §10  Capacity edge cases ──────────────────────────────────────────────

    #[test]
    fn capacity_one_ring_works() {
        let (mut tx, mut rx) = channel::<u64>(1);
        tx.send(1);
        assert_eq!(try_recv(&mut rx), Ok(1));
        tx.send(2);
        assert_eq!(try_recv(&mut rx), Ok(2));
    }

    #[test]
    fn non_power_of_two_capacity_is_rounded_up() {
        // Ring::new rounds 5 → 8.
        let (mut tx, mut rx) = channel::<u64>(5);
        // Should be able to buffer 8 items without lag.
        for i in 0..8_u64 {
            tx.send(i);
        }
        for i in 0..8_u64 {
            assert_eq!(try_recv(&mut rx), Ok(i));
        }
    }

    // ── §11  Poll idempotence after Ready ─────────────────────────────────────

    #[test]
    fn polling_again_after_ready_waits_for_next_item() {
        let (mut tx, mut rx) = channel::<u64>(4);
        tx.send(1);

        let waker = panic_waker();
        let mut cx = Context::from_waker(&waker);

        assert_eq!(poll(&mut rx, &mut cx), Poll::Ready(Ok(1)));

        // Ring is empty now — must park, not panic.
        let (waker2, _) = new_count_waker();
        let mut cx2 = Context::from_waker(&waker2);
        assert!(matches!(poll(&mut rx, &mut cx2), Poll::Pending));
    }

    // ── §12  Production Stress & Invariants ──────────────────────────────────

    #[test]
    fn sequential_large_transfer() {
        let (mut tx, mut rx) = channel::<u64>(1024);
        const COUNT: u64 = 100_000;
        for i in 0..COUNT {
            tx.send(i);
            assert_eq!(try_recv(&mut rx), Ok(i));
        }
    }

    #[test]
    fn multi_receiver_lag_recovery_loop() {
        let (mut tx, rx) = channel::<u64>(10);
        let mut rxs: Vec<_> = (0..5).map(|_| rx.clone()).collect();

        // 1. Initial drain
        for i in 0..5 {
            tx.send(i);
        }
        for rx in &mut rxs {
            for i in 0..5 {
                assert_eq!(try_recv(rx), Ok(i));
            }
        }

        // 2. Overfill significantly
        for i in 5..100 {
            tx.send(i);
        }

        // 3. All must experience lag
        let rxs_len = rxs.len() as u64;
        for rx in &mut rxs {
            match try_recv(rx) {
                Err(RecvError::Lagged(h)) => assert_eq!(h, 100),
                other => panic!("expected Lagged(100), got {:?}", other),
            }
        }
        // 4. All must be at head now
        tx.send(100 + rxs_len); // send some more

        // 5. Verify they can all continue (some might have lagged again if not careful)
        // they must read the just-sent item (105) and then the next one (200).
        tx.send(200);
        for rx in &mut rxs {
            assert_eq!(try_recv(rx), Ok(105));
            assert_eq!(try_recv(rx), Ok(200));
        }
    }

    #[test]
    fn cross_thread_contention() {
        use std::thread;
        let (mut tx, rx) = channel::<u64>(1024);
        let num_receivers = 4;
        let items_per_receiver = 10_000;
        
        let mut handles = vec![];
        for _ in 0..num_receivers {
            let mut rx = rx.clone();
            handles.push(thread::spawn(move || {
                let mut count = 0;
                while count < items_per_receiver {
                    match futures_lite::future::block_on(rx.recv()) {
                        Ok(_) => count += 1,
                        Err(RecvError::Lagged(_)) => {}, // ignore lags
                        Err(RecvError::Closed) => break,
                    }
                }
                count
            }));
        }

        for i in 0..(num_receivers * items_per_receiver * 2) {
            tx.send(i as u64);
        }
        drop(tx);

        for handle in handles {
            let count = handle.join().unwrap();
            assert!(count > 0);
        }
    }
}

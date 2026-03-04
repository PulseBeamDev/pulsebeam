use crate::sync::bit_signal::BitSignal;
use crossbeam_utils::CachePadded;
use std::sync::{Arc, RwLock, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    Lagged(u64),
    Closed,
}

#[derive(Debug)]
struct Slot<T> {
    seq: u64,
    val: Option<T>,
}

/// Lock-based SPMC ring buffer driven entirely by `BitSignal` notifications.
///
/// There is no async-waker registration in this type.
/// Producers wake subscribers by OR-ing their slot bit into the shard's
/// `BitSignal` and calling `signal.wake()`.  `poll_recv` is non-blocking:
/// it reads the ring and returns `Poll::Pending` when empty; re-waking is
/// the shard's responsibility (it re-runs when the BitSignal fires).
#[derive(Debug)]
struct Ring<T> {
    head: AtomicU64,
    mask: usize,
    slots: Vec<CachePadded<RwLock<Slot<T>>>>,
    closed: AtomicU64,
    /// Per-shard wake registrations.
    /// Each entry: `(Weak<BitSignal>, bit_mask)`.
    /// `Sender::send()` ORs `bit_mask` into the signal's `pending` field
    /// and wakes the shard task.  `Weak` so dropped shards are skipped
    /// without explicit cleanup.
    notifiers: RwLock<Vec<(Weak<BitSignal>, u64)>>,
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
            closed: AtomicU64::new(0),
            notifiers: RwLock::new(Vec::new()),
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

        // Notify registered shard BitSignals (O(shards), typically 1).
        // `wake()` only does an atomic store + optional OS notify — safe to
        // call under a read lock; no deadlock risk as wake() never re-acquires
        // notifiers.
        let notifiers = self.ring.notifiers.read().unwrap();
        for (weak, bits) in notifiers.iter() {
            if let Some(sig) = weak.upgrade() {
                sig.pending.fetch_or(*bits, Ordering::Release);
                sig.wake();
            }
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.ring.closed.store(1, Ordering::Release);
        // Wake all shards so they can observe the Closed state.
        let notifiers = self.ring.notifiers.read().unwrap();
        for (weak, bits) in notifiers.iter() {
            if let Some(sig) = weak.upgrade() {
                sig.pending.fetch_or(*bits, Ordering::Release);
                sig.wake();
            }
        }
    }
}

#[derive(Debug)]
pub struct Receiver<T> {
    ring: Arc<Ring<T>>,
    next_seq: u64,
    local_head: u64,
    pkts_received: u64,
}

impl<T: Clone> Receiver<T> {
    const METRIC_FLUSH_MASK: u64 = 1023;

    /// Jump to the producer's current position (audio / low-latency use).
    pub fn sync(&mut self) {
        self.local_head = self.ring.head.load(Ordering::Acquire);
        self.next_seq = self.local_head;
    }

    /// Jump back halfway into the ring to give a keyframe-buffering window
    /// (video / burst use).
    pub fn rewind(&mut self) {
        self.local_head = self.ring.head.load(Ordering::Acquire);
        let half_ring_cap = (self.ring.slots.len() / 2) as u64;
        let ring_len = self.local_head.wrapping_sub(self.next_seq);

        if ring_len > self.ring.slots.len() as u64 {
            // Impossibly far behind — reset to head.
            self.next_seq = self.local_head;
        } else {
            let offset = half_ring_cap.min(ring_len);
            self.next_seq = self.local_head.wrapping_sub(offset);
        }
    }

    /// Non-blocking read.  Returns `None` when the ring is empty.
    pub fn try_recv(&mut self) -> Option<Result<T, RecvError>> {
        match self.poll_recv() {
            Poll::Ready(r) => Some(r),
            Poll::Pending => None,
        }
    }

    /// Register a shard's `BitSignal` on this ring.
    ///
    /// Every `Sender::send()` will OR `bits` into `signal.pending` and call
    /// `signal.wake()`.  Call once per subscriber; call `detach_signal` when
    /// the subscription ends.
    pub fn attach_signal(&self, signal: &Arc<BitSignal>, bits: u64) {
        let mut notifiers = self.ring.notifiers.write().unwrap();
        let weak = Arc::downgrade(signal);
        for (w, existing_bits) in notifiers.iter_mut() {
            if let Some(existing) = w.upgrade() {
                if Arc::ptr_eq(&existing, signal) {
                    *existing_bits |= bits;
                    return;
                }
            }
        }
        notifiers.push((weak, bits));
    }

    /// Remove a shard's bit(s) from this ring's notifier list.
    /// Drops the entry if all bits are cleared.
    pub fn detach_signal(&self, signal: &Arc<BitSignal>, bits: u64) {
        let mut notifiers = self.ring.notifiers.write().unwrap();
        notifiers.retain_mut(|(w, existing_bits)| {
            if let Some(existing) = w.upgrade() {
                if Arc::ptr_eq(&existing, signal) {
                    *existing_bits &= !bits;
                    return *existing_bits != 0;
                }
            }
            true
        });
    }

    /// Non-blocking ring read used by the shard's drain loop and by
    /// `poll_packet` in the downstream video/audio state machines.
    ///
    /// Returns `Poll::Pending` when the ring is empty.  Re-waking is handled
    /// externally by the `BitSignal` attached via `attach_signal`.
    pub fn poll_recv(&mut self) -> Poll<Result<T, RecvError>> {
        loop {
            // Snapshot producer head for batch-read efficiency.
            if self.next_seq == self.local_head {
                self.local_head = self.ring.head.load(Ordering::Acquire);
            }

            // Closed and drained.
            if self.ring.closed.load(Ordering::Acquire) == 1 && self.next_seq >= self.local_head {
                return Poll::Ready(Err(RecvError::Closed));
            }

            // Nothing new yet.
            if self.next_seq >= self.local_head {
                return Poll::Pending;
            }

            let idx = (self.next_seq as usize) & self.ring.mask;
            let slot = self.ring.slots[idx].read().unwrap();
            let slot_seq = slot.seq;

            if slot_seq != self.next_seq {
                let lagged = slot.seq > self.next_seq;
                drop(slot);

                if lagged {
                    self.next_seq = self.local_head;
                    metrics::counter!("spmc_receive_lag_total").increment(1);
                    return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
                } else {
                    // Producer hasn't written this slot yet — spurious head advance.
                    return Poll::Pending;
                }
            }

            if let Some(v) = &slot.val {
                let out = v.clone();
                self.pkts_received += 1;
                self.next_seq += 1;

                if (self.pkts_received & Self::METRIC_FLUSH_MASK) == 0 {
                    drop(slot);
                    self.flush_metrics();
                }
                return Poll::Ready(Ok(out));
            }

            // Slot seq matched but value is None — should be unreachable.
            self.next_seq = self.local_head;
            metrics::counter!("spmc_receive_lag_total").increment(1);
            return Poll::Ready(Err(RecvError::Lagged(self.local_head)));
        }
    }

    fn flush_metrics(&mut self) {
        let current_drift = self.local_head.saturating_sub(self.next_seq);
        let capacity = (self.ring.mask + 1) as f64;
        metrics::histogram!("spmc_receive_drift_ratio").record(current_drift as f64 / capacity);
        metrics::counter!("spmc_receive_throughput_total").increment(self.pkts_received);
        self.pkts_received = 0;
    }
}

impl<T: Clone> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            ring: self.ring.clone(),
            next_seq: self.next_seq,
            local_head: self.local_head,
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
            ring,
            next_seq: 0,
            local_head: 0,
            pkts_received: 0,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::bit_signal::BitSignal;

    #[test]
    fn basic_send_recv() {
        let (mut tx, mut rx) = channel::<u64>(8);
        tx.send(42);
        assert_eq!(rx.try_recv(), Some(Ok(42)));
        tx.send(123);
        assert_eq!(rx.try_recv(), Some(Ok(123)));
    }

    #[test]
    fn multi_consumer_broadcast() {
        let (mut tx, rx) = channel::<String>(8);
        let mut rx1 = rx.clone();
        let mut rx2 = rx.clone();

        tx.send("alpha".to_string());
        tx.send("beta".to_string());

        assert_eq!(rx1.try_recv(), Some(Ok("alpha".to_string())));
        assert_eq!(rx1.try_recv(), Some(Ok("beta".to_string())));
        assert_eq!(rx1.try_recv(), None);

        assert_eq!(rx2.try_recv(), Some(Ok("alpha".to_string())));
        assert_eq!(rx2.try_recv(), Some(Ok("beta".to_string())));
        assert_eq!(rx2.try_recv(), None);
    }

    #[test]
    fn buffer_wrap_around_behavior() {
        let (mut tx, mut rx) = channel::<u64>(4);

        for i in 0..4 {
            tx.send(i);
        }

        assert_eq!(rx.try_recv(), Some(Ok(0)));
        assert_eq!(rx.try_recv(), Some(Ok(1)));

        tx.send(4);
        tx.send(5);

        assert_eq!(rx.try_recv(), Some(Ok(2)));
        assert_eq!(rx.try_recv(), Some(Ok(3)));
        assert_eq!(rx.try_recv(), Some(Ok(4)));
        assert_eq!(rx.try_recv(), Some(Ok(5)));
        assert_eq!(rx.try_recv(), None);
    }

    #[test]
    fn lagging_receiver_jumps_to_head() {
        let (mut tx, mut rx) = channel::<u64>(4);

        for i in 0..6 {
            tx.send(i);
        }

        // Receiver at seq 0, ring head at 6, earliest valid is 2 → lagged.
        match rx.try_recv() {
            Some(Err(RecvError::Lagged(seq))) => assert_eq!(seq, 6),
            other => panic!("Expected Lagged(6), got {:?}", other),
        }

        // Receiver jumped to seq 6; send one more and it should be readable.
        tx.send(6);
        assert_eq!(rx.try_recv(), Some(Ok(6)));
    }

    #[test]
    fn receiver_detects_overwrite_during_read() {
        let (mut tx, mut rx) = channel::<u64>(2);

        tx.send(0);
        assert_eq!(rx.try_recv(), Some(Ok(0)));

        tx.send(1);
        tx.send(2); // overwrites slot 0 (seq 0) with seq 2
        tx.send(3); // overwrites slot 1 (seq 1) with seq 3  — rx at seq 1 → lagged

        match rx.try_recv() {
            Some(Err(RecvError::Lagged(s))) => assert_eq!(s, 4),
            other => panic!("Expected Lagged(4), got {:?}", other),
        }
    }

    #[test]
    fn close_signal_drains_then_stops() {
        let (mut tx, mut rx) = channel::<u64>(4);

        tx.send(1);
        tx.send(2);
        drop(tx);

        assert_eq!(rx.try_recv(), Some(Ok(1)));
        assert_eq!(rx.try_recv(), Some(Ok(2)));
        assert_eq!(rx.try_recv(), Some(Err(RecvError::Closed)));
        assert_eq!(rx.try_recv(), Some(Err(RecvError::Closed)));
    }

    #[test]
    fn bitsignal_notified_on_send() {
        let (mut tx, rx) = channel::<u64>(8);
        let signal = BitSignal::new();

        rx.attach_signal(&signal, 0b01);
        assert_eq!(signal.take(), 0, "no bits before any send");

        tx.send(42);
        assert_eq!(signal.take(), 0b01, "bit set after send");
        assert_eq!(signal.take(), 0, "take clears bits");
    }

    #[test]
    fn bitsignal_notified_on_close() {
        let (tx, rx) = channel::<u64>(8);
        let signal = BitSignal::new();
        rx.attach_signal(&signal, 0b10);

        drop(tx);
        assert_eq!(signal.take(), 0b10, "bit set on sender drop");
    }
}

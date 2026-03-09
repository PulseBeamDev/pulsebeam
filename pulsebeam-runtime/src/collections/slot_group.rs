//! A fixed-capacity, index-stable group of streams with O(woken) hot-path polling.
//!
//! Streams are stored inline in a [`Vec`] (contiguous memory, single allocation)
//! addressed by a stable [`usize`] index. The hot path ([`Stream::poll_next`]) only
//! polls slots that have been woken since the last drain, tracked via an atomic `u64`
//! readiness bitmask — the same mechanism used by [`TaskGroup`].
//!
//! The cold path has **direct** mutable access to each stream through [`get_mut`] and
//! [`iter_mut`], making `SlotGroup` suitable for types that need both async polling
//! *and* external state mutation (e.g. video/audio simulcast slot drivers).
//!
//! **Capacity is capped at 64** — one bit per slot in the `u64` readiness word.
//!
//! # Waker contract
//!
//! Every stream stored in a `SlotGroup` **must** re-register the waker it receives
//! on every `Poll::Pending` return — the standard `Stream` contract. Caching wakers
//! across polls is unsafe here because the per-slot [`Waker`] identity is stable but
//! the outer task waker may change between polls.
//!
//! # Cold-path mutation and wakeups
//!
//! After mutating a slot through [`get_mut`] or [`iter_mut`] in a way that transitions
//! it from a non-emitting to an emitting state, call [`poke`] on that slot index to
//! guarantee it is visited on the next [`poll_next`]. Without a poke, the slot's bit
//! stays dark until the underlying stream self-notifies.
//!
//! [`TaskGroup`]: crate::sync::task_group::TaskGroup
//! [`get_mut`]: SlotGroup::get_mut
//! [`iter_mut`]: SlotGroup::iter_mut
//! [`poke`]: SlotGroup::poke
//! [`poll_next`]: Stream::poll_next

use crate::sync::Arc;
use crate::sync::bit_signal::BitSignal;
use crate::sync::task_group::make_slot_waker;
use diatomic_waker::WakeSink;
use futures_lite::stream::Stream;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

/// RAII guard returned by [`SlotGroup::get_mut`] and [`SlotGroup::iter_mut`].
///
/// Derefs to `&mut S` for the duration of the borrow. On drop, the slot is
/// automatically poked so it is visited on the next
/// [`poll_next`](Stream::poll_next) call, regardless of whether the mutation
/// actually changed the stream's state. A spurious poke costs one bit-set; a
/// missing poke can freeze a slot indefinitely.
pub struct SlotGuard<'a, S> {
    slot: &'a mut S,
    signal: &'a Arc<BitSignal>,
    index: u8,
}

impl<S> Deref for SlotGuard<'_, S> {
    type Target = S;
    #[inline]
    fn deref(&self) -> &S {
        self.slot
    }
}

impl<S> DerefMut for SlotGuard<'_, S> {
    #[inline]
    fn deref_mut(&mut self) -> &mut S {
        self.slot
    }
}

impl<S> Drop for SlotGuard<'_, S> {
    #[inline]
    fn drop(&mut self) {
        self.signal.notify(self.index);
    }
}

/// See [module docs](self).
pub struct SlotGroup<S> {
    signal: Arc<BitSignal>,
    wake_sink: WakeSink,
    /// Locally-cached readiness bits from the current batch.
    ///
    /// `signal.take()` (an atomic swap) runs **once per outer `poll_next` call**
    /// and is OR-merged here. Subsequent items from the same batch are served
    /// from this field with no atomic operations at all.
    pending_bits: u64,
    /// Bitset tracking which slot indices are currently occupied.
    occupied: u64,
    /// Cached outer waker — compared via `will_wake` so `wake_sink.register`
    /// (which does an atomic store even on a no-op path) is skipped in the
    /// common case where the task waker hasn't changed between polls.
    cached_waker: Option<Waker>,
    /// Inline storage — one element per slot, `None` for free/vacant slots.
    slots: Vec<Option<S>>,
    /// Pre-built per-slot wakers. Created once on [`insert`](SlotGroup::insert),
    /// reused across all polls — waker allocation is a construction-time cost only.
    slot_wakers: Vec<Option<Waker>>,
    /// Last polled slot index for fair round-robin scheduling.
    last_index: u8,
}

impl<S> SlotGroup<S> {
    /// Create a group that can hold up to `capacity` streams.
    ///
    /// All slots are pre-allocated here so no heap allocation occurs after
    /// construction.
    ///
    /// # Panics
    ///
    /// Panics if `capacity > 64`.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(
            capacity <= 64,
            "SlotGroup capacity must be ≤ 64, got {capacity}"
        );
        let (signal, wake_sink) = BitSignal::new_pair();
        let slots = (0..capacity).map(|_| None).collect();
        let slot_wakers = (0..capacity).map(|_| None).collect();
        Self {
            signal,
            wake_sink,
            pending_bits: 0,
            occupied: 0,
            cached_waker: None,
            slots,
            slot_wakers,
            // Start at 63 so the first rotation lands on slot 0.
            last_index: 63,
        }
    }

    /// Number of occupied slots.
    #[inline]
    pub fn len(&self) -> usize {
        self.occupied.count_ones() as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.occupied == 0
    }

    /// Immutable access to slot `index`. Returns `None` if the slot is free.
    #[inline]
    pub fn get(&self, index: usize) -> Option<&S> {
        self.slots.get(index)?.as_ref()
    }

    /// Mutable access to slot `index`, returning a [`SlotGuard`] that
    /// automatically pokes the slot on drop.
    ///
    /// Returns `None` if the slot is free.
    #[inline]
    pub fn get_mut(&mut self, index: usize) -> Option<SlotGuard<'_, S>> {
        if index >= self.slots.len() || self.slots[index].is_none() {
            return None;
        }
        Some(SlotGuard {
            slot: self.slots[index].as_mut().unwrap(),
            signal: &self.signal,
            index: index as u8,
        })
    }

    /// Iterate over `(index, &S)` for all occupied slots.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &S)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|s| (i, s)))
    }

    /// Iterate over `(index, [`SlotGuard`])` for all occupied slots.
    ///
    /// Each guard automatically pokes its slot on drop.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (usize, SlotGuard<'_, S>)> + '_ {
        let signal = &self.signal;
        self.slots.iter_mut().enumerate().filter_map(move |(i, s)| {
            s.as_mut().map(|slot| {
                (
                    i,
                    SlotGuard {
                        slot,
                        signal,
                        index: i as u8,
                    },
                )
            })
        })
    }

    /// Pokes a slot directly. Private — external callers get automatic pokes
    /// via [`get_mut`](SlotGroup::get_mut) and [`iter_mut`](SlotGroup::iter_mut).
    #[inline]
    fn poke(&self, index: usize) {
        if index < 64 && (self.occupied >> index) & 1 == 1 {
            self.signal.notify(index as u8);
        }
    }

    /// Remove the stream at `index`, freeing the slot for reuse.
    ///
    /// Returns the evicted stream, or `None` if the slot was already free.
    pub fn remove(&mut self, index: usize) -> Option<S> {
        if index >= self.slots.len() {
            return None;
        }
        let stream = self.slots[index].take()?;
        self.occupied &= !(1u64 << index);
        self.pending_bits &= !(1u64 << index);
        Some(stream)
    }
}

impl<S: Stream + Unpin> SlotGroup<S> {
    /// Insert a stream into the next free slot, returning its stable index.
    ///
    /// The per-slot [`Waker`] is built here once per slot lifecycle and reused
    /// across all polls — waker allocation is a construction-time cost, not a
    /// hot-path cost.
    ///
    /// Marks the new slot as immediately ready so it is polled on the very first
    /// [`poll_next`](Stream::poll_next).
    ///
    /// # Panics
    ///
    /// Panics if all slots are occupied.
    pub fn insert(&mut self, stream: S) -> usize {
        let index = (!self.occupied).trailing_zeros() as usize;
        assert!(
            index < self.slots.len(),
            "SlotGroup is full (capacity {})",
            self.slots.len()
        );
        self.occupied |= 1u64 << index;
        self.slots[index] = Some(stream);
        // Always rebuild the waker on insert so that a reused slot never
        // inherits a waker identity from a previous occupant. Streams that
        // cache wakers via `will_wake` would otherwise silently skip
        // re-registration, leaving the slot dark after its first Pending.
        self.slot_wakers[index] = Some(make_slot_waker(self.signal.clone(), index as u8));
        // Mark the new slot as immediately ready so the first poll visits it.
        self.signal.notify(index as u8);
        index
    }
}

impl<S: Stream + Unpin> Stream for SlotGroup<S> {
    type Item = S::Item;

    /// Hot path: registers the outer waker, drains only ready slots, returns
    /// one item per call.
    ///
    /// ## Waker registration guarantee
    ///
    /// The outer waker is registered unconditionally before any slot is polled.
    /// This ensures that a notification arriving between `signal.take()` and the
    /// return of `Poll::Pending` is never lost: the signal will re-fire the
    /// wake_sink, which holds the freshly-registered waker.
    ///
    /// ## Atomic budget
    ///
    /// Exactly **one** `signal.take()` (atomic swap) per outer `poll_next`
    /// call, regardless of how many slots are ready. Remaining bits from the
    /// current batch are kept in `self.pending_bits` (a plain `u64` field) and
    /// consumed on subsequent calls with zero atomic operations.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // Always register the outer waker BEFORE taking signal bits.
        //
        // Ordering matters: if we took bits first and a new notification arrived
        // between take() and register(), the wake_sink would fire with a stale
        // (or absent) waker and the task would never be re-scheduled.
        //
        // `will_wake` is a cheap pointer comparison; `wake_sink.register` has an
        // unconditional atomic store even when the waker is identical, so we
        // gate it here.
        let waker = cx.waker();
        if !this
            .cached_waker
            .as_ref()
            .is_some_and(|w| w.will_wake(waker))
        {
            this.wake_sink.register(waker);
            this.cached_waker = Some(waker.clone());
        }

        // Merge newly-signalled bits — one atomic swap per outer poll call.
        // AND with occupied so evicted-but-re-signalled slots are ignored.
        this.pending_bits |= this.signal.take() & this.occupied;

        if this.pending_bits == 0 {
            return Poll::Pending;
        }

        // Rotate pending_bits so that the slot after last_index is checked
        // first, giving fair round-robin service across all ready slots.
        let rotation = (this.last_index.wrapping_add(1) & 63) as u32;
        let rotated = this.pending_bits.rotate_right(rotation);

        // Fast path: find the first ready slot in rotation order.
        if rotated != 0 {
            let i_rotated = rotated.trailing_zeros();
            let i = (i_rotated.wrapping_add(rotation) & 63) as usize;

            if let Some(stream) = this.slots[i].as_mut() {
                let waker = this.slot_wakers[i].as_ref().expect("waker built on insert");
                let mut slot_cx = Context::from_waker(waker);

                match Pin::new(stream).poll_next(&mut slot_cx) {
                    Poll::Ready(Some(item)) => {
                        this.last_index = i as u8;
                        // Leave bit i set so we revisit this slot next call
                        // if no other bits are pending (prevents starvation
                        // of a single always-ready stream by always clearing).
                        //
                        // The bit will be naturally superseded once another
                        // slot signals and the rotation moves past it.
                        return Poll::Ready(Some(item));
                    }
                    Poll::Ready(None) => {
                        // Stream terminated — evict and fall through to slow path.
                        this.slots[i] = None;
                        this.occupied &= !(1u64 << i);
                        this.pending_bits &= !(1u64 << i);
                    }
                    Poll::Pending => {
                        // Stream has re-registered its waker via slot_cx.
                        // Clear the bit — it will be re-set when the stream
                        // calls the slot waker.
                        this.pending_bits &= !(1u64 << i);
                    }
                }
            } else {
                // Occupied bit was set but slot is None — shouldn't happen,
                // but clean up defensively.
                this.pending_bits &= !(1u64 << i);
                this.occupied &= !(1u64 << i);
            }
        }

        // Slow path: drain remaining pending_bits in LSB order.
        // Reached when the fast-path slot returned Pending/None and there
        // are more bits to service.
        while this.pending_bits != 0 {
            // Isolate and clear the lowest set bit.
            let lsb = this.pending_bits & this.pending_bits.wrapping_neg();
            let i = lsb.trailing_zeros() as usize;
            this.pending_bits &= !lsb;

            if let Some(stream) = this.slots[i].as_mut() {
                let waker = this.slot_wakers[i].as_ref().expect("waker built on insert");
                let mut slot_cx = Context::from_waker(waker);

                match Pin::new(stream).poll_next(&mut slot_cx) {
                    Poll::Ready(Some(item)) => {
                        this.last_index = i as u8;
                        // Re-set bit so this slot is revisited next call.
                        this.pending_bits |= lsb;
                        return Poll::Ready(Some(item));
                    }
                    Poll::Ready(None) => {
                        this.slots[i] = None;
                        this.occupied &= !(1u64 << i);
                        // bit already cleared above
                    }
                    Poll::Pending => {
                        // bit already cleared above; stream re-registered waker
                    }
                }
            } else {
                this.occupied &= !(1u64 << i);
            }
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::StreamExt;
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Context;

    // ── TestStream — a single enum that covers all test behaviours ────────────
    //
    // SlotGroup<S> requires a single concrete S. Rather than boxing, we unify
    // all test stream variants into one enum so every SlotGroup in the tests is
    // homogeneous: `SlotGroup<TestStream>`.

    enum TestStream {
        /// Emits `val` forever.
        AlwaysReady(i32),
        /// Emits `val` exactly `remaining` times, then terminates.
        Finite { val: i32, remaining: usize },
        /// Pending until `ready` is set to `true`, then emits `val` forever.
        Gated { val: i32, ready: bool },
        /// Always pending; increments `poll_count` on every poll.
        CountedPending(StdArc<AtomicUsize>),
    }

    impl Stream for TestStream {
        type Item = i32;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<i32>> {
            match self.get_mut() {
                TestStream::AlwaysReady(v) => Poll::Ready(Some(*v)),
                TestStream::Finite { val, remaining } => {
                    if *remaining == 0 {
                        Poll::Ready(None)
                    } else {
                        *remaining -= 1;
                        Poll::Ready(Some(*val))
                    }
                }
                TestStream::Gated { val, ready } => {
                    if *ready {
                        Poll::Ready(Some(*val))
                    } else {
                        Poll::Pending
                    }
                }
                TestStream::CountedPending(c) => {
                    c.fetch_add(1, Ordering::Relaxed);
                    Poll::Pending
                }
            }
        }
    }

    // Convenience constructors
    fn always(val: i32) -> TestStream {
        TestStream::AlwaysReady(val)
    }
    fn finite(val: i32, n: usize) -> TestStream {
        TestStream::Finite { val, remaining: n }
    }
    fn gated(val: i32) -> TestStream {
        TestStream::Gated { val, ready: false }
    }
    fn counted_pending(c: &StdArc<AtomicUsize>) -> TestStream {
        TestStream::CountedPending(c.clone())
    }

    // Helper: open the gate on a Gated slot. The SlotGuard auto-pokes on drop.
    fn open_gate(g: &mut SlotGroup<TestStream>, idx: usize) {
        if let Some(mut guard) = g.get_mut(idx) {
            if let TestStream::Gated { ready, .. } = &mut *guard {
                *ready = true;
            }
        } // guard drops here → automatic poke
    }

    // ── Basic functionality ───────────────────────────────────────────────────

    #[tokio::test]
    async fn single_stream_emits() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(always(42));
        assert_eq!(g.next().await, Some(42));
    }

    #[tokio::test]
    async fn multiple_streams_all_emit() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(finite(1, 1));
        g.insert(finite(2, 1));
        g.insert(finite(3, 1));

        let mut results = vec![
            g.next().await.unwrap(),
            g.next().await.unwrap(),
            g.next().await.unwrap(),
        ];
        results.sort();
        assert_eq!(results, vec![1, 2, 3]);
    }

    #[test]
    fn empty_group_reports_empty() {
        let g: SlotGroup<TestStream> = SlotGroup::with_capacity(4);
        assert!(g.is_empty());
        assert_eq!(g.len(), 0);
    }

    // ── Round-robin fairness ──────────────────────────────────────────────────

    #[tokio::test]
    async fn round_robin_three_slots() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(always(1));
        g.insert(always(2));
        g.insert(always(3));

        assert_eq!(g.next().await, Some(1));
        assert_eq!(g.last_index, 0);

        assert_eq!(g.next().await, Some(2));
        assert_eq!(g.last_index, 1);

        assert_eq!(g.next().await, Some(3));
        assert_eq!(g.last_index, 2);

        // Wraps back to slot 0.
        assert_eq!(g.next().await, Some(1));
        assert_eq!(g.last_index, 0);
    }

    #[tokio::test]
    async fn round_robin_skips_pending_slots() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(always(1)); // slot 0 — always ready
        g.insert(gated(2)); // slot 1 — pending
        g.insert(always(3)); // slot 2 — always ready

        assert_eq!(g.next().await, Some(1));
        assert_eq!(g.next().await, Some(3));
        assert_eq!(g.next().await, Some(1));
    }

    // ── Poke / waker notification ─────────────────────────────────────────────

    #[tokio::test]
    async fn poke_wakes_pending_slot() {
        let mut g = SlotGroup::with_capacity(4);
        let idx = g.insert(gated(99));

        {
            let mut fut = g.next();
            tokio::select! {
                _ = &mut fut => panic!("should be pending"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
        }

        open_gate(&mut g, idx);
        // No manual poke needed — open_gate drops a SlotGuard which pokes automatically.

        assert_eq!(g.next().await, Some(99));
    }

    #[test]
    fn get_mut_on_unoccupied_slot_returns_none() {
        let mut g: SlotGroup<TestStream> = SlotGroup::with_capacity(4);
        // get_mut on a free slot must return None, not panic.
        assert!(g.get_mut(0).is_none());
        assert!(g.get_mut(63).is_none());
        assert!(g.get_mut(100).is_none());
    }

    // ── Stream termination (Ready(None)) ─────────────────────────────────────

    #[tokio::test]
    async fn terminated_stream_is_evicted() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(finite(7, 1));
        g.insert(always(8));

        let first = g.next().await.unwrap();

        // Drive once more so the terminated slot's eviction is processed.
        let _second = g.next().await.unwrap();
        assert_eq!(g.len(), 1);

        assert_eq!(g.next().await, Some(8));
        assert_eq!(first, 7);
    }

    #[tokio::test]
    async fn all_streams_terminate_evicts_all() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(finite(1, 1));
        g.insert(finite(2, 1));

        let a = g.next().await.unwrap();
        let b = g.next().await.unwrap();
        let mut got = vec![a, b];
        got.sort();
        assert_eq!(got, vec![1, 2]);

        // Flush evictions with manual polls.
        for _ in 0..5 {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let _ = Pin::new(&mut g).poll_next(&mut cx);
        }
        assert_eq!(g.len(), 0);
    }

    // ── Remove ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn remove_frees_slot() {
        let mut g = SlotGroup::with_capacity(4);
        let idx = g.insert(always(5));
        assert_eq!(g.len(), 1);

        assert!(g.remove(idx).is_some());
        assert_eq!(g.len(), 0);

        // Slot is free — insert should reuse it.
        let new_idx = g.insert(always(6));
        assert_eq!(new_idx, idx);
        assert_eq!(g.next().await, Some(6));
    }

    #[tokio::test]
    async fn remove_clears_pending_bits() {
        let mut g = SlotGroup::with_capacity(4);
        let idx = g.insert(always(5));
        // Touch via get_mut to trigger a poke, ensuring pending_bits is set.
        let _ = g.get_mut(idx); // guard drops immediately, poking the slot
        g.remove(idx);
        assert_eq!(g.pending_bits & (1u64 << idx), 0);
        assert_eq!(g.occupied & (1u64 << idx), 0);
    }

    #[test]
    fn remove_on_free_slot_returns_none() {
        let mut g: SlotGroup<TestStream> = SlotGroup::with_capacity(4);
        assert!(g.remove(0).is_none());
        assert!(g.remove(63).is_none());
    }

    // ── Slot reuse waker isolation ────────────────────────────────────────────

    #[tokio::test]
    async fn reused_slot_gets_fresh_waker() {
        let mut g = SlotGroup::with_capacity(4);

        let idx = g.insert(finite(1, 1));
        let _ = g.next().await; // drain + evict

        g.remove(idx); // no-op if already evicted

        let idx2 = g.insert(gated(42));
        assert_eq!(idx2, idx, "slot should be reused");

        open_gate(&mut g, idx2);
        // open_gate drops the SlotGuard which pokes automatically.

        assert_eq!(g.next().await, Some(42));
    }

    // ── Capacity limits ───────────────────────────────────────────────────────

    #[test]
    #[should_panic(expected = "SlotGroup capacity must be ≤ 64")]
    fn capacity_over_64_panics() {
        let _: SlotGroup<TestStream> = SlotGroup::with_capacity(65);
    }

    #[test]
    #[should_panic(expected = "SlotGroup is full")]
    fn insert_when_full_panics() {
        let mut g = SlotGroup::with_capacity(2);
        g.insert(always(1));
        g.insert(always(2));
        g.insert(always(3)); // should panic
    }

    #[tokio::test]
    async fn capacity_1_works() {
        let mut g = SlotGroup::with_capacity(1);
        g.insert(always(77));
        assert_eq!(g.next().await, Some(77));
        assert_eq!(g.next().await, Some(77));
    }

    // ── No missed wakeups ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn no_missed_wakeup_after_pending() {
        let mut g = SlotGroup::with_capacity(4);
        let idx = g.insert(gated(1));

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let _ = tx.send(());
        });

        let _ = rx.await;
        open_gate(&mut g, idx);
        // open_gate's SlotGuard drop auto-pokes — no manual poke needed.

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), g.next())
            .await
            .expect("timed out — wakeup was lost");

        assert_eq!(result, Some(1));
    }

    // ── Hot-path poll count ───────────────────────────────────────────────────

    #[tokio::test]
    async fn pending_slot_not_re_polled_without_notification() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let mut g = SlotGroup::with_capacity(4);

        g.insert(counted_pending(&counter)); // slot 0 — always pending
        g.insert(always(1)); // slot 1 — keeps group alive

        g.next().await;
        g.next().await;

        let polls = counter.load(Ordering::Relaxed);
        assert!(
            polls <= 2,
            "pending slot was polled {polls} times — should be ≤ 2"
        );
    }

    // ── SlotGuard auto-poke ───────────────────────────────────────────────────

    #[tokio::test]
    async fn guard_drop_pokes_without_explicit_call() {
        let mut g = SlotGroup::with_capacity(4);
        let idx = g.insert(gated(55));

        // Drain the insert-time poke so the slot goes dark.
        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let _ = Pin::new(&mut g).poll_next(&mut cx); // slot returns Pending, bit cleared
        }

        // Mutate via guard — no manual poke.
        {
            let mut guard = g.get_mut(idx).unwrap();
            if let TestStream::Gated { ready, .. } = &mut *guard {
                *ready = true;
            }
        } // guard drops here → automatic poke

        assert_eq!(g.next().await, Some(55));
    }

    #[tokio::test]
    async fn iter_mut_guard_pokes_each_slot() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(gated(1));
        g.insert(gated(2));

        // Drain insert-time pokes.
        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let _ = Pin::new(&mut g).poll_next(&mut cx);
            let _ = Pin::new(&mut g).poll_next(&mut cx);
        }

        // Open all gates via iter_mut — guards auto-poke on drop.
        for (_, mut guard) in g.iter_mut() {
            if let TestStream::Gated { ready, .. } = &mut *guard {
                *ready = true;
            }
        }

        let mut results = vec![g.next().await.unwrap(), g.next().await.unwrap()];
        results.sort();
        assert_eq!(results, vec![1, 2]);
    }

    #[test]
    fn iter_returns_occupied_slots_only() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(always(1));
        g.insert(always(2));
        assert_eq!(g.iter().count(), 2);
    }

    #[test]
    fn get_returns_none_for_free_slot() {
        let g: SlotGroup<TestStream> = SlotGroup::with_capacity(4);
        assert!(g.get(0).is_none());
        assert!(g.get(63).is_none());
        assert!(g.get(100).is_none());
    }

    #[test]
    fn get_mut_returns_some_for_occupied_slot() {
        let mut g = SlotGroup::with_capacity(4);
        let idx = g.insert(always(5));
        assert!(g.get_mut(idx).is_some());
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn noop_waker() -> Waker {
        use std::task::{RawWaker, RawWakerVTable};
        const VTABLE: RawWakerVTable =
            RawWakerVTable::new(|p| RawWaker::new(p, &VTABLE), |_| {}, |_| {}, |_| {});
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }
}

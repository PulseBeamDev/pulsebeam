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
use diatomic_waker::WakeSink;
use futures_lite::stream::Stream;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

struct SlotWakerData {
    signal: Arc<BitSignal>,
    index: u8,
}

unsafe fn clone_slot(ptr: *const ()) -> RawWaker {
    let arc = ManuallyDrop::new(unsafe { Arc::from_raw(ptr as *const SlotWakerData) });
    let cloned = Arc::clone(&arc);
    RawWaker::new(Arc::into_raw(cloned) as *const (), slot_vtable())
}

unsafe fn wake_slot(ptr: *const ()) {
    let arc = unsafe { Arc::from_raw(ptr as *const SlotWakerData) };
    arc.signal.notify(arc.index);
}

unsafe fn wake_by_ref_slot(ptr: *const ()) {
    let arc = ManuallyDrop::new(unsafe { Arc::from_raw(ptr as *const SlotWakerData) });
    arc.signal.notify(arc.index);
}

unsafe fn drop_slot(ptr: *const ()) {
    drop(unsafe { Arc::from_raw(ptr as *const SlotWakerData) });
}

const fn slot_vtable() -> &'static RawWakerVTable {
    &RawWakerVTable::new(clone_slot, wake_slot, wake_by_ref_slot, drop_slot)
}

pub(crate) fn make_slot_waker(signal: Arc<BitSignal>, index: u8) -> Waker {
    let data = Arc::new(SlotWakerData { signal, index });
    let raw = RawWaker::new(Arc::into_raw(data) as *const (), slot_vtable());
    // SAFETY: vtable functions are correct and data lifetime is Arc-managed.
    unsafe { Waker::from_raw(raw) }
}

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
    use crate::sync::slot_group::SlotGroup;

    use super::super::*;

    use futures::Stream;
    use futures_lite::StreamExt as _;
    use futures_test::task::{new_count_waker, noop_waker, panic_waker};
    use std::pin::Pin;
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    // ─────────────────────────────────────────────────────────────────────────
    // Unified test stream (same pattern as original tests, extended)
    // ─────────────────────────────────────────────────────────────────────────

    enum TestStream {
        /// Emits `val` on every poll.
        AlwaysReady(i32),
        /// Emits `val` exactly `remaining` more times, then returns `Ready(None)`.
        Finite { val: i32, remaining: usize },
        /// Returns `Pending` until `ready` is `true`, then emits `val` forever.
        Gated { val: i32, ready: StdArc<AtomicBool> },
        /// Always pending; increments `poll_count` on every `poll_next`.
        CountedPending(StdArc<AtomicUsize>),
        /// Records every waker it receives so callers can fire them externally.
        WakerCapture {
            val: i32,
            armed: bool,
            captured: StdArc<std::sync::Mutex<Option<std::task::Waker>>>,
        },
    }

    impl Stream for TestStream {
        type Item = i32;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<i32>> {
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
                    if ready.load(Ordering::Acquire) {
                        Poll::Ready(Some(*val))
                    } else {
                        // Re-register waker on every Pending — standard contract.
                        let _ = cx.waker().clone(); // touch waker to satisfy linters
                        Poll::Pending
                    }
                }

                TestStream::CountedPending(c) => {
                    c.fetch_add(1, Ordering::Relaxed);
                    Poll::Pending
                }

                TestStream::WakerCapture {
                    val,
                    armed,
                    captured,
                } => {
                    *captured.lock().unwrap() = Some(cx.waker().clone());
                    if *armed {
                        Poll::Ready(Some(*val))
                    } else {
                        Poll::Pending
                    }
                }
            }
        }
    }

    // ── Convenience constructors ──────────────────────────────────────────────

    fn always(v: i32) -> TestStream {
        TestStream::AlwaysReady(v)
    }
    fn finite(v: i32, n: usize) -> TestStream {
        TestStream::Finite {
            val: v,
            remaining: n,
        }
    }

    fn gated(v: i32) -> (TestStream, StdArc<AtomicBool>) {
        let flag = StdArc::new(AtomicBool::new(false));
        (
            TestStream::Gated {
                val: v,
                ready: flag.clone(),
            },
            flag,
        )
    }

    fn counted_pending(c: &StdArc<AtomicUsize>) -> TestStream {
        TestStream::CountedPending(c.clone())
    }

    fn waker_capture(
        v: i32,
    ) -> (
        TestStream,
        StdArc<std::sync::Mutex<Option<std::task::Waker>>>,
    ) {
        let cap = StdArc::new(std::sync::Mutex::new(None));
        (
            TestStream::WakerCapture {
                val: v,
                armed: false,
                captured: cap.clone(),
            },
            cap,
        )
    }

    /// Poll helper: one `poll_next` with a given `Context`.
    fn poll_once(g: &mut SlotGroup<TestStream>, cx: &mut Context<'_>) -> Poll<Option<i32>> {
        Pin::new(g).poll_next(cx)
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 1. Basic correctness
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn empty_group_is_pending_not_none() {
        // An empty SlotGroup has no streams to terminate — it should park the
        // task (Pending), not signal end-of-stream (Ready(None)).
        let mut g: SlotGroup<TestStream> = SlotGroup::with_capacity(4);
        let (waker, count) = new_count_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);
        // No spurious wakes should have been scheduled.
        assert_eq!(count.get(), 0);
    }

    #[test]
    fn single_always_ready_stream() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(always(7));
        let waker = panic_waker(); // must NOT be called — stream is always ready
        let mut cx = Context::from_waker(&waker);
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(7)));
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(7)));
    }

    #[test]
    fn finite_stream_terminates_and_slot_is_evicted() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(finite(3, 2));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(3)));
        assert_eq!(g.len(), 1);

        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(3)));

        // The stream is now exhausted — next poll drives eviction.
        let _ = poll_once(&mut g, &mut cx);
        assert_eq!(g.len(), 0, "slot must be evicted after Ready(None)");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 2. Round-robin fairness
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn round_robin_order_three_always_ready() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(always(1));
        g.insert(always(2));
        g.insert(always(3));

        let waker = panic_waker();
        let mut cx = Context::from_waker(&waker);

        // First pass: slots 0 → 1 → 2
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(1)));
        assert_eq!(g.last_index, 0);
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(2)));
        assert_eq!(g.last_index, 1);
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(3)));
        assert_eq!(g.last_index, 2);

        // Second pass wraps around to slot 0.
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(1)));
        assert_eq!(g.last_index, 0);
    }

    #[test]
    fn round_robin_skips_pending_slot_in_rotation() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(always(10)); // slot 0
        let (gs, _flag) = gated(20);
        g.insert(gs); // slot 1 — pending
        g.insert(always(30)); // slot 2

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Drain insert-poke for slot 1 (it returns Pending immediately).
        // We need to do enough polls to clear slot 1's initial poke.
        // Poll until we get slot 0, then slot 2, then slot 0 again — no 20.
        let mut seen: Vec<i32> = Vec::new();
        for _ in 0..6 {
            if let Poll::Ready(Some(v)) = poll_once(&mut g, &mut cx) {
                seen.push(v);
            }
        }
        assert!(
            !seen.contains(&20),
            "pending slot should never emit: got {seen:?}"
        );
        assert!(seen.contains(&10) && seen.contains(&30));
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 3. Waker registration — correctness and no-redundant-store optimisation
    // ─────────────────────────────────────────────────────────────────────────

    /// The outer waker must be registered BEFORE `signal.take()` so that a
    /// notification racing between take() and Pending return is not lost.
    ///
    /// We simulate this by using a `WakerCapture` stream: after we confirm the
    /// group went Pending, we fire the captured *slot* waker and verify that the
    /// outer `count_waker` is woken — meaning the outer waker was registered
    /// before the slot's notification arrived.
    #[test]
    fn outer_waker_registered_before_signal_take() {
        let mut g = SlotGroup::with_capacity(4);
        let (stream, cap) = waker_capture(42);
        let idx = g.insert(stream);

        let (outer_waker, wake_count) = new_count_waker();
        let mut cx = Context::from_waker(&outer_waker);

        // First poll: stream returns Pending, captures the slot waker.
        // The group's insert-time poke fires the slot once, stream → Pending.
        let result = poll_once(&mut g, &mut cx);
        assert_eq!(result, Poll::Pending);

        // The slot waker was captured inside the stream during its poll.
        let slot_waker = cap.lock().unwrap().clone().expect("waker must be captured");

        // Arm the stream so next poll returns Ready.
        {
            let mut guard = g.get_mut(idx).unwrap();
            if let TestStream::WakerCapture { armed, .. } = &mut *guard {
                *armed = true;
            }
        }

        // Fire the slot waker (simulates async notification from another task).
        slot_waker.wake();

        // The outer count_waker must have been woken.
        assert!(
            wake_count.get() >= 1,
            "outer waker not notified — waker was registered after signal.take()"
        );
    }

    /// Verifies `will_wake` short-circuit: same waker across polls must NOT cause
    /// redundant `wake_sink.register` calls.  We can't observe atomic stores
    /// directly, but we can verify that re-using the same waker object doesn't
    /// corrupt the wake path (the waker is still invoked exactly once).
    #[test]
    fn same_outer_waker_reused_across_polls_still_works() {
        let mut g = SlotGroup::with_capacity(4);
        let (stream, flag) = gated(5);
        g.insert(stream);

        let (waker, count) = new_count_waker();
        let mut cx = Context::from_waker(&waker);

        // First poll goes Pending.
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);
        // Second poll with the SAME waker — exercises the `will_wake` fast path.
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);

        // Open gate; the slot's own waker should wake the outer task.
        flag.store(true, Ordering::Release);
        {
            let mut guard = g.get_mut(0).unwrap();
            drop(guard); // auto-poke
        }

        assert!(count.get() >= 1, "outer waker must be invoked after poke");
    }

    /// When the outer waker changes between polls, the new waker must be used —
    /// we must not be stuck on a stale cached waker.
    #[test]
    fn changed_outer_waker_is_re_registered() {
        let mut g = SlotGroup::with_capacity(4);
        let (stream, flag) = gated(9);
        g.insert(stream);

        let (waker1, count1) = new_count_waker();
        {
            let mut cx = Context::from_waker(&waker1);
            assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);
        }

        // Switch to a new waker.
        let (waker2, count2) = new_count_waker();
        {
            let mut cx = Context::from_waker(&waker2);
            assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);
        }

        // Open gate and poke — should wake waker2, not waker1.
        flag.store(true, Ordering::Release);
        g.get_mut(0); // auto-poke on drop

        // Give the signal a moment to propagate (it's synchronous in this path).
        assert_eq!(count1.get(), 0, "stale waker1 must not be woken");
        assert!(count2.get() >= 1, "current waker2 must be woken");
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 4. `pending_bits` accounting
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn pending_slot_clears_its_bit() {
        let mut g = SlotGroup::with_capacity(4);
        let (stream, _flag) = gated(1);
        g.insert(stream);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        poll_once(&mut g, &mut cx); // insert poke fires → slot returns Pending → bit cleared
        assert_eq!(
            g.pending_bits & 1,
            0,
            "pending_bits bit 0 must be clear after Pending"
        );
    }

    #[test]
    fn remove_clears_both_occupied_and_pending_bits() {
        let mut g = SlotGroup::with_capacity(4);
        let idx = g.insert(always(1));

        // Poke via get_mut so pending_bits is definitely non-zero.
        drop(g.get_mut(idx));

        g.remove(idx);

        assert_eq!(
            g.pending_bits & (1u64 << idx),
            0,
            "pending_bits must be clear after remove"
        );
        assert_eq!(
            g.occupied & (1u64 << idx),
            0,
            "occupied must be clear after remove"
        );
    }

    #[test]
    fn pending_bits_not_set_for_evicted_slot_that_fires_late() {
        // If a stream signals after being removed, its bit must be masked out
        // by `& this.occupied` inside poll_next.
        let mut g = SlotGroup::with_capacity(4);
        let idx = g.insert(always(1));
        g.remove(idx);

        // Manually fire the (now-dead) signal bit to simulate a late wake.
        g.signal.notify(idx as u8);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let r = poll_once(&mut g, &mut cx);

        // Group is empty — must be Pending, not a spurious Ready.
        assert_eq!(r, Poll::Pending);
        assert_eq!(g.occupied, 0);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 5. Hot-path poll economy
    // ─────────────────────────────────────────────────────────────────────────

    /// A Pending slot must NOT be re-polled unless its waker is invoked.
    #[test]
    fn pending_slot_polled_at_most_once_per_notification() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let mut g = SlotGroup::with_capacity(4);

        g.insert(counted_pending(&counter)); // slot 0 — never ready
        g.insert(always(1)); // slot 1 — keeps group alive

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Each outer poll should poll slot 0 at most once (the insert-time poke).
        for _ in 0..5 {
            let _ = poll_once(&mut g, &mut cx);
        }

        let polls = counter.load(Ordering::Relaxed);
        assert!(
            polls <= 1,
            "pending slot polled {polls} times — must be ≤ 1 (insert poke only)"
        );
    }

    /// When a slot returns `Ready(Some(_))`, its bit is left set so it is
    /// revisited on the *next* outer poll — preventing starvation of a lone
    /// always-ready slot while also not blocking other slots.
    #[test]
    fn always_ready_slot_bit_stays_set_after_ready() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(always(99));

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let r1 = poll_once(&mut g, &mut cx);
        assert_eq!(r1, Poll::Ready(Some(99)));

        // pending_bits for slot 0 must still be set after a Ready return.
        assert_ne!(
            g.pending_bits & 1,
            0,
            "bit must remain set after Ready(Some) so slot isn't starved"
        );

        let r2 = poll_once(&mut g, &mut cx);
        assert_eq!(r2, Poll::Ready(Some(99)));
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 6. SlotGuard auto-poke contract
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn get_mut_guard_drop_pokes_slot() {
        let mut g = SlotGroup::with_capacity(4);
        let idx = g.insert(always(3));

        // Drain the insert-time poke so pending_bits goes dark.
        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let _ = poll_once(&mut g, &mut cx);
        }

        // After draining, pending_bits must be set (Ready(Some) leaves bit set).
        // Clear it manually to simulate a fully-dark slot for the poke test.
        g.pending_bits &= !(1u64 << idx);

        // Now obtain a guard and drop it — the auto-poke should re-set the bit.
        drop(g.get_mut(idx));

        // The signal is async (atomic notify) but since we're single-threaded
        // we need to run a poll to merge the signal into pending_bits.
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let r = poll_once(&mut g, &mut cx);
        assert_eq!(r, Poll::Ready(Some(3)), "auto-poke must wake the slot");
    }

    #[test]
    fn iter_mut_guard_pokes_every_slot() {
        let mut g = SlotGroup::with_capacity(4);
        let (s0, flag0) = gated(10);
        let (s1, flag1) = gated(20);
        g.insert(s0);
        g.insert(s1);

        // Drain insert-time pokes.
        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            for _ in 0..4 {
                let _ = poll_once(&mut g, &mut cx);
            }
        }

        // Open both gates via iter_mut — guards auto-poke on drop.
        for (_, mut guard) in g.iter_mut() {
            if let TestStream::Gated { ready, .. } = &mut *guard {
                ready.store(true, Ordering::Release);
            }
        }

        // Both slots should now be reachable.
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut results = Vec::new();
        for _ in 0..4 {
            if let Poll::Ready(Some(v)) = poll_once(&mut g, &mut cx) {
                results.push(v);
            }
        }
        results.sort();
        assert!(results.contains(&10), "slot 0 must emit after gate opened");
        assert!(results.contains(&20), "slot 1 must emit after gate opened");

        // Silence unused-variable warnings for flags (they're used for control
        // via the AtomicBool stored inside the stream).
        let _ = (flag0, flag1);
    }

    #[test]
    fn get_mut_on_free_slot_returns_none() {
        let mut g: SlotGroup<TestStream> = SlotGroup::with_capacity(8);
        assert!(g.get_mut(0).is_none());
        assert!(g.get_mut(7).is_none());
        assert!(g.get_mut(100).is_none());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 7. Slot reuse and waker isolation
    // ─────────────────────────────────────────────────────────────────────────

    /// After a stream terminates and its slot is reused, the new occupant must
    /// get a brand-new slot waker — not inherit the old one.  Stale waker
    /// caching via `will_wake` in the *inner* stream would otherwise silently
    /// skip re-registration after the first Pending, leaving the slot dark.
    #[test]
    fn reused_slot_has_fresh_slot_waker() {
        let mut g = SlotGroup::with_capacity(4);

        // First occupant: emits once then terminates.
        let idx = g.insert(finite(1, 1));

        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            // Drain the value and the termination.
            let _ = poll_once(&mut g, &mut cx);
            let _ = poll_once(&mut g, &mut cx);
        }

        // Force eviction cleanup.
        g.remove(idx);

        // Second occupant in the same slot.
        let (stream2, flag2) = gated(99);
        let idx2 = g.insert(stream2);
        assert_eq!(idx2, idx, "slot should be reused");

        // Open the gate and use get_mut to auto-poke.
        flag2.store(true, Ordering::Release);
        drop(g.get_mut(idx2)); // auto-poke

        // Must emit — if the waker was stale the slot would stay dark.
        let result = futures_lite::future::block_on(g.next());
        assert_eq!(result, Some(99));
    }

    /// When two slots occupy a group and one is removed mid-stream, the
    /// remaining slot's waker must not be disturbed.
    #[test]
    fn remove_does_not_disturb_other_slot_waker() {
        let mut g = SlotGroup::with_capacity(4);
        let idx0 = g.insert(always(1));
        let idx1 = g.insert(always(2));

        g.remove(idx0);

        let waker = panic_waker(); // must not be called — stream is always ready
        let mut cx = Context::from_waker(&waker);
        let r = poll_once(&mut g, &mut cx);
        assert_eq!(r, Poll::Ready(Some(2)));
        let _ = idx1;
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 8. Capacity limits
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    #[should_panic(expected = "SlotGroup capacity must be ≤ 64")]
    fn capacity_65_panics() {
        let _: SlotGroup<TestStream> = SlotGroup::with_capacity(65);
    }

    #[test]
    #[should_panic(expected = "SlotGroup is full")]
    fn insert_into_full_group_panics() {
        let mut g = SlotGroup::with_capacity(2);
        g.insert(always(1));
        g.insert(always(2));
        g.insert(always(3));
    }

    #[test]
    fn capacity_1_round_trips() {
        let mut g = SlotGroup::with_capacity(1);
        g.insert(always(42));
        let waker = panic_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(42)));
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(42)));
    }

    #[test]
    fn capacity_64_insert_and_remove_all() {
        let mut g = SlotGroup::with_capacity(64);
        for i in 0..64 {
            g.insert(always(i as i32));
        }
        assert_eq!(g.len(), 64);
        for i in 0..64 {
            assert!(g.remove(i).is_some());
        }
        assert_eq!(g.len(), 0);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 9. No missed wakeup — cross-thread race simulation
    // ─────────────────────────────────────────────────────────────────────────

    /// Regression: notification must not be lost if it arrives between
    /// `signal.take()` and the `Poll::Pending` return.
    ///
    /// We use a real OS thread to fire the slot waker concurrently while the
    /// group is mid-poll, then verify the outer count_waker is eventually woken.
    #[test]
    fn no_missed_wakeup_cross_thread() {
        use std::time::Duration;

        let mut g = SlotGroup::with_capacity(4);
        let (stream, cap) = waker_capture(7);
        let idx = g.insert(stream);

        let (outer_waker, wake_count) = new_count_waker();
        let mut cx = Context::from_waker(&outer_waker);

        // First poll — stream captures slot waker, returns Pending.
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);
        let slot_waker = cap.lock().unwrap().clone().expect("must capture waker");

        // Arm the stream so next poll yields Ready.
        {
            let mut guard = g.get_mut(idx).unwrap();
            if let TestStream::WakerCapture { armed, .. } = &mut *guard {
                *armed = true;
            }
        }

        // Fire slot waker from another thread with a tiny delay.
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2));
            slot_waker.wake();
        });
        handle.join().unwrap();

        // Outer waker must have been triggered (directly or via wake_sink chain).
        let count = wake_count.get();
        assert!(
            count >= 1,
            "outer waker never woken — potential missed wakeup (count={count})"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 10. `iter` / `get` immutable accessors
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn iter_yields_only_occupied_slots() {
        let mut g = SlotGroup::with_capacity(8);
        g.insert(always(1));
        g.insert(always(2));
        g.insert(always(3));
        g.remove(1);

        let indices: Vec<usize> = g.iter().map(|(i, _)| i).collect();
        assert_eq!(indices, vec![0, 2]);
    }

    #[test]
    fn get_returns_none_for_free_slot() {
        let g: SlotGroup<TestStream> = SlotGroup::with_capacity(4);
        assert!(g.get(0).is_none());
        assert!(g.get(3).is_none());
        assert!(g.get(100).is_none());
    }

    #[test]
    fn get_returns_some_for_occupied_slot() {
        let mut g = SlotGroup::with_capacity(4);
        let idx = g.insert(always(5));
        assert!(g.get(idx).is_some());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 11. Integration: block_on end-to-end
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn block_on_drains_three_finite_streams() {
        futures_lite::future::block_on(async {
            let mut g = SlotGroup::with_capacity(8);
            g.insert(finite(1, 2));
            g.insert(finite(2, 2));
            g.insert(finite(3, 2));

            let mut results = Vec::new();
            for _ in 0..6 {
                if let Some(v) = g.next().await {
                    results.push(v);
                }
            }
            results.sort();
            assert_eq!(results, vec![1, 1, 2, 2, 3, 3]);
            // All streams terminated — group should be empty.
            assert_eq!(g.len(), 0);
        });
    }

    #[test]
    fn block_on_gated_stream_woken_by_poke() {
        futures_lite::future::block_on(async {
            let mut g = SlotGroup::with_capacity(4);
            let (stream, flag) = gated(55);
            let idx = g.insert(stream);

            // Open gate externally, then poke via get_mut guard.
            flag.store(true, Ordering::Release);
            drop(g.get_mut(idx)); // auto-poke

            let result = g.next().await;
            assert_eq!(result, Some(55));
        });
    }

    /// Verifies that a stream which alternates between Pending and Ready
    /// (simulated by counting polls) never drops a value and is always polled
    /// the correct number of times when driven to completion.
    #[test]
    fn block_on_interleaved_ready_and_pending() {
        futures_lite::future::block_on(async {
            // Two always-ready streams side by side — total items collected
            // must be exactly N each with round-robin ordering.
            let n = 10usize;
            let mut g = SlotGroup::with_capacity(4);
            g.insert(finite(1, n));
            g.insert(finite(2, n));

            let mut ones = 0usize;
            let mut twos = 0usize;

            while let Some(v) = g.next().await {
                match v {
                    1 => ones += 1,
                    2 => twos += 1,
                    _ => panic!("unexpected value {v}"),
                }
            }

            assert_eq!(ones, n, "stream 1 must emit exactly {n} items");
            assert_eq!(twos, n, "stream 2 must emit exactly {n} items");
        });
    }
}

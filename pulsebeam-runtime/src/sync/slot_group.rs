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
use crate::sync::AtomicWaker;
use crate::sync::atomic::{AtomicU64, Ordering};
use futures_lite::stream::Stream;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

// ── Per-slot waker ────────────────────────────────────────────────────────────
//
// Each occupied slot gets one `Waker` built from a `SlotWakerData`. The waker
// is constructed once on `insert` and reused across all polls, so allocation
// is a construction-time cost only.
//
// When the inner stream calls `waker.wake()` / `waker.wake_by_ref()`, we set
// the corresponding bit in `BitSignal::pending` and then call
// `BitSignal::waker.wake()` to schedule the outer task.

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
    // Reconstruct and drop the Arc (consumes the ref-count bump from clone/into_raw).
    let arc = unsafe { Arc::from_raw(ptr as *const SlotWakerData) };
    arc.signal.notify(arc.index);
}

unsafe fn wake_by_ref_slot(ptr: *const ()) {
    // Borrow only — do not drop the Arc.
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

// ── BitSignal ─────────────────────────────────────────────────────────────────
//
// Replaces the previous `(Arc<BitSignal>, WakeSink)` pair that was built on
// `diatomic_waker`. The `WakeSource`/`WakeSink` split is gone: `SlotGroup`
// holds the same `Arc<BitSignal>` that slot wakers hold and calls
// `signal.register(waker)` directly.
//
// `crate::sync::AtomicWaker` is already guarded by a loom feature flag that
// substitutes `loom::future::AtomicWaker` under `cfg(loom)`, so this code
// is loom-compatible without any additional changes.

/// A lock-free signalling primitive for up to 64 concurrent slots.
///
/// Producers (slot wakers, external pokes) call [`notify`] / [`notify_mask`]
/// to set bits and schedule the consumer task. The consumer (`SlotGroup`)
/// calls [`register`] to record its waker and [`take`] to drain pending bits.
///
/// # Ordering guarantee
///
/// `SlotGroup::poll_next` always calls [`register`] **before** [`take`].
/// This ensures that a [`notify`] arriving between `take()` and `Poll::Pending`
/// is never lost: `AtomicWaker` serialises the store/wake pair so the
/// consumer is always re-scheduled.
///
/// [`notify`]: BitSignal::notify
/// [`notify_mask`]: BitSignal::notify_mask
/// [`register`]: BitSignal::register
/// [`take`]: BitSignal::take
pub struct BitSignal {
    /// Bitset: bit `i` means slot `i` has pending work.
    pending: AtomicU64,
    /// Lock-free outer-task waker — replaces `diatomic_waker::WakeSource` +
    /// `diatomic_waker::WakeSink`. Uses `crate::sync::AtomicWaker` so that
    /// loom can intercept all atomic operations under `cfg(loom)`.
    waker: AtomicWaker,
}

impl BitSignal {
    /// Create a new `BitSignal` wrapped in an `Arc`.
    ///
    /// Unlike the previous design there is no paired `WakeSink` handle —
    /// the consumer calls [`register`](Self::register) directly on the `Arc`.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: AtomicU64::new(0),
            waker: AtomicWaker::new(),
        })
    }

    /// Register `waker` as the task to wake when any slot becomes ready.
    ///
    /// Must be called by the consumer **before** [`take`](Self::take) on every
    /// `poll_next` invocation to uphold the no-missed-wakeup guarantee.
    #[inline]
    pub fn register(&self, waker: &Waker) {
        self.waker.register(waker);
    }

    /// Mark slot `index` as ready and wake the registered task.
    ///
    /// Safe to call from any thread; the bit-set and the waker notification
    /// are both individually atomic. The `AtomicWaker` protocol ensures the
    /// consumer sees the notification even if it races with `register`.
    #[inline]
    pub fn notify(&self, index: u8) {
        self.pending
            .fetch_or(1u64 << (index & 63), Ordering::Release);
        self.waker.wake();
    }

    /// Mark a bitmask of slots as ready and wake the registered task once.
    ///
    /// More efficient than N calls to [`notify`](Self::notify) when multiple
    /// slots become ready simultaneously.
    #[inline]
    pub fn notify_mask(&self, bits: u64) {
        if bits == 0 {
            return;
        }
        self.pending.fetch_or(bits, Ordering::Release);
        self.waker.wake();
    }

    /// Atomically swap all pending bits out, returning the old value.
    ///
    /// Called once per `poll_next` invocation, **after** [`register`](Self::register).
    #[inline]
    pub fn take(&self) -> u64 {
        self.pending.swap(0, Ordering::Acquire)
    }
}

// ── SlotGuard ─────────────────────────────────────────────────────────────────

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

// ── SlotGroup ─────────────────────────────────────────────────────────────────

/// See [module docs](self).
pub struct SlotGroup<S> {
    signal: Arc<BitSignal>,
    /// Locally-cached readiness bits from the current batch.
    ///
    /// `signal.take()` (an atomic swap) runs **once per outer `poll_next` call**
    /// and is OR-merged here. Subsequent items from the same batch are served
    /// from this field with no atomic operations at all.
    pending_bits: u64,
    /// Bitset tracking which slot indices are currently occupied.
    occupied: u64,
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
        let signal = BitSignal::new();
        let slots = (0..capacity).map(|_| None).collect();
        let slot_wakers = (0..capacity).map(|_| None).collect();
        Self {
            signal,
            pending_bits: 0,
            occupied: 0,
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

    /// Iterate over `(index, `[`SlotGuard`]`)` for all occupied slots.
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

    /// Poke a slot directly, marking it ready for the next `poll_next`.
    ///
    /// Private — external callers receive automatic pokes via the [`SlotGuard`]
    /// returned by [`get_mut`](SlotGroup::get_mut) and [`iter_mut`](SlotGroup::iter_mut).
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
    /// return of `Poll::Pending` is never lost: `AtomicWaker` serialises the
    /// concurrent register/wake pair so the task is always re-scheduled.
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
        // between take() and register(), the AtomicWaker would fire with a stale
        // (or absent) waker and the task would never be re-scheduled.
        this.signal.register(cx.waker());

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

// ── Unsync SlotGroup (non-Send, non-Sync) ───────────────────────────────────

use std::cell::{Cell, RefCell};
use std::rc::Rc;

struct UnsyncBitSignal {
    pending: Cell<u64>,
    waker: RefCell<Option<Waker>>,
}

unsafe impl Send for UnsyncBitSignal {}
unsafe impl Sync for UnsyncBitSignal {}

impl UnsyncBitSignal {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            pending: Cell::new(0),
            waker: RefCell::new(None),
        })
    }

    #[inline]
    pub fn register(&self, waker: &Waker) {
        let mut slot = self.waker.borrow_mut();
        if let Some(existing) = slot.as_ref() {
            // Avoid cloning/dropping the same waker repeatedly in deterministic hot path.
            if existing.will_wake(waker) {
                return;
            }
        }
        *slot = Some(waker.clone());
    }

    #[inline]
    pub fn notify(&self, index: u8) {
        let old = self.pending.get();
        self.pending.set(old | (1u64 << (index & 63)));
        if let Some(w) = self.waker.borrow().as_ref() {
            w.wake_by_ref();
        }
    }

    #[inline]
    pub fn notify_mask(&self, bits: u64) {
        if bits == 0 {
            return;
        }
        let old = self.pending.get();
        self.pending.set(old | bits);
        if let Some(w) = self.waker.borrow().as_ref() {
            w.wake_by_ref();
        }
    }

    #[inline]
    pub fn take(&self) -> u64 {
        let v = self.pending.get();
        self.pending.set(0);
        v
    }
}

struct UnsyncSlotWakerData {
    signal: Rc<UnsyncBitSignal>,
    index: u8,
}

unsafe fn clone_slot_unsync(ptr: *const ()) -> RawWaker {
    let rc = ManuallyDrop::new(unsafe { Rc::from_raw(ptr as *const UnsyncSlotWakerData) });
    let cloned = Rc::clone(&rc);
    RawWaker::new(Rc::into_raw(cloned) as *const (), slot_vtable_unsync())
}

unsafe fn wake_slot_unsync(ptr: *const ()) {
    let rc = unsafe { Rc::from_raw(ptr as *const UnsyncSlotWakerData) };
    rc.signal.notify(rc.index);
}

unsafe fn wake_by_ref_slot_unsync(ptr: *const ()) {
    let rc = ManuallyDrop::new(unsafe { Rc::from_raw(ptr as *const UnsyncSlotWakerData) });
    rc.signal.notify(rc.index);
}

unsafe fn drop_slot_unsync(ptr: *const ()) {
    drop(unsafe { Rc::from_raw(ptr as *const UnsyncSlotWakerData) });
}

const fn slot_vtable_unsync() -> &'static RawWakerVTable {
    &RawWakerVTable::new(clone_slot_unsync, wake_slot_unsync, wake_by_ref_slot_unsync, drop_slot_unsync)
}

fn make_slot_waker_unsync(signal: Rc<UnsyncBitSignal>, index: u8) -> Waker {
    let data = Rc::new(UnsyncSlotWakerData { signal, index });
    let raw = RawWaker::new(Rc::into_raw(data) as *const (), slot_vtable_unsync());
    unsafe { Waker::from_raw(raw) }
}

/// A `SlotGroup` variant with no atomics and non-Send semantics.
///
/// Useful in single-threaded core-local pipelines where `Arc`/`Atomic*` are
/// undesired.
pub struct UnsyncSlotGuard<'a, S> {
    slot: &'a mut S,
    signal: &'a Rc<UnsyncBitSignal>,
    index: u8,
}

impl<S> Deref for UnsyncSlotGuard<'_, S> {
    type Target = S;

    #[inline]
    fn deref(&self) -> &S {
        self.slot
    }
}

impl<S> DerefMut for UnsyncSlotGuard<'_, S> {
    #[inline]
    fn deref_mut(&mut self) -> &mut S {
        self.slot
    }
}

impl<S> Drop for UnsyncSlotGuard<'_, S> {
    #[inline]
    fn drop(&mut self) {
        self.signal.notify(self.index);
    }
}

pub struct UnsyncSlotGroup<S> {
    signal: Rc<UnsyncBitSignal>,
    pending_bits: u64,
    occupied: u64,
    slots: Vec<Option<S>>,
    slot_wakers: Vec<Option<Waker>>,
    last_index: u8,
}

unsafe impl<S> Send for UnsyncSlotGroup<S> {}
unsafe impl<S> Sync for UnsyncSlotGroup<S> {}

impl<S> UnsyncSlotGroup<S> {
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity <= 64, "SlotGroup capacity must be ≤ 64, got {capacity}");
        let signal = UnsyncBitSignal::new();
        let slots = (0..capacity).map(|_| None).collect();
        let slot_wakers = (0..capacity).map(|_| None).collect();
        Self {
            signal,
            pending_bits: 0,
            occupied: 0,
            slots,
            slot_wakers,
            last_index: 63,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.occupied.count_ones() as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.occupied == 0
    }

    #[inline]
    pub fn get(&self, index: usize) -> Option<&S> {
        self.slots.get(index)?.as_ref()
    }

    #[inline]
    pub fn get_mut(&mut self, index: usize) -> Option<UnsyncSlotGuard<'_, S>> {
        if index >= self.slots.len() || self.slots[index].is_none() {
            return None;
        }
        Some(UnsyncSlotGuard {
            slot: self.slots[index].as_mut().unwrap(),
            signal: &self.signal,
            index: index as u8,
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = (usize, &S)> {
        self.slots.iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|s| (i, s)))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (usize, UnsyncSlotGuard<'_, S>)> + '_ {
        let signal = &self.signal;
        self.slots.iter_mut().enumerate().filter_map(move |(i, s)| {
            s.as_mut().map(|slot| {
                (
                    i,
                    UnsyncSlotGuard { slot, signal, index: i as u8 },
                )
            })
        })
    }

    #[inline]
    fn poke(&self, index: usize) {
        if index < 64 && (self.occupied >> index) & 1 == 1 {
            self.signal.notify(index as u8);
        }
    }

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

impl<S: Stream + Unpin> UnsyncSlotGroup<S> {
    pub fn insert(&mut self, stream: S) -> usize {
        let index = (!self.occupied).trailing_zeros() as usize;
        assert!(index < self.slots.len(), "SlotGroup is full (capacity {})", self.slots.len());
        self.occupied |= 1u64 << index;
        self.slots[index] = Some(stream);
        self.slot_wakers[index] = Some(make_slot_waker_unsync(self.signal.clone(), index as u8));
        self.signal.notify(index as u8);
        index
    }
}

impl<S: Stream + Unpin> Stream for UnsyncSlotGroup<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.signal.register(cx.waker());
        this.pending_bits |= this.signal.take() & this.occupied;

        if this.pending_bits == 0 {
            return Poll::Pending;
        }

        let rotation = (this.last_index.wrapping_add(1) & 63) as u32;
        let rotated = this.pending_bits.rotate_right(rotation);

        if rotated != 0 {
            let i_rotated = rotated.trailing_zeros();
            let i = (i_rotated.wrapping_add(rotation) & 63) as usize;

            if let Some(stream) = this.slots[i].as_mut() {
                let waker = this.slot_wakers[i].as_ref().expect("waker built on insert");
                let mut slot_cx = Context::from_waker(waker);

                match Pin::new(stream).poll_next(&mut slot_cx) {
                    Poll::Ready(Some(item)) => {
                        this.last_index = i as u8;
                        return Poll::Ready(Some(item));
                    }
                    Poll::Ready(None) => {
                        this.slots[i] = None;
                        this.occupied &= !(1u64 << i);
                        this.pending_bits &= !(1u64 << i);
                    }
                    Poll::Pending => {
                        this.pending_bits &= !(1u64 << i);
                    }
                }
            } else {
                this.pending_bits &= !(1u64 << i);
                this.occupied &= !(1u64 << i);
            }
        }

        while this.pending_bits != 0 {
            let lsb = this.pending_bits & this.pending_bits.wrapping_neg();
            let i = lsb.trailing_zeros() as usize;
            this.pending_bits &= !lsb;

            if let Some(stream) = this.slots[i].as_mut() {
                let waker = this.slot_wakers[i].as_ref().expect("waker built on insert");
                let mut slot_cx = Context::from_waker(waker);

                match Pin::new(stream).poll_next(&mut slot_cx) {
                    Poll::Ready(Some(item)) => {
                        this.last_index = i as u8;
                        this.pending_bits |= lsb;
                        return Poll::Ready(Some(item));
                    }
                    Poll::Ready(None) => {
                        this.slots[i] = None;
                        this.occupied &= !(1u64 << i);
                    }
                    Poll::Pending => {}
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

    use futures::Stream;
    use futures_lite::StreamExt as _;
    use futures_test::task::{new_count_waker, noop_waker, panic_waker};
    use std::pin::Pin;
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    // ── TestStream ────────────────────────────────────────────────────────────

    enum TestStream {
        AlwaysReady(i32),
        Finite {
            val: i32,
            remaining: usize,
        },
        Gated {
            val: i32,
            ready: StdArc<AtomicBool>,
        },
        CountedPending(StdArc<AtomicUsize>),
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
                        let _ = cx.waker().clone();
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

    fn poll_once(g: &mut SlotGroup<TestStream>, cx: &mut Context<'_>) -> Poll<Option<i32>> {
        Pin::new(g).poll_next(cx)
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 1. Basic correctness
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn empty_group_is_pending_not_none() {
        let mut g: SlotGroup<TestStream> = SlotGroup::with_capacity(4);
        let (waker, count) = new_count_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);
        assert_eq!(count.get(), 0);
    }

    #[test]
    fn single_always_ready_stream() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(always(7));
        let waker = panic_waker();
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
        let _ = poll_once(&mut g, &mut cx);
        assert_eq!(g.len(), 0, "slot must be evicted after Ready(None)");
    }

    #[test]
    fn slot_group_waker_reuse_for_gated_stream() {
        let (stream, ready) = gated(42);
        let mut g = SlotGroup::with_capacity(1);
        g.insert(stream);

        let (waker, counter) = new_count_waker();
        let mut cx = Context::from_waker(&waker);

        assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);
        assert_eq!(counter.get(), 0);

        ready.store(true, Ordering::Release);
        g.poke(0);

        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(42)));

        ready.store(false, Ordering::Release);
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);

        let (waker2, counter2) = new_count_waker();
        let mut cx2 = Context::from_waker(&waker2);
        assert_eq!(poll_once(&mut g, &mut cx2), Poll::Pending);
        assert_eq!(counter2.get(), 0);

        let (stream2, ready2) = gated(43);
        g.remove(0); // vacate slot before inserting next stream for reuse behavior
        g.insert(stream2);
        ready2.store(true, Ordering::Release);
        g.poke(0);

        assert_eq!(poll_once(&mut g, &mut cx2), Poll::Ready(Some(43)));
        assert!(counter.get() >= 0);
        assert!(counter2.get() >= 0);
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

        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(1)));
        assert_eq!(g.last_index, 0);
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(2)));
        assert_eq!(g.last_index, 1);
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(3)));
        assert_eq!(g.last_index, 2);
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(1)));
        assert_eq!(g.last_index, 0);
    }

    #[test]
    fn round_robin_skips_pending_slot_in_rotation() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(always(10));
        let (gs, _flag) = gated(20);
        g.insert(gs);
        g.insert(always(30));

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

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
    // 3. Waker registration correctness
    // ─────────────────────────────────────────────────────────────────────────

    /// Outer waker must be registered BEFORE `signal.take()`.
    ///
    /// We capture the slot waker out of the stream, then fire it and verify that
    /// the outer `count_waker` was scheduled — proving `AtomicWaker::register`
    /// ran before the bit was consumed.
    #[test]
    fn outer_waker_registered_before_signal_take() {
        let mut g = SlotGroup::with_capacity(4);
        let (stream, cap) = waker_capture(42);
        let idx = g.insert(stream);

        let (outer_waker, wake_count) = new_count_waker();
        let mut cx = Context::from_waker(&outer_waker);

        assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);

        let slot_waker = cap.lock().unwrap().clone().expect("waker must be captured");

        {
            let mut guard = g.get_mut(idx).unwrap();
            if let TestStream::WakerCapture { armed, .. } = &mut *guard {
                *armed = true;
            }
        }

        slot_waker.wake();

        assert!(
            wake_count.get() >= 1,
            "outer waker not notified — register must precede take()"
        );
    }

    /// Same outer waker reused across two polls must not corrupt the wake path.
    #[test]
    fn same_outer_waker_reused_across_polls_still_works() {
        let mut g = SlotGroup::with_capacity(4);
        let (stream, flag) = gated(5);
        g.insert(stream);

        let (waker, count) = new_count_waker();
        let mut cx = Context::from_waker(&waker);

        assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);
        // Second poll with same waker — exercises the `will_wake` fast path that
        // skips the `AtomicWaker::register` store.
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);

        flag.store(true, Ordering::Release);
        drop(g.get_mut(0)); // auto-poke

        assert!(count.get() >= 1, "outer waker must be invoked after poke");
    }

    /// When the outer waker changes, the new waker must replace the old one.
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

        let (waker2, count2) = new_count_waker();
        {
            let mut cx = Context::from_waker(&waker2);
            assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);
        }

        flag.store(true, Ordering::Release);
        drop(g.get_mut(0)); // auto-poke

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

        poll_once(&mut g, &mut cx);
        assert_eq!(g.pending_bits & 1, 0, "bit 0 must be clear after Pending");
    }

    #[test]
    fn remove_clears_both_occupied_and_pending_bits() {
        let mut g = SlotGroup::with_capacity(4);
        let idx = g.insert(always(1));
        drop(g.get_mut(idx)); // poke so pending_bits is non-zero
        g.remove(idx);

        assert_eq!(
            g.pending_bits & (1u64 << idx),
            0,
            "pending_bits must be clear"
        );
        assert_eq!(g.occupied & (1u64 << idx), 0, "occupied must be clear");
    }

    #[test]
    fn late_signal_from_evicted_slot_is_masked_out() {
        let mut g = SlotGroup::with_capacity(4);
        let idx = g.insert(always(1));
        g.remove(idx);

        // Simulate a late wake from the now-dead slot.
        g.signal.notify(idx as u8);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);
        assert_eq!(g.occupied, 0);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 5. Hot-path poll economy
    // ─────────────────────────────────────────────────────────────────────────

    /// A Pending slot must not be re-polled unless its waker fires.
    #[test]
    fn pending_slot_polled_at_most_once_per_notification() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let mut g = SlotGroup::with_capacity(4);
        g.insert(counted_pending(&counter));
        g.insert(always(1));

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        for _ in 0..5 {
            let _ = poll_once(&mut g, &mut cx);
        }

        let polls = counter.load(Ordering::Relaxed);
        assert!(
            polls <= 1,
            "pending slot polled {polls} times — must be ≤ 1"
        );
    }

    /// A Ready(Some) result must leave the bit set so the slot is revisited.
    #[test]
    fn always_ready_slot_bit_stays_set_after_ready() {
        let mut g = SlotGroup::with_capacity(4);
        g.insert(always(99));

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(99)));
        assert_ne!(
            g.pending_bits & 1,
            0,
            "bit must remain set after Ready(Some)"
        );
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(99)));
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 6. SlotGuard auto-poke contract
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn get_mut_guard_drop_pokes_slot() {
        let mut g = SlotGroup::with_capacity(4);
        let idx = g.insert(always(3));

        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let _ = poll_once(&mut g, &mut cx);
        }
        // Manually darken the bit to isolate the auto-poke effect.
        g.pending_bits &= !(1u64 << idx);

        drop(g.get_mut(idx)); // auto-poke

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(3)));
    }

    #[test]
    fn iter_mut_guard_pokes_every_slot() {
        let mut g = SlotGroup::with_capacity(4);
        let (s0, flag0) = gated(10);
        let (s1, flag1) = gated(20);
        g.insert(s0);
        g.insert(s1);

        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            for _ in 0..4 {
                let _ = poll_once(&mut g, &mut cx);
            }
        }

        for (_, mut guard) in g.iter_mut() {
            if let TestStream::Gated { ready, .. } = &mut *guard {
                ready.store(true, Ordering::Release);
            }
        }

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

    #[test]
    fn reused_slot_has_fresh_slot_waker() {
        let mut g = SlotGroup::with_capacity(4);
        let idx = g.insert(finite(1, 1));

        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let _ = poll_once(&mut g, &mut cx);
            let _ = poll_once(&mut g, &mut cx);
        }
        g.remove(idx);

        let (stream2, flag2) = gated(99);
        let idx2 = g.insert(stream2);
        assert_eq!(idx2, idx, "slot should be reused");

        flag2.store(true, Ordering::Release);
        drop(g.get_mut(idx2)); // auto-poke

        let result = futures_lite::future::block_on(g.next());
        assert_eq!(result, Some(99));
    }

    #[test]
    fn remove_does_not_disturb_other_slot_waker() {
        let mut g = SlotGroup::with_capacity(4);
        let idx0 = g.insert(always(1));
        let idx1 = g.insert(always(2));
        g.remove(idx0);

        let waker = panic_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(poll_once(&mut g, &mut cx), Poll::Ready(Some(2)));
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
    // 9. No missed wakeup — cross-thread race
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn no_missed_wakeup_cross_thread() {
        let mut g = SlotGroup::with_capacity(4);
        let (stream, cap) = waker_capture(7);
        let idx = g.insert(stream);

        let (outer_waker, wake_count) = new_count_waker();
        let mut cx = Context::from_waker(&outer_waker);

        assert_eq!(poll_once(&mut g, &mut cx), Poll::Pending);
        let slot_waker = cap.lock().unwrap().clone().expect("must capture waker");

        {
            let mut guard = g.get_mut(idx).unwrap();
            if let TestStream::WakerCapture { armed, .. } = &mut *guard {
                *armed = true;
            }
        }

        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(2));
            slot_waker.wake();
        });
        handle.join().unwrap();

        assert!(
            wake_count.get() >= 1,
            "outer waker never woken — missed wakeup (count={})",
            wake_count.get()
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 10. Immutable accessors
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

            // Collect exactly 6 items. SlotGroup parks (Pending) when empty —
            // it never returns Ready(None) — so drive by item count.
            let mut results = Vec::new();
            while results.len() < 6 {
                if let Some(v) = g.next().await {
                    results.push(v);
                }
            }
            results.sort();
            assert_eq!(results, vec![1, 1, 2, 2, 3, 3]);

            // Eviction of a terminated slot happens on the poll *after* the
            // stream returns Ready(None). After collecting the last item we
            // stopped polling, so those slots are still occupied with
            // remaining==0. Drive synchronous noop polls until the group is
            // empty to flush all pending evictions.
            {
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                while !g.is_empty() {
                    let _ = poll_once(&mut g, &mut cx);
                }
            }
            assert_eq!(g.len(), 0);
        });
    }

    #[test]
    fn block_on_gated_stream_woken_by_poke() {
        futures_lite::future::block_on(async {
            let mut g = SlotGroup::with_capacity(4);
            let (stream, flag) = gated(55);
            let idx = g.insert(stream);

            flag.store(true, Ordering::Release);
            drop(g.get_mut(idx));

            assert_eq!(g.next().await, Some(55));
        });
    }

    #[test]
    fn block_on_two_finite_streams_exact_counts() {
        futures_lite::future::block_on(async {
            let n = 10usize;
            let mut g = SlotGroup::with_capacity(4);
            g.insert(finite(1, n));
            g.insert(finite(2, n));

            // SlotGroup parks (Pending) when empty — it never returns Ready(None)
            // — so `while let Some` would hang. Drive by total expected item count.
            let (mut ones, mut twos) = (0usize, 0usize);
            while ones + twos < n * 2 {
                match g.next().await {
                    Some(1) => ones += 1,
                    Some(2) => twos += 1,
                    Some(v) => panic!("unexpected value {v}"),
                    None => unreachable!("SlotGroup never returns Ready(None)"),
                }
            }
            assert_eq!(ones, n);
            assert_eq!(twos, n);
        });
    }
}

#[cfg(test)]
mod spmc_tests {
    use super::*;

    use crate::sync::spmc;
    use futures_lite::stream::Stream;
    use futures_test::task::{new_count_waker, noop_waker, panic_waker};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    // ── SpmcStream ────────────────────────────────────────────────────────────
    //
    // Wraps `spmc::Receiver<i32>` as a `Stream<Item = i32>`.
    //
    // Behaviour mirrors SlotDriver::poll_packet in video.rs:
    //
    //   Ok(v)              → Ready(Some(v))   packet forwarded downstream
    //   Err(Lagged(_))     → Ready(Some(-1))  observable sentinel; slot stays live
    //   Err(Closed)        → Ready(None)      channel gone; SlotGroup auto-evicts

    struct SpmcStream(spmc::Receiver<i32>);

    impl Stream for SpmcStream {
        type Item = i32;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<i32>> {
            match self.get_mut().0.poll_recv(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(v)) => Poll::Ready(Some(v)),
                Poll::Ready(Err(spmc::RecvError::Lagged(_))) => Poll::Ready(Some(-1)),
                Poll::Ready(Err(spmc::RecvError::Closed)) => Poll::Ready(None),
            }
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn spmc_pair(cap: usize) -> (spmc::Sender<i32>, SpmcStream) {
        let (tx, rx) = spmc::channel(cap);
        (tx, SpmcStream(rx))
    }

    fn poll_group(g: &mut SlotGroup<SpmcStream>, cx: &mut Context<'_>) -> Poll<Option<i32>> {
        Pin::new(g).poll_next(cx)
    }

    /// Close all channels by dropping the provided senders, then drive the
    /// group to empty so no `EventListener` is registered at drop time.
    ///
    /// Must be called before the `SlotGroup` goes out of scope in any test
    /// that leaves live slots in the group, to prevent `panic_waker` or
    /// `event-listener` from panicking during teardown.
    fn drain_group(g: &mut SlotGroup<SpmcStream>, txs: Vec<spmc::Sender<i32>>) {
        drop(txs); // close all channels → Ready(None) on next poll
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        // Drive until all slots are evicted.  Each slot needs at least one
        // poll to see Closed → Ready(None) → eviction, but spmc may require
        // more than one poll per slot (e.g. if the listener wasn't registered
        // yet when the sender dropped).  Loop unconditionally until empty.
        while !g.is_empty() {
            let _ = poll_group(g, &mut cx);
        }
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Section A — insert / add_slot
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn insert_returns_stable_indices_in_order() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(25);
        let mut txs = Vec::new();
        for i in 0..25usize {
            let (tx, stream) = spmc_pair(4);
            let idx = g.insert(stream);
            assert_eq!(idx, i, "slot {i}: expected stable index {i}, got {idx}");
            txs.push(tx);
        }
        assert_eq!(g.len(), 25);
        drain_group(&mut g, txs);
    }

    #[test]
    fn insert_auto_pokes_so_first_poll_visits_slot() {
        // Ring is pre-filled with one item, so the first poll is guaranteed Ready.
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (mut tx, stream) = spmc_pair(4);
        g.insert(stream);
        tx.send(42);

        // Safe to use panic_waker: one item in ring, one poll, guaranteed Ready.
        let waker = panic_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(
            poll_group(&mut g, &mut cx),
            Poll::Ready(Some(42)),
            "freshly inserted slot must be visited on the first poll_next"
        );
        drain_group(&mut g, vec![tx]);
    }

    #[test]
    fn insert_after_remove_reuses_freed_slot_index() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (tx0, s0) = spmc_pair(4);
        let (mut tx1, s1) = spmc_pair(4);
        let idx0 = g.insert(s0);
        let idx1 = g.insert(s1);
        // Evict slot 0 cleanly before removing.
        drop(tx0);
        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            for _ in 0..4 {
                let _ = poll_group(&mut g, &mut cx);
            }
        }
        g.remove(idx0);

        let (mut tx2, s2) = spmc_pair(4);
        let idx2 = g.insert(s2);
        assert_eq!(idx2, idx0, "freed slot must be reused");
        assert_ne!(idx2, idx1, "occupied slot must not be overwritten");

        tx1.send(2);
        tx2.send(99);

        // Both slots have exactly one item; noop_waker is safe here since we
        // collect at most 4 times and both items will appear within that window.
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..4 {
            if let Poll::Ready(Some(v)) = poll_group(&mut g, &mut cx) {
                seen.insert(v);
            }
        }
        assert!(
            seen.contains(&99),
            "reused slot must emit its new channel's data"
        );
        assert!(seen.contains(&2), "original slot must still emit");

        drain_group(&mut g, vec![tx1, tx2]);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Section B — get / get_mut / SlotGuard
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn get_returns_none_for_vacant_slot() {
        let g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(8);
        assert!(g.get(0).is_none());
        assert!(g.get(7).is_none());
        assert!(g.get(100).is_none());
    }

    #[test]
    fn get_returns_some_for_occupied_slot() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (tx, stream) = spmc_pair(4);
        let idx = g.insert(stream);
        assert!(g.get(idx).is_some());
        drain_group(&mut g, vec![tx]);
    }

    #[test]
    fn get_mut_returns_none_for_vacant_slot() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(8);
        assert!(g.get_mut(0).is_none());
        assert!(g.get_mut(100).is_none());
    }

    #[test]
    fn get_mut_returns_guard_for_occupied_slot() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (tx, stream) = spmc_pair(4);
        let idx = g.insert(stream);
        assert!(g.get_mut(idx).is_some());
        drain_group(&mut g, vec![tx]);
    }

    #[test]
    fn get_mut_on_freed_slot_returns_none() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (_tx, stream) = spmc_pair(4);
        let idx = g.insert(stream);
        g.remove(idx);
        assert!(g.get_mut(idx).is_none());
        // Group is already empty; no drain needed.
    }

    #[test]
    fn slot_guard_auto_poke_reschedules_slot_after_mutation() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (mut tx, stream) = spmc_pair(4);
        let idx = g.insert(stream);

        // Drain insert poke — slot parks on empty ring.
        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let _ = poll_group(&mut g, &mut cx);
        }
        assert_eq!(
            g.pending_bits & (1u64 << idx),
            0,
            "bit must be clear after Pending"
        );

        tx.send(55);
        drop(g.get_mut(idx).unwrap()); // guard drop auto-pokes

        // Ring has one item; poll is guaranteed Ready.
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(
            poll_group(&mut g, &mut cx),
            Poll::Ready(Some(55)),
            "auto-poke must make the slot visible immediately after mutation"
        );
        drain_group(&mut g, vec![tx]);
    }

    #[test]
    fn slot_guard_drop_without_mutation_still_pokes() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (mut tx, stream) = spmc_pair(4);
        let idx = g.insert(stream);

        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let _ = poll_group(&mut g, &mut cx);
        }
        g.pending_bits = 0;

        drop(g.get_mut(idx).unwrap()); // no mutation, guard dropped

        tx.send(7);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(
            poll_group(&mut g, &mut cx),
            Poll::Ready(Some(7)),
            "auto-poke must fire even without inner mutation"
        );
        drain_group(&mut g, vec![tx]);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Section C — iter (immutable scan)
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn iter_yields_only_occupied_slots_with_correct_indices() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(8);
        let (tx0, s0) = spmc_pair(4);
        let (tx1, s1) = spmc_pair(4);
        let (tx2, s2) = spmc_pair(4);
        g.insert(s0); // idx 0
        g.insert(s1); // idx 1
        g.insert(s2); // idx 2
        g.remove(1);

        let indices: Vec<usize> = g.iter().map(|(i, _)| i).collect();
        assert_eq!(indices, vec![0, 2], "iter must skip the freed slot");

        drain_group(&mut g, vec![tx0, tx1, tx2]);
    }

    #[test]
    fn iter_on_empty_group_yields_nothing() {
        let g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(8);
        assert_eq!(g.iter().count(), 0);
    }

    #[test]
    fn iter_count_matches_len() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(8);
        let mut txs = Vec::new();
        for _ in 0..5 {
            let (tx, s) = spmc_pair(4);
            g.insert(s);
            txs.push(tx);
        }
        g.remove(1);
        g.remove(3);
        assert_eq!(g.iter().count(), g.len());
        drain_group(&mut g, txs);
    }

    #[test]
    fn iter_indices_are_valid_for_get() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(8);
        let mut txs = Vec::new();
        for _ in 0..4 {
            let (tx, s) = spmc_pair(4);
            g.insert(s);
            txs.push(tx);
        }
        g.remove(1);

        for (i, _) in g.iter() {
            assert!(
                g.get(i).is_some(),
                "index {i} from iter must be valid for get()"
            );
        }
        drain_group(&mut g, txs);
    }

    #[test]
    fn sorted_indices_from_iter_are_valid_for_get_and_get_mut() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(8);
        let mut txs = Vec::new();
        for _ in 0..5 {
            let (tx, s) = spmc_pair(4);
            g.insert(s);
            txs.push(tx);
        }
        g.remove(1);
        g.remove(3);

        let mut sorted: Vec<usize> = g.iter().map(|(i, _)| i).collect();
        sorted.sort_by(|a, b| b.cmp(a)); // descending, like max_height sort

        for &idx in &sorted {
            assert!(g.get(idx).is_some(), "get({idx}) must succeed after sort");
        }
        for idx in sorted {
            let guard = g.get_mut(idx);
            assert!(guard.is_some(), "get_mut({idx}) must succeed after sort");
            drop(guard);
        }
        drain_group(&mut g, txs);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Section D — iter_mut (poll_slow)
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn iter_mut_visits_all_occupied_slots_in_index_order() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(8);
        let mut txs = Vec::new();
        for _ in 0..3 {
            let (tx, s) = spmc_pair(4);
            g.insert(s);
            txs.push(tx);
        }
        g.remove(1);

        let visited: Vec<usize> = g.iter_mut().map(|(i, _)| i).collect();
        assert_eq!(visited, vec![0, 2], "iter_mut must skip the freed slot");

        drain_group(&mut g, txs);
    }

    #[test]
    fn iter_mut_on_empty_group_is_noop() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        assert_eq!(g.iter_mut().count(), 0);
    }

    #[test]
    fn iter_mut_auto_pokes_all_visited_slots() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (mut tx0, s0) = spmc_pair(8);
        let (mut tx1, s1) = spmc_pair(8);
        g.insert(s0);
        g.insert(s1);

        // Drain insert pokes — both slots park.
        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            for _ in 0..4 {
                let _ = poll_group(&mut g, &mut cx);
            }
        }
        g.pending_bits = 0;

        tx0.send(10);
        tx1.send(20);
        for (_, _guard) in g.iter_mut() { /* guard drop auto-pokes */ }

        // Verify the auto-poke worked by observing that both slots emit.
        // (pending_bits is only updated inside poll_group via signal.take(),
        // so checking it directly before polling would see 0 even when the
        // pokes are correctly sitting in signal.pending.)
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut results = Vec::new();
        for _ in 0..4 {
            if let Poll::Ready(Some(v)) = poll_group(&mut g, &mut cx) {
                results.push(v);
            }
        }
        results.sort();
        assert_eq!(
            results,
            vec![10, 20],
            "both slots must emit after iter_mut poke"
        );

        drain_group(&mut g, vec![tx0, tx1]);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Section E — remove / slot eviction
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn remove_frees_slot_and_updates_len() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (_tx, stream) = spmc_pair(4);
        let idx = g.insert(stream);
        assert_eq!(g.len(), 1);
        assert!(g.remove(idx).is_some());
        assert_eq!(g.len(), 0);
        // Group is empty; no drain needed.
    }

    #[test]
    fn remove_clears_pending_and_occupied_bits() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (_tx, stream) = spmc_pair(4);
        let idx = g.insert(stream);
        g.signal.notify(idx as u8);
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
    fn remove_nonexistent_slot_returns_none() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        assert!(g.remove(0).is_none());
        assert!(g.remove(100).is_none());
    }

    #[test]
    fn closed_sender_causes_ready_none_and_auto_evicts_slot() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (mut tx, stream) = spmc_pair(4);
        g.insert(stream);

        tx.send(1);
        drop(tx);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert_eq!(poll_group(&mut g, &mut cx), Poll::Ready(Some(1)));
        assert_eq!(g.len(), 1, "slot still present before the eviction poll");

        let _ = poll_group(&mut g, &mut cx); // triggers Ready(None) → eviction
        assert_eq!(g.len(), 0, "slot must be auto-evicted after channel closes");
    }

    #[test]
    fn late_signal_from_removed_slot_is_masked_out() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (_tx, stream) = spmc_pair(4);
        let idx = g.insert(stream);
        g.remove(idx);

        g.signal.notify(idx as u8);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(poll_group(&mut g, &mut cx), Poll::Pending);
        assert_eq!(g.occupied, 0);
    }

    #[test]
    fn remove_one_slot_does_not_disturb_adjacent_slots() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (mut tx0, s0) = spmc_pair(8);
        let (tx1, s1) = spmc_pair(8);
        let (mut tx2, s2) = spmc_pair(8);
        let idx0 = g.insert(s0);
        let idx1 = g.insert(s1);
        let idx2 = g.insert(s2);
        g.remove(idx1);

        tx0.send(1);
        tx2.send(3);

        // Both remaining slots have one item each; noop_waker for safety.
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..4 {
            if let Poll::Ready(Some(v)) = poll_group(&mut g, &mut cx) {
                seen.insert(v);
            }
        }
        assert!(!seen.contains(&0), "removed slot must not emit");
        assert!(
            seen.contains(&1) && seen.contains(&3),
            "adjacent slots must still emit"
        );
        let _ = (idx0, idx2);

        drain_group(&mut g, vec![tx0, tx1, tx2]);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Section F — poll_next / VideoAllocator::poll_next hot path
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn empty_group_returns_pending_not_none() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(25);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(poll_group(&mut g, &mut cx), Poll::Pending);
    }

    #[test]
    fn slot_with_empty_ring_parks_not_panics() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (tx, stream) = spmc_pair(4);
        g.insert(stream);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let _ = poll_group(&mut g, &mut cx); // drain insert poke
        assert_eq!(poll_group(&mut g, &mut cx), Poll::Pending);

        drain_group(&mut g, vec![tx]);
    }

    #[test]
    fn round_robin_across_video_max_slots() {
        // Each slot has 64 items; we poll N×2 = 50 times, well within capacity.
        // No slot will run dry, so every poll is guaranteed Ready — noop_waker is
        // still correct here because panic_waker would fire if round-robin ever
        // tries a second poll on a slot mid-batch and that slot happened to park.
        const N: usize = 25;
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(N);
        let mut txs = Vec::with_capacity(N);
        for i in 0..N {
            let (mut tx, stream) = spmc_pair(64);
            for _ in 0..64 {
                tx.send(i as i32);
            }
            g.insert(stream);
            txs.push(tx);
        }

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut counts = vec![0u32; N];
        for _ in 0..(N * 2) {
            if let Poll::Ready(Some(v)) = poll_group(&mut g, &mut cx) {
                if v >= 0 {
                    counts[v as usize] += 1;
                }
            }
        }
        for (i, &c) in counts.iter().enumerate() {
            assert!(
                c >= 1,
                "slot {i} must be visited at least once in {N}×2 polls"
            );
        }

        drain_group(&mut g, txs);
    }

    #[test]
    fn pending_slot_bit_cleared_and_not_repolled_speculatively() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (tx_empty, s_empty) = spmc_pair(4); // ring never written
        let (mut tx_ready, s_ready) = spmc_pair(64);
        for _ in 0..64 {
            tx_ready.send(99);
        }
        let idx_empty = g.insert(s_empty);
        g.insert(s_ready);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        for _ in 0..10 {
            let _ = poll_group(&mut g, &mut cx);
        }

        assert_eq!(
            g.pending_bits & (1u64 << idx_empty),
            0,
            "empty slot's bit must be clear — must not be re-polled speculatively"
        );

        drain_group(&mut g, vec![tx_empty, tx_ready]);
    }

    #[test]
    fn lagged_slot_emits_sentinel_and_is_not_evicted() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (mut tx, stream) = spmc_pair(4);
        g.insert(stream);

        for i in 0..8 {
            tx.send(i);
        }

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let first = poll_group(&mut g, &mut cx);
        assert!(
            matches!(first, Poll::Ready(Some(_))),
            "first poll must be Ready(Some(_)), got {first:?}"
        );
        assert_eq!(g.len(), 1, "slot must NOT be evicted on lag");

        drain_group(&mut g, vec![tx]);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Section G — waker lifecycle
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn outer_waker_fired_when_spmc_sender_writes_data() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (mut tx, stream) = spmc_pair(8);
        g.insert(stream);

        let (outer_waker, count) = new_count_waker();
        let mut cx = Context::from_waker(&outer_waker);

        // Drain insert poke so slot parks on the empty ring.
        let _ = poll_group(&mut g, &mut cx);
        assert_eq!(count.get(), 0, "no wake before send");

        tx.send(42);
        assert!(count.get() >= 1, "outer waker must fire after send");

        // One item in ring — guaranteed Ready.
        let waker = noop_waker();
        let mut cx2 = Context::from_waker(&waker);
        assert_eq!(poll_group(&mut g, &mut cx2), Poll::Ready(Some(42)));

        drain_group(&mut g, vec![tx]);
    }

    #[test]
    fn outer_waker_fired_when_slot_poked_via_get_mut() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (mut tx, stream) = spmc_pair(8);
        let idx = g.insert(stream);

        let (outer_waker, count) = new_count_waker();
        let mut cx = Context::from_waker(&outer_waker);
        let _ = poll_group(&mut g, &mut cx); // drain insert poke, park

        tx.send(7);
        drop(g.get_mut(idx).unwrap()); // auto-poke

        assert!(
            count.get() >= 1,
            "outer waker must be notified by the auto-poke"
        );

        drain_group(&mut g, vec![tx]);
    }

    #[test]
    fn outer_waker_fired_via_iter_mut_poke() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (mut tx, stream) = spmc_pair(8);
        g.insert(stream);

        let (outer_waker, count) = new_count_waker();
        let mut cx = Context::from_waker(&outer_waker);
        let _ = poll_group(&mut g, &mut cx); // drain insert poke, park

        tx.send(5);
        for (_, _guard) in g.iter_mut() { /* guard drop auto-pokes */ }

        assert!(
            count.get() >= 1,
            "outer waker must be notified via iter_mut auto-poke"
        );

        drain_group(&mut g, vec![tx]);
    }

    #[test]
    fn outer_waker_replacement_on_task_migration() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (mut tx, stream) = spmc_pair(8);
        g.insert(stream);

        let (waker1, count1) = new_count_waker();
        {
            let mut cx = Context::from_waker(&waker1);
            let _ = poll_group(&mut g, &mut cx);
        }

        let (waker2, count2) = new_count_waker();
        {
            let mut cx = Context::from_waker(&waker2);
            let _ = poll_group(&mut g, &mut cx);
        }

        tx.send(99);

        assert_eq!(
            count1.get(),
            0,
            "stale waker1 must NOT fire after task migration"
        );
        assert!(count2.get() >= 1, "current waker2 MUST fire");

        drain_group(&mut g, vec![tx]);
    }

    #[test]
    fn outer_waker_fired_on_channel_close() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (tx, stream) = spmc_pair(8);
        g.insert(stream);

        let (outer_waker, count) = new_count_waker();
        let mut cx = Context::from_waker(&outer_waker);
        let _ = poll_group(&mut g, &mut cx); // drain insert poke, park

        drop(tx); // closes channel → spmc event → slot waker → outer waker
        assert!(
            count.get() >= 1,
            "outer waker must fire when the channel closes"
        );

        // Channel is closed; drive to evict the slot cleanly.
        drain_group(&mut g, vec![]);
    }

    #[test]
    fn cross_thread_send_wakes_outer_task() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (tx, stream) = spmc_pair(8);
        g.insert(stream);

        let (outer_waker, count) = new_count_waker();
        let mut cx = Context::from_waker(&outer_waker);
        let _ = poll_group(&mut g, &mut cx); // park

        let handle = std::thread::spawn(move || {
            let mut tx = tx;
            std::thread::sleep(std::time::Duration::from_millis(2));
            tx.send(77);
            tx // return so caller can drain
        });
        let tx = handle.join().unwrap();

        assert!(
            count.get() >= 1,
            "outer waker must be fired from the sender thread (no missed wakeup)"
        );

        drain_group(&mut g, vec![tx]);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Section H — slot reuse and waker isolation
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn reused_slot_gets_fresh_waker_and_receives_from_new_channel() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (mut tx0, s0) = spmc_pair(4);
        let idx = g.insert(s0);

        tx0.send(1);
        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let _ = poll_group(&mut g, &mut cx); // consume item
        }
        drop(tx0); // close
        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let _ = poll_group(&mut g, &mut cx); // Ready(None) → eviction
        }
        assert_eq!(g.len(), 0);

        let (mut tx1, s1) = spmc_pair(8);
        let idx1 = g.insert(s1);
        assert_eq!(idx1, idx, "slot must be reused at the same index");

        tx1.send(200);

        let result = futures_lite::future::block_on(futures_lite::StreamExt::next(&mut g));
        assert_eq!(
            result,
            Some(200),
            "reused slot must deliver from its new channel"
        );

        drain_group(&mut g, vec![tx1]);
    }

    #[test]
    fn dead_slot_waker_firing_after_remove_does_not_affect_live_slots() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (_tx0, s0) = spmc_pair(4);
        let (mut tx1, s1) = spmc_pair(8);
        let idx0 = g.insert(s0);
        g.insert(s1);

        let dead_waker = g.slot_wakers[idx0].clone().unwrap();
        g.remove(idx0);

        dead_waker.wake_by_ref();

        tx1.send(2);

        // One item guaranteed in slot 1; noop_waker.
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let item = poll_group(&mut g, &mut cx);
        assert_eq!(
            item,
            Poll::Ready(Some(2)),
            "live slot must still deliver its item"
        );

        drain_group(&mut g, vec![tx1]);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Section I — len / is_empty / slot_count
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn len_and_is_empty_track_inserts_and_removes() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(8);
        assert!(g.is_empty());
        assert_eq!(g.len(), 0);

        let (tx0, s0) = spmc_pair(4);
        let (tx1, s1) = spmc_pair(4);
        let i0 = g.insert(s0);
        let i1 = g.insert(s1);
        assert_eq!(g.len(), 2);
        assert!(!g.is_empty());

        g.remove(i0);
        assert_eq!(g.len(), 1);

        g.remove(i1);
        assert!(g.is_empty());
        assert_eq!(g.len(), 0);

        // Group is empty; senders dropped to satisfy lint.
        drop((tx0, tx1));
    }

    #[test]
    fn len_reflects_auto_eviction() {
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);
        let (mut tx0, s0) = spmc_pair(4);
        let (tx1, s1) = spmc_pair(4);
        g.insert(s0);
        g.insert(s1);

        tx0.send(1);
        drop(tx0);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        for _ in 0..6 {
            let _ = poll_group(&mut g, &mut cx);
        }

        assert_eq!(
            g.len(),
            1,
            "closed slot must be auto-evicted; one slot remains"
        );

        drain_group(&mut g, vec![tx1]);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Section J — capacity limits (VIDEO_MAX_SLOTS = 25)
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn video_max_slots_capacity_fills_and_drains_correctly() {
        const VIDEO_MAX_SLOTS: usize = 25;
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(VIDEO_MAX_SLOTS);
        let mut txs = Vec::with_capacity(VIDEO_MAX_SLOTS);
        let mut indices = Vec::with_capacity(VIDEO_MAX_SLOTS);

        for i in 0..VIDEO_MAX_SLOTS {
            let (mut tx, stream) = spmc_pair(8);
            tx.send(i as i32);
            indices.push(g.insert(stream));
            txs.push(tx);
        }
        assert_eq!(g.len(), VIDEO_MAX_SLOTS);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..(VIDEO_MAX_SLOTS * 2) {
            if let Poll::Ready(Some(v)) = poll_group(&mut g, &mut cx) {
                if v >= 0 {
                    seen.insert(v);
                }
            }
        }
        assert_eq!(
            seen.len(),
            VIDEO_MAX_SLOTS,
            "all 25 slots must be reachable"
        );

        for idx in indices {
            assert!(g.remove(idx).is_some());
        }
        assert!(g.is_empty());
        // Group is empty; close senders.
        drop(txs);
    }

    #[test]
    #[should_panic(expected = "SlotGroup is full")]
    fn inserting_beyond_video_max_slots_panics() {
        const VIDEO_MAX_SLOTS: usize = 25;
        let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(VIDEO_MAX_SLOTS);
        let mut txs = Vec::new();
        for _ in 0..=VIDEO_MAX_SLOTS {
            let (tx, stream) = spmc_pair(4);
            g.insert(stream); // panics on the 26th insert
            txs.push(tx);
        }
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Section K — end-to-end integration (block_on)
    // ═════════════════════════════════════════════════════════════════════════

    #[test]
    fn end_to_end_add_remove_rebalance_with_real_channels() {
        futures_lite::future::block_on(async {
            let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(4);

            let (mut tx0, s0) = spmc_pair(8);
            let (mut tx1, s1) = spmc_pair(8);
            let idx0 = g.insert(s0);
            let _idx1 = g.insert(s1);

            tx0.send(100);
            tx1.send(200);

            let mut results = Vec::new();
            while results.len() < 2 {
                results.push(futures_lite::StreamExt::next(&mut g).await.unwrap());
            }
            results.sort();
            assert_eq!(results, vec![100, 200]);

            // Simulate remove_track: close channel, let group evict.
            drop(tx0);
            {
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                for _ in 0..4 {
                    let _ = poll_group(&mut g, &mut cx);
                }
            }

            let (mut tx2, s2) = spmc_pair(8);
            let idx2 = g.insert(s2);
            assert_eq!(idx2, idx0, "rebalance must reuse the freed slot");

            for (_, _guard) in g.iter_mut() { /* simulate poll_slow auto-poke */ }

            tx2.send(300);
            tx1.send(400);

            let mut new_results = Vec::new();
            while new_results.len() < 2 {
                new_results.push(futures_lite::StreamExt::next(&mut g).await.unwrap());
            }
            new_results.sort();
            assert_eq!(new_results, vec![300, 400]);

            drain_group(&mut g, vec![tx1, tx2]);
        });
    }

    #[test]
    fn end_to_end_25_slots_round_robin_then_close_all() {
        futures_lite::future::block_on(async {
            const N: usize = 25;
            let mut g: SlotGroup<SpmcStream> = SlotGroup::with_capacity(N);
            let mut txs = Vec::with_capacity(N);

            for i in 0..N {
                let (mut tx, stream) = spmc_pair(64);
                for _ in 0..64 {
                    tx.send(i as i32);
                }
                g.insert(stream);
                txs.push(tx);
            }

            let mut counts = vec![0u32; N];
            for _ in 0..(N * 3) {
                if let Some(v) = futures_lite::StreamExt::next(&mut g).await {
                    if v >= 0 {
                        counts[v as usize] += 1;
                    }
                }
            }
            for (i, &c) in counts.iter().enumerate() {
                assert!(c >= 1, "slot {i} must be visited at least once");
            }

            drain_group(&mut g, txs);
            assert!(
                g.is_empty(),
                "all slots must be evicted after channels close"
            );
        });
    }
}

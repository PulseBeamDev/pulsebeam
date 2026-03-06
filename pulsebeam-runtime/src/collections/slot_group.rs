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
//! [`TaskGroup`]: crate::sync::task_group::TaskGroup
//! [`get_mut`]: SlotGroup::get_mut
//! [`iter_mut`]: SlotGroup::iter_mut

use crate::sync::bit_signal::BitSignal;
use crate::sync::task_group::make_slot_waker;
use diatomic_waker::WakeSink;
use futures_lite::stream::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

/// See [module docs](self).
pub struct SlotGroup<S> {
    signal: Arc<BitSignal>,
    wake_sink: WakeSink,
    /// Locally-cached readiness bits from the current batch.
    ///
    /// `signal.take()` (an atomic swap) runs **once per outer `poll_next` call**
    /// and is OR-merged here.  Subsequent items from the same batch are served
    /// from this field with no atomic operations at all.
    pending_bits: u64,
    /// Bitset tracking which slot indices are currently occupied.
    occupied: u64,
    /// Inline storage — one element per slot, `None` for free/vacant slots.
    slots: Vec<Option<S>>,
    /// Pre-built per-slot wakers.  Created once on [`insert`](SlotGroup::insert),
    /// reused across all polls — waker allocation is a construction-time cost only.
    slot_wakers: Vec<Option<Waker>>,
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
            slots,
            slot_wakers,
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

    /// Mutable access to slot `index`. Returns `None` if the slot is free.
    #[inline]
    pub fn get_mut(&mut self, index: usize) -> Option<&mut S> {
        self.slots.get_mut(index)?.as_mut()
    }

    /// Iterate over `(index, &S)` for all occupied slots.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &S)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|s| (i, s)))
    }

    /// Iterate over `(index, &mut S)` for all occupied slots.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (usize, &mut S)> + '_ {
        self.slots
            .iter_mut()
            .enumerate()
            .filter_map(|(i, s)| s.as_mut().map(|s| (i, s)))
    }

    /// Mark slot `index` as immediately ready so it is visited on the next
    /// [`poll_next`](Stream::poll_next).
    ///
    /// **Must be called after any cold-path mutation** that transitions a slot
    /// into a state that needs to be polled (e.g. Idle/Paused → Resuming).
    /// Without a poke the slot's bit stays dark: the spmc channel waker is only
    /// registered the first time the slot is polled, so it can never self-notify.
    #[inline]
    pub fn poke(&self, index: usize) {
        // Goes through the shared signal so it's safe to call from `&self`.
        // The next poll_next merges it into pending_bits via signal.take().
        if index < 64 && (self.occupied >> index) & 1 == 1 {
            self.signal.notify(index as u8);
        }
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
        // Build the waker once; if a slot is vacated and later reused the waker
        // is already present.
        if self.slot_wakers[index].is_none() {
            self.slot_wakers[index] = Some(make_slot_waker(self.signal.clone(), index as u8));
        }
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
    /// ## Atomic budget
    ///
    /// Exactly **one** `signal.take()` (atomic swap) per outer `poll_next`
    /// call, regardless of how many slots are ready.  Remaining bits from the
    /// current batch are kept in `self.pending_bits` (a plain `u64` field) and
    /// consumed on subsequent calls with zero atomic operations.
    ///
    /// Compared to calling `notify_mask` on every item return (the previous
    /// approach), this eliminates N−1 atomic fetch_or + N−1 `WakeSource::notify`
    /// calls for a batch of N simultaneously-ready slots.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // Register the outer waker so any slot waker can reschedule this task.
        this.wake_sink.register(cx.waker());

        // Merge newly-signalled bits into the local cache — one atomic per call.
        this.pending_bits |= this.signal.take() & this.occupied;

        while this.pending_bits != 0 {
            let i = this.pending_bits.trailing_zeros() as usize;
            this.pending_bits &= this.pending_bits.wrapping_sub(1); // clear bit i

            if let Some(stream) = this.slots[i].as_mut() {
                let waker = this.slot_wakers[i].as_ref().expect("waker built on insert");
                let mut slot_cx = Context::from_waker(waker);

                match Pin::new(stream).poll_next(&mut slot_cx) {
                    Poll::Ready(Some(item)) => {
                        // Re-arm slot i for the next call — pure bit-or, no atomic.
                        // Remaining bits are already in pending_bits.
                        this.pending_bits |= 1u64 << i;
                        return Poll::Ready(Some(item));
                    }
                    Poll::Ready(None) => {
                        // Stream exhausted — vacate the slot.
                        this.slots[i] = None;
                        this.occupied &= !(1u64 << i);
                    }
                    Poll::Pending => {}
                }
            }
        }

        Poll::Pending
    }
}

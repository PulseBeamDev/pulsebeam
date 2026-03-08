use crate::sync::bit_signal::BitSignal;
use diatomic_waker::WakeSink;
use std::future::Future;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use crate::sync::Arc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

type BoxFuture<O> = Pin<Box<dyn Future<Output = O> + Send + 'static>>;

// ---------------------------------------------------------------------------
// Per-slot waker
//
// Each slot has its own `Waker` backed by `SlotWakerData`. When a future
// inside the group registers this waker (e.g. tokio::time::sleep, mpsc::recv)
// and later wakes it, the only effect is:
//
//   1. `signal.notify(index)`  ->  `pending |= 1 << index`  (one atomic OR)
//   2. `wake_src.notify()`     ->  wake the group Tokio task if parked
//
// This means `poll_group` only re-polls the exact slot(s) that became ready --
// not all N slots blindly.
//
// Without per-slot wakers, all participant futures would register the room
// task's Waker directly. Then signal.take() would always return 0 on wakeup
// because nothing called notify(i) -- the entire benefit is lost.
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// TaskGroup
// ---------------------------------------------------------------------------

/// An inline cooperative scheduler for up to 64 futures within a single Tokio task.
///
/// # How wakeups work
///
/// ```text
/// Tokio task polls TaskGroup::poll_group(cx)
///   -> wake_sink.register(cx.waker())     store room-task Waker in WakeSink (lock-free)
///   -> for each flagged slot i:
///       poll future with slot_wakers[i]  <- per-slot Waker, NOT cx.waker()
///
/// Later: gateway_rx fires for participant in slot 3
///   -> slot_wakers[3].wake()
///       -> signal.notify(3)               pending |= 1<<3  (one fetch_or)
///       -> wake_src.notify()              wakes room task via WakeSink (lock-free)
///
/// Room task wakes
///   -> poll_group: signal.take() == 0b1000
///   -> polls only slot 3
/// ```
///
/// Cost per forwarded packet to N participants: **O(1)** task reschedules.
pub struct TaskGroup<O: Send + 'static> {
    signal: Arc<BitSignal>,
    wake_sink: WakeSink,
    occupied: u64,
    slots: [Option<BoxFuture<O>>; 64],
    /// Pre-built per-slot wakers. Built once per slot lifecycle, reused across polls.
    slot_wakers: [Option<Waker>; 64],
}

// SAFETY: WakeSink is Send; Waker is Send; BoxFuture<O> requires O: Send.
unsafe impl<O: Send + 'static> Send for TaskGroup<O> {}

impl<O: Send + 'static> TaskGroup<O> {
    /// Creates a new group and its associated [`BitSignal`].
    pub fn new() -> (Self, Arc<BitSignal>) {
        let (signal, wake_sink) = BitSignal::new_pair();
        let group = Self {
            signal: signal.clone(),
            wake_sink,
            occupied: 0,
            slots: std::array::from_fn(|_| None),
            slot_wakers: std::array::from_fn(|_| None),
        };
        (group, signal)
    }

    /// Insert `task` into the next free slot.
    ///
    /// Per-slot wakers are built here once per slot lifecycle -- paid at
    /// participant join time, not on every poll.
    ///
    /// Returns `Some(slot_index)` or `None` when all 64 slots are occupied.
    pub fn try_push(&mut self, task: impl Future<Output = O> + Send + 'static) -> Option<u8> {
        let index = (!self.occupied).trailing_zeros() as usize;
        if index >= 64 {
            return None;
        }
        self.occupied |= 1u64 << index;
        self.slots[index] = Some(Box::pin(task));
        if self.slot_wakers[index].is_none() {
            self.slot_wakers[index] = Some(make_slot_waker(self.signal.clone(), index as u8));
        }
        self.signal.notify(index as u8);
        Some(index as u8)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.occupied == 0
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.occupied.count_ones() as usize
    }

    /// Poll all slots whose bit is set in `BitSignal`.
    ///
    /// Each flagged slot is polled with its own per-slot `Waker` so internal
    /// futures (timers, channels, SPMC receivers) notify the correct slot on
    /// wakeup -- not the entire group.
    pub fn poll_group(&mut self, cx: &mut Context<'_>) -> Poll<(u8, O)> {
        self.wake_sink.register(cx.waker());

        let mut bits = self.signal.take();
        bits &= self.occupied;

        while bits != 0 {
            let i = bits.trailing_zeros() as usize;
            bits &= bits.wrapping_sub(1);

            if let Some(task) = &mut self.slots[i] {
                let waker = self.slot_wakers[i].as_ref().expect("waker built in try_push");
                let mut slot_cx = Context::from_waker(waker);

                if let Poll::Ready(output) = task.as_mut().poll(&mut slot_cx) {
                    self.slots[i] = None;
                    self.occupied &= !(1u64 << i);
                    self.wake_sink.unregister();
                    return Poll::Ready((i as u8, output));
                }
            }
        }

        Poll::Pending
    }

    /// Future that resolves to `Some(output)` when any task completes,
    /// or `None` immediately if the group is empty.
    ///
    /// Mirrors `JoinSet::join_next` for drop-in use in `tokio::select!`.
    pub fn join_next(&mut self) -> JoinNext<'_, O> {
        JoinNext { group: self }
    }
}

/// Future returned by [`TaskGroup::join_next`].
pub struct JoinNext<'a, O: Send + 'static> {
    group: &'a mut TaskGroup<O>,
}

impl<O: Send + 'static> Future for JoinNext<'_, O> {
    type Output = Option<O>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<O>> {
        if self.group.is_empty() {
            return Poll::Ready(None);
        }
        match self.group.poll_group(cx) {
            Poll::Ready((_, output)) => Poll::Ready(Some(output)),
            Poll::Pending => Poll::Pending,
        }
    }
}

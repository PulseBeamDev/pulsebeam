use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Waker};

/// A signaling primitive for up to 64 concurrent participant slots.
///
/// Producers (spmc::Sender) call `notify(bits)` to mark which slots have new
/// data. The consumer (RoomShard) calls `register` + `take` to sleep and
/// drain only the ready slots — no O(N) list walk, no EventListener alloc.
pub struct BitSignal {
    /// Bitset: bit i set means participant slot i has pending downstream work.
    pub(crate) pending: AtomicU64,
    /// The shard task's waker. Cloned under lock; woken outside the lock so
    /// the scheduler is never called while holding a mutex.
    waker: Mutex<Option<Waker>>,
}

impl BitSignal {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Set `bits` and wake the shard. Safe to call from any thread.
    ///
    /// On the hot path this takes the mutex only to clone the waker, then
    /// releases it before scheduling the wake — preventing the tokio
    /// scheduler from being entered under a lock.
    #[inline]
    pub fn notify(&self, bits: u64) {
        self.pending.fetch_or(bits, Ordering::Release);
        let maybe_waker = self.waker.lock().unwrap().clone();
        if let Some(w) = maybe_waker {
            w.wake();
        }
    }

    /// Mark all 64 slots ready and wake. Used on broad events such as a
    /// room-wide track update where every participant must re-check.
    #[inline]
    pub fn notify_all(&self) {
        self.pending.store(u64::MAX, Ordering::Release);
        let maybe_waker = self.waker.lock().unwrap().clone();
        if let Some(w) = maybe_waker {
            w.wake();
        }
    }

    /// Register the shard task's waker. Only stores a new clone when the
    /// waker actually changes, avoiding redundant Arc allocations.
    pub fn register(&self, cx: &mut Context<'_>) {
        let mut lock = self.waker.lock().unwrap();
        if lock.as_ref().is_none_or(|w: &Waker| !w.will_wake(cx.waker())) {
            *lock = Some(cx.waker().clone());
        }
    }

    /// Atomically take all pending bits. Returns 0 when nothing is ready.
    #[inline]
    pub fn take(&self) -> u64 {
        self.pending.swap(0, Ordering::Acquire)
    }

    /// Non-destructive check — true if any bits are pending.
    #[inline]
    pub fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire) != 0
    }

    /// Wake the registered waker if one is stored. Clone under lock, wake
    /// outside — safe to call from any context without scheduler re-entrancy.
    #[inline]
    pub fn wake(&self) {
        let maybe_waker = self.waker.lock().unwrap().clone();
        if let Some(w) = maybe_waker {
            w.wake();
        }
    }
}
impl Default for BitSignal {
    fn default() -> Self {
        Self {
            pending: AtomicU64::new(0),
            waker: Mutex::new(None),
        }
    }
}

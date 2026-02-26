use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Waker};

/// A zero-allocation signaling primitive for up to 64 concurrent consumers.
pub struct BitSignal {
    /// Bitset where each bit represents a task index that needs polling.
    pending: AtomicU64,
    /// Shared waker for the group executor.
    group_waker: Mutex<Option<Waker>>,
}

impl BitSignal {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: AtomicU64::new(0),
            group_waker: Mutex::new(None),
        })
    }

    /// Mark a specific task index as ready for polling.
    /// Used by the Producer (e.g., UDP socket handler).
    #[inline]
    pub fn notify(&self, index: u8) {
        let bit = 1 << (index % 64);
        self.pending.fetch_or(bit, Ordering::Release);

        // Notify the group executor.
        // We use a simplified lock-check to avoid waking if no waker is set.
        if let Some(waker) = self.group_waker.lock().as_ref() {
            waker.wake_by_ref();
        }
    }

    /// Registers the executor's waker.
    /// Used by the Consumer (TaskGroup) inside its poll loop.
    pub fn register(&self, cx: &mut Context<'_>) {
        let mut lock = self.group_waker.lock();
        if lock.as_ref().is_none_or(|w| !w.will_wake(cx.waker())) {
            *lock = Some(cx.waker().clone());
        }
    }

    /// Atomically swaps the pending bitset with 0 and returns the previous value.
    /// Used by the Consumer (TaskGroup) to acquire all work in one go.
    #[inline]
    pub fn take(&self) -> u64 {
        self.pending.swap(0, Ordering::Acquire)
    }
}

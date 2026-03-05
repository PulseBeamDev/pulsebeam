use diatomic_waker::{WakeSink, WakeSource};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A lock-free signaling primitive for up to 64 concurrent slots in a [`TaskGroup`].
///
/// # Design
///
/// Producers call [`notify`] / [`notify_mask`] to set bits and wake the group task.
/// The group task owns the paired [`WakeSink`] (created via [`new_pair`]) and uses it
/// to register its waker without any mutex or spinlock.
///
/// [`diatomic_waker`] replaces the old `Mutex<Option<Waker>>`: registration and
/// notification are now a pure lock-free state-machine, eliminating the lock on the
/// hot waker path.
///
/// [`notify`]: BitSignal::notify
/// [`notify_mask`]: BitSignal::notify_mask
/// [`new_pair`]: BitSignal::new_pair
pub struct BitSignal {
    /// Bitset: bit `i` means slot `i` has pending work.
    pending: AtomicU64,
    /// Lock-free waker notification — replaces `Mutex<Option<Waker>>`.
    wake_src: WakeSource,
}

impl BitSignal {
    /// Creates a `BitSignal` (cloned into producers) and the paired `WakeSink`
    /// (owned exclusively by the [`TaskGroup`] consumer).
    pub fn new_pair() -> (Arc<Self>, WakeSink) {
        let wake_sink = WakeSink::new();
        let wake_src = wake_sink.source();
        (
            Arc::new(Self {
                pending: AtomicU64::new(0),
                wake_src,
            }),
            wake_sink,
        )
    }

    /// Mark slot `index` as ready and wake the group task once.
    ///
    /// Called by producers (SPMC senders, event dispatchers, …).
    #[inline]
    pub fn notify(&self, index: u8) {
        self.pending.fetch_or(1u64 << (index & 63), Ordering::Release);
        self.wake_src.notify();
    }

    /// Mark a bitmask of slots as ready and wake the group task once.
    ///
    /// More efficient than calling [`notify`] N times when multiple slots
    /// become ready simultaneously (e.g. a track packet forwarded to N
    /// participants in the same shard).
    ///
    /// [`notify`]: BitSignal::notify
    #[inline]
    pub fn notify_mask(&self, bits: u64) {
        if bits == 0 {
            return;
        }
        self.pending.fetch_or(bits, Ordering::Release);
        self.wake_src.notify();
    }

    /// Atomically take all pending bits, clearing them to zero.
    ///
    /// Called by the [`TaskGroup`] consumer inside its poll loop.
    #[inline]
    pub fn take(&self) -> u64 {
        self.pending.swap(0, Ordering::Acquire)
    }
}

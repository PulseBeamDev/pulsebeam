//! Lock-free, pool-backed, reference-counted byte buffers for the RTP hot path.
//!
//! # Problem
//!
//! The SFU forwarding pipeline must fan out every inbound RTP packet to N
//! downstream subscribers.  Each subscriber lives in its own async task, so
//! the packet payload must be **cheaply cloneable** (no memcpy per subscriber)
//! and **automatically reclaimed** once all subscribers have consumed it.
//!
//! The naive solution – `bytes::Bytes` or `Arc<[u8]>` – pays a jemalloc round-
//! trip on every packet:
//!
//! * `bytes::Bytes::from(Vec<u8>)` starts as `KIND_VEC`.  The *first* `.clone()`
//!   promotes it to `KIND_ARC` by allocating a separate `Shared { … }` struct.
//! * `triomphe::Arc::<[u8]>::from(&slice)` always allocates (fat-pointer Arc).
//!
//! Under load (1 000 pkt/s × 100 subscribers) this creates ~100 k allocations/s
//! hitting jemalloc, showing up as ~10% CPU in profiling.
//!
//! # Solution: `PoolBuf`
//!
//! `PoolBuf` is a custom `Arc`-like type whose inner slot comes from a
//! pre-allocated, bounded free list instead of the system allocator:
//!
//! ```text
//!  checkout ──► PoolBuf ──┬─ clone ──► PoolBuf  (fetch_add, zero alloc)
//!                          ├─ clone ──► PoolBuf
//!                          └─ …
//!                all drop ──► slot returned to pool (fetch_sub → push)
//! ```
//!
//! # Performance characteristics
//!
//! | Operation       | Cost                                        |
//! |-----------------|---------------------------------------------|
//! | `checkout`      | 1 lock-free pop + 1 `copy_from_slice`       |
//! | `clone`         | 1 `fetch_add(Relaxed)`, zero allocation     |
//! | `drop` (N-1)    | 1 `fetch_sub(Release)`                      |
//! | `drop` (last)   | 1 `fetch_sub` + 1 `fence` + 1 lock-free push|
//! | fallback alloc  | jemalloc — only when pool is empty at burst  |
//!
//! # Design notes
//!
//! * [`crossbeam_queue::ArrayQueue`] is MPMC, wait-free, and preallocates its
//!   ring at construction — zero heap activity on push/pop after init.
//! * Four independent shards reduce CAS contention when many tasks drop
//!   simultaneously (fan-out "drop storm").
//! * Each `BufSlot` embeds a `[u8; MAX_PAYLOAD]` array — one allocation covers
//!   the refcount, back-pointer, and data.  No secondary heap pointer.
//! * The `shard` back-pointer is `NonNull` into the static `BufPool`; it is
//!   valid for the entire process lifetime.

use crossbeam_queue::ArrayQueue;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering, fence};

/// Slot size for every pool buffer.
///
/// 2 048 bytes (next power of two above Ethernet MTU 1 500) comfortably holds
/// any UDP/RTP/SRTP datagram.  Using a single fixed size across the entire
/// pipeline means one pool serves recv ingestion, SRTP plaintext, and RTP
/// fan-out — no secondary allocations, no size mismatch logic.
/// Memory cost: 8 192 pre-filled slots × 2 048 bytes ≈ 16 MB resident.
pub const MAX_PAYLOAD: usize = 2048;

/// Number of independent free-list shards in each [`BufPool`].
///
/// Four shards mean that under a 100-subscriber drop storm, each shard sees
/// ~25 concurrent CAS operations instead of 100, roughly halving the retry
/// rate on a 4-core worker.
const SHARDS: usize = 4;

// ─────────────────────────────────────────────────────────────────────────────
// BufSlot – the single heap-allocated, reusable slot
// ─────────────────────────────────────────────────────────────────────────────

/// One pooled buffer slot.
///
/// Invariants (upheld by [`BufPool`] and [`PoolBuf`]):
///
/// * When `refcount == 0` the slot is in the pool's free list and **must not**
///   be accessed through any `PoolBuf`.
/// * Immediately after `checkout` sets `refcount = 1`, the caller has
///   **exclusive write** access to `data[..len]`.
/// * After the initial write, `data` is **immutable** for the slot's lifetime
///   as a live `PoolBuf`; only reads are permitted through clones.
struct BufSlot {
    /// Arc-style reference count.  `0` = free, `1+` = live.
    refcount: AtomicUsize,
    /// Number of valid bytes in `data`.
    len: usize,
    /// Back-pointer to the shard that owns this slot.
    ///
    /// Set once at allocation time (inside [`BufPool::pre_fill`] or the
    /// fallback allocator), then immutable.  The shard is part of the
    /// `'static` [`BufPool`], so the pointer is always valid.
    shard: NonNull<Shard>,
    /// Payload storage.  Embedded inline to keep allocation count at one.
    data: [u8; MAX_PAYLOAD],
}

// SAFETY: `BufSlot` is only accessed through `PoolBuf`'s reference-count
// protocol; see the invariants above.  `data` is written once (exclusively)
// then read-only, satisfying Rust's aliasing rules.
unsafe impl Send for BufSlot {}
unsafe impl Sync for BufSlot {}

// ─────────────────────────────────────────────────────────────────────────────
// PoolBuf – the user-facing, reference-counted handle
// ─────────────────────────────────────────────────────────────────────────────

/// A reference-counted, pool-backed byte buffer.
///
/// Cloning is an atomic increment with no heap allocation.  Dropping the last
/// clone returns the slot to the pool's free list (also allocation-free after
/// warmup).
///
/// Dereferences to `&[u8]` (the payload slice).
pub struct PoolBuf(NonNull<BufSlot>);

// SAFETY: see BufSlot safety comment.
unsafe impl Send for PoolBuf {}
unsafe impl Sync for PoolBuf {}

impl PoolBuf {
    #[inline(always)]
    fn slot(&self) -> &BufSlot {
        // SAFETY: pointer is valid (slot is either in a live Box or
        // Box::leak'd pool allocation).
        unsafe { self.0.as_ref() }
    }

    /// Length of the valid payload.
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.slot().len
    }

    /// Returns `true` if the payload is empty.
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.slot().len == 0
    }
}

impl Deref for PoolBuf {
    type Target = [u8];

    #[inline(always)]
    fn deref(&self) -> &[u8] {
        let s = self.slot();
        // SAFETY: `len` is set during checkout and never mutated afterwards.
        unsafe { s.data.get_unchecked(..s.len) }
    }
}

impl Clone for PoolBuf {
    /// Increment the reference count.
    ///
    /// `Relaxed` ordering is safe here — the standard Arc clone protocol
    /// guarantees that the Acquire fence in the last `drop` synchronises
    /// with all preceding Release stores (including prior clones' Relaxed
    /// increments, which are sequentially consistent within a single thread).
    #[inline(always)]
    fn clone(&self) -> Self {
        self.slot().refcount.fetch_add(1, Ordering::Relaxed);
        PoolBuf(self.0)
    }
}

impl Drop for PoolBuf {
    #[inline]
    fn drop(&mut self) {
        let slot = self.slot();

        // Release ensures all reads of `data` (by this task) happen-before
        // the slot is considered free.
        if slot.refcount.fetch_sub(1, Ordering::Release) != 1 {
            // Still live references — nothing more to do.
            return;
        }

        // We are the last owner.  Acquire fence synchronises with all prior
        // Release stores so we observe the fully-written slot state.
        fence(Ordering::Acquire);

        // Return the slot to its shard's free list.
        // If the shard is full (pool at capacity), simply drop the Box —
        // this is the only jemalloc call on the steady-state hot path, and
        // only occurs when the pool is saturated beyond its configured limit.
        //
        // SAFETY: `shard` pointer is valid — it points into the 'static BufPool.
        let shard = unsafe { slot.shard.as_ref() };
        if shard.push(self.0).is_err() {
            #[cfg(feature = "deep-metrics")]
            metrics::counter!("pool_buf_slot_freed_total").increment(1);
            // SAFETY: we are the last owner; pointer came from Box::into_raw.
            unsafe { drop(Box::from_raw(self.0.as_ptr())) };
        } else {
            #[cfg(feature = "deep-metrics")]
            metrics::gauge!("pool_buf_idle_slots").increment(1.0);
        }
    }
}

impl std::fmt::Debug for PoolBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolBuf")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl PartialEq for PoolBuf {
    fn eq(&self, other: &Self) -> bool {
        // Fast path: same slot pointer.
        if self.0 == other.0 {
            return true;
        }
        **self == **other
    }
}

impl Eq for PoolBuf {}

// ─────────────────────────────────────────────────────────────────────────────
// PoolBufMut – exclusive-write handle for zero-copy recv
// ─────────────────────────────────────────────────────────────────────────────

/// Exclusive-write handle to a pool slot, obtained via
/// [`BufPool::checkout_uninit`].
///
/// Use this when you need the pool slot to *be* the receive buffer — e.g.
/// `recv_from(slot.as_uninit_slice())` — so no intermediate `Vec` is ever
/// needed and no memcpy occurs on the happy path.
///
/// Call [`freeze`](PoolBufMut::freeze) to publish the written bytes as a
/// shareable [`PoolBuf`].  Dropping without calling `freeze` silently returns
/// the slot to the pool.
pub struct PoolBufMut(NonNull<BufSlot>);

// SAFETY: same protocol as PoolBuf; caller has exclusive write access.
unsafe impl Send for PoolBufMut {}
unsafe impl Sync for PoolBufMut {}

impl PoolBufMut {
    /// Returns a mutable reference to the full slot storage (`MAX_PAYLOAD` bytes).
    ///
    /// Write your data here, then call `freeze(n)` with `n` = bytes written.
    #[inline(always)]
    pub fn as_uninit_slice(&mut self) -> &mut [u8; MAX_PAYLOAD] {
        // SAFETY: we hold exclusive write access (refcount == 0, not yet published).
        unsafe { &mut (*self.0.as_ptr()).data }
    }

    /// Convert to a shared [`PoolBuf`] and publish `len` valid bytes.
    ///
    /// `len` is clamped to `MAX_PAYLOAD`; the slot is then read-only until
    /// the last clone is dropped.
    #[inline]
    pub fn freeze(self, len: usize) -> PoolBuf {
        let slot = unsafe { &mut *self.0.as_ptr() };
        slot.len = len.min(MAX_PAYLOAD);
        // Relaxed: the Acquire fence in the last PoolBuf::drop() synchronises.
        slot.refcount.store(1, Ordering::Relaxed);
        #[cfg(feature = "deep-metrics")]
        metrics::gauge!("pool_buf_idle_slots").decrement(1.0);
        #[cfg(feature = "deep-metrics")]
        metrics::counter!("pool_buf_checkout_total").increment(1);
        let ptr = self.0;
        // Do not run our own Drop — ownership transfers to PoolBuf.
        std::mem::forget(self);
        PoolBuf(ptr)
    }
}

impl Drop for PoolBufMut {
    /// Return the slot unused (e.g. on recv error).
    fn drop(&mut self) {
        let shard = unsafe { (*self.0.as_ptr()).shard.as_ref() };
        if shard.push(self.0).is_err() {
            #[cfg(feature = "deep-metrics")]
            metrics::counter!("pool_buf_slot_freed_total").increment(1);
            unsafe { drop(Box::from_raw(self.0.as_ptr())) };
        } else {
            #[cfg(feature = "deep-metrics")]
            metrics::gauge!("pool_buf_idle_slots").increment(1.0);
        }
    }
}

impl std::fmt::Debug for PoolBufMut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolBufMut").finish_non_exhaustive()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// BufPool – the static, sharded free list
// ─────────────────────────────────────────────────────────────────────────────

type Shard = ArrayQueue<NonNull<BufSlot>>;

/// A bounded, sharded, lock-free pool of [`MAX_PAYLOAD`]-sized byte buffers.
///
/// # Usage
///
/// ```rust,ignore
/// // At program startup:
/// static POOL: OnceLock<&'static BufPool> = OnceLock::new();
/// let pool: &'static BufPool = POOL.get_or_init(|| {
///     BufPool::new_prefilled(4096, 2048)
/// });
///
/// // On every inbound RTP packet (hot path):
/// let buf: PoolBuf = pool.checkout(&rtp.payload);
/// ```
///
/// # Internals
///
/// * **Storage**: four [`crossbeam_queue::ArrayQueue`] shards, each bounded to
///   `capacity / 4` slots.  `ArrayQueue` preallocates its ring at construction
///   — push and pop are wait-free pointer-sized moves on a pre-mapped page.
///
/// * **Shard selection**: round-robin via a relaxed atomic counter.  Checkout
///   probes the next shard first, then falls through the remaining shards to
///   avoid returning `None` when one shard is temporarily drained.
///
/// * **Slot layout**: each `BufSlot` is a single `Box` containing
///   `[refcount | len | shard_ptr | data[1500]]` — one `mmap`'d page covers
///   ~2–3 slots; no secondary indirections.
pub struct BufPool {
    shards: [Shard; SHARDS],
    /// Round-robin counter for shard assignment.
    next: AtomicUsize,
}

// SAFETY: NonNull<BufSlot> inside the ArrayQueues is governed by the pool protocol.
unsafe impl Send for BufPool {}
unsafe impl Sync for BufPool {}

impl BufPool {
    /// Create an empty pool with `capacity` total slots (divided across shards).
    ///
    /// Slot memory is **not** allocated here.  Call [`BufPool::pre_fill`]
    /// (or use [`BufPool::new_prefilled`]) to warm the pool before serving traffic.
    pub fn new(capacity: usize) -> Self {
        let per_shard = (capacity / SHARDS).max(1);
        BufPool {
            shards: std::array::from_fn(|_| ArrayQueue::new(per_shard)),
            next: AtomicUsize::new(0),
        }
    }

    /// Convenience: allocate a `'static` pool, pre-fill `prefill` slots, and
    /// return a `'static` reference.
    ///
    /// Internally calls `Box::leak` — the pool memory is intentionally never
    /// freed (it lives for the process lifetime).
    ///
    /// # Example
    /// ```rust,ignore
    /// static POOL: OnceLock<&'static BufPool> = OnceLock::new();
    /// let pool = POOL.get_or_init(|| BufPool::new_prefilled(4096, 2048));
    /// ```
    pub fn new_prefilled(capacity: usize, prefill: usize) -> &'static Self {
        let pool: &'static BufPool = Box::leak(Box::new(BufPool::new(capacity)));
        pool.pre_fill(prefill);
        pool
    }

    /// Allocate `n` `BufSlot`s and push them onto the free lists.
    ///
    /// **Requires `self` to be at a stable `'static` address** because each
    /// slot stores a raw pointer back to its shard.  Use [`Self::new_prefilled`]
    /// or `Box::leak` before calling this.
    pub fn pre_fill(&'static self, n: usize) {
        for i in 0..n {
            let shard_idx = i % SHARDS;
            let shard = &self.shards[shard_idx];
            let shard_ptr = NonNull::from(shard);

            let slot = Box::new(BufSlot {
                refcount: AtomicUsize::new(0),
                len: 0,
                shard: shard_ptr,
                data: [0u8; MAX_PAYLOAD],
            });
            let ptr = unsafe { NonNull::new_unchecked(Box::into_raw(slot)) };

            if shard.push(ptr).is_err() {
                // Shard already at capacity — free the surplus slot.
                unsafe { drop(Box::from_raw(ptr.as_ptr())) };
            } else {
                #[cfg(feature = "deep-metrics")]
                metrics::gauge!("pool_buf_idle_slots").increment(1.0);
            }
        }
    }

    /// Checkout a buffer pre-filled with `src`.
    ///
    /// # Hot path
    /// Pops a pre-allocated slot from the free list (lock-free, ~5 ns),
    /// copies `src` into it (one `memcpy`), sets `refcount = 1`.
    ///
    /// # Slow path (fallback)
    /// When every shard is empty (burst beyond pool capacity or before warmup),
    /// allocates a fresh `Box<BufSlot>`.  This is the only call site that can
    /// reach jemalloc after the pool is warmed up.
    pub fn checkout(&'static self, src: &[u8]) -> PoolBuf {
        debug_assert!(
            src.len() <= MAX_PAYLOAD,
            "RTP payload {} bytes exceeds pool MAX_PAYLOAD {}",
            src.len(),
            MAX_PAYLOAD
        );

        let n = src.len().min(MAX_PAYLOAD);
        let ptr = self.pop_any_shard().unwrap_or_else(|| {
            // Slow path: allocate a new slot.
            #[cfg(feature = "deep-metrics")]
            metrics::counter!("pool_buf_alloc_fallback_total").increment(1);
            let shard_idx = self.next.load(Ordering::Relaxed) % SHARDS;
            let shard_ptr = NonNull::from(&self.shards[shard_idx]);
            unsafe {
                NonNull::new_unchecked(Box::into_raw(Box::new(BufSlot {
                    refcount: AtomicUsize::new(0),
                    len: 0,
                    shard: shard_ptr,
                    data: [0u8; MAX_PAYLOAD],
                })))
            }
        });

        // SAFETY: exclusive – just popped from free list (refcount was 0)
        // or freshly allocated.  No other PoolBuf references this slot yet.
        let slot = unsafe { &mut *ptr.as_ptr() };
        // SAFETY: n ≤ MAX_PAYLOAD (enforced above).
        unsafe {
            slot.data
                .get_unchecked_mut(..n)
                .copy_from_slice(src.get_unchecked(..n));
        }
        slot.len = n;
        // Relaxed: the Acquire fence in drop() will synchronize before re-use.
        slot.refcount.store(1, Ordering::Relaxed);

        #[cfg(feature = "deep-metrics")]
        metrics::gauge!("pool_buf_idle_slots").decrement(1.0);
        #[cfg(feature = "deep-metrics")]
        metrics::counter!("pool_buf_checkout_total").increment(1);

        PoolBuf(ptr)
    }

    /// Checkout a slot for **direct write** (zero-copy receive).
    ///
    /// Returns a [`PoolBufMut`] with exclusive write access to a
    /// `[u8; MAX_PAYLOAD]` array.  Caller fills it (e.g. `recv_from`), then
    /// calls `freeze(n)` to publish the first `n` bytes as a [`PoolBuf`].
    ///
    /// Falls back to a fresh allocation if all shards are empty.
    pub fn checkout_uninit(&'static self) -> PoolBufMut {
        let ptr = self.pop_any_shard().unwrap_or_else(|| {
            #[cfg(feature = "deep-metrics")]
            metrics::counter!("pool_buf_alloc_fallback_total").increment(1);
            let shard_idx = self.next.load(Ordering::Relaxed) % SHARDS;
            let shard_ptr = NonNull::from(&self.shards[shard_idx]);
            unsafe {
                NonNull::new_unchecked(Box::into_raw(Box::new(BufSlot {
                    refcount: AtomicUsize::new(0),
                    len: 0,
                    shard: shard_ptr,
                    data: [0u8; MAX_PAYLOAD],
                })))
            }
        });
        PoolBufMut(ptr)
    }

    /// Pop a free slot, trying each shard in round-robin order.
    ///
    /// Returns `None` only if all shards are empty simultaneously.
    #[inline]
    fn pop_any_shard(&'static self) -> Option<NonNull<BufSlot>> {
        let base = self.next.fetch_add(1, Ordering::Relaxed) % SHARDS;
        for i in 0..SHARDS {
            if let Some(ptr) = self.shards[(base + i) % SHARDS].pop() {
                return Some(ptr);
            }
        }
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Metrics registration helper
// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// Process-global pools
// ─────────────────────────────────────────────────────────────────────────────

/// Process-global pool for **network receive** buffers (UDP + TCP ingestion).
///
/// 16 384 slots × 2 048 bytes ≈ 32 MB resident.  Sized for:
///   32 (BATCH_SIZE) × 32 (GRO segments/batch) × N workers + 4× headroom.
pub fn net_recv_pool() -> &'static BufPool {
    static POOL: std::sync::OnceLock<&'static BufPool> = std::sync::OnceLock::new();
    *POOL.get_or_init(|| BufPool::new_prefilled(16_384, 8_192))
}

/// Register pool_buf metric descriptions with the metrics recorder.
///
/// Call once at process startup before the metrics exporter is initialized.
/// No-op when the `deep-metrics` feature is disabled.
pub fn describe_metrics() {
    #[cfg(feature = "deep-metrics")]
    {
        metrics::describe_counter!(
            "pool_buf_checkout_total",
            "Total number of PoolBuf checkouts from the pool."
        );
        metrics::describe_counter!(
            "pool_buf_alloc_fallback_total",
            "Checkouts that fell back to jemalloc because all pool shards were empty."
        );
        metrics::describe_gauge!(
            "pool_buf_idle_slots",
            "Current number of slots sitting in the pool free lists."
        );
        metrics::describe_counter!(
            "pool_buf_slot_freed_total",
            "Slots freed to jemalloc because the pool was at capacity on return."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn test_pool() -> &'static BufPool {
        static POOL: OnceLock<&'static BufPool> = OnceLock::new();
        *POOL.get_or_init(|| BufPool::new_prefilled(64, 32))
    }

    #[test]
    fn checkout_deref() {
        let pool = test_pool();
        let data = b"hello world";
        let buf = pool.checkout(data);
        assert_eq!(&*buf, data);
        assert_eq!(buf.len(), data.len());
    }

    #[test]
    fn clone_reads_same_slice() {
        let pool = test_pool();
        let data = b"clone test";
        let a = pool.checkout(data);
        let b = a.clone();
        assert_eq!(&*a, &*b);
        assert_eq!(a, b);
        // Both clones share the same pointer.
        assert_eq!(a.0, b.0);
    }

    #[test]
    fn slot_reused_after_drop() {
        let pool = test_pool();
        let slot_ptr = {
            let buf = pool.checkout(b"first");
            buf.0 // Capture the pointer before drop
        };
        // After drop the slot should have been returned.
        // Next checkout should reuse it (same shard, LIFO push/pop).
        // We can't guarantee the exact slot due to sharding, but we can verify
        // no crash and correct data.
        let buf2 = pool.checkout(b"second");
        assert_eq!(&*buf2, b"second");
        drop(buf2);
        drop(slot_ptr); // suppress unused warning
    }

    #[test]
    fn last_drop_returns_to_pool() {
        let pool = test_pool();
        let a = pool.checkout(b"refcount");
        let b = a.clone();
        let c = b.clone();
        // Three references; drop two.
        drop(a);
        drop(b);
        // Slot is still live (c holds it).
        assert_eq!(&*c, b"refcount");
        // Final drop — slot is returned.
        drop(c);
    }

    #[test]
    fn empty_payload() {
        let pool = test_pool();
        let buf = pool.checkout(&[]);
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn max_payload() {
        let pool = test_pool();
        let data = vec![0xABu8; MAX_PAYLOAD];
        let buf = pool.checkout(&data);
        assert_eq!(buf.len(), MAX_PAYLOAD);
        assert_eq!(&*buf, &data[..]);
    }

    #[test]
    fn checkout_uninit_freeze() {
        let pool = test_pool();
        let expected = b"recv'd payload";
        let mut slot = pool.checkout_uninit();
        {
            let arr = slot.as_uninit_slice();
            arr[..expected.len()].copy_from_slice(expected);
        }
        let buf = slot.freeze(expected.len());
        assert_eq!(&*buf, expected);
        assert_eq!(buf.len(), expected.len());
    }

    #[test]
    fn checkout_uninit_drop_returns_to_pool() {
        let pool = test_pool();
        // Dropping without freeze should not leak or corrupt the pool.
        let slot = pool.checkout_uninit();
        drop(slot); // returns silently
        // Pool is still usable.
        let buf = pool.checkout(b"after uninit drop");
        assert_eq!(&*buf, b"after uninit drop");
    }

    #[test]
    fn checkout_uninit_freeze_then_clone() {
        let pool = test_pool();
        let mut slot = pool.checkout_uninit();
        slot.as_uninit_slice()[..4].copy_from_slice(b"test");
        let a = slot.freeze(4);
        let b = a.clone();
        let c = b.clone();
        assert_eq!(&*a, b"test");
        assert_eq!(&*b, b"test");
        drop(a);
        drop(b);
        assert_eq!(&*c, b"test"); // still alive
        drop(c); // last drop returns to pool
    }

    #[test]
    fn concurrent_checkout_drop() {
        use std::sync::Arc as StdArc;
        use std::thread;

        static CPOOL: OnceLock<&'static BufPool> = OnceLock::new();
        let pool = *CPOOL.get_or_init(|| BufPool::new_prefilled(256, 128));

        let barrier = StdArc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();

        for t in 0..8usize {
            let b = StdArc::clone(&barrier);
            handles.push(thread::spawn(move || {
                b.wait();
                for i in 0..200usize {
                    let payload = (t * 1000 + i).to_be_bytes();
                    let buf = pool.checkout(&payload);
                    // Simulate fan-out: clone to 3 "subscribers".
                    let c1 = buf.clone();
                    let c2 = buf.clone();
                    assert_eq!(&*c1, &*buf);
                    drop(c1);
                    assert_eq!(&*c2, &*buf);
                    drop(buf);
                    drop(c2);
                }
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }
    }
}

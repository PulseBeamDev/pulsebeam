//! Simple buffer wrapper for the disabling of pooled buffer free list.
//!
//! For maximum perf on jemalloc-backed allocations, we avoid custom ring pools
//! and use `Arc<Vec<u8>>` for shared zero-copy payloads.

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, OnceLock};

/// Maximum payload size used by RTP and net recv pools.
pub const MAX_PAYLOAD: usize = 2048;

/// Shared, clone-on-write packet payload.
#[derive(Clone, PartialEq, Eq)]
pub struct PoolBuf(Arc<Vec<u8>>);

impl PoolBuf {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn into_arc(self) -> Arc<Vec<u8>> {
        self.0
    }
}

impl Deref for PoolBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for PoolBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolBuf").field("len", &self.len()).finish_non_exhaustive()
    }
}

/// Mutable builder for a buffer that will become shareable.
pub struct PoolBufMut(Vec<u8>);

impl PoolBufMut {
    pub fn new() -> Self {
        Self(Vec::with_capacity(MAX_PAYLOAD))
    }

    pub fn resize(&mut self, len: usize, value: u8) {
        self.0.resize(len, value);
    }

    pub fn extend_from_slice(&mut self, slice: &[u8]) {
        self.0.extend_from_slice(slice);
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn freeze(self, len: usize) -> PoolBuf {
        let mut v = self.0;
        v.truncate(len);
        PoolBuf(Arc::new(v))
    }
}

impl Deref for PoolBufMut {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl DerefMut for PoolBufMut {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

/// A no-op polyfill pool handle.
pub struct BufPool;

impl BufPool {
    pub fn new(_capacity: usize) -> Self {
        Self
    }

    pub fn new_prefilled(_capacity: usize, _prefill: usize) -> Self {
        Self
    }

    pub fn checkout(&self, src: &[u8]) -> PoolBuf {
        PoolBuf(Arc::new(src.to_vec()))
    }

    pub fn checkout_uninit(&self) -> PoolBufMut {
        PoolBufMut::new()
    }
}

pub fn net_recv_pool() -> &'static BufPool {
    static POOL: OnceLock<BufPool> = OnceLock::new();
    POOL.get_or_init(|| BufPool::new(MAX_PAYLOAD))
}

pub fn describe_metrics() {
    // no-op
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pool() -> &'static BufPool {
        static POOL: OnceLock<BufPool> = OnceLock::new();
        POOL.get_or_init(|| BufPool::new(MAX_PAYLOAD))
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
        assert!(Arc::ptr_eq(&a.0, &b.0));
    }

    #[test]
    fn checkout_uninit_and_freeze() {
        let pool = test_pool();
        let mut slot = pool.checkout_uninit();
        slot.extend_from_slice(b"xyz");
        let b = slot.freeze(3);
        assert_eq!(&*b, b"xyz");
    }
}

pub mod bit_signal;
pub mod mpsc;
pub mod pool_buf;
pub mod spmc;
pub mod task_group;

pub use pool_buf::{BufPool, PoolBuf, PoolBufMut, MAX_PAYLOAD as POOL_MAX_PAYLOAD};

#[cfg(not(feature = "loom"))]
mod primitives {
    pub use std::sync::*;
    /// Zero-weak-count Arc — drops the weak-count word from every allocation and
    /// the matching atomic acquire on every `clone()`.  This is the canonical
    /// `Arc` for all internal SFU code; import via `crate::sync::Arc` (runtime)
    /// or `pulsebeam_runtime::sync::Arc` (other crates).
    pub use triomphe::Arc;

    pub mod atomic {
        pub use std::sync::atomic::*;
    }
}

#[cfg(feature = "loom")]
mod primitives {
    pub use std::sync::Arc;

    pub mod atomic {
        pub use loom::sync::atomic::*;
    }
}

pub use primitives::*;

pub mod spmc;

#[cfg(not(loom))]
mod primitives {
    pub use std::sync::Arc;

    pub mod atomic {
        pub use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    }
}

#[cfg(loom)]
mod primitives {
    pub use loom::sync::Arc;
    pub mod atomic {
        pub use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    }
}

pub use primitives::*;

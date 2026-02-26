pub mod bit_signal;
pub mod mpsc;
pub mod spmc;
pub mod task_group;

#[cfg(not(feature = "loom"))]
mod primitives {
    pub use std::sync::*;

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

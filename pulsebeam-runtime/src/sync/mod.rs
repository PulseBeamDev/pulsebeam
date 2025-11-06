pub mod spmc;

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

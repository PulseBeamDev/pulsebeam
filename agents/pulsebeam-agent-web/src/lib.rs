#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used,
    )
)]

#[cfg(any(target_arch = "wasm32", test))]
mod engine;

#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(target_arch = "wasm32")]
mod logger;

#[cfg(target_arch = "wasm32")]
pub use browser::BrowserRuntime;
#[cfg(target_arch = "wasm32")]
pub use logger::configure_logging;

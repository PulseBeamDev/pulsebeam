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

#[cfg(any(target_arch = "wasm32", test))]
mod channel;

#[cfg(any(target_arch = "wasm32", test))]
mod local_operation;

#[cfg(any(target_arch = "wasm32", test))]
mod sender_config;

#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(target_arch = "wasm32")]
mod logger;

#[cfg(feature = "uniffi")]
mod ffi;

#[cfg(target_arch = "wasm32")]
pub use browser::BrowserRuntime;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

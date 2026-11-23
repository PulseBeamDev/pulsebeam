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
extern crate alloc;

mod agent;
mod catalog;
pub mod data_v1;
mod effect;
mod event;
mod http;
mod id;
mod identity;
mod logging;
mod model;
mod native_state;
mod rtc_preparation;
mod signaling;
mod topic;
mod track;

pub mod client_sdp;
pub mod ffi;
pub mod observer;

pub use ffi::UniFfiTag;

pub use agent::*;
pub use effect::*;
pub use event::*;
pub use http::*;
pub use id::*;
pub use identity::*;
pub use logging::DeferredLog;
pub use model::*;
pub use rtc_preparation::adopt_bootstrap_media;
pub use signaling::SignalingError;
pub use topic::*;
pub use track::*;

#[cfg(test)]
mod tests;

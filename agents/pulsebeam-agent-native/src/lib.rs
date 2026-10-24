#![cfg_attr(not(test), forbid(unsafe_code))]
//! Caller-fed native media and the shared production/controlled libwebrtc actor.
//! Platform-independent policy remains in `pulsebeam-agent-core`.
// Shared-state exception: this SDK is an ordinary async application, not an SFU shard.
#![allow(clippy::disallowed_types)]
#![cfg_attr(
    test,
    allow(
        clippy::unreachable,
        clippy::string_slice,
        clippy::disallowed_methods,
        clippy::float_cmp,
        clippy::arithmetic_side_effects,
    )
)]

pub use agent_core;
pub use pulsebeam_core::net::AsyncHttpClient;
pub mod ffi;
pub mod media;
mod native_actor;
mod native_media;
#[cfg(feature = "sim")]
mod native_packet_diagnostics;
mod native_peer;
mod native_receiver;
mod native_source;

pub use native_actor::*;
pub use native_media::*;
#[cfg(feature = "sim")]
pub use native_packet_diagnostics::capture_native_transport;

uniffi::setup_scaffolding!();

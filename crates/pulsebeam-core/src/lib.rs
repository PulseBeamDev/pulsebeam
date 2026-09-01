//! Shared-state exception, crate-wide: Shared codec and protocol helpers, used from both sides of the wire.
//! The thread-per-core restriction in `crates/pulsebeam/docs/thread-per-core.md` applies to the
//! `pulsebeam` SFU crate.
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

pub mod dd;
pub mod framing;
pub mod h264;
pub mod net;
pub mod simulcast;

pub mod prelude {
    pub use super::net::AsyncHttpClient;
    pub use super::simulcast::LayerQuality;
}

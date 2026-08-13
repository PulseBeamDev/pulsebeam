//! Shared-state exception, crate-wide: Shared codec and protocol helpers, used from both sides of the wire.
//! The thread-per-core restriction in `docs/thread-per-core.md` applies to the
//! `pulsebeam` SFU crate.

pub mod dd;
pub mod framing;
pub mod h264;
pub mod net;
pub mod simulcast;

pub mod prelude {
    pub use super::net::AsyncHttpClient;
    pub use super::simulcast::LayerQuality;
}

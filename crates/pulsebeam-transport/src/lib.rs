#![forbid(unsafe_code)]
//! Caller-driven secure transports with explicit native datagram and DTLS-SRTP profiles.

mod demux;
pub use demux::{PacketKind, classify};
#[cfg(feature = "datagram")]
mod backend;
#[cfg(feature = "datagram")]
mod datagram;
#[cfg(feature = "datagram")]
mod quic;
#[cfg(feature = "datagram")]
mod service;
#[cfg(feature = "datagram")]
mod wire;
#[cfg(feature = "datagram")]
pub use datagram::*;
#[cfg(feature = "datagram")]
pub use quic::{QuicCongestion, QuicCredential, QuicPeer, QuicStats, QuicTuning};
#[cfg(feature = "datagram")]
pub use service::{Application, Datagram, Error, Health, SendOutcome, Staging, Transport};
#[cfg(feature = "dtls-srtp")]
pub mod dtls_srtp;
#[cfg(all(feature = "datagram", feature = "test-utils"))]
pub mod experiment;

#[cfg(all(test, feature = "datagram"))]
mod tests;
#[cfg(all(test, feature = "datagram"))]
mod wire_tests;

use std::{net::SocketAddr, time::Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EcnCodepoint {
    NotEct,
    Ect0,
    Ect1,
    Ce,
}

#[allow(
    clippy::disallowed_types,
    reason = "preserves RTC immutable owned I/O; packet storage stays connection-local"
)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NetworkInput {
    Udp {
        local: SocketAddr,
        remote: SocketAddr,
        ecn: Option<EcnCodepoint>,
        payload: bytes::Bytes,
    },
    /// One complete RFC4571 frame. The caller filters retired socket incarnations.
    IceTcp {
        local: SocketAddr,
        remote: SocketAddr,
        frame: bytes::Bytes,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransmitTarget {
    Udp {
        local: SocketAddr,
        remote: SocketAddr,
        ecn: Option<EcnCodepoint>,
    },
    /// Bytes belong to the currently selected socket at this tuple and must not
    /// be redirected to a replacement stream after partial writes.
    IceTcp {
        local: SocketAddr,
        remote: SocketAddr,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkProtocol {
    Udp,
    IceTcp,
}

/// A tuple authenticated by ICE, attributed to the input that proved it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthenticatedPath {
    pub protocol: NetworkProtocol,
    pub local: SocketAddr,
    pub remote: SocketAddr,
    pub arrival: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransmitKind {
    Original { ssrc: u32 },
    Repair { ssrc: u32 },
    Padding { ssrc: u32 },
    Control,
}

#[allow(
    clippy::disallowed_types,
    reason = "preserves RTC immutable owned I/O; packet storage stays connection-local"
)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transmit {
    pub kind: TransmitKind,
    pub target: TransmitTarget,
    pub payload: bytes::Bytes,
}

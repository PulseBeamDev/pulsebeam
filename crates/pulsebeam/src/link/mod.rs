//! SFU adapters for explicit Link provisioning and host-owned UDP delivery.

mod codec;
mod control;
pub use control::Control;
pub(crate) mod host;
#[cfg(test)]
mod tests;
pub(crate) use codec::ShardCodec;

use pulsebeam_link::DeliveryTag;

/// Trusted process-control input, not a tenant-facing signaling command.
/// Successful provisioning admits an association; readiness remains asynchronous.
pub struct OpenPeer {
    pub peer: pulsebeam_link::PeerConfig,
    pub reply: pulsebeam_runtime::sync::oneshot::Sender<
        Result<pulsebeam_link::NetworkId, pulsebeam_link::LinkError>,
    >,
}

impl std::fmt::Debug for OpenPeer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenPeer")
            .field("endpoint", &self.peer.endpoint())
            .finish_non_exhaustive()
    }
}
use crate::route::{NodeRouteAddress, RouteId};

/// A receive binding is an exact destination snapshot. Retiring any destination
/// retires the binding; its owner must explicitly provision a replacement.
#[derive(Debug)]
pub struct BindReceive {
    pub peer: pulsebeam_link::NetworkId,
    pub destinations: Vec<NodeRouteAddress>,
    pub reply:
        pulsebeam_runtime::sync::oneshot::Sender<Result<ReceiveRoute, pulsebeam_link::BindError>>,
}

/// Explicit trusted publication branch. The controller resolves the source
/// shard from the existing materialized publication, never from packet bytes.
#[derive(Debug)]
pub struct BindSend {
    pub room: crate::entity::RoomId,
    pub track: crate::entity::TrackId,
    pub peer: pulsebeam_link::NetworkId,
    pub address: pulsebeam_link::NodeRouteKey,
    pub reply: pulsebeam_runtime::sync::oneshot::Sender<
        Result<pulsebeam_link::BindingId, pulsebeam_link::BindError>,
    >,
}

/// Select one explicit reverse next hop for an existing non-origin runtime.
/// Removing the binding restores the lifecycle-owned reverse route, if any.
#[derive(Debug)]
pub struct BindReverse {
    pub room: crate::entity::RoomId,
    pub track: crate::entity::TrackId,
    pub shard: crate::id::ShardId,
    pub peer: pulsebeam_link::NetworkId,
    pub address: pulsebeam_link::NodeRouteKey,
    pub reply: pulsebeam_runtime::sync::oneshot::Sender<
        Result<pulsebeam_link::BindingId, pulsebeam_link::BindError>,
    >,
}

#[derive(Debug)]
pub struct UnbindSend(pub pulsebeam_link::BindingId);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiveKind {
    Forward,
    Reverse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiveDestination {
    pub address: NodeRouteAddress,
    pub kind: ReceiveKind,
}

#[derive(Debug)]
pub struct Destinations {
    pub room: crate::entity::RoomId,
    pub track: crate::entity::TrackId,
    pub reply: pulsebeam_runtime::sync::oneshot::Sender<
        Result<Vec<ReceiveDestination>, pulsebeam_link::BindError>,
    >,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiveRoute {
    pub binding: pulsebeam_link::BindingId,
    pub address: pulsebeam_link::NodeRouteKey,
}

#[derive(Clone, Debug)]
pub struct PeerHealth {
    pub peer: pulsebeam_link::NetworkId,
    pub endpoint: std::net::SocketAddr,
    pub transport: pulsebeam_link::io::Health,
    pub failure: Option<pulsebeam_link::io::Error>,
}

/// Bounded active and recently closed association observations from the socket owner.
#[derive(Clone, Debug)]
pub struct HostHealth {
    pub local_addr: std::net::SocketAddr,
    pub peers: Vec<PeerHealth>,
    pub overload_events: u64,
}

pub(crate) fn delivery_tag(address: NodeRouteAddress) -> DeliveryTag {
    DeliveryTag::from_raw((u64::from(address.epoch) << 32) | u64::from(address.route.get()))
}

pub(crate) fn delivery_address(tag: DeliveryTag) -> Option<NodeRouteAddress> {
    let raw = tag.as_raw();
    let epoch = u16::try_from(raw >> 32).ok()?;
    let route = u32::try_from(raw & u64::from(u32::MAX)).ok()?;
    Some(NodeRouteAddress::new(RouteId::from_raw(route), epoch))
}

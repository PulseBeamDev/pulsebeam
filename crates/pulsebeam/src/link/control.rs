use crate::control::controller::{ControllerCommand, ControllerSender};
use crate::entity::{RoomId, TrackId};
use crate::id::ShardId;
use crate::route::NodeRouteAddress;
use pulsebeam_link::{BindError, BindingId, LinkError, NetworkId, NodeRouteKey, PeerConfig};
use pulsebeam_runtime::sync::oneshot;

use super::{BindReceive, BindReverse, BindSend, OpenPeer, ReceiveRoute, UnbindSend};

/// A trusted embedding capability for Link provisioning, not tenant signaling.
/// It cannot create participants, authorize sources, or discover peer identities.
#[derive(Clone)]
pub struct Control {
    sender: ControllerSender,
}

impl Control {
    pub(crate) fn new(sender: ControllerSender) -> Self {
        Self { sender }
    }

    fn submit(&self, command: ControllerCommand) -> Result<(), LinkError> {
        self.sender.try_send(command).map_err(|error| match error {
            pulsebeam_queue::TrySendError::Full(_) => LinkError::Capacity,
            pulsebeam_queue::TrySendError::Closed(_) => LinkError::Closed,
        })
    }

    fn submit_binding(&self, command: ControllerCommand) -> Result<(), BindError> {
        self.submit(command).map_err(|error| match error {
            LinkError::Capacity => BindError::Capacity,
            _ => BindError::Closed,
        })
    }

    /// Reads the socket owner's active and bounded recently closed observations.
    pub async fn health(&self) -> Result<super::HostHealth, LinkError> {
        let (reply, result) = oneshot::channel();
        self.submit(ControllerCommand::InspectLinks(reply))?;
        result.await.map_err(|_| LinkError::Closed)?
    }

    /// Admits a pinned datagram peer. Security readiness is asynchronous.
    pub async fn open_peer(&self, peer: impl Into<PeerConfig>) -> Result<NetworkId, LinkError> {
        let peer = peer.into();
        let (reply, result) = oneshot::channel();
        self.submit(ControllerCommand::OpenLink(OpenPeer { peer, reply }))?;
        result.await.map_err(|_| LinkError::Closed)?
    }

    /// Lists acknowledged existing forward/reverse routes for a current publication.
    /// This neither creates a runtime nor guarantees the snapshot remains current.
    pub async fn destinations(
        &self,
        room: RoomId,
        track: TrackId,
    ) -> Result<Vec<super::ReceiveDestination>, BindError> {
        let (reply, result) = oneshot::channel();
        self.submit_binding(ControllerCommand::LinkDestinations(super::Destinations {
            room,
            track,
            reply,
        }))?;
        result.await.map_err(|_| BindError::Closed)?
    }

    /// Binds only routes whose existing shard installations have been acknowledged.
    pub async fn bind_receive(
        &self,
        peer: NetworkId,
        destinations: Vec<NodeRouteAddress>,
    ) -> Result<ReceiveRoute, BindError> {
        let (reply, result) = oneshot::channel();
        self.submit_binding(ControllerCommand::BindLinkReceive(BindReceive {
            peer,
            destinations,
            reply,
        }))?;
        result.await.map_err(|_| BindError::Closed)?
    }

    pub async fn bind_send(
        &self,
        room: RoomId,
        track: TrackId,
        peer: NetworkId,
        address: NodeRouteKey,
    ) -> Result<BindingId, BindError> {
        let (reply, result) = oneshot::channel();
        self.submit_binding(ControllerCommand::BindLinkSend(BindSend {
            room,
            track,
            peer,
            address,
            reply,
        }))?;
        result.await.map_err(|_| BindError::Closed)?
    }

    pub async fn bind_reverse(
        &self,
        room: RoomId,
        track: TrackId,
        shard: ShardId,
        peer: NetworkId,
        address: NodeRouteKey,
    ) -> Result<BindingId, BindError> {
        let (reply, result) = oneshot::channel();
        self.submit_binding(ControllerCommand::BindLinkReverse(BindReverse {
            room,
            track,
            shard,
            peer,
            address,
            reply,
        }))?;
        result.await.map_err(|_| BindError::Closed)?
    }

    /// Queues peer closure. Success means mailbox admission, not completed retirement.
    pub fn request_close_peer(&self, peer: NetworkId) -> Result<(), LinkError> {
        self.submit(ControllerCommand::CloseLink(peer))
    }

    /// Queues receive-binding cancellation, without waiting for controller execution.
    pub fn request_unbind_receive(&self, binding: BindingId) -> Result<(), BindError> {
        self.submit_binding(ControllerCommand::UnbindLinkReceive(binding))
    }

    /// Queues plan removal. The binding remains live until the shard cutoff acknowledgment.
    pub fn request_unbind_send(&self, binding: BindingId) -> Result<(), BindError> {
        self.submit_binding(ControllerCommand::UnbindLinkSend(UnbindSend(binding)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_capability_reports_mailbox_backpressure_and_closure() {
        let (sender, mut receiver) = pulsebeam_queue::bounded(std::num::NonZeroUsize::MIN);
        let control = Control::new(sender);
        let mut links =
            pulsebeam_link::LinkControl::<()>::new(|| pulsebeam_link::LinkTime::from_nanos(0))
                .unwrap();
        let (port, _packets) = links.create_port().unwrap();
        let (binding, _tx) = links
            .bind_send(pulsebeam_link::Destination::Local {
                port,
                tag: pulsebeam_link::DeliveryTag::from_raw(0),
            })
            .unwrap();
        assert_eq!(control.request_unbind_send(binding), Ok(()));
        assert_eq!(
            control.request_unbind_send(binding),
            Err(BindError::Capacity)
        );
        assert!(
            matches!(receiver.try_recv(), Ok(ControllerCommand::UnbindLinkSend(UnbindSend(actual))) if actual == binding)
        );
        drop(receiver);
        assert_eq!(control.request_unbind_send(binding), Err(BindError::Closed));
    }
}

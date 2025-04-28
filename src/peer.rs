use std::sync::Arc;

use str0m::{
    Candidate, Rtc, RtcError,
    change::{SdpAnswer, SdpOffer},
    error::SdpError,
};
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::{
    egress::EgressHandle,
    group::GroupHandle,
    ingress::IngressHandle,
    message::{self, PeerId},
};

#[derive(thiserror::Error, Debug)]
pub enum PeerError {
    #[error("invalid sdp offer format")]
    InvalidOfferFormat(#[from] SdpError),

    #[error(transparent)]
    OfferRejected(RtcError),
}

#[derive(Debug)]
pub enum PeerMessage {
    UdpPacket(message::EgressUDPPacket),
}

#[derive(Debug)]
pub struct PeerActor {
    ingress: IngressHandle,
    egress: EgressHandle,
    group: GroupHandle,
    peer_id: Arc<PeerId>,
    rtc: str0m::Rtc,
}

impl PeerActor {
    pub fn new(
        group: GroupHandle,
        peer_id: Arc<PeerId>,
        offer: SdpOffer,
    ) -> Result<(Self, SdpAnswer), PeerError> {
        let actor = PeerActor {
            group,
            rtc,
            peer_id,
        };
        Ok((actor, answer))
    }

    async fn run(self, mut receiver: mpsc::Receiver<PeerMessage>) {
        // TODO: notify ingress to add self to the routing table

        while let Some(msg) = receiver.recv().await {}
    }
}

#[derive(Clone, Debug)]
pub struct PeerHandle {
    sender: mpsc::Sender<PeerMessage>,
}

impl PeerHandle {
    pub fn spawn(
        ingress: IngressHandle,
        egress: EgressHandle,
        group: GroupHandle,
        peer_id: Arc<PeerId>,
        rtc: Rtc,
    ) -> Self {
        let actor = PeerActor {
            ingress,
            egress,
            group,
            peer_id,
            rtc,
        };
        let (sender, receiver) = mpsc::channel(8);
        tokio::spawn(actor.run(receiver));
        Self { sender }
    }

    pub async fn send(
        &self,
        msg: message::EgressUDPPacket,
    ) -> Result<(), TrySendError<PeerMessage>> {
        self.sender.try_send(PeerMessage::UdpPacket(msg))
    }
}

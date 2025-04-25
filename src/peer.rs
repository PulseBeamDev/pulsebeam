use std::sync::Arc;

use str0m::{
    Candidate, Rtc, RtcError,
    change::{SdpAnswer, SdpOffer},
    error::SdpError,
};
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::{
    group::GroupHandle,
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
        let mut rtc = Rtc::builder()
            // Uncomment this to see statistics
            // .set_stats_interval(Some(Duration::from_secs(1)))
            // .set_ice_lite(true)
            .build();

        // Add the shared UDP socket as a host candidate
        // let candidate = Candidate::host(addr, "udp").expect("a host candidate");
        // rtc.add_local_candidate(candidate);

        // Create an SDP Answer.
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(PeerError::OfferRejected)?;

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
    pub fn spawn(actor: PeerActor) -> Self {
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

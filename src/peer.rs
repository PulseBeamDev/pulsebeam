use str0m::{Candidate, Rtc, RtcError, change::SdpOffer, error::SdpError};
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::message::{self, GroupId, PeerId};

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

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct PeerInfo {
    group_id: GroupId,
    peer_id: PeerId,
}

#[derive(Debug)]
pub struct PeerActor {
    rtc: str0m::Rtc,
    pub info: PeerInfo,
}

impl PeerActor {
    pub fn offer(info: PeerInfo, sdp: &str) -> Result<(Self, String), PeerError> {
        let offer = SdpOffer::from_sdp_string(sdp).map_err(PeerError::InvalidOfferFormat)?;
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

        let actor = PeerActor { rtc, info };
        Ok((actor, answer.to_sdp_string()))
    }

    async fn run(self, mut receiver: mpsc::Receiver<PeerMessage>) {
        while let Some(msg) = receiver.recv().await {}
    }
}

#[derive(Clone)]
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

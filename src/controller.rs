use std::{collections::HashMap, io, net::SocketAddr, sync::Arc};

use crate::{
    egress::EgressHandle,
    group::GroupHandle,
    ingress::IngressHandle,
    message::{GroupId, PeerId},
    peer::PeerHandle,
};
use str0m::{Candidate, Rtc, RtcError, change::SdpOffer, error::SdpError};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

#[derive(thiserror::Error, Debug)]
pub enum ControllerError {
    #[error("sdp offer is invalid: {0}")]
    OfferInvalid(#[from] SdpError),

    #[error("sdp offer is rejected: {0}")]
    OfferRejected(#[from] RtcError),

    #[error("server is busy, please try again later.")]
    ServiceUnavailable,

    #[error("IO error: {0}")]
    IOError(#[from] io::Error),

    #[error("unknown error: {0}")]
    Unknown(String),
}

pub enum ControllerMessage {
    Allocate(
        GroupId,
        PeerId,
        String,
        oneshot::Sender<Result<String, ControllerError>>,
    ),
}

pub struct ControllerActor {
    handle: ControllerHandle,
    ingress: IngressHandle,
    egress: EgressHandle,
    groups: HashMap<Arc<GroupId>, GroupHandle>,

    local_addrs: Vec<SocketAddr>,
}

impl ControllerActor {
    pub async fn run(mut self, mut receiver: mpsc::Receiver<ControllerMessage>) {
        while let Some(msg) = receiver.recv().await {
            match msg {
                ControllerMessage::Allocate(group_id, peer_id, offer, resp) => {
                    resp.send(self.allocate(group_id, peer_id, offer).await);
                }
            }
        }
    }

    pub async fn allocate(
        &mut self,
        group_id: GroupId,
        peer_id: PeerId,
        offer: String,
    ) -> Result<String, ControllerError> {
        let offer = SdpOffer::from_sdp_string(&offer)?;
        let mut rtc = Rtc::builder()
            // Uncomment this to see statistics
            // .set_stats_interval(Some(Duration::from_secs(1)))
            // .set_ice_lite(true)
            .build();

        for addr in self.local_addrs.iter() {
            // TODO: add tcp and ssltcp later
            let candidate = Candidate::host(*addr, "udp").expect("a host candidate");
            rtc.add_local_candidate(candidate);
        }

        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(ControllerError::OfferRejected)?;

        let group_id = Arc::new(group_id);
        let group_handle = if let Some(handle) = self.groups.get(&group_id) {
            handle.clone()
        } else {
            let (handle, join) = GroupHandle::spawn(self.handle.clone(), group_id.clone());
            self.groups.insert(group_id, handle.clone());
            // TODO: handle join
            handle
        };

        let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
        let peer_id = Arc::new(peer_id);
        let peer = PeerHandle::spawn(
            self.egress.clone(),
            group_handle.clone(),
            peer_id.clone(),
            rtc,
        );
        group_handle.add_peer(peer.clone());
        tracing::trace!("added {ufrag} to connection map");
        self.ingress.add_peer(ufrag, peer);

        Ok(answer.to_sdp_string())
    }
}

#[derive(Clone)]
pub struct ControllerHandle {
    sender: mpsc::Sender<ControllerMessage>,
}

impl ControllerHandle {
    pub fn spawn(ingress: IngressHandle, egress: EgressHandle) -> (Self, JoinHandle<()>) {
        let (sender, receiver) = mpsc::channel(1);
        let handle = ControllerHandle { sender };

        let actor = ControllerActor {
            handle: handle.clone(),
            ingress,
            egress,
            groups: HashMap::new(),
            local_addrs: Vec::new(),
        };
        let join = tokio::spawn(actor.run(receiver));
        (handle, join)
    }

    pub async fn allocate(
        &self,
        group_id: GroupId,
        peer_id: PeerId,
        offer: String,
    ) -> Result<String, ControllerError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(ControllerMessage::Allocate(group_id, peer_id, offer, tx))
            .await
            .map_err(|_| ControllerError::ServiceUnavailable)?;
        rx.await.map_err(|_| ControllerError::ServiceUnavailable)?
    }
}

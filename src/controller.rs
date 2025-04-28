use std::sync::Arc;

use crate::{
    egress::EgressHandle,
    ingress::IngressHandle,
    message::{GroupId, PeerId},
    peer::PeerHandle,
};
use dashmap::DashMap;
use str0m::{Rtc, RtcError, change::SdpOffer, error::SdpError};

#[derive(thiserror::Error, Debug)]
pub enum ControllerError {
    #[error("sdp offer is invalid: {0}")]
    OfferInvalid(#[from] SdpError),

    #[error("sdp offer is rejected: {0}")]
    OfferRejected(#[from] RtcError),

    #[error("server is busy, please try again later.")]
    ServiceUnavailable,

    #[error("unknown error: {0}")]
    Unknown(String),
}

#[derive(Clone)]
pub struct Controller(Arc<ControllerState>);

pub struct ControllerState {
    ingress: IngressHandle,
    egress: EgressHandle,
    conns: DashMap<String, PeerHandle>,
    groups: DashMap<Arc<GroupId>, Group>,
}

impl Controller {
    pub fn allocate(
        &self,
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

        // Add the shared UDP socket as a host candidate
        // let candidate = Candidate::host(addr, "udp").expect("a host candidate");
        // rtc.add_local_candidate(candidate);

        // Create an SDP Answer.
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(ControllerError::OfferRejected)?;

        let group_id = Arc::new(group_id);
        let entry = self
            .0
            .groups
            .entry(group_id.clone())
            .or_insert_with(|| Group::new(self.clone(), group_id));

        let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
        let peer_handle = entry.spawn(peer_id, rtc);
        self.0.conns.insert(ufrag, peer_handle);

        Ok(answer.to_sdp_string())
    }
}

#[derive(Clone)]
pub struct Group(Arc<GroupState>);

pub struct GroupState {
    controller: Controller,
    group_id: Arc<GroupId>,
    peers: DashMap<Arc<PeerId>, PeerHandle>,
}

impl Group {
    fn new(controller: Controller, group_id: Arc<GroupId>) -> Self {
        let state = GroupState {
            controller,
            group_id,
            peers: DashMap::new(),
        };
        Self(Arc::new(state))
    }

    pub fn size(&self) -> usize {
        self.0.peers.len()
    }

    pub fn spawn(&self, peer_id: PeerId, rtc: Rtc) -> PeerHandle {
        // PeerHandle::spawn(ingress, egress, group, peer_id, rtc)
        todo!()
    }
}

use std::{collections::HashMap, io, net::SocketAddr, sync::Arc};

use crate::{
    entity::{ExternalParticipantId, ExternalRoomId, ParticipantId, RoomId},
    message::ActorResult,
    participant::ParticipantHandle,
    rng::Rng,
    room::RoomHandle,
    sink::UdpSinkHandle,
    source::UdpSourceHandle,
};
use str0m::{Candidate, Rtc, RtcError, change::SdpOffer, error::SdpError};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use tracing::Instrument;

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
        ExternalRoomId,
        ExternalParticipantId,
        String,
        oneshot::Sender<Result<String, ControllerError>>,
    ),
}

pub struct ControllerActor {
    rng: Rng,
    handle: ControllerHandle,
    source: UdpSourceHandle,
    sink: UdpSinkHandle,
    receiver: mpsc::Receiver<ControllerMessage>,
    rooms: HashMap<Arc<RoomId>, RoomHandle>,

    local_addrs: Vec<SocketAddr>,
    children: JoinSet<()>,
}

impl ControllerActor {
    #[tracing::instrument(skip(self), fields(controller_id = "root"))]
    pub async fn run(mut self) -> ActorResult {
        while let Some(msg) = self.receiver.recv().await {
            match msg {
                ControllerMessage::Allocate(room_id, participant_id, offer, resp) => {
                    let room_id = RoomId::new(room_id);
                    let participant_id = ParticipantId::new(&mut self.rng, participant_id);
                    let _ = resp.send(self.allocate(room_id, participant_id, offer).await);
                }
            }
        }
        Ok(())
    }

    pub async fn allocate(
        &mut self,
        room_id: RoomId,
        participant_id: ParticipantId,
        offer: String,
    ) -> Result<String, ControllerError> {
        let offer = SdpOffer::from_sdp_string(&offer)?;
        let mut rtc = Rtc::builder()
            // Uncomment this to see statistics
            // .set_stats_interval(Some(Duration::from_secs(1)))
            .set_ice_lite(true)
            .enable_vp9(false)
            .enable_h264(false)
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

        let room_id = Arc::new(room_id);
        let room_handle = if let Some(handle) = self.rooms.get(&room_id) {
            handle.clone()
        } else {
            let (handle, actor) =
                RoomHandle::new(self.rng.clone(), self.handle.clone(), room_id.clone());
            // TODO: handle shutdown
            self.children.spawn(actor.run());

            self.rooms.insert(room_id, handle.clone());
            handle
        };

        let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
        let participant_id = Arc::new(participant_id);
        let (participant_handle, participant_actor) = ParticipantHandle::new(
            self.rng.clone(),
            self.source.clone(),
            self.sink.clone(),
            room_handle.clone(),
            participant_id.clone(),
            rtc,
        );

        {
            let ingress = self.source.clone();
            let ufrag = ufrag.clone();
            let room = room_handle.clone();
            let participant_id = participant_handle.participant_id.clone();

            self.children.spawn(
                async move {
                    participant_actor.run().await;
                    ingress.remove_participant(ufrag).await;
                    room.remove_participant(participant_id).await;
                }
                .in_current_span(),
            );
        }

        // room and participant will self-monitor and hit a timeout to cleanup itself
        room_handle
            .add_participant(participant_handle.clone())
            .await
            .map_err(|_| ControllerError::ServiceUnavailable)?;
        self.source
            .add_participant(ufrag, participant_handle)
            .await
            .map_err(|_| ControllerError::ServiceUnavailable)?;

        Ok(answer.to_sdp_string())
    }
}

#[derive(Clone)]
pub struct ControllerHandle {
    sender: mpsc::Sender<ControllerMessage>,
}

impl ControllerHandle {
    pub fn new(
        rng: Rng,
        source: UdpSourceHandle,
        sink: UdpSinkHandle,
        local_addrs: Vec<SocketAddr>,
    ) -> (Self, ControllerActor) {
        let (sender, receiver) = mpsc::channel(1);
        let handle = ControllerHandle { sender };

        let actor = ControllerActor {
            handle: handle.clone(),
            rng,
            receiver,
            source,
            sink,
            rooms: HashMap::new(),
            local_addrs,
            children: JoinSet::new(),
        };
        (handle, actor)
    }

    pub async fn allocate(
        &self,
        room_id: ExternalRoomId,
        participant_id: ExternalParticipantId,
        offer: String,
    ) -> Result<String, ControllerError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(ControllerMessage::Allocate(
                room_id,
                participant_id,
                offer,
                tx,
            ))
            .await
            .map_err(|_| ControllerError::ServiceUnavailable)?;
        rx.await.map_err(|_| ControllerError::ServiceUnavailable)?
    }
}

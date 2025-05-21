use std::{collections::HashMap, io, net::SocketAddr, sync::Arc};

use crate::{
    actor::{self, Actor, ActorError},
    context,
    entity::{ExternalParticipantId, ExternalRoomId, ParticipantId, RoomId},
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
    id: Arc<String>,
    ctx: context::Context,
    receiver: mpsc::Receiver<ControllerMessage>,
    local_addrs: Vec<SocketAddr>,

    rooms: HashMap<Arc<RoomId>, RoomHandle>,
    room_tasks: JoinSet<Arc<RoomId>>,
}

impl Actor for ControllerActor {
    type ID = Arc<String>;

    fn kind(&self) -> &'static str {
        "controller"
    }

    fn id(&self) -> Self::ID {
        self.id.clone()
    }

    async fn run(&mut self) -> Result<(), ActorError> {
        loop {
            tokio::select! {
                Some(msg) = self.receiver.recv() => {
                    match msg {
                        ControllerMessage::Allocate(room_id, participant_id, offer, resp) => {
                            let room_id = RoomId::new(room_id);
                            let participant_id = ParticipantId::new(&mut self.ctx.rng, participant_id);
                            let _ = resp.send(self.allocate(room_id, participant_id, offer).await);
                        }
                    }
                }

                Some(Ok(participant_id)) = self.room_tasks.join_next() => {
                    // TODO: notify participant leaving
                    self.rooms.remove(&participant_id);
                }

                else => break,
            }
        }
        Ok(())
    }
}

impl ControllerActor {
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
        let room_handle = self.get_or_create_room(room_id);
        // TODO: probably retry? Or, let the client to retry instead?
        // Each room will always have a graceful timeout before closing.
        // But, a data race can still occur nonetheless
        room_handle
            .add_participant(Arc::new(participant_id), rtc)
            .await
            .map_err(|_| ControllerError::ServiceUnavailable)?;

        Ok(answer.to_sdp_string())
    }

    fn get_or_create_room(&mut self, room_id: Arc<RoomId>) -> RoomHandle {
        if let Some(handle) = self.rooms.get(&room_id) {
            handle.clone()
        } else {
            let (room_handle, room_actor) = RoomHandle::new(self.ctx.clone(), room_id.clone());
            self.rooms.insert(room_id.clone(), room_handle.clone());
            self.room_tasks.spawn(
                async move {
                    actor::run(room_actor).await;
                    room_id
                }
                .in_current_span(),
            );

            room_handle
        }
    }
}

#[derive(Clone)]
pub struct ControllerHandle {
    sender: mpsc::Sender<ControllerMessage>,
}

impl ControllerHandle {
    pub fn new(
        ctx: context::Context,
        local_addrs: Vec<SocketAddr>,
        id: Arc<String>,
    ) -> (Self, ControllerActor) {
        let (sender, receiver) = mpsc::channel(1);
        let handle = ControllerHandle { sender };

        let actor = ControllerActor {
            id,
            ctx,
            receiver,
            local_addrs,
            rooms: HashMap::new(),
            room_tasks: JoinSet::new(),
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

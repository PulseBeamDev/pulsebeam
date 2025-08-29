use std::{collections::HashMap, io, net::SocketAddr, sync::Arc};

use crate::{
    entity::{ParticipantId, RoomId},
    room, system,
};
use pulsebeam_runtime::actor;
use pulsebeam_runtime::prelude::*;
use str0m::{Candidate, Rtc, RtcError, change::SdpOffer, error::SdpError};
use tokio::{sync::oneshot, task::JoinSet};

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
        RoomId,
        ParticipantId,
        String,
        oneshot::Sender<Result<String, ControllerError>>,
    ),
}

pub struct ControllerActor {
    system_ctx: system::SystemContext,

    id: Arc<String>,
    local_addrs: Vec<SocketAddr>,

    rooms: HashMap<Arc<RoomId>, room::RoomHandle>,
    room_tasks: JoinSet<(Arc<RoomId>, actor::ActorStatus)>,
}

impl actor::Actor for ControllerActor {
    type HighPriorityMessage = ControllerMessage;
    type LowPriorityMessage = ();
    type ID = Arc<String>;

    fn kind(&self) -> &'static str {
        "controller"
    }

    fn id(&self) -> Self::ID {
        self.id.clone()
    }

    async fn run(&mut self, ctx: &mut actor::ActorContext<Self>) -> Result<(), actor::ActorError> {
        loop {
            tokio::select! {
                biased;
                Some(msg) = ctx.hi_rx.recv() => {
                    match msg {
                        ControllerMessage::Allocate(room_id, participant_id, offer, resp) => {
                            let _ = resp.send(self.allocate(ctx, room_id, participant_id, offer).await);
                        }
                    }
                }

                Some(Ok((room_id, _))) = self.room_tasks.join_next() => {
                    self.rooms.remove(&room_id);
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
        ctx: &mut actor::ActorContext<Self>,
        room_id: RoomId,
        participant_id: ParticipantId,
        offer: String,
    ) -> Result<String, ControllerError> {
        let offer = SdpOffer::from_sdp_string(&offer)?;
        let mut rtc = Rtc::builder()
            // Uncomment this to see statistics
            // .set_stats_interval(Some(Duration::from_secs(1)))
            // Some WHIP client like OBS WHIP doesn't actively send a binding request,
            // we need this to keep the timeout refreshed.
            .set_ice_lite(false)
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
            .hi_send(room::RoomMessage::AddParticipant(
                Arc::new(participant_id),
                rtc,
            ))
            .await
            .map_err(|_| ControllerError::ServiceUnavailable)?;

        Ok(answer.to_sdp_string())
    }

    fn get_or_create_room(&mut self, room_id: Arc<RoomId>) -> room::RoomHandle {
        if let Some(handle) = self.rooms.get(&room_id) {
            tracing::info!("get_room: {}", room_id);
            handle.clone()
        } else {
            tracing::info!("create_room: {}", room_id);
            let room_actor = room::RoomActor::new(self.system_ctx.clone(), room_id.clone());
            let (room_handle, room_runner) =
                actor::LocalActorHandle::new(room_actor, actor::RunnerConfig::default());
            self.room_tasks.spawn(room_runner.run());

            self.rooms.insert(room_id.clone(), room_handle.clone());

            room_handle
        }
    }
}

impl ControllerActor {
    pub fn new(
        system_ctx: system::SystemContext,
        local_addrs: Vec<SocketAddr>,
        id: Arc<String>,
    ) -> Self {
        Self {
            id,
            system_ctx,
            local_addrs,
            rooms: HashMap::new(),
            room_tasks: JoinSet::new(),
        }
    }
}

pub type ControllerHandle = actor::LocalActorHandle<ControllerActor>;

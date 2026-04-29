use std::io;

use crate::{
    control::{
        core::{ControllerCore, ControllerEvent, ControllerEventQueue},
        negotiator::NegotiatorError,
        router::ShardRouter,
    },
    entity::{ConnectionId, ParticipantId, RoomId},
    shard::worker::{ClusterCommand, ShardCommand, ShardEvent},
};
use pulsebeam_runtime::mailbox;
use str0m::{
    Candidate,
    change::{SdpAnswer, SdpOffer},
};
use tokio::sync::oneshot;

#[derive(Debug, Clone)]
pub struct ParticipantState {
    pub manual_sub: bool,
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
    pub connection_id: ConnectionId,
    pub old_connection_id: Option<ConnectionId>,
}

#[derive(Debug, derive_more::From)]
pub enum ControllerCommand {
    CreateParticipant(
        CreateParticipant,
        oneshot::Sender<Result<CreateParticipantReply, ControllerError>>,
    ),
    DeleteParticipant(DeleteParticipant),
    PatchParticipant(
        PatchParticipant,
        oneshot::Sender<Result<PatchParticipantReply, ControllerError>>,
    ),
}

#[derive(Debug)]
pub struct CreateParticipant {
    pub state: ParticipantState,
    pub offer: SdpOffer,
}

#[derive(Debug)]
pub struct CreateParticipantReply {
    pub answer: SdpAnswer,
}

#[derive(Debug)]
pub struct DeleteParticipant {
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
}

#[derive(Debug)]
pub struct PatchParticipant {
    pub state: ParticipantState,
    pub offer: SdpOffer,
}

#[derive(Debug)]
pub struct PatchParticipantReply {
    pub answer: SdpAnswer,
}

#[derive(thiserror::Error, Debug)]
pub enum ControllerError {
    #[error("sdp offer is rejected: {0}")]
    OfferRejected(#[from] NegotiatorError),

    #[error("server is busy, please try again later.")]
    ServiceUnavailable,

    #[error("IO error: {0}")]
    IOError(#[from] io::Error),

    #[error("unknown error: {0}")]
    Unknown(String),
}

pub struct ControllerActor {
    router: ShardRouter,
    core: ControllerCore,
    eq: ControllerEventQueue,
}

impl ControllerActor {
    pub fn new(
        _rng: pulsebeam_runtime::rand::Rng,
        shard_command_txs: Vec<mailbox::Sender<ShardCommand>>,
        candidates: Vec<Candidate>,
    ) -> Self {
        let router = ShardRouter::new(shard_command_txs);

        Self {
            router,
            core: ControllerCore::new(candidates),
            eq: ControllerEventQueue::default(),
        }
    }

    pub async fn run(
        mut self,
        mut command_rx: mailbox::Receiver<ControllerCommand>,
        mut shard_event_rx: mailbox::Receiver<ShardEvent>,
    ) {
        loop {
            tokio::select! {
                // let command to backpressure to signal clients to slow down.
                biased;

                Some(ev) = shard_event_rx.recv() => {
                    self.core.process_shard_event(ev, &mut self.eq);
                }

                _ = self.core.next_expired() => {}

                Some(cmd) = command_rx.recv() => {
                    self.process_command(cmd);
                }

                else => break,
            }

            self.drain_core_events().await;
        }
    }

    pub fn process_command(&mut self, cmd: ControllerCommand) {
        match cmd {
            ControllerCommand::CreateParticipant(m, reply_tx) => {
                let answer = self
                    .handle_create_participant(&m.state, m.offer)
                    .map(|res| CreateParticipantReply { answer: res });
                let _ = reply_tx.send(answer);
            }

            ControllerCommand::DeleteParticipant(m) => {
                self.core
                    .delete_participant(&m.participant_id, &mut self.eq);
            }
            ControllerCommand::PatchParticipant(m, reply_tx) => {
                let answer = self
                    .handle_create_participant(&m.state, m.offer)
                    .map(|res| PatchParticipantReply { answer: res });
                let _ = reply_tx.send(answer);
            }
        }
    }

    async fn drain_core_events(&mut self) {
        while let Some(ev) = self.eq.pop() {
            match ev {
                ControllerEvent::ShardCommandBroadcasted(cmd) => self.router.broadcast(cmd).await,
                ControllerEvent::ShardCommandSent(shard_id, cmd) => {
                    self.router.send(shard_id, cmd).await
                }
            }
        }
    }

    pub fn handle_create_participant(
        &mut self,
        state: &ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        let mut stg = self.core.create_participant(state, offer)?;
        let shard_id = self
            .router
            .try_route(&stg.routing_key)
            .ok_or(ControllerError::ServiceUnavailable)?;
        let ufrag = stg.cfg.ufrag();
        self.eq.broadcast(ClusterCommand::RegisterParticipant {
            shard_id,
            participant_id: stg.cfg.participant_id,
            ufrag,
        });
        self.eq
            .send(shard_id, ShardCommand::AddParticipant(stg.cfg));
        Ok(stg.answer)
    }
}

pub type ControllerHandle = mailbox::Sender<ControllerCommand>;

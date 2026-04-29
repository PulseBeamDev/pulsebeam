use std::io;

use crate::{
    control::{
        core::{ControllerCore, ControllerEventQueue},
        negotiator::NegotiatorError,
        router::ShardRouter,
    },
    entity::{ConnectionId, ParticipantId, RoomId},
    shard::worker::{ShardCommand, ShardEvent},
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

struct ParticipantMeta {
    shard_id: usize,
    room_id: RoomId,
}

pub struct ControllerActor {
    router: ShardRouter,
    core: ControllerCore,
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
        }
    }

    pub async fn run(
        mut self,
        mut command_rx: mailbox::Receiver<ControllerCommand>,
        mut shard_event_rx: mailbox::Receiver<ShardEvent>,
    ) {
        let mut eq = ControllerEventQueue::default();
        loop {
            tokio::select! {
                // let command to backpressure to signal clients to slow down.
                biased;

                Some(ev) = shard_event_rx.recv() => {
                    self.core.process_shard_event(ev, &mut eq);
                }

                _ = self.core.next_expired() => {}

                Some(cmd) = command_rx.recv() => {
                    self.core.process_command(cmd, &mut eq);
                }

                else => break,
            }
        }
    }
}

pub type ControllerHandle = mailbox::Sender<ControllerCommand>;

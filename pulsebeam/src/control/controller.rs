use std::io;

use crate::{
    control::{
        core::{ControllerCore, ControllerEvent, ControllerEventQueue},
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
                    self.process_shard_event(ev);
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

    pub fn process_shard_event(&mut self, ev: ShardEvent) {
        todo!();
        // match ev {
        //     ShardEvent::TrackPublished(track) => {
        //         let origin = track.meta.origin;
        //         let Some(room) = self.registry.room_mut_for(&track.meta.origin) else {
        //             return;
        //         };
        //
        //         // TODO: make room shard aware?
        //         let mut shard_ids: IndexMap<usize, ()> = IndexMap::new();
        //         for participant_id in room.participants_iter() {
        //             if *participant_id == origin {
        //                 continue;
        //             }
        //             if let Some(p) = self.registry.get(participant_id) {
        //                 shard_ids.entry(p.shard_id).or_default();
        //             }
        //         }
        //
        //         tracing::info!(
        //             track = %track.meta.id,
        //             %origin,
        //             room_id = ?room_id,
        //             shard_count = shard_ids.len(),
        //             "fanning out track to shards"
        //         );
        //         room.publish_track(track.clone());
        //         for (shard_id, _) in shard_ids {
        //             self.router
        //                 .send(shard_id, ShardCommand::PublishTrack(track.clone(), room_id))
        //                 .await;
        //         }
        //     }
        //
        //     ShardEvent::ParticipantExited(participant_id) => {
        //         self.delete_participant(&participant_id).await;
        //     }
        //     ShardEvent::KeyframeRequest(req) => {
        //         let meta = self.registry.get(&req.origin).or_else(|| {
        //             tracing::warn!(origin = %req.origin, track = ?req.stream_id.0, "KeyframeRequest: origin participant not found in controller");
        //             None
        //         })?;
        //         self.router
        //             .send(meta.shard_id, ShardCommand::RequestKeyframe(req))
        //             .await;
        //     }
        // }
        //
        // Some(())
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
                ControllerEvent::BroadcastShardCommand(cmd) => self.router.broadcast(&cmd).await,
            }
        }
    }

    pub fn handle_create_participant(
        &mut self,
        state: &ParticipantState,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, ControllerError> {
        let stg = self.core.create_participant(state, offer)?;
        let shard_id = self
            .router
            .try_route(&stg.routing_key)
            .ok_or_else(|| ControllerError::ServiceUnavailable)?;
        self.eq.broadcast(ShardCommand::RegisterParticipant {
            shard_id,
            cfg: stg.cfg,
        });
        Ok(stg.answer)
    }
}

pub type ControllerHandle = mailbox::Sender<ControllerCommand>;

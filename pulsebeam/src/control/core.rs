use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};

use crate::{
    control::{
        controller::{ControllerCommand, ControllerError, ParticipantState},
        negotiator::Negotiator,
        registry::RoomRegistry,
        room::Room,
    },
    entity::{ParticipantId, RoomId},
    participant::ParticipantConfig,
    shard::worker::{ShardCommand, ShardEvent},
};
use futures_lite::StreamExt;
use indexmap::IndexMap;
use str0m::{
    Candidate, Rtc, RtcConfig, RtcError,
    change::{SdpAnswer, SdpOffer},
    format::{Codec, FormatParams},
    media::{Direction, Frequency, MediaKind, Pt},
};
use tokio::time::Instant;

const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

pub enum ControllerEvent {
    BroadcastShardCommand(ShardCommand),
}

pub struct ControllerEventQueue {
    queue: VecDeque<ControllerEvent>,
}

impl ControllerEventQueue {
    pub fn default() -> Self {
        Self {
            queue: VecDeque::with_capacity(1024),
        }
    }

    pub fn push(&mut self, ev: ControllerEvent) {
        self.queue.push_back(ev);
    }

    pub fn pop(&mut self) -> Option<ControllerEvent> {
        self.queue.pop_front()
    }

    pub fn broadcast(&mut self, cmd: ShardCommand) {
        self.push(ControllerEvent::BroadcastShardCommand(cmd));
    }
}

struct ParticipantMeta {
    shard_id: usize,
    room_id: RoomId,
}

pub struct ParticipantStaging {
    pub routing_key: String,
    pub cfg: ParticipantConfig,
    pub answer: SdpAnswer,
}

pub struct ControllerCore {
    negotiator: Negotiator,
    registry: RoomRegistry,
}

impl ControllerCore {
    pub fn new(candidates: Vec<Candidate>) -> Self {
        Self {
            negotiator: Negotiator::new(candidates),
            registry: RoomRegistry::new(),
        }
    }

    pub async fn next_expired(&mut self) {
        self.registry.next_expired().await;
    }

    pub fn create_participant(
        &mut self,
        state: &ParticipantState,
        offer: SdpOffer,
    ) -> Result<ParticipantStaging, ControllerError> {
        let (rtc, answer) = self.negotiator.create_answer(offer)?;
        let room = self.registry.get_or_create_room(state.room_id);
        let tracks = room.tracks_for(&state.participant_id);
        let participant_id = state.participant_id;
        let key = room.routing_key();
        let cfg = ParticipantConfig {
            manual_sub: state.manual_sub,
            room_id: state.room_id,
            participant_id,
            rtc,
            available_tracks: tracks.cloned().collect(),
        };
        let stg = ParticipantStaging {
            routing_key: key,
            cfg,
            answer,
        };

        Ok(stg)
    }

    pub fn delete_participant(
        &mut self,
        participant_id: &ParticipantId,
        eq: &mut ControllerEventQueue,
    ) {
        self.registry.remove_participant(participant_id);
        eq.broadcast(ShardCommand::UnregisterParticipant {
            participant_id: *participant_id,
        });
    }
}

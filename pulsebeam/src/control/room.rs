use ahash::{HashMap, HashMapExt};
use pulsebeam_runtime::sync::Arc;
use std::pin::Pin;
use std::{collections::BTreeMap, time::Duration};

use pulsebeam_runtime::{
    actor::{ActorKind, ActorStatus, RunnerConfig},
    prelude::*,
};
use tokio::task::JoinSet;

use crate::participant::ParticipantCore;
use crate::{
    entity::{ConnectionId, ParticipantId, RoomId, TrackId},
    node, participant, track,
};
use futures_util::FutureExt;
use pulsebeam_runtime::actor;
use str0m::media::MediaKind;

const EMPTY_ROOM_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(derive_more::From)]
pub enum RoomMessage {
    PublishTrack(track::TrackReceiver),
    AddParticipant(AddParticipant),
    RemoveParticipant(RemoveParticipant),
}

pub struct AddParticipant {
    pub participant: ParticipantCore,
    pub connection_id: ConnectionId,
    pub old_connection_id: Option<ConnectionId>,
}

pub struct RemoveParticipant {
    pub participant_id: ParticipantId,
}

pub struct Room {
    room_id: RoomId,
    state: RoomState,
}

#[derive(Default, Clone, Debug)]
pub struct RoomState {
    participants: BTreeMap<(ParticipantId, ConnectionId), TrackId>,
}

impl Room {
    pub fn new(room_id: RoomId) -> Self {
        Self {
            room_id,
            state: RoomState::default(),
        }
    }

    pub fn add_participant(&mut self, m: AddParticipant) {
        todo!()
    }

    pub fn remove_participant(&mut self, participant_id: &ParticipantId) {
        todo!()
    }
}

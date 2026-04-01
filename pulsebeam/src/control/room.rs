use std::{collections::BTreeMap, time::Duration};


use crate::participant::ParticipantCore;
use crate::{
    entity::{ConnectionId, ParticipantId, RoomId, TrackId}, track,
};

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

    pub fn add_participant(&mut self, _m: AddParticipant) {
        todo!()
    }

    pub fn remove_participant(&mut self, _participant_id: &ParticipantId) {
        todo!()
    }
}

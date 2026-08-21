use crate::entity::{ParticipantId, TrackKind};
use crate::keys::TrackKey;
use crate::track::Track;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TrackRole {
    Published,
    Subscribed,
}

#[derive(Debug, Clone)]
pub struct CompiledTrack {
    pub key: TrackKey,
    pub role: TrackRole,
    pub track: Track,
}

impl CompiledTrack {
    pub fn kind(&self) -> TrackKind {
        self.track.meta.id.kind()
    }
}

#[derive(Debug, Clone)]
pub enum ParticipantEffect {
    ParticipantsChanged {
        added: Vec<ParticipantId>,
        removed: Vec<ParticipantId>,
    },
    TrackInstalled(CompiledTrack),
    TrackRemoved {
        key: TrackKey,
        role: TrackRole,
        kind: TrackKind,
    },
}

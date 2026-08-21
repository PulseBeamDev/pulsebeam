use crate::entity::{ParticipantId, TrackKind};
use crate::keys::{ReliableStreamKey, TrackKey, UnreliableStreamKey};
use crate::track::{Topic, Track};
use str0m::channel::ChannelId;

#[derive(Debug, Clone)]
pub struct CompiledTrack {
    pub key: TrackKey,
    pub track: Track,
}

impl CompiledTrack {
    pub fn kind(&self) -> TrackKind {
        self.track.kind()
    }
}

#[derive(Debug, Clone)]
pub enum ParticipantEffect {
    ParticipantsChanged {
        added: Vec<ParticipantId>,
        removed: Vec<ParticipantId>,
    },
    TrackInstalled(CompiledTrack),
    TrackRemoved(TrackKey),
    DataPublished {
        topic: Topic,
        stream: UnreliableStreamKey,
    },
    ReliableDataPublished {
        topic: Topic,
        stream: ReliableStreamKey,
    },
    DataSubscribed {
        stream: UnreliableStreamKey,
        channel: ChannelId,
    },
    ReliableDataSubscribed {
        stream: ReliableStreamKey,
        channel: ChannelId,
    },
}

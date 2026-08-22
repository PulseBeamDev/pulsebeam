use crate::entity::ParticipantId;
use crate::keys::TrackKey;
use crate::track::{Topic, Track};
use str0m::channel::ChannelId;

#[derive(Debug, Clone)]
pub enum ParticipantEffect {
    ParticipantsChanged {
        added: Vec<ParticipantId>,
        removed: Vec<ParticipantId>,
    },
    TrackInstalled {
        key: TrackKey,
        track: Track,
    },
    TrackSourceBound {
        key: TrackKey,
        track_id: crate::entity::TrackId,
    },
    TrackSourceUnbound {
        key: TrackKey,
        track_id: crate::entity::TrackId,
    },
    TrackRemoved(TrackKey),
    TrackPublished {
        topic: Topic,
        key: TrackKey,
        lane: crate::track::DataLane,
    },
    TrackSubscribed {
        key: TrackKey,
        channel: ChannelId,
        lane: crate::track::DataLane,
    },
}

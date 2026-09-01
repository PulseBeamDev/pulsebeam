use crate::entity::ParticipantId;
use crate::keys::TrackKey;
use crate::track::Track;

#[derive(Debug, Clone)]
pub enum ParticipantEffect {
    ParticipantsChanged {
        added: Vec<ParticipantId>,
        removed: Vec<ParticipantId>,
    },
    TrackCandidateAdded {
        key: TrackKey,
        track: Track,
    },
    TrackCandidateRemoved {
        key: TrackKey,
        track_id: crate::entity::TrackId,
    },
    TrackSubscribed {
        key: TrackKey,
        track_id: crate::entity::TrackId,
    },
    TrackUnsubscribed {
        key: TrackKey,
        track_id: crate::entity::TrackId,
    },
    TrackPublished {
        key: TrackKey,
        track_id: crate::entity::TrackId,
    },
    TrackUnpublished {
        key: TrackKey,
        track_id: crate::entity::TrackId,
    },
}

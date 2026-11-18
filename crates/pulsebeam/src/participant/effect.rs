use crate::track::Track;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackRemoval {
    Withdrawn,
    SourceEnded,
}

#[derive(Debug, Clone)]
pub enum ParticipantEffect {
    TrackCandidateAdded {
        track: Track,
    },
    TrackCandidateRemoved {
        track_id: crate::entity::TrackId,
        reason: TrackRemoval,
    },
    TrackSubscribed {
        track_id: crate::entity::TrackId,
    },
    TrackUnsubscribed {
        track_id: crate::entity::TrackId,
    },
    TrackPublished {
        track_id: crate::entity::TrackId,
    },
    TrackUnpublished {
        track_id: crate::entity::TrackId,
    },
}

impl ParticipantEffect {
    pub(crate) fn track_id(&self) -> Option<crate::entity::TrackId> {
        match self {
            Self::TrackCandidateAdded { track } => Some(track.id()),
            Self::TrackCandidateRemoved { track_id, .. }
            | Self::TrackSubscribed { track_id }
            | Self::TrackUnsubscribed { track_id }
            | Self::TrackPublished { track_id }
            | Self::TrackUnpublished { track_id } => Some(*track_id),
        }
    }
}

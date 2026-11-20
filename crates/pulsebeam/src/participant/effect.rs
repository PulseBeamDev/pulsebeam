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
        publication_id: crate::entity::PublicationKey,
        reason: TrackRemoval,
    },
    TrackSubscribed {
        publication_id: crate::entity::PublicationKey,
    },
    TrackUnsubscribed {
        publication_id: crate::entity::PublicationKey,
    },
    TrackPublished {
        publication_id: crate::entity::PublicationKey,
    },
    TrackUnpublished {
        publication_id: crate::entity::PublicationKey,
    },
}

impl ParticipantEffect {
    pub(crate) fn publication_id(&self) -> Option<crate::entity::PublicationKey> {
        match self {
            Self::TrackCandidateAdded { track } => Some(track.id()),
            Self::TrackCandidateRemoved { publication_id, .. }
            | Self::TrackSubscribed { publication_id }
            | Self::TrackUnsubscribed { publication_id }
            | Self::TrackPublished { publication_id }
            | Self::TrackUnpublished { publication_id } => Some(*publication_id),
        }
    }
}

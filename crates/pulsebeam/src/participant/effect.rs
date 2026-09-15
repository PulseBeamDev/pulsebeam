use crate::entity::{ConnectionId, ParticipantExternalId, ParticipantId};
use crate::track::Track;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthorizationRequestId(u64);

impl AuthorizationRequestId {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }
}

/// Secret-bearing renewal input.  Keep the token out of derived formatting so
/// a queued shard event can never disclose it through diagnostics.
pub(crate) struct RenewalToken(String);

impl RenewalToken {
    pub(crate) const MAX_BYTES: usize = 16_384;

    pub(crate) fn new(token: String) -> Option<Self> {
        (!token.is_empty() && token.len() <= Self::MAX_BYTES).then_some(Self(token))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for RenewalToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RenewalToken(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomParticipant {
    pub id: ParticipantId,
    pub external_id: ParticipantExternalId,
}

#[derive(Debug, Clone)]
pub enum ParticipantEffect {
    ParticipantsChanged {
        added: Vec<RoomParticipant>,
        removed: Vec<ParticipantId>,
    },
    TrackCandidateAdded {
        track: Track,
    },
    TrackCandidateRemoved {
        track_id: crate::entity::TrackId,
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
    AuthorizationRenewed {
        participant_id: ParticipantId,
        connection_id: ConnectionId,
        request_id: AuthorizationRequestId,
        expires_at_unix_seconds: i64,
    },
    AuthorizationRejected {
        participant_id: ParticipantId,
        connection_id: ConnectionId,
        request_id: AuthorizationRequestId,
    },
}

impl ParticipantEffect {
    pub(crate) fn track_id(&self) -> Option<crate::entity::TrackId> {
        match self {
            Self::ParticipantsChanged { .. } => None,
            Self::TrackCandidateAdded { track } => Some(track.id()),
            Self::TrackCandidateRemoved { track_id }
            | Self::TrackSubscribed { track_id }
            | Self::TrackUnsubscribed { track_id }
            | Self::TrackPublished { track_id }
            | Self::TrackUnpublished { track_id } => Some(*track_id),
            Self::AuthorizationRenewed { .. } | Self::AuthorizationRejected { .. } => None,
        }
    }
}

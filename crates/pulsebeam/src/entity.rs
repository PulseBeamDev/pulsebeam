pub use pulsebeam_core::identity::{
    ConnectionId, IdValidationError, ParticipantId, ProjectId, PublicationId, PublicationKey,
    RoomId, TrackId, TrackKind,
};

#[cfg(test)]
pub(crate) fn fixture_participant_id() -> ParticipantId {
    ParticipantId::new(&ConnectionId::new().as_str()).unwrap()
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectRoomKey {
    pub project: ProjectId,
    pub room: RoomId,
}

impl std::fmt::Debug for ProjectRoomKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectRoomKey")
            .field("project", &self.project)
            .finish_non_exhaustive()
    }
}

impl ProjectRoomKey {
    #[cfg(test)]
    pub(crate) fn fixture(room: RoomId) -> Self {
        Self {
            project: *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID,
            room,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct EndpointKey {
    pub room: ProjectRoomKey,
    pub participant: ParticipantId,
}

impl std::fmt::Debug for EndpointKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointKey")
            .field("room", &self.room)
            .finish_non_exhaustive()
    }
}

/// Who a forwarded audio packet came from.
///
/// The subscriber's audio slots are shared, so both identities travel with a
/// packet to preserve attribution when a slot changes owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AudioOrigin {
    pub participant: ParticipantId,
    pub track: PublicationKey,
}

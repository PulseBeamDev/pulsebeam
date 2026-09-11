pub use pulsebeam_core::identity::{
    AudioTrackId, ConnectionId, DataTrackId, IdValidationError, ParticipantExternalId,
    ParticipantId, ProjectId, RoomExternalId, RoomId, TrackId, TrackKind, VideoTrackId,
};

/// Who a forwarded audio packet came from.
///
/// The subscriber's audio slots are shared, so both identities travel with a
/// packet to preserve attribution when a slot changes owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AudioOrigin {
    pub participant: ParticipantId,
    pub track: TrackId,
}

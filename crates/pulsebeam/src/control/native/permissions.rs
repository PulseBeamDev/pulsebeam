use super::ResourcePermissions;
use pulsebeam_core::auth::grants::{Action, TrackKind};
use pulsebeam_proto::state as wire;

impl ResourcePermissions {
    pub(super) fn can_discover_participant(&self, participant: Option<&str>) -> bool {
        self.allows(Action::Discover, participant, None, None)
    }

    pub(super) fn can_discover_source(&self, source: &super::ResolvedSource<'_>) -> bool {
        self.allows_source(
            Action::Discover,
            Some(source.publisher.as_str()),
            source.track,
        )
    }

    pub(super) fn can_discover_track(&self, publisher: &str, track: &wire::Track) -> bool {
        let Some(kind) = descriptor_kind(track) else {
            return false;
        };
        self.allows(
            Action::Discover,
            Some(publisher),
            Some(kind),
            Some(&track.label),
        )
    }

    pub(super) fn permits_intent(&self, spec: &wire::ResourceSpec) -> bool {
        match spec.declaration.as_ref() {
            Some(wire::resource_spec::Declaration::Publication(publication)) => {
                self.can_publish(publication)
            }
            Some(wire::resource_spec::Declaration::Subscription(subscription)) => {
                self.may_subscribe(subscription.kind)
            }
            None => false,
        }
    }

    pub(super) fn can_publish(&self, publication: &wire::PublicationSpec) -> bool {
        let kind = match publication.kind.as_ref() {
            Some(wire::publication_spec::Kind::Audio(_)) => TrackKind::Audio,
            Some(wire::publication_spec::Kind::Video(_)) => TrackKind::Video,
            Some(wire::publication_spec::Kind::Data(_)) => TrackKind::Data,
            None => return false,
        };
        self.allows(Action::Publish, None, Some(kind), Some(&publication.label))
    }

    pub(super) fn may_subscribe(&self, kind: i32) -> bool {
        track_kind(kind).is_some_and(|kind| self.may_subscribe_kind(kind))
    }

    pub(super) fn can_subscribe(
        &self,
        participant: &wire::Participant,
        track: &wire::Track,
    ) -> bool {
        let Some(kind) = descriptor_kind(track) else {
            return false;
        };
        self.allows(
            Action::Subscribe,
            Some(&participant.participant_external_id),
            Some(kind),
            Some(&track.label),
        )
    }
}

fn descriptor_kind(track: &wire::Track) -> Option<TrackKind> {
    match track.kind.as_ref()? {
        wire::track::Kind::Audio(_) => Some(TrackKind::Audio),
        wire::track::Kind::Video(_) => Some(TrackKind::Video),
        wire::track::Kind::Data(_) => Some(TrackKind::Data),
    }
}

fn track_kind(kind: i32) -> Option<TrackKind> {
    match wire::TrackKind::try_from(kind).ok()? {
        wire::TrackKind::Audio => Some(TrackKind::Audio),
        wire::TrackKind::Video => Some(TrackKind::Video),
        wire::TrackKind::Data => Some(TrackKind::Data),
        wire::TrackKind::Unspecified => None,
    }
}

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
            Some(&track.track_id),
        )
    }

    pub(super) fn permits_intent(&self, spec: &wire::ResourceSpec) -> bool {
        match spec.declaration.as_ref() {
            Some(wire::resource_spec::Declaration::Publication(publication)) => {
                self.can_publish(publication)
            }
            Some(wire::resource_spec::Declaration::Subscription(subscription)) => {
                self.may_subscribe_in_room(&subscription.source_room, subscription.kind)
            }
            Some(wire::resource_spec::Declaration::SourceLookup(lookup)) => lookup
                .source
                .as_ref()
                .is_some_and(|source| self.can_lookup_source(source)),
            Some(wire::resource_spec::Declaration::LogicalSubscription(subscription)) => {
                subscription
                    .source
                    .as_ref()
                    .is_some_and(|source| self.can_lookup_source(source))
            }
            Some(wire::resource_spec::Declaration::SubscriptionSelector(_)) | None => false,
        }
    }

    fn can_lookup_source(&self, source: &wire::LogicalSourceAddress) -> bool {
        let Some(kind) = track_kind(source.kind) else {
            return false;
        };
        match self {
            super::ResourcePermissions::Denied => false,
            #[cfg(test)]
            super::ResourcePermissions::RoomBound => true,
            super::ResourcePermissions::Grants { grants, .. } => grants.allows(
                Action::Subscribe,
                &source.room,
                Some(&source.participant_id),
                Some(kind),
                Some(&source.track_id),
            ),
        }
    }

    pub(super) fn can_publish(&self, publication: &wire::PublicationSpec) -> bool {
        let kind = match publication.kind.as_ref() {
            Some(wire::publication_spec::Kind::Audio(_)) => TrackKind::Audio,
            Some(wire::publication_spec::Kind::Video(_)) => TrackKind::Video,
            Some(wire::publication_spec::Kind::Data(_)) => TrackKind::Data,
            None => return false,
        };
        self.allows(
            Action::Publish,
            None,
            Some(kind),
            Some(&publication.track_id),
        )
    }

    pub(super) fn may_subscribe_in_room(&self, room: &str, kind: i32) -> bool {
        track_kind(kind).is_some_and(|kind| self.has_subscription_kind_in_room(room, kind))
    }

    pub(super) fn can_subscribe(
        &self,
        source_room: &str,
        participant: &wire::Participant,
        track: &wire::Track,
    ) -> bool {
        let Some(kind) = descriptor_kind(track) else {
            return false;
        };
        self.allows_at_room(
            Action::Subscribe,
            source_room,
            Some(&participant.participant_id),
            Some(kind),
            Some(&track.track_id),
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

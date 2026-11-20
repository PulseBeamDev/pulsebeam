use crate::entity::RoomId;
use crate::track::Track;
use pulsebeam_core::auth::grants::{Action, GrantSet, TrackKind};

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResourcePermissions {
    #[default]
    Denied,
    // Unrestricted unit-fixture policy is never an accepted credential profile.
    #[cfg(test)]
    RoomBound,
    Grants {
        room: RoomId,
        grants: GrantSet,
    },
}

impl ResourcePermissions {
    pub(super) fn allows(
        &self,
        action: Action,
        participant: Option<&str>,
        kind: Option<TrackKind>,
        label: Option<&str>,
    ) -> bool {
        match self {
            Self::Denied => false,
            #[cfg(test)]
            Self::RoomBound => true,
            Self::Grants { room, grants } => {
                grants.allows(action, room.as_str(), participant, kind, label)
            }
        }
    }

    pub(super) fn allows_join(&self, room: &RoomId) -> bool {
        if matches!(self, Self::Grants { room: scoped, .. } if scoped != room) {
            return false;
        }
        self.allows(Action::Join, None, None, None)
    }

    pub(super) fn allows_at_room(
        &self,
        action: Action,
        room: &str,
        participant: Option<&str>,
        kind: Option<TrackKind>,
        label: Option<&str>,
    ) -> bool {
        match self {
            Self::Denied => false,
            #[cfg(test)]
            Self::RoomBound => true,
            Self::Grants { grants, .. } => grants.allows(action, room, participant, kind, label),
        }
    }

    pub(super) fn allows_source(
        &self,
        action: Action,
        publisher: Option<&str>,
        track: &Track,
    ) -> bool {
        let kind = match track {
            Track::Audio(_) => TrackKind::Audio,
            Track::Video(_) => TrackKind::Video,
            Track::Data(_) => TrackKind::Data,
        };
        self.allows(action, publisher, Some(kind), track.meta().label.as_deref())
    }

    pub(super) fn allows_source_at_room(
        &self,
        action: Action,
        room: &str,
        publisher: Option<&str>,
        track: &Track,
    ) -> bool {
        let kind = match track {
            Track::Audio(_) => TrackKind::Audio,
            Track::Video(_) => TrackKind::Video,
            Track::Data(_) => TrackKind::Data,
        };
        self.allows_at_room(
            action,
            room,
            publisher,
            Some(kind),
            track.meta().label.as_deref(),
        )
    }

    pub(super) fn topic_publications(&self) -> crate::participant::policy::TopicPublications {
        use crate::participant::policy::TopicPublications;
        match self {
            Self::Denied => TopicPublications::Denied,
            #[cfg(test)]
            Self::RoomBound => TopicPublications::All,
            Self::Grants { room, grants } => {
                let mut labels = Vec::new();
                for label in grants.publication_labels(room.as_str(), TrackKind::Data) {
                    let Some(label) = label else {
                        return TopicPublications::All;
                    };
                    labels.push(label.to_owned());
                }
                if labels.is_empty() {
                    TopicPublications::Denied
                } else {
                    TopicPublications::Labels(labels)
                }
            }
        }
    }

    pub(super) fn has_subscription_kind_in_room(&self, room: &str, kind: TrackKind) -> bool {
        match self {
            Self::Denied => false,
            #[cfg(test)]
            Self::RoomBound => true,
            Self::Grants { grants, .. } => grants.may_subscribe(room, kind),
        }
    }
}

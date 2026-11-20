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
        joined_room: Option<RoomId>,
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
            Self::Grants {
                joined_room,
                grants,
            } => joined_room
                .as_ref()
                .is_some_and(|room| grants.allows(action, room.as_str(), participant, kind, label)),
        }
    }

    pub(super) fn allows_join(&self, room: &RoomId) -> bool {
        self.allows_at_room(Action::Join, room.as_str(), None, None, None)
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
            Self::Grants {
                joined_room,
                grants,
            } => {
                let scope_matches = !matches!(action, Action::Join | Action::Publish)
                    || joined_room
                        .as_ref()
                        .is_some_and(|joined| joined.as_str() == room);
                scope_matches && grants.allows(action, room, participant, kind, label)
            }
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
        self.allows(
            action,
            publisher,
            Some(kind),
            track
                .meta()
                .track_id
                .as_ref()
                .map(crate::entity::TrackId::as_str),
        )
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
            track
                .meta()
                .track_id
                .as_ref()
                .map(crate::entity::TrackId::as_str),
        )
    }

    pub(super) fn topic_publications(&self) -> crate::participant::policy::TopicPublications {
        use crate::participant::policy::TopicPublications;
        match self {
            Self::Denied => TopicPublications::Denied,
            #[cfg(test)]
            Self::RoomBound => TopicPublications::All,
            Self::Grants {
                joined_room,
                grants,
            } => {
                let Some(room) = joined_room else {
                    return TopicPublications::Denied;
                };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::participant::policy::TopicPublications;

    #[test]
    fn explicit_room_grants_do_not_grant_presence_or_publication() {
        let room = RoomId::new("stage").unwrap();
        let grants: GrantSet = serde_json::from_value(serde_json::json!([
            {"rooms": ["*"], "allow": ["join", "publish", "discover", "subscribe"]}
        ]))
        .unwrap();
        let listener = ResourcePermissions::Grants {
            joined_room: None,
            grants: grants.clone(),
        };
        assert!(!listener.allows_join(&room));
        assert!(!listener.allows(Action::Publish, None, Some(TrackKind::Data), Some("chat")));
        assert!(!listener.allows_at_room(
            Action::Publish,
            room.as_str(),
            None,
            Some(TrackKind::Data),
            Some("chat")
        ));
        assert!(matches!(
            listener.topic_publications(),
            TopicPublications::Denied
        ));
        assert!(listener.allows_at_room(
            Action::Discover,
            room.as_str(),
            Some("alice"),
            None,
            None
        ));
        assert!(listener.allows_at_room(
            Action::Subscribe,
            room.as_str(),
            Some("alice"),
            Some(TrackKind::Data),
            Some("chat")
        ));
        let publisher = ResourcePermissions::Grants {
            joined_room: Some(room),
            grants,
        };
        assert!(publisher.allows_join(&room));
        assert!(publisher.allows(Action::Publish, None, Some(TrackKind::Data), Some("chat")));
        assert!(!publisher.allows_at_room(
            Action::Publish,
            "other",
            None,
            Some(TrackKind::Data),
            Some("chat")
        ));
    }
}

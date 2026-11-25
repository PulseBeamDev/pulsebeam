use crate::entity::{ParticipantId, ProjectId, ProjectRoomKey, RoomId, TrackId};
use crate::track::Track;
use pulsebeam_core::auth::grants::{Action, GrantSet, TrackKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Component<T> {
    Literal(T),
    Any,
}

impl<T: Copy + Eq> Component<T> {
    pub(super) fn literal(self) -> Option<T> {
        match self {
            Self::Literal(value) => Some(value),
            Self::Any => None,
        }
    }

    pub(super) fn matches(self, actual: T) -> bool {
        self.literal().is_none_or(|expected| expected == actual)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SubscriptionSelector {
    pub project: Component<ProjectId>,
    pub room: Component<RoomId>,
    pub participant: Component<ParticipantId>,
    pub kind: Component<crate::entity::TrackKind>,
    pub track: Component<TrackId>,
}

impl SubscriptionSelector {
    pub(super) fn from_wire(
        spec: &pulsebeam_proto::state::SubscriptionSelectorSpec,
    ) -> Option<Self> {
        fn parse<T>(value: Option<&str>) -> Option<Component<T>>
        where
            T: std::str::FromStr,
        {
            match value {
                Some(value) => value.parse().ok().map(Component::Literal),
                None => Some(Component::Any),
            }
        }
        Some(Self {
            project: parse(spec.project_id.as_deref())?,
            room: parse(spec.room.as_deref())?,
            participant: parse(spec.participant_id.as_deref())?,
            kind: match spec.kind {
                Some(kind) => Component::Literal(super::native::track_kind(kind)?),
                None => Component::Any,
            },
            track: parse(spec.track_id.as_deref())?,
        })
    }
}

pub(super) fn grant_kind(kind: crate::entity::TrackKind) -> TrackKind {
    match kind {
        crate::entity::TrackKind::Audio => TrackKind::Audio,
        crate::entity::TrackKind::Video => TrackKind::Video,
        crate::entity::TrackKind::Data => TrackKind::Data,
    }
}

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
    pub(super) fn admits_selector(
        &self,
        project: ProjectId,
        selector: SubscriptionSelector,
    ) -> bool {
        if !selector.project.matches(project) {
            return false;
        }
        match self {
            Self::Denied => false,
            #[cfg(test)]
            Self::RoomBound => true,
            Self::Grants { grants, .. } => grants.may_match_subscription(
                selector.room.literal().as_ref().map(RoomId::as_str),
                selector
                    .participant
                    .literal()
                    .as_ref()
                    .map(ParticipantId::as_str),
                selector.kind.literal().map(grant_kind),
                selector.track.literal().as_ref().map(TrackId::as_str),
            ),
        }
    }

    pub(super) fn export_subscribe_scope(
        &self,
        project: ProjectId,
        selector: SubscriptionSelector,
    ) -> Option<pulsebeam_core::auth::grants::SubscribeScope> {
        if !selector.project.matches(project) {
            return None;
        }
        let Self::Grants { grants, .. } = self else {
            return None;
        };
        grants.subscribe_scope(
            selector.room.literal().as_ref().map(RoomId::as_str),
            selector
                .participant
                .literal()
                .as_ref()
                .map(ParticipantId::as_str),
            selector.kind.literal().map(grant_kind),
            selector.track.literal().as_ref().map(TrackId::as_str),
        )
    }

    pub(super) fn allows_selector_match(
        &self,
        project: ProjectId,
        selector: SubscriptionSelector,
        room: ProjectRoomKey,
        publisher: ParticipantId,
        kind: crate::entity::TrackKind,
        track: TrackId,
    ) -> bool {
        // Even Any project cannot escape the connection's authority.
        room.project == project
            && selector.project.matches(project)
            && selector.room.matches(room.room)
            && selector.participant.matches(publisher)
            && selector.kind.matches(kind)
            && selector.track.matches(track)
            && self.allows_at_room(
                Action::Subscribe,
                room.room.as_str(),
                Some(publisher.as_str()),
                Some(grant_kind(kind)),
                Some(track.as_str()),
            )
    }

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

    #[test]
    fn selector_wire_components_parse_as_typed_literals_or_any() {
        use pulsebeam_proto::state::{SubscriptionSelectorSpec, TrackKind as WireKind};

        let project = ProjectId::new();
        let spec = SubscriptionSelectorSpec {
            project_id: Some(project.to_string()),
            room: None,
            participant_id: Some("alice".into()),
            kind: Some(WireKind::Video as i32),
            track_id: None,
        };
        assert_eq!(
            SubscriptionSelector::from_wire(&spec),
            Some(SubscriptionSelector {
                project: Component::Literal(project),
                room: Component::Any,
                participant: Component::Literal(ParticipantId::new("alice").unwrap()),
                kind: Component::Literal(crate::entity::TrackKind::Video),
                track: Component::Any,
            })
        );
        for malformed in [
            SubscriptionSelectorSpec {
                project_id: Some("p_bad".into()),
                ..spec.clone()
            },
            SubscriptionSelectorSpec {
                room: Some("*".into()),
                ..spec.clone()
            },
            SubscriptionSelectorSpec {
                track_id: Some("camera*".into()),
                ..spec.clone()
            },
            SubscriptionSelectorSpec {
                kind: Some(WireKind::Unspecified as i32),
                ..spec
            },
        ] {
            assert!(SubscriptionSelector::from_wire(&malformed).is_none());
        }
    }

    #[test]
    fn wildcard_selector_does_not_widen_room_grants_or_project_authority() {
        let project = ProjectId::new();
        let room = ProjectRoomKey {
            project,
            room: RoomId::new("allowed").unwrap(),
        };
        let publisher = ParticipantId::new("alice").unwrap();
        let track = TrackId::new("camera").unwrap();
        let permissions = ResourcePermissions::Grants {
            joined_room: None,
            grants: serde_json::from_value(serde_json::json!([
                {"rooms": ["allowed"], "allow": ["subscribe"], "participant": "alice", "kind": "video", "label": "camera"}
            ])).unwrap(),
        };
        let selector = SubscriptionSelector {
            project: Component::Any,
            room: Component::Any,
            participant: Component::Any,
            kind: Component::Any,
            track: Component::Any,
        };
        let allows = |room, publisher| {
            permissions.allows_selector_match(
                project,
                selector,
                room,
                publisher,
                crate::entity::TrackKind::Video,
                track,
            )
        };
        assert!(permissions.admits_selector(project, selector));
        assert!(allows(room, publisher));
        assert!(!allows(
            ProjectRoomKey {
                project: ProjectId::new(),
                ..room
            },
            publisher
        ));
        assert!(!allows(
            ProjectRoomKey {
                room: RoomId::new("denied").unwrap(),
                ..room
            },
            publisher
        ));
        assert!(!allows(room, ParticipantId::new("bob").unwrap()));
        assert!(!permissions.admits_selector(
            project,
            SubscriptionSelector {
                project: Component::Literal(ProjectId::new()),
                ..selector
            }
        ));
        assert!(!permissions.admits_selector(
            project,
            SubscriptionSelector {
                track: Component::Literal(TrackId::new("other").unwrap()),
                ..selector
            }
        ));
    }

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

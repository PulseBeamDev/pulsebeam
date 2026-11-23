use std::num::NonZeroU64;

use pulsebeam_core::fabric::{
    MAJOR, Profile, REGISTRY_REVISION,
    wan::{WINDOW_SPAN, Window},
};
use serde::{Deserialize, Serialize};

use crate::{
    control::controller::ownership::observations::{EndpointFact, Snapshot},
    entity::{
        ConnectionId, EndpointKey, ProjectId, ProjectRoomKey, PublicationId, PublicationKey, RoomId,
    },
    route::fabric::local::Emission,
    shard_update::PublisherAnchor,
};

use super::fabric::WanLiveRouteAck;

pub(in crate::control) const WINDOWS_PER_SOURCE: usize = 3;

/// Keep the current position window for every eligible source first. Under
/// pressure, retain an advertised predecessor before prearming a successor.
pub(in crate::control) fn plan_windows(
    mut current: Vec<WanSourceWindow>,
    previous: &[WanSourceWindow],
    limit: usize,
) -> Box<[WanSourceWindow]> {
    current.sort_unstable_by_key(|source| source.scope());
    current.truncate(limit);
    let stable = |candidate: WanSourceWindow| {
        previous
            .iter()
            .find(|&source| {
                source.room == candidate.room
                    && source.publisher == candidate.publisher
                    && source.window_start == candidate.window_start
            })
            .copied()
    };
    for source in &mut current {
        *source = stable(*source).unwrap_or(*source);
    }
    let sources = current.len();
    for index in 0..sources {
        if current.len() >= limit {
            break;
        }
        if let Some(&source) = current.get(index)
            && let Some(window_start) = source.window_start.checked_sub(WINDOW_SPAN)
            && let Some(predecessor) = stable(WanSourceWindow {
                window_start,
                ..source
            })
        {
            current.push(predecessor);
        }
    }
    for index in 0..sources {
        if current.len() >= limit {
            break;
        }
        if let Some(&source) = current.get(index)
            && let Some(window_start) = source.window_start.checked_add(WINDOW_SPAN)
        {
            let successor = WanSourceWindow {
                window_start,
                ..source
            };
            current.push(stable(successor).unwrap_or(successor));
        }
    }
    current.sort_unstable_by_key(|source| (source.scope(), source.window_start));
    current.into_boxed_slice()
}

/// One publisher-owned position window, before the receiver allocates its SSRC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::control) struct WanSourceWindow {
    pub room: ProjectRoomKey,
    pub publisher: PublisherAnchor,
    pub sequence_anchor: u64,
    pub window_start: u64,
}

impl WanSourceWindow {
    pub(in crate::control) fn from_sample(
        room: ProjectRoomKey,
        publisher: PublisherAnchor,
        sequence_anchor: u64,
    ) -> Self {
        Self {
            room,
            publisher,
            sequence_anchor,
            window_start: Window::start_for(sequence_anchor),
        }
    }

    pub(in crate::control) fn scope(self) -> super::Key {
        super::Key::new(self.room, self.publisher.publication, false)
    }

    pub(in crate::control) fn forms(self) -> u8 {
        if self.publisher.emission.wire_profile().1 == 0 {
            0
        } else {
            3
        }
    }

    pub(in crate::control) fn matches_ack(self, ack: WanLiveRouteAck) -> bool {
        self.room == ack.room
            && self.publisher.publication == ack.publication
            && self.publisher.incarnation == ack.incarnation
            && self.sequence_anchor == ack.sequence_anchor
            && self.window_start == ack.window.start()
            && self.forms() == ack.window.forms()
            && self.publisher.emission.profile() == ack.window.profile()
            && self.publisher.emission.wire_profile().1 == ack.window.emission()
    }

    pub(in crate::control) fn present_in(self, snapshot: &Snapshot) -> bool {
        snapshot
            .iter()
            .any(|(key, endpoint)| self.matches_fact(*key, endpoint))
    }

    pub(in crate::control) fn matches_fact(
        self,
        key: EndpointKey,
        endpoint: &EndpointFact,
    ) -> bool {
        key.room == self.room
            && endpoint.claim.connection() == self.publisher.connection
            && endpoint
                .publications
                .iter()
                .any(|fact| match self.publisher.emission.profile() {
                    Profile::Data => fact
                        .data_publisher(self.publisher.connection)
                        .is_some_and(|data| PublisherAnchor::from(data) == self.publisher),
                    Profile::Opus => fact.opus_publication() == Some(self.publisher.publication),
                    _ => false,
                })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::control) struct WireWanSourceWindow {
    project: ProjectId,
    room: RoomId,
    publication: PublicationId,
    connection: ConnectionId,
    incarnation: NonZeroU64,
    sequence_anchor: u64,
    window_start: u64,
    profile: u8,
    extensions: u16,
    forms: u8,
}

impl From<WanSourceWindow> for WireWanSourceWindow {
    fn from(source: WanSourceWindow) -> Self {
        let (profile, extensions) = source.publisher.emission.wire_profile();
        Self {
            project: source.room.project,
            room: source.room.room,
            publication: source.publisher.publication.publication_id(),
            connection: source.publisher.connection,
            incarnation: source.publisher.incarnation,
            sequence_anchor: source.sequence_anchor,
            window_start: source.window_start,
            profile,
            extensions,
            forms: source.forms(),
        }
    }
}

impl WireWanSourceWindow {
    pub(in crate::control) fn decode(self) -> Option<WanSourceWindow> {
        let emission =
            Emission::from_wire_profile(self.profile, self.extensions, MAJOR, REGISTRY_REVISION)?;
        let publication = match emission.profile() {
            Profile::Data => PublicationKey::Data(self.publication),
            Profile::Opus => PublicationKey::Audio(self.publication),
            _ => return None,
        };
        let source = WanSourceWindow {
            room: ProjectRoomKey {
                project: self.project,
                room: self.room,
            },
            publisher: PublisherAnchor {
                publication,
                connection: self.connection,
                incarnation: self.incarnation,
                emission,
            },
            sequence_anchor: self.sequence_anchor,
            window_start: self.window_start,
        };
        (Window::start_for(self.window_start) == self.window_start && source.forms() == self.forms)
            .then_some(source)
    }
}

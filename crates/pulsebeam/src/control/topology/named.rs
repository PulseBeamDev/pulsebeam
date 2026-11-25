use std::{
    collections::{BTreeMap, HashMap, HashSet},
    num::NonZeroUsize,
    ops::Bound,
};

use super::super::permissions::SubscriptionSelector;
use crate::entity::{
    ParticipantId, ProjectId, ProjectRoomKey, PublicationKey, RoomId, TrackId, TrackKind,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NamedTrack {
    pub room: RoomId,
    pub participant: ParticipantId,
    pub kind: TrackKind,
    pub track: TrackId,
}

// Absent suffixes are seek bounds only, before all concrete keys with that prefix.
type Position = (RoomId, Option<ParticipantId>, Option<(TrackKind, TrackId)>);

impl NamedTrack {
    fn position(self) -> Position {
        (
            self.room,
            Some(self.participant),
            Some((self.kind, self.track)),
        )
    }
}

#[derive(Debug, Default)]
pub(super) struct NamedSources {
    projects: HashMap<ProjectId, BTreeMap<Position, HashSet<PublicationKey>>>,
}

impl NamedSources {
    pub(super) fn insert(&mut self, project: ProjectId, name: NamedTrack, id: PublicationKey) {
        let inserted = self
            .projects
            .entry(project)
            .or_default()
            .entry(name.position())
            .or_default()
            .insert(id);
        debug_assert!(inserted);
    }

    pub(super) fn remove(&mut self, project: ProjectId, name: NamedTrack, id: PublicationKey) {
        let Some(index) = self.projects.get_mut(&project) else {
            debug_assert!(false, "missing named project index");
            return;
        };
        if let std::collections::btree_map::Entry::Occupied(mut entry) =
            index.entry(name.position())
        {
            let removed = entry.get_mut().remove(&id);
            debug_assert!(removed);
            if entry.get().is_empty() {
                entry.remove();
            }
        } else {
            debug_assert!(false, "missing named publication index");
        }
        if index.is_empty() {
            self.projects.remove(&project);
        }
    }

    pub(super) fn exact(
        &self,
        room: ProjectRoomKey,
        participant: ParticipantId,
        kind: TrackKind,
        track: TrackId,
    ) -> impl Iterator<Item = PublicationKey> + '_ {
        self.projects
            .get(&room.project)
            .and_then(|index| {
                index.get(
                    &NamedTrack {
                        room: room.room,
                        participant,
                        kind,
                        track,
                    }
                    .position(),
                )
            })
            .into_iter()
            .flat_map(|publications| publications.iter().copied())
    }

    /// Visit at most `budget` indexed names, not publications or successful
    /// matches. Return the last inspected name if another step may be needed.
    /// The controller checks authority and resolves each emitted name separately.
    pub(super) fn scan(
        &self,
        project: ProjectId,
        selector: SubscriptionSelector,
        after: Option<NamedTrack>,
        budget: NonZeroUsize,
        mut visit: impl FnMut(NamedTrack),
    ) -> Option<NamedTrack> {
        if !selector.project.matches(project) {
            return None;
        }
        let index = self.projects.get(&project)?;
        let room = selector.room.literal();
        let participant = selector.participant.literal();
        let kind = selector.kind.literal();
        let track = selector.track.literal();
        let prefix =
            room.map(|room| (room, participant, participant.and_then(|_| kind.zip(track))));
        let start = match (after.map(NamedTrack::position), prefix) {
            (Some(after), Some(prefix)) if after < prefix => Bound::Included(prefix),
            (Some(after), _) => Bound::Excluded(after),
            (None, Some(prefix)) => Bound::Included(prefix),
            (None, None) => Bound::Unbounded,
        };
        let mut entries = index
            .range((start, Bound::Unbounded))
            .take_while(|((r, p, k), _)| {
                prefix.is_none_or(|(room, publisher, track)| {
                    *r == room
                        && publisher.is_none_or(|publisher| *p == Some(publisher))
                        && track.is_none_or(|track| *k == Some(track))
                })
            });
        let mut last = None;
        for (&(room, participant, kind_track), _) in entries.by_ref().take(budget.get()) {
            let (Some(participant), Some((kind, track))) = (participant, kind_track) else {
                return None;
            };
            let name = NamedTrack {
                room,
                participant,
                kind,
                track,
            };
            last = Some(name);
            if selector.participant.matches(name.participant)
                && selector.kind.matches(name.kind)
                && selector.track.matches(name.track)
            {
                visit(name);
            }
        }
        entries.next().and(last)
    }
}

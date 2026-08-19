//! What exists: every publication on this node, whatever kind it is.
//!
//! Video, audio and the two data lanes used to be filed in three places —
//! `track_bindings` for the two media kinds and a `bindings` map inside each of
//! two lane registries — so every procedure that walked publications was
//! written once per leg. This is that layer, written once.
//!
//! A publication is named by its subject, `(room, publisher, kind, label)`, and
//! keyed by the `TrackId` derived from it. The id is stable and cluster-wide;
//! any node derives the same one from the same inputs. It is never what the
//! data plane carries — control compiles it to a dense route and arena keys.
#![deny(clippy::arithmetic_side_effects)]

use indexmap::{IndexMap, IndexSet};

use crate::entity::{ParticipantId, RoomId, TrackId, TrackKind};
use crate::id::ShardId;
use crate::keys::{ParticipantKey, ReliableStreamKey, TrackKey, UnreliableStreamKey};
use crate::route::RouteHandle;

/// What a publication is called. The `TrackId` keying it is derived from
/// exactly these fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Subject {
    pub room: RoomId,
    pub publisher: ParticipantId,
    pub kind: TrackKind,
    pub label: String,
}

impl Subject {
    pub fn id(&self) -> TrackId {
        self.publisher.derive_track_id(self.kind, &self.label)
    }
}

/// A publication's key in one shard's arena, whichever arena that is.
///
/// The variant is the arena. Video and audio share `tracks`; the data lanes
/// have their own, which is the only thing about a lane the routing layer still
/// needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeKey {
    Track(TrackKey),
    Unreliable(UnreliableStreamKey),
    Reliable(ReliableStreamKey),
}

impl RuntimeKey {
    /// The media arena's key, for the paths that only ever hold one.
    pub fn track(self) -> Option<TrackKey> {
        match self {
            Self::Track(key) => Some(key),
            _ => None,
        }
    }

    /// The data arenas' key, likewise. `RuntimeStreamKey` is the compiled form
    /// the shard already speaks; this is the same distinction seen from the
    /// catalog, where a media key is also possible.
    pub fn stream(self) -> Option<crate::shard::router::RuntimeStreamKey> {
        use crate::shard::router::RuntimeStreamKey;
        match self {
            Self::Unreliable(key) => Some(RuntimeStreamKey::Unreliable(key)),
            Self::Reliable(key) => Some(RuntimeStreamKey::Reliable(key)),
            Self::Track(_) => None,
        }
    }
}

impl From<crate::shard::router::RuntimeStreamKey> for RuntimeKey {
    fn from(key: crate::shard::router::RuntimeStreamKey) -> Self {
        use crate::shard::router::RuntimeStreamKey;
        match key {
            RuntimeStreamKey::Unreliable(key) => Self::Unreliable(key),
            RuntimeStreamKey::Reliable(key) => Self::Reliable(key),
        }
    }
}

/// One shard that receives this publication: what it calls it there, and the
/// route serving it.
///
/// One record, not the two pairs of maps this replaces. An arena key is only
/// meaningful on the shard that minted it, and pairing it with its shard here
/// is what stops it being compared against a key from somewhere else — the
/// mistake that made cross-shard audio silently undeliverable.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Destination {
    pub key: RuntimeKey,
    pub route: Option<RouteHandle>,
}

/// Only what a kind genuinely adds beyond the subject.
#[derive(Debug, Clone)]
pub(crate) enum Media {
    Video {
        publication: crate::track::Track,
        encodings: Vec<Option<str0m::media::Rid>>,
        states: crate::track::TrackStates,
    },
    Audio,
    Data {
        lane: crate::track::DataLane,
        topic: crate::track::Topic,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct Publication {
    pub subject: Subject,
    pub publisher_shard: ShardId,
    pub publisher_key: ParticipantKey,
    pub origin_key: RuntimeKey,
    pub reverse_route: Option<RouteHandle>,
    pub destinations: IndexMap<ShardId, Destination>,
    pub media: Media,
}

/// Every publication on the node, with the indexes a declaration needs to find
/// the ones it matches.
///
/// The three indexes mirror the three pattern forms that are not fully
/// concrete; a fully concrete pattern needs no index at all, because its
/// subject derives the id directly.
#[derive(Debug, Default)]
pub(crate) struct Catalog {
    publications: IndexMap<TrackId, Publication>,
    by_room: IndexMap<(RoomId, TrackKind), IndexSet<TrackId>>,
    by_label: IndexMap<(RoomId, TrackKind, String), IndexSet<TrackId>>,
    by_publisher: IndexMap<(RoomId, TrackKind, ParticipantId), IndexSet<TrackId>>,
}

impl Publication {
    /// `TrackMeta` is a projection of the subject and the publisher's shard,
    /// not state: room, id and origin all come from the subject, and the shard
    /// is where the publisher lives. Storing it would be a second copy of
    /// facts already held here, free to drift.
    pub fn meta(&self) -> crate::track::TrackMeta {
        crate::track::TrackMeta {
            room_id: self.subject.room,
            shard_id: self.publisher_shard,
            id: self.subject.id(),
            origin: self.subject.publisher,
        }
    }
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// File a publication, returning the id it is known by.
    ///
    /// A derived id is a 128-bit hash, so this is the one place a collision
    /// could put two subjects on one key, and the one place it is checked.
    pub fn insert(&mut self, publication: Publication) -> TrackId {
        let id = publication.subject.id();
        let subject = publication.subject.clone();
        if let Some(existing) = self.publications.get(&id) {
            debug_assert_eq!(
                existing.subject, subject,
                "two subjects derived one identity"
            );
        }
        self.by_room
            .entry((subject.room, subject.kind))
            .or_default()
            .insert(id);
        self.by_label
            .entry((subject.room, subject.kind, subject.label.clone()))
            .or_default()
            .insert(id);
        self.by_publisher
            .entry((subject.room, subject.kind, subject.publisher))
            .or_default()
            .insert(id);
        self.publications.insert(id, publication);
        id
    }

    pub fn remove(&mut self, id: &TrackId) -> Option<Publication> {
        let publication = self.publications.shift_remove(id)?;
        let s = &publication.subject;
        Self::unindex(&mut self.by_room, &(s.room, s.kind), id);
        Self::unindex(&mut self.by_label, &(s.room, s.kind, s.label.clone()), id);
        Self::unindex(&mut self.by_publisher, &(s.room, s.kind, s.publisher), id);
        Some(publication)
    }

    fn unindex<K: std::hash::Hash + Eq>(
        index: &mut IndexMap<K, IndexSet<TrackId>>,
        key: &K,
        id: &TrackId,
    ) {
        let Some(entry) = index.get_mut(key) else {
            return;
        };
        entry.shift_remove(id);
        if entry.is_empty() {
            index.shift_remove(key);
        }
    }

    pub fn get(&self, id: &TrackId) -> Option<&Publication> {
        self.publications.get(id)
    }

    pub fn get_mut(&mut self, id: &TrackId) -> Option<&mut Publication> {
        self.publications.get_mut(id)
    }

    /// The publications a declaration reaches.
    ///
    /// The inverse of matching a subject against the pattern table: that asks
    /// "which audiences does this publication have", this asks "which
    /// publications does this audience want", and a subscribe needs both.
    pub fn matching(
        &self,
        room: RoomId,
        kind: TrackKind,
        publisher: Option<ParticipantId>,
        label: Option<&str>,
    ) -> Vec<TrackId> {
        match (publisher, label) {
            (Some(publisher), Some(label)) => {
                // The room is not one of the hashed inputs — a participant
                // belongs to one room, so the room is determined by the
                // publisher rather than independent of it. Determined is not
                // checked: the derived id alone resolves a publication in
                // another room, so the subject is verified before it counts.
                let id = publisher.derive_track_id(kind, label);
                self.publications
                    .get(&id)
                    .filter(|held| held.subject.room == room)
                    .map(|_| id)
                    .into_iter()
                    .collect()
            }
            (None, Some(label)) => self
                .by_label
                .get(&(room, kind, label.to_string()))
                .map(|ids| ids.iter().copied().collect())
                .unwrap_or_default(),
            (Some(publisher), None) => self
                .by_publisher
                .get(&(room, kind, publisher))
                .map(|ids| ids.iter().copied().collect())
                .unwrap_or_default(),
            (None, None) => self
                .by_room
                .get(&(room, kind))
                .map(|ids| ids.iter().copied().collect())
                .unwrap_or_default(),
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.publications.len()
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::entity::ExternalRoomId;

    fn room(name: &str) -> RoomId {
        RoomId::from_external(&ExternalRoomId::new(name).unwrap())
    }

    fn pid(seed: u8) -> ParticipantId {
        ParticipantId::from_bytes([seed; 16])
    }

    fn publication(r: RoomId, publisher: u8, kind: TrackKind, label: &str) -> Publication {
        Publication {
            subject: Subject {
                room: r,
                publisher: pid(publisher),
                kind,
                label: label.to_string(),
            },
            publisher_shard: ShardId::new(0),
            publisher_key: ParticipantKey::default(),
            origin_key: RuntimeKey::Track(TrackKey::default()),
            reverse_route: None,
            destinations: IndexMap::new(),
            media: Media::Audio,
        }
    }

    /// The four ways a declaration can name what it wants, against one catalog.
    /// A fully concrete one needs no index: its subject derives the id.
    #[test]
    fn every_pattern_form_finds_what_it_names() {
        let mut catalog = Catalog::new();
        let r = room("r");
        for (publisher, label) in [(1, "mic"), (2, "mic"), (1, "screen")] {
            catalog.insert(publication(r, publisher, TrackKind::Audio, label));
        }

        assert_eq!(
            catalog
                .matching(r, TrackKind::Audio, Some(pid(1)), Some("mic"))
                .len(),
            1,
            "exact names one publication"
        );
        assert_eq!(
            catalog
                .matching(r, TrackKind::Audio, None, Some("mic"))
                .len(),
            2,
            "a label across publishers names both"
        );
        assert_eq!(
            catalog
                .matching(r, TrackKind::Audio, Some(pid(1)), None)
                .len(),
            2,
            "a publisher's whole output"
        );
        assert_eq!(
            catalog.matching(r, TrackKind::Audio, None, None).len(),
            3,
            "everything of that kind in the room"
        );
    }

    /// Kind partitions the catalog, so audio declarations never reach video.
    #[test]
    fn a_kind_never_matches_another() {
        let mut catalog = Catalog::new();
        let r = room("r");
        catalog.insert(publication(r, 1, TrackKind::Audio, "x"));
        catalog.insert(publication(r, 1, TrackKind::Video, "x"));

        assert_eq!(catalog.matching(r, TrackKind::Audio, None, None).len(), 1);
        assert_eq!(catalog.matching(r, TrackKind::Video, None, None).len(), 1);
        assert_eq!(catalog.matching(r, TrackKind::Data, None, None).len(), 0);
    }

    /// Room isolation is structural: the room is a field of every index key, so
    /// no query can reach across one.
    #[test]
    fn nothing_matches_across_rooms() {
        let mut catalog = Catalog::new();
        catalog.insert(publication(room("a"), 1, TrackKind::Audio, "mic"));

        assert!(
            catalog
                .matching(room("b"), TrackKind::Audio, None, None)
                .is_empty()
        );
        assert!(
            catalog
                .matching(room("b"), TrackKind::Audio, Some(pid(1)), Some("mic"))
                .is_empty()
        );
    }

    /// Removal has to clear all three indexes, or a later query resolves an id
    /// the catalog no longer holds.
    #[test]
    fn removal_leaves_no_index_entry_behind() {
        let mut catalog = Catalog::new();
        let r = room("r");
        let id = catalog.insert(publication(r, 1, TrackKind::Audio, "mic"));
        catalog.insert(publication(r, 2, TrackKind::Audio, "mic"));

        assert!(catalog.remove(&id).is_some());
        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog.matching(r, TrackKind::Audio, None, Some("mic")),
            catalog.matching(r, TrackKind::Audio, Some(pid(2)), Some("mic")),
            "only the surviving publisher is reachable, by either route"
        );
        assert!(
            catalog
                .matching(r, TrackKind::Audio, Some(pid(1)), None)
                .is_empty()
        );
    }

    /// Re-announcing the same subject is an update, not a second entry.
    #[test]
    fn re_announcing_one_subject_does_not_duplicate_it() {
        let mut catalog = Catalog::new();
        let r = room("r");
        let first = catalog.insert(publication(r, 1, TrackKind::Audio, "mic"));
        let again = catalog.insert(publication(r, 1, TrackKind::Audio, "mic"));

        assert_eq!(first, again);
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog.matching(r, TrackKind::Audio, None, None).len(), 1);
    }
}

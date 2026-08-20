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
use crate::keys::{
    AudioTrackKey, ParticipantKey, ReliableStreamKey, TrackKey, UnreliableStreamKey, VideoTrackKey,
};
use crate::route::RouteHandle;

/// A publication's key in one shard's arena, whichever arena that is.
///
/// The variant is the arena. Video and audio share `tracks`; the data lanes
/// have their own, which is the only thing about a lane the routing layer still
/// needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeKey {
    Video(VideoTrackKey),
    Audio(AudioTrackKey),
    Unreliable(UnreliableStreamKey),
    Reliable(ReliableStreamKey),
}

impl RuntimeKey {
    pub fn track(self) -> Option<TrackKey> {
        match self {
            Self::Video(key) => Some(key.raw()),
            Self::Audio(key) => Some(key.raw()),
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
            Self::Video(_) | Self::Audio(_) => None,
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
pub(crate) enum Destination {
    Discovery { key: VideoTrackKey },
    Forwarding { key: RuntimeKey, route: RouteHandle },
}

impl Destination {
    pub(crate) fn key(self) -> RuntimeKey {
        match self {
            Self::Discovery { key } => RuntimeKey::Video(key),
            Self::Forwarding { key, .. } => key,
        }
    }
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
    /// The stable, cluster-wide identity. Derived from the publisher, the kind
    /// and the label at announce; held rather than re-derived, because the
    /// label is a property of how a track was named and nothing downstream
    /// needs it again.
    pub id: TrackId,
    pub room: RoomId,
    pub publisher: ParticipantId,
    pub publisher_shard: ShardId,
    pub publisher_key: ParticipantKey,
    pub origin_key: RuntimeKey,
    pub reverse_route: Option<RouteHandle>,
    pub destinations: IndexMap<ShardId, Destination>,
    pub media: Media,
}

impl Publication {
    pub fn kind(&self) -> TrackKind {
        self.id.kind()
    }

    /// `TrackMeta` is a projection of what is already here, not state. Storing
    /// it would be a second copy free to drift.
    pub fn meta(&self) -> crate::track::TrackMeta {
        crate::track::TrackMeta {
            room_id: self.room,
            shard_id: self.publisher_shard,
            id: self.id,
            origin: self.publisher,
        }
    }

    /// The label this was published under, for declarations that name a topic
    /// rather than a track. Media publications are named by id, which embeds
    /// the publisher, so they need no label to be found.
    pub fn data_label(&self) -> Option<String> {
        match &self.media {
            Media::Data { lane, topic } => Some(crate::track::publication_label(*lane, topic)),
            _ => None,
        }
    }
}

/// Compile a publication's forwarding plan for one shard.
///
/// The same for every kind, because by this point everything that differed has
/// already been resolved: the audiences are group ids, the destinations are a
/// map, and the delivery key lives in the group image on the shard rather than
/// here. What a caller still supplies is which groups matched, since the
/// pattern tables are keyed differently per kind — that is a detail of
/// matching, not of planning.
///
/// Remote routes only appear on the publisher's own plan: every other shard
/// receives over a route rather than forwarding onward, so listing them
/// elsewhere would invite a second hop.
pub(crate) fn forwarding_plan<G>(
    destinations: &IndexMap<ShardId, Destination>,
    publisher_shard: ShardId,
    reverse_route: Option<RouteHandle>,
    groups: arrayvec::ArrayVec<crate::view::GroupId<G>, 4>,
    shard: ShardId,
) -> crate::view::ForwardingPlan<G> {
    let remote_routes = if shard == publisher_shard {
        destinations
            .iter()
            .filter_map(|(destination, held)| {
                if *destination == publisher_shard {
                    return None;
                }
                match held {
                    Destination::Forwarding { route, .. } => {
                        Some(crate::view::RemoteRoutePlan { handle: *route })
                    }
                    Destination::Discovery { .. } => None,
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    crate::view::ForwardingPlan {
        groups,
        remote_routes,
        reverse_route: reverse_route.map(|handle| crate::view::RemoteRoutePlan { handle }),
    }
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
    by_publisher: IndexMap<(RoomId, TrackKind, ParticipantId), IndexSet<TrackId>>,
    by_origin: IndexMap<ParticipantId, IndexSet<TrackId>>,
    /// Data only. A media publication is named by its id, which embeds the
    /// publisher, so a declaration naming one resolves without an index; a data
    /// declaration names a topic across publishers and cannot.
    by_label: IndexMap<(RoomId, String), IndexSet<TrackId>>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// File a publication. A derived id is a 128-bit hash, so this is the one
    /// place two publications could collide on a key, and the one place it is
    /// checked.
    pub fn insert(&mut self, publication: Publication) {
        let id = publication.id;
        let (room, kind) = (publication.room, publication.kind());
        if let Some(existing) = self.publications.get(&id) {
            debug_assert!(
                existing.room == room && existing.publisher == publication.publisher,
                "two publications derived one identity"
            );
        }
        self.by_room.entry((room, kind)).or_default().insert(id);
        self.by_publisher
            .entry((room, kind, publication.publisher))
            .or_default()
            .insert(id);
        self.by_origin
            .entry(publication.publisher)
            .or_default()
            .insert(id);
        if let Some(label) = publication.data_label() {
            self.by_label.entry((room, label)).or_default().insert(id);
        }
        self.publications.insert(id, publication);
    }

    pub fn remove(&mut self, id: &TrackId) -> Option<Publication> {
        let publication = self.publications.shift_remove(id)?;
        let (room, kind) = (publication.room, publication.kind());
        Self::unindex(&mut self.by_room, &(room, kind), id);
        Self::unindex(
            &mut self.by_publisher,
            &(room, kind, publication.publisher),
            id,
        );
        Self::unindex(&mut self.by_origin, &publication.publisher, id);
        if let Some(label) = publication.data_label() {
            Self::unindex(&mut self.by_label, &(room, label), id);
        }
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

    pub fn contains(&self, id: &TrackId) -> bool {
        self.publications.contains_key(id)
    }

    /// Every publication of a kind in a room.
    pub fn in_room(&self, room: RoomId, kind: TrackKind) -> impl Iterator<Item = TrackId> + '_ {
        self.by_room
            .get(&(room, kind))
            .into_iter()
            .flat_map(IndexSet::iter)
            .copied()
    }

    /// Everything one publisher sends of a kind.
    #[cfg(test)]
    pub fn published_by(
        &self,
        room: RoomId,
        kind: TrackKind,
        publisher: ParticipantId,
    ) -> impl Iterator<Item = TrackId> + '_ {
        self.by_publisher
            .get(&(room, kind, publisher))
            .into_iter()
            .flat_map(IndexSet::iter)
            .copied()
    }

    pub fn published_by_participant(
        &self,
        publisher: ParticipantId,
    ) -> impl Iterator<Item = TrackId> + '_ {
        self.by_origin
            .get(&publisher)
            .into_iter()
            .flat_map(IndexSet::iter)
            .copied()
    }

    /// Every publisher of a data label in a room.
    pub fn on_label(&self, room: RoomId, label: &str) -> impl Iterator<Item = TrackId> + '_ {
        self.by_label
            .get(&(room, label.to_string()))
            .into_iter()
            .flat_map(IndexSet::iter)
            .copied()
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
    use crate::track::DataLane;

    fn room(name: &str) -> RoomId {
        RoomId::from_external(&ExternalRoomId::new(name).unwrap())
    }

    fn pid(seed: u8) -> ParticipantId {
        ParticipantId::from_bytes([seed; 16])
    }

    fn media(r: RoomId, publisher: u8, kind: TrackKind, label: &str) -> Publication {
        Publication {
            id: pid(publisher).derive_track_id(kind, label),
            room: r,
            publisher: pid(publisher),
            publisher_shard: ShardId::new(0),
            publisher_key: ParticipantKey::default(),
            origin_key: RuntimeKey::Audio(AudioTrackKey::new(TrackKey::default())),
            reverse_route: None,
            destinations: IndexMap::new(),
            media: Media::Audio,
        }
    }

    fn data(r: RoomId, publisher: u8, lane: DataLane, topic: &str) -> Publication {
        let topic = crate::track::Topic::for_test(topic);
        let label = crate::track::publication_label(lane, &topic);
        Publication {
            id: pid(publisher).derive_track_id(TrackKind::Data, &label),
            room: r,
            publisher: pid(publisher),
            publisher_shard: ShardId::new(0),
            publisher_key: ParticipantKey::default(),
            origin_key: RuntimeKey::Unreliable(Default::default()),
            reverse_route: None,
            destinations: IndexMap::new(),
            media: Media::Data { lane, topic },
        }
    }

    fn label_of(lane: DataLane, topic: &str) -> String {
        crate::track::publication_label(lane, &crate::track::Topic::for_test(topic))
    }

    /// A media declaration names a track, and a track id embeds its publisher,
    /// so it resolves without an index at all.
    #[test]
    fn a_media_publication_is_found_by_its_id() {
        let mut catalog = Catalog::new();
        let r = room("r");
        let one = media(r, 1, TrackKind::Audio, "mic");
        let id = one.id;
        catalog.insert(one);

        assert!(catalog.contains(&id));
        assert_eq!(
            catalog.in_room(r, TrackKind::Audio).collect::<Vec<_>>(),
            vec![id]
        );
    }

    /// A data declaration names a topic across publishers, which is the case
    /// the label index exists for.
    #[test]
    fn a_data_label_finds_every_publisher_of_it() {
        let mut catalog = Catalog::new();
        let r = room("r");
        for publisher in [1u8, 2, 3] {
            catalog.insert(data(r, publisher, DataLane::Realtime, "chat"));
        }
        catalog.insert(data(r, 1, DataLane::Reliable, "chat"));

        assert_eq!(
            catalog
                .on_label(r, &label_of(DataLane::Realtime, "chat"))
                .count(),
            3,
            "every publisher on that lane, and only that lane"
        );
    }

    /// The lanes are separate publications of one topic, told apart by the
    /// label rather than by a lane field the catalog has to carry.
    #[test]
    fn the_lanes_do_not_share_an_audience() {
        let mut catalog = Catalog::new();
        let r = room("r");
        catalog.insert(data(r, 1, DataLane::Realtime, "chat"));
        catalog.insert(data(r, 1, DataLane::Reliable, "chat"));

        let realtime = catalog.on_label(r, &label_of(DataLane::Realtime, "chat"));
        let reliable = catalog.on_label(r, &label_of(DataLane::Reliable, "chat"));
        assert_eq!(realtime.count(), 1);
        assert_eq!(reliable.count(), 1);
    }

    /// A topic is scoped to its room, which the lane registry's index used to
    /// assert before the catalog took the question over.
    #[test]
    fn a_data_label_is_scoped_to_its_room() {
        let mut catalog = Catalog::new();
        catalog.insert(data(room("a"), 1, DataLane::Realtime, "chat"));

        let label = label_of(DataLane::Realtime, "chat");
        assert_eq!(catalog.on_label(room("a"), &label).count(), 1);
        assert_eq!(catalog.on_label(room("b"), &label).count(), 0);
    }

    #[test]
    fn a_publishers_output_is_scoped_to_its_kind_and_room() {
        let mut catalog = Catalog::new();
        let r = room("r");
        catalog.insert(media(r, 1, TrackKind::Audio, "mic"));
        catalog.insert(media(r, 1, TrackKind::Video, "cam"));
        // A different participant: the same one cannot be in two rooms, and
        // the id would not distinguish them if it were, since the room is not
        // a hashed input. The collision assert in `insert` catches that.
        catalog.insert(media(room("other"), 9, TrackKind::Audio, "mic"));

        assert_eq!(catalog.published_by(r, TrackKind::Audio, pid(1)).count(), 1);
        assert_eq!(catalog.published_by(r, TrackKind::Video, pid(1)).count(), 1);
        assert_eq!(
            catalog
                .published_by(room("nowhere"), TrackKind::Audio, pid(1))
                .count(),
            0
        );
    }

    /// Removal has to clear every index, or a later query resolves an id the
    /// catalog no longer holds.
    #[test]
    fn removal_leaves_no_index_entry_behind() {
        let mut catalog = Catalog::new();
        let r = room("r");
        let one = data(r, 1, DataLane::Realtime, "chat");
        let id = one.id;
        catalog.insert(one);
        catalog.insert(data(r, 2, DataLane::Realtime, "chat"));

        assert!(catalog.remove(&id).is_some());
        assert_eq!(catalog.len(), 1);
        assert!(!catalog.contains(&id));
        assert_eq!(
            catalog
                .on_label(r, &label_of(DataLane::Realtime, "chat"))
                .count(),
            1
        );
        assert_eq!(catalog.published_by(r, TrackKind::Data, pid(1)).count(), 0);
    }
}

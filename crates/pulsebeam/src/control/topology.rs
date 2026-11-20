use std::collections::{HashMap, HashSet};

use pulsebeam_runtime::time::Instant;
use slotmap::{SlotMap, new_key_type};

use crate::{
    entity::{ConnectionId, EndpointKey, ParticipantId, ProjectRoomKey, PublicationKey, TrackKind},
    id::ShardId,
    route::{NodeRouteAddress, PackedRoute, RouteId, SlotAllocator},
    track::{SelectionPolicy, Track, TrackSelector},
};

new_key_type! {
    pub(crate) struct SubscriptionKey;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TrackAncestry {
    pub room_id: ProjectRoomKey,
    pub publisher: ParticipantId,
    pub publication_id: PublicationKey,
}

impl TrackAncestry {
    pub(crate) fn from_track(track: &Track) -> Self {
        Self {
            room_id: track.meta().room,
            publisher: track.meta().origin,
            publication_id: track.id(),
        }
    }

    pub(crate) fn kind(self) -> TrackKind {
        self.publication_id.kind()
    }
}

#[derive(Debug)]
pub(crate) struct OwnedSubscription {
    pub(crate) source_room: ProjectRoomKey,
    pub(crate) subscriber: ParticipantId,
    pub(crate) reference: u64,
    pub(crate) track: PublicationKey,
    pub(crate) enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Subscriber {
    Joined(EndpointKey),
    Connection(ConnectionId),
}

impl From<EndpointKey> for Subscriber {
    fn from(endpoint: EndpointKey) -> Self {
        Self::Joined(endpoint)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Subscription {
    pub id: SubscriptionKey,
    pub subscriber: Subscriber,
    pub selector: TrackSelector,
    pub selection: SelectionPolicy,
    pub owned_ref: Option<u64>,
}

#[derive(Debug)]
struct Publication {
    track: Track,
    label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SubscriptionAnchor {
    Track(PublicationKey),
    Publisher(ParticipantId),
    Label(String),
    Kind(TrackKind),
    Any,
}

#[derive(Debug, Default)]
struct RoomTrackTopology {
    catalog_version: u64,
    tracks: HashMap<PublicationKey, Publication>,
    by_publisher: HashMap<ParticipantId, HashSet<PublicationKey>>,
    by_incarnation: HashMap<(ParticipantId, ConnectionId), HashSet<PublicationKey>>,
    by_kind: HashMap<TrackKind, HashSet<PublicationKey>>,
    by_label: HashMap<String, HashSet<PublicationKey>>,
    subscriptions: SlotMap<SubscriptionKey, Subscription>,
    owned: HashMap<(Subscriber, u64), SubscriptionKey>,
    by_track: HashMap<PublicationKey, HashSet<SubscriptionKey>>,
    by_subscriber_publisher: HashMap<ParticipantId, HashSet<SubscriptionKey>>,
    by_subscription_kind: HashMap<TrackKind, HashSet<SubscriptionKey>>,
    by_subscription_label: HashMap<String, HashSet<SubscriptionKey>>,
    unconstrained: HashSet<SubscriptionKey>,
    candidate_reasons: HashMap<(Subscriber, PublicationKey), HashSet<SubscriptionKey>>,
    automatic_reasons: HashMap<(Subscriber, PublicationKey), HashSet<SubscriptionKey>>,
    allocated: HashSet<(Subscriber, PublicationKey)>,
}

#[derive(Debug, Default)]
pub(crate) struct TrackTopology {
    catalog_version: u64,
    publication_ids: HashSet<PublicationKey>,
    rooms: HashMap<ProjectRoomKey, RoomTrackTopology>,
}

impl TrackTopology {
    pub(crate) fn publish(&mut self, track: Track) -> Option<TrackAncestry> {
        let identity = TrackAncestry::from_track(&track);
        // Link plans and shard runtimes use the publication key without room scope.
        if !self.publication_ids.insert(identity.publication_id) {
            return None;
        }
        let label = track.publication_label();
        let room = self.rooms.entry(identity.room_id).or_default();
        self.catalog_version = self
            .catalog_version
            .checked_add(1)
            .unwrap_or_else(|| pulsebeam_runtime::fatal!("publication catalog version exhausted"));
        room.catalog_version = self.catalog_version;
        room.index_track(identity, track.meta().connection_id, label.as_deref());
        room.tracks
            .insert(identity.publication_id, Publication { track, label });
        room.add_publication_reasons(identity);
        Some(identity)
    }

    pub(crate) fn unpublish(&mut self, identity: TrackAncestry) -> Option<Track> {
        let room = self.rooms.get_mut(&identity.room_id)?;
        if room
            .tracks
            .get(&identity.publication_id)?
            .track
            .meta()
            .origin
            != identity.publisher
        {
            return None;
        }
        let publication = room.tracks.remove(&identity.publication_id)?;
        self.publication_ids.remove(&identity.publication_id);
        self.catalog_version = self
            .catalog_version
            .checked_add(1)
            .unwrap_or_else(|| pulsebeam_runtime::fatal!("publication catalog version exhausted"));
        room.catalog_version = self.catalog_version;
        debug_assert_eq!(TrackAncestry::from_track(&publication.track), identity);
        room.unindex_track(
            identity,
            publication.track.meta().connection_id,
            publication.label.as_deref(),
        );
        room.candidate_reasons
            .retain(|(_, track), _| *track != identity.publication_id);
        room.automatic_reasons
            .retain(|(_, track), _| *track != identity.publication_id);
        room.allocated
            .retain(|(_, track)| *track != identity.publication_id);
        Some(publication.track)
    }

    pub(crate) fn track(&self, identity: TrackAncestry) -> Option<&Track> {
        self.source(identity.room_id, identity.publication_id)
            .filter(|track| track.meta().origin == identity.publisher)
    }

    pub(crate) fn source(&self, room_id: ProjectRoomKey, track: PublicationKey) -> Option<&Track> {
        Some(&self.rooms.get(&room_id)?.tracks.get(&track)?.track)
    }

    // Publisher publications are capped at 64 by admission. Logical labels are
    // metadata; routing-label indexes deliberately omit audio and video.
    pub(crate) fn exact_named_sources(
        &self,
        room_id: ProjectRoomKey,
        publisher: ParticipantId,
        kind: TrackKind,
        track_id: crate::entity::TrackId,
    ) -> Option<impl Iterator<Item = &Track>> {
        let room = self.rooms.get(&room_id)?;
        let publications = room.by_publisher.get(&publisher)?;
        let kinds = room.by_kind.get(&kind)?;
        Some(
            publications
                .iter()
                .filter(move |track| kinds.contains(track))
                .filter_map(move |track| {
                    room.tracks.get(track).map(|publication| &publication.track)
                })
                .filter(move |track| track.meta().track_id == Some(track_id)),
        )
    }

    pub(super) fn sources_in_room(&self, room_id: ProjectRoomKey) -> impl Iterator<Item = &Track> {
        self.rooms
            .get(&room_id)
            .into_iter()
            .flat_map(|room| room.tracks.values().map(|publication| &publication.track))
    }

    pub(crate) fn catalog_version(&self) -> u64 {
        self.catalog_version
    }

    pub(crate) fn room_catalog_version(&self, room: ProjectRoomKey) -> u64 {
        self.rooms.get(&room).map_or(0, |room| room.catalog_version)
    }

    pub(super) fn sources_by_publisher(
        &self,
        endpoint: EndpointKey,
    ) -> impl Iterator<Item = &Track> {
        self.rooms
            .get(&endpoint.room)
            .into_iter()
            .flat_map(move |room| {
                room.by_publisher
                    .get(&endpoint.participant)
                    .into_iter()
                    .flatten()
                    .filter_map(|id| room.tracks.get(id).map(|publication| &publication.track))
            })
    }

    pub(crate) fn publisher_sources(
        &self,
        room: ProjectRoomKey,
        publisher: ParticipantId,
        connection: ConnectionId,
    ) -> impl Iterator<Item = &Track> {
        self.rooms.get(&room).into_iter().flat_map(move |room| {
            room.by_incarnation
                .get(&(publisher, connection))
                .into_iter()
                .flatten()
                .filter_map(|id| room.tracks.get(id).map(|publication| &publication.track))
        })
    }

    pub(crate) fn track_mut(&mut self, identity: TrackAncestry) -> Option<&mut Track> {
        let room = self.rooms.get_mut(&identity.room_id)?;
        let publication = room.tracks.get_mut(&identity.publication_id)?;
        self.catalog_version = self
            .catalog_version
            .checked_add(1)
            .unwrap_or_else(|| pulsebeam_runtime::fatal!("publication catalog version exhausted"));
        room.catalog_version = self.catalog_version;
        Some(&mut publication.track)
    }

    pub(crate) fn subscribe(
        &mut self,
        room_id: ProjectRoomKey,
        subscriber: ParticipantId,
        selector: TrackSelector,
        selection: SelectionPolicy,
    ) -> SubscriptionKey {
        let room = self.rooms.entry(room_id).or_default();
        let id = room.subscriptions.insert_with_key(|id| Subscription {
            id,
            subscriber: Subscriber::Joined(EndpointKey {
                room: room_id,
                participant: subscriber,
            }),
            selector: selector.clone(),
            selection,
            owned_ref: None,
        });
        room.index_subscription(&selector, id);
        room.add_subscription_reasons(id);
        id
    }

    #[cfg(test)]
    pub(crate) fn set_owned(
        &mut self,
        room_id: ProjectRoomKey,
        subscriber: ParticipantId,
        reference: u64,
        track: PublicationKey,
        enabled: bool,
    ) -> bool {
        self.set_owned_scoped(
            room_id,
            Subscriber::Joined(EndpointKey {
                room: room_id,
                participant: subscriber,
            }),
            reference,
            track,
            enabled,
        )
    }

    pub(crate) fn set_owned_scoped(
        &mut self,
        source_room: ProjectRoomKey,
        subscriber: Subscriber,
        reference: u64,
        track: PublicationKey,
        enabled: bool,
    ) -> bool {
        let room = self.rooms.entry(source_room).or_default();
        let selector = TrackSelector::track(track);
        if let Some(previous) = room.owned.get(&(subscriber, reference)).copied() {
            if room
                .subscriptions
                .get(previous)
                .is_none_or(|subscription| subscription.selector != selector)
            {
                return false;
            }
            room.unsubscribe(previous);
        }
        let id = room.subscriptions.insert_with_key(|id| Subscription {
            id,
            subscriber,
            selector: selector.clone(),
            selection: if enabled {
                SelectionPolicy::All
            } else {
                SelectionPolicy::Allocated
            },
            owned_ref: Some(reference),
        });
        room.owned.insert((subscriber, reference), id);
        room.index_subscription(&selector, id);
        room.add_subscription_reasons(id);
        true
    }

    pub(crate) fn remove_owned(
        &mut self,
        room_id: ProjectRoomKey,
        subscriber: impl Into<Subscriber>,
        reference: u64,
    ) -> Option<Subscription> {
        let room = self.rooms.get_mut(&room_id)?;
        let id = *room.owned.get(&(subscriber.into(), reference))?;
        room.unsubscribe(id)
    }

    pub(crate) fn remove_matching(
        &mut self,
        room_id: ProjectRoomKey,
        subscriber: ParticipantId,
        selector: TrackSelector,
    ) -> Option<Subscription> {
        let room = self.rooms.get_mut(&room_id)?;
        let subscriber = Subscriber::Joined(EndpointKey {
            room: room_id,
            participant: subscriber,
        });
        let id = room.subscriptions.iter().find_map(|(id, subscription)| {
            (subscription.owned_ref.is_none()
                && subscription.subscriber == subscriber
                && subscription.selector == selector)
                .then_some(id)
        });
        let id = id?;
        room.unsubscribe(id)
    }

    #[cfg(test)]
    pub(crate) fn matches(
        &self,
        identity: TrackAncestry,
    ) -> impl Iterator<Item = Subscription> + '_ {
        self.rooms
            .get(&identity.room_id)
            .into_iter()
            .flat_map(move |room| room.matching_subscriptions(identity))
    }

    pub(crate) fn has_candidate(&self, identity: TrackAncestry, subscriber: Subscriber) -> bool {
        self.rooms.get(&identity.room_id).is_some_and(|room| {
            room.candidate_reasons
                .contains_key(&(subscriber, identity.publication_id))
        })
    }

    pub(crate) fn candidate_subscribers(
        &self,
        identity: TrackAncestry,
    ) -> impl Iterator<Item = Subscriber> + '_ {
        self.rooms
            .get(&identity.room_id)
            .into_iter()
            .flat_map(move |room| room.candidate_subscribers(identity.publication_id))
    }

    pub(crate) fn active_subscribers(
        &self,
        identity: TrackAncestry,
    ) -> impl Iterator<Item = Subscriber> + '_ {
        self.rooms
            .get(&identity.room_id)
            .into_iter()
            .flat_map(move |room| room.active_subscribers(identity.publication_id))
    }

    pub(crate) fn activate(
        &mut self,
        identity: TrackAncestry,
        subscriber: impl Into<Subscriber>,
    ) -> bool {
        let subscriber = subscriber.into();
        if matches!(subscriber, Subscriber::Joined(endpoint) if endpoint.room != identity.room_id) {
            return false;
        }
        let Some(room) = self.rooms.get_mut(&identity.room_id) else {
            debug_assert!(false, "activation must target a live room catalog");
            return false;
        };
        room.activate(identity.publication_id, subscriber)
    }

    pub(crate) fn deactivate(
        &mut self,
        identity: TrackAncestry,
        subscriber: impl Into<Subscriber>,
    ) -> bool {
        self.rooms.get_mut(&identity.room_id).is_some_and(|room| {
            room.allocated
                .remove(&(subscriber.into(), identity.publication_id))
        })
    }

    pub(crate) fn contains(&self, identity: TrackAncestry) -> bool {
        self.track(identity).is_some()
    }

    pub(crate) fn matching_tracks(
        &self,
        room_id: ProjectRoomKey,
        selector: &TrackSelector,
    ) -> Vec<TrackAncestry> {
        self.rooms.get(&room_id).map_or_else(Vec::new, |room| {
            room.selector_tracks(selector)
                .into_iter()
                .filter_map(|track| room.identity(room_id, track))
                .collect()
        })
    }

    pub(crate) fn identities(&self) -> impl Iterator<Item = TrackAncestry> + '_ {
        self.rooms.iter().flat_map(|(room_id, room)| {
            room.tracks
                .keys()
                .filter_map(move |track| room.identity(*room_id, *track))
        })
    }

    pub(crate) fn remove_subscriber(&mut self, subscriber: Subscriber) {
        for room in self.rooms.values_mut() {
            room.remove_subscriber(subscriber);
        }
    }

    #[cfg(test)]
    fn remove_participant(&mut self, endpoint: EndpointKey) {
        self.remove_subscriber(endpoint.into());
        let published: Vec<_> = self
            .sources_by_publisher(endpoint)
            .map(TrackAncestry::from_track)
            .collect();
        for identity in published {
            self.unpublish(identity);
        }
    }
}

impl RoomTrackTopology {
    fn identity(&self, room_id: ProjectRoomKey, track: PublicationKey) -> Option<TrackAncestry> {
        let publication = self.tracks.get(&track)?;
        Some(TrackAncestry {
            room_id,
            publisher: publication.track.meta().origin,
            publication_id: track,
        })
    }

    fn index_track(
        &mut self,
        identity: TrackAncestry,
        connection: ConnectionId,
        label: Option<&str>,
    ) {
        debug_assert!(!self.tracks.contains_key(&identity.publication_id));
        let inserted = self
            .by_incarnation
            .entry((identity.publisher, connection))
            .or_default()
            .insert(identity.publication_id);
        debug_assert!(inserted);
        let publisher_inserted = self
            .by_publisher
            .entry(identity.publisher)
            .or_default()
            .insert(identity.publication_id);
        debug_assert!(publisher_inserted);
        let kind_inserted = self
            .by_kind
            .entry(identity.kind())
            .or_default()
            .insert(identity.publication_id);
        debug_assert!(kind_inserted);
        if let Some(label) = label {
            let label_inserted = self
                .by_label
                .entry(label.to_owned())
                .or_default()
                .insert(identity.publication_id);
            debug_assert!(label_inserted);
        }
    }

    fn unindex_track(
        &mut self,
        identity: TrackAncestry,
        connection: ConnectionId,
        label: Option<&str>,
    ) {
        let removed = remove_indexed(
            &mut self.by_incarnation,
            (identity.publisher, connection),
            identity.publication_id,
        );
        debug_assert!(removed);
        let publisher_removed = remove_indexed(
            &mut self.by_publisher,
            identity.publisher,
            identity.publication_id,
        );
        debug_assert!(publisher_removed);
        let kind_removed =
            remove_indexed(&mut self.by_kind, identity.kind(), identity.publication_id);
        debug_assert!(kind_removed);
        if let Some(label) = label {
            let label_removed = remove_indexed(
                &mut self.by_label,
                label.to_owned(),
                identity.publication_id,
            );
            debug_assert!(label_removed);
        }
    }

    fn selector_tracks(&self, selector: &TrackSelector) -> Vec<PublicationKey> {
        let candidates: Vec<_> = if let Some(track) = selector.track {
            self.tracks
                .contains_key(&track)
                .then_some(track)
                .into_iter()
                .collect()
        } else if let Some(publisher) = selector.publisher {
            self.by_publisher
                .get(&publisher)
                .into_iter()
                .flatten()
                .copied()
                .collect()
        } else if let Some(label) = selector.label.as_ref() {
            self.by_label
                .get(label)
                .into_iter()
                .flatten()
                .copied()
                .collect()
        } else if let Some(kind) = selector.kind {
            self.by_kind
                .get(&kind)
                .into_iter()
                .flatten()
                .copied()
                .collect()
        } else {
            self.tracks.keys().copied().collect()
        };
        candidates
            .into_iter()
            .filter(|track| {
                let publication = self.tracks.get(track);
                debug_assert!(
                    publication.is_some(),
                    "catalog index must reference a publication"
                );
                publication.is_some_and(|publication| selector.matches(&publication.track))
            })
            .collect()
    }

    fn anchor(selector: &TrackSelector) -> SubscriptionAnchor {
        if let Some(track) = selector.track {
            SubscriptionAnchor::Track(track)
        } else if let Some(publisher) = selector.publisher {
            SubscriptionAnchor::Publisher(publisher)
        } else if let Some(label) = selector.label.as_ref() {
            SubscriptionAnchor::Label(label.clone())
        } else if let Some(kind) = selector.kind {
            SubscriptionAnchor::Kind(kind)
        } else {
            SubscriptionAnchor::Any
        }
    }

    fn index_subscription(&mut self, selector: &TrackSelector, id: SubscriptionKey) {
        let inserted = match Self::anchor(selector) {
            SubscriptionAnchor::Track(track) => self.by_track.entry(track).or_default().insert(id),
            SubscriptionAnchor::Publisher(publisher) => self
                .by_subscriber_publisher
                .entry(publisher)
                .or_default()
                .insert(id),
            SubscriptionAnchor::Label(label) => self
                .by_subscription_label
                .entry(label)
                .or_default()
                .insert(id),
            SubscriptionAnchor::Kind(kind) => self
                .by_subscription_kind
                .entry(kind)
                .or_default()
                .insert(id),
            SubscriptionAnchor::Any => self.unconstrained.insert(id),
        };
        debug_assert!(inserted);
    }

    fn unindex_subscription(&mut self, selector: &TrackSelector, id: SubscriptionKey) {
        let removed = match Self::anchor(selector) {
            SubscriptionAnchor::Track(track) => remove_indexed(&mut self.by_track, track, id),
            SubscriptionAnchor::Publisher(publisher) => {
                remove_indexed(&mut self.by_subscriber_publisher, publisher, id)
            }
            SubscriptionAnchor::Label(label) => {
                remove_indexed(&mut self.by_subscription_label, label, id)
            }
            SubscriptionAnchor::Kind(kind) => {
                remove_indexed(&mut self.by_subscription_kind, kind, id)
            }
            SubscriptionAnchor::Any => self.unconstrained.remove(&id),
        };
        debug_assert!(
            removed,
            "subscription index must contain every subscription"
        );
    }

    fn matching_subscription_keys(&self, identity: TrackAncestry) -> HashSet<SubscriptionKey> {
        let Some(publication) = self.tracks.get(&identity.publication_id) else {
            return HashSet::new();
        };
        debug_assert_eq!(TrackAncestry::from_track(&publication.track), identity);
        let mut candidates = self.unconstrained.clone();
        for ids in [
            self.by_track.get(&identity.publication_id),
            self.by_subscriber_publisher.get(&identity.publisher),
            self.by_subscription_kind.get(&identity.kind()),
        ]
        .into_iter()
        .flatten()
        {
            candidates.extend(ids.iter().copied());
        }
        if let Some(label) = publication.label.as_ref()
            && let Some(ids) = self.by_subscription_label.get(label)
        {
            candidates.extend(ids.iter().copied());
        }
        candidates.retain(|id| {
            self.subscriptions
                .get(*id)
                .is_some_and(|subscription| subscription.selector.matches(&publication.track))
        });
        candidates
    }

    #[cfg(test)]
    fn matching_subscriptions(
        &self,
        identity: TrackAncestry,
    ) -> impl Iterator<Item = Subscription> + '_ {
        self.matching_subscription_keys(identity)
            .into_iter()
            .filter_map(|id| self.subscriptions.get(id).cloned())
    }

    fn add_publication_reasons(&mut self, identity: TrackAncestry) {
        for id in self.matching_subscription_keys(identity) {
            self.add_reason(identity.publication_id, id);
        }
    }

    fn add_subscription_reasons(&mut self, id: SubscriptionKey) {
        let Some(subscription) = self.subscriptions.get(id).cloned() else {
            debug_assert!(false, "new subscription must be live");
            return;
        };
        for track in self.selector_tracks(&subscription.selector) {
            self.add_reason(track, id);
        }
    }

    fn add_reason(&mut self, track: PublicationKey, id: SubscriptionKey) {
        let Some(subscription) = self.subscriptions.get(id) else {
            debug_assert!(false, "reason must reference a live subscription");
            return;
        };
        let candidate_inserted = self
            .candidate_reasons
            .entry((subscription.subscriber, track))
            .or_default()
            .insert(id);
        debug_assert!(candidate_inserted);
        if subscription.selection == SelectionPolicy::All {
            let automatic_inserted = self
                .automatic_reasons
                .entry((subscription.subscriber, track))
                .or_default()
                .insert(id);
            debug_assert!(automatic_inserted);
        }
    }

    fn unsubscribe(&mut self, id: SubscriptionKey) -> Option<Subscription> {
        let subscription = self.subscriptions.remove(id)?;
        if let Some(reference) = subscription.owned_ref {
            self.owned.remove(&(subscription.subscriber, reference));
        }
        self.unindex_subscription(&subscription.selector, id);
        let affected = self.selector_tracks(&subscription.selector);
        for track in affected {
            let key = (subscription.subscriber, track);
            let candidate_removed = remove_reason(&mut self.candidate_reasons, key, id);
            debug_assert!(candidate_removed);
            if subscription.selection == SelectionPolicy::All {
                let automatic_removed = remove_reason(&mut self.automatic_reasons, key, id);
                debug_assert!(automatic_removed);
            }
            let allocated_candidate_remains = self
                .candidate_reasons
                .get(&key)
                .into_iter()
                .flatten()
                .any(|reason| {
                    self.subscriptions
                        .get(*reason)
                        .is_some_and(|remaining| remaining.selection == SelectionPolicy::Allocated)
                });
            if !allocated_candidate_remains {
                self.allocated.remove(&key);
            }
        }
        Some(subscription)
    }

    fn candidate_subscribers(
        &self,
        track: PublicationKey,
    ) -> impl Iterator<Item = Subscriber> + '_ {
        self.candidate_reasons
            .keys()
            .filter_map(move |(subscriber, candidate)| (*candidate == track).then_some(*subscriber))
    }

    fn active_subscribers(&self, track: PublicationKey) -> impl Iterator<Item = Subscriber> + '_ {
        self.candidate_subscribers(track).filter(move |subscriber| {
            self.automatic_reasons.contains_key(&(*subscriber, track))
                || self.allocated.contains(&(*subscriber, track))
        })
    }

    fn activate(&mut self, track: PublicationKey, subscriber: Subscriber) -> bool {
        let key = (subscriber, track);
        let valid = self
            .candidate_reasons
            .get(&key)
            .into_iter()
            .flatten()
            .any(|id| {
                self.subscriptions.get(*id).is_some_and(|subscription| {
                    subscription.selection == SelectionPolicy::Allocated
                })
            });
        if !valid {
            debug_assert!(false, "activation must target an allocated candidate");
            return false;
        }
        self.allocated.insert(key)
    }

    fn remove_subscriber(&mut self, subscriber: Subscriber) {
        let subscriptions: Vec<_> = self
            .subscriptions
            .iter()
            .filter_map(|(id, subscription)| (subscription.subscriber == subscriber).then_some(id))
            .collect();
        for id in subscriptions {
            let removed = self.unsubscribe(id);
            debug_assert!(removed.is_some());
        }
    }
}

fn remove_indexed<K: Eq + std::hash::Hash, V: Eq + std::hash::Hash + Copy>(
    index: &mut HashMap<K, HashSet<V>>,
    key: K,
    value: V,
) -> bool {
    let Some(values) = index.get_mut(&key) else {
        return false;
    };
    let removed = values.remove(&value);
    if values.is_empty() {
        index.remove(&key);
    }
    removed
}

fn remove_reason(
    reasons: &mut HashMap<(Subscriber, PublicationKey), HashSet<SubscriptionKey>>,
    key: (Subscriber, PublicationKey),
    id: SubscriptionKey,
) -> bool {
    let Some(ids) = reasons.get_mut(&key) else {
        return false;
    };
    let removed = ids.remove(&id);
    if ids.is_empty() {
        reasons.remove(&key);
    }
    removed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrackPlacement {
    pub publication_id: PublicationKey,
    pub route: NodeRouteAddress,
}

#[derive(Debug)]
pub(crate) struct TrackPlacementAllocator {
    routes: Vec<SlotAllocator>,
}

impl TrackPlacementAllocator {
    pub(crate) fn new(shard_count: usize) -> Self {
        Self {
            routes: (0..shard_count)
                .map(|index| {
                    SlotAllocator::with_max_slots(
                        ShardId::new(index),
                        PackedRoute::MAX_SLOT.saturating_add(1),
                    )
                })
                .collect(),
        }
    }

    #[allow(
        clippy::expect_used,
        reason = "a track placement for an unconfigured shard is a lifecycle invariant violation"
    )]
    pub(crate) fn allocate(
        &mut self,
        shard: ShardId,
        identity: TrackAncestry,
        now: Instant,
    ) -> TrackPlacement {
        let allocator = self
            .routes
            .get_mut(shard.index())
            .expect("track placement must target a configured shard");

        let (slot, epoch) = allocator.allocate(now);

        TrackPlacement {
            publication_id: identity.publication_id,
            route: NodeRouteAddress::new(RouteId::new(shard, slot), epoch),
        }
    }

    pub(crate) fn release(&mut self, placement: TrackPlacement, now: Instant) {
        let Some(allocator) = self.routes.get_mut(placement.route.shard().index()) else {
            debug_assert!(false, "track release targeted an unknown shard");
            return;
        };
        allocator.retire(placement.route.route.slot(), now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::RoomId;

    fn room(seed: u8) -> ProjectRoomKey {
        let name = match seed {
            1 => "topology-1",
            2 => "topology-2",
            _ => "topology-other",
        };
        ProjectRoomKey::fixture(RoomId::new(name).unwrap())
    }

    fn participant(seed: u8) -> ParticipantId {
        ParticipantId::new(&format!("participant-{seed}")).unwrap()
    }

    fn track(kind: TrackKind, publisher: u8, room_seed: u8, label: &str) -> Track {
        let connection = crate::entity::ConnectionId::new();
        let publisher = participant(publisher);
        let meta = crate::track::TrackMeta {
            room: room(room_seed),
            id: connection.fixture_publication(kind, label),
            origin: publisher,
            connection_id: connection,
            track_id: Some(
                crate::entity::TrackId::new(label.strip_prefix("rel:").unwrap_or(label)).unwrap(),
            ),
        };
        match kind {
            TrackKind::Data => {
                let (lane, topic) = label
                    .strip_prefix("rel:")
                    .map_or((crate::track::DataLane::Latest, label), |topic| {
                        (crate::track::DataLane::Ordered, topic)
                    });
                Track::data(meta, crate::track::Topic::for_test(topic), lane, None)
            }
            TrackKind::Audio => Track::audio(meta, None),
            TrackKind::Video => Track::video(meta, Vec::new(), None),
        }
    }

    #[test]
    fn a_publication_is_not_visible_in_the_same_room_of_another_project() {
        let mut topology = TrackTopology::default();
        let publication = track(TrackKind::Data, 1, 1, "topic");
        let id = publication.id();
        let owner = publication.meta().room;
        let other = ProjectRoomKey {
            project: crate::entity::ProjectId::new(),
            ..owner
        };
        let mut collision = publication.clone();
        collision.meta_mut().room = other;
        topology.publish(publication).unwrap();
        assert!(topology.publish(collision).is_none());
        assert!(topology.source(owner, id).is_some());
        assert!(topology.source(other, id).is_none());
        assert_eq!(topology.sources_in_room(other).count(), 0);
        assert_eq!(topology.room_catalog_version(other), 0);

        let mut publication = track(TrackKind::Data, 1, 1, "topic");
        publication.meta_mut().room = other;
        let other_id = publication.id();
        topology.publish(publication).unwrap();
        topology.remove_participant(EndpointKey {
            room: owner,
            participant: participant(1),
        });
        assert!(topology.source(owner, id).is_none());
        assert!(topology.source(other, other_id).is_some());
    }

    #[test]
    fn catalog_sources_are_indexed_by_publisher_incarnation_and_release_on_both_cleanup_paths() {
        let mut topology = TrackTopology::default();
        let old = track(TrackKind::Data, 1, 1, "same-label");
        let current = track(TrackKind::Data, 1, 1, "same-label");
        let old_connection = old.meta().connection_id;
        let current_connection = current.meta().connection_id;
        let old_identity = topology.publish(old).unwrap();
        let current_identity = topology.publish(current).unwrap();
        assert_eq!(
            topology
                .publisher_sources(room(1), participant(1), old_connection)
                .map(Track::id)
                .collect::<Vec<_>>(),
            vec![old_identity.publication_id]
        );
        assert_eq!(
            topology
                .publisher_sources(room(1), participant(1), current_connection)
                .map(Track::id)
                .collect::<Vec<_>>(),
            vec![current_identity.publication_id]
        );
        topology.unpublish(old_identity).unwrap();
        assert!(
            topology
                .publisher_sources(room(1), participant(1), old_connection)
                .next()
                .is_none()
        );
        topology.remove_participant(EndpointKey {
            room: room(1),
            participant: participant(1),
        });
        assert!(
            topology
                .publisher_sources(room(1), participant(1), current_connection)
                .next()
                .is_none()
        );
        assert!(topology.rooms[&room(1)].by_incarnation.is_empty());
    }

    #[test]
    fn exact_logical_source_lookup_uses_metadata_labels_for_every_kind() {
        let mut topology = TrackTopology::default();
        for kind in [TrackKind::Audio, TrackKind::Video, TrackKind::Data] {
            let mut publication = track(kind, 1, 1, "routing-label");
            publication.meta_mut().track_id =
                Some(crate::entity::TrackId::new("logical-label").unwrap());
            let id = publication.id();
            topology.publish(publication).unwrap();
            topology
                .publish(track(kind, 2, 1, "logical-label"))
                .unwrap();
            topology
                .publish(track(kind, 1, 2, "logical-label"))
                .unwrap();
            let found: Vec<_> = topology
                .exact_named_sources(
                    room(1),
                    participant(1),
                    kind,
                    crate::entity::TrackId::new("logical-label").unwrap(),
                )
                .unwrap()
                .map(Track::id)
                .collect();
            assert_eq!(found, [id]);
            assert_eq!(
                topology
                    .exact_named_sources(
                        room(1),
                        participant(1),
                        kind,
                        crate::entity::TrackId::new("routing-label").unwrap()
                    )
                    .unwrap()
                    .count(),
                0
            );
        }
    }

    #[test]
    fn selector_is_room_local_and_kind_constrained() {
        let mut topology = TrackTopology::default();
        let subscriber = participant(3);
        let _ = topology.subscribe(
            room(1),
            subscriber,
            TrackSelector::audio(),
            SelectionPolicy::All,
        );
        let room_audio = topology
            .publish(track(TrackKind::Audio, 1, 1, "audio"))
            .unwrap();
        let other_room_audio = topology
            .publish(track(TrackKind::Audio, 1, 2, "audio"))
            .unwrap();
        let room_data = topology
            .publish(track(TrackKind::Data, 2, 1, "data"))
            .unwrap();

        assert_eq!(topology.candidate_subscribers(room_audio).count(), 1);
        assert_eq!(topology.active_subscribers(room_audio).count(), 1);
        assert_eq!(topology.candidate_subscribers(other_room_audio).count(), 0);
        assert_eq!(topology.candidate_subscribers(room_data).count(), 0);
    }

    #[test]
    fn selector_constraints_are_conjunctive() {
        let mut topology = TrackTopology::default();
        let realtime_identity = topology
            .publish(track(TrackKind::Data, 1, 1, "chat"))
            .unwrap();
        let reliable_identity = topology
            .publish(track(TrackKind::Data, 1, 1, "rel:chat"))
            .unwrap();
        let other_publisher = topology
            .publish(track(TrackKind::Data, 2, 1, "chat"))
            .unwrap();
        let selector =
            TrackSelector::data_topic(Some(participant(1)), "v2/latest/chat".to_string());
        let _ = topology.subscribe(room(1), participant(3), selector, SelectionPolicy::All);

        assert_eq!(topology.matches(realtime_identity).count(), 1);
        assert_eq!(topology.matches(reliable_identity).count(), 0);
        assert_eq!(topology.matches(other_publisher).count(), 0);
    }

    #[test]
    fn overlapping_subscriptions_deduplicate_and_retain_reasons() {
        let mut topology = TrackTopology::default();
        let identity = topology
            .publish(track(TrackKind::Audio, 1, 1, "audio"))
            .unwrap();
        let subscriber = participant(3);
        let exact = TrackSelector::track(identity.publication_id);
        let _ = topology.subscribe(
            room(1),
            subscriber,
            TrackSelector::audio(),
            SelectionPolicy::All,
        );
        let _ = topology.subscribe(room(1), subscriber, exact.clone(), SelectionPolicy::All);

        assert_eq!(topology.candidate_subscribers(identity).count(), 1);
        assert!(
            topology
                .remove_matching(room(1), subscriber, exact.clone())
                .is_some()
        );
        assert_eq!(topology.candidate_subscribers(identity).count(), 1);
        assert!(
            topology
                .remove_matching(room(1), subscriber, TrackSelector::audio())
                .is_some()
        );
        assert_eq!(topology.candidate_subscribers(identity).count(), 0);
        assert!(
            topology
                .remove_matching(room(1), subscriber, exact.clone())
                .is_none()
        );
        topology.subscribe(
            room(1),
            subscriber,
            TrackSelector::audio(),
            SelectionPolicy::Allocated,
        );
        assert_eq!(topology.active_subscribers(identity).count(), 0);
        topology.set_owned(room(1), subscriber, 1, identity.publication_id, true);
        let unrelated = topology
            .publish(track(TrackKind::Audio, 2, 1, "other"))
            .unwrap();
        assert!(!topology.set_owned(room(1), subscriber, 1, unrelated.publication_id, true));
        let second_connection = Subscriber::Connection(ConnectionId::new());
        assert!(topology.set_owned_scoped(
            room(1),
            second_connection,
            1,
            identity.publication_id,
            true
        ));
        assert_eq!(topology.active_subscribers(identity).count(), 2);
        assert!(
            topology
                .remove_owned(room(1), second_connection, 1)
                .is_some()
        );
        assert_eq!(topology.active_subscribers(unrelated).count(), 0);
        assert_eq!(topology.active_subscribers(identity).count(), 1);
        topology.unpublish(unrelated).unwrap();
        topology.set_owned(room(1), subscriber, 2, identity.publication_id, true);
        assert_eq!(topology.active_subscribers(identity).count(), 1);
        topology.set_owned(room(1), subscriber, 1, identity.publication_id, false);
        assert_eq!(topology.active_subscribers(identity).count(), 1);
        assert!(
            topology
                .remove_matching(room(1), subscriber, exact)
                .is_none()
        );
        assert_eq!(
            topology
                .remove_owned(
                    room(1),
                    EndpointKey {
                        room: room(1),
                        participant: subscriber
                    },
                    2
                )
                .unwrap()
                .owned_ref,
            Some(2)
        );
        assert_eq!(topology.active_subscribers(identity).count(), 0);
        topology.set_owned(room(1), subscriber, 1, identity.publication_id, true);
        assert_eq!(topology.rooms[&room(1)].subscriptions.len(), 2);
        let other = participant(4);
        topology.set_owned(room(1), other, 1, identity.publication_id, true);
        topology
            .remove_owned(
                room(1),
                EndpointKey {
                    room: room(1),
                    participant: subscriber,
                },
                1,
            )
            .unwrap();
        assert_eq!(
            topology.active_subscribers(identity).collect::<Vec<_>>(),
            vec![Subscriber::Joined(EndpointKey {
                room: room(1),
                participant: other,
            })]
        );
        topology.unpublish(identity).unwrap();
        topology
            .remove_owned(
                room(1),
                EndpointKey {
                    room: room(1),
                    participant: other,
                },
                1,
            )
            .unwrap();
        assert!(topology.rooms[&room(1)].owned.is_empty());
    }

    #[test]
    fn allocated_matches_are_candidates_until_activated() {
        let mut topology = TrackTopology::default();
        let subscriber = participant(3);
        let identity = topology
            .publish(track(TrackKind::Video, 1, 1, "video"))
            .unwrap();
        let _ = topology.subscribe(
            room(1),
            subscriber,
            TrackSelector::video(),
            SelectionPolicy::Allocated,
        );

        assert_eq!(topology.candidate_subscribers(identity).count(), 1);
        assert_eq!(topology.active_subscribers(identity).count(), 0);
        assert!(topology.activate(
            identity,
            EndpointKey {
                room: identity.room_id,
                participant: subscriber
            }
        ));
        assert_eq!(topology.active_subscribers(identity).count(), 1);
        assert!(!topology.activate(
            identity,
            EndpointKey {
                room: identity.room_id,
                participant: subscriber
            }
        ));
        assert!(topology.deactivate(
            identity,
            EndpointKey {
                room: identity.room_id,
                participant: subscriber
            }
        ));
        assert_eq!(topology.active_subscribers(identity).count(), 0);
    }

    #[test]
    fn a_retired_destination_gets_a_fresh_track_route() {
        let mut allocator = TrackPlacementAllocator::new(2);
        let track = TrackAncestry::from_track(&track(TrackKind::Audio, 1, 1, "audio"));
        let first = allocator.allocate(ShardId::new(1), track, Instant::now());
        allocator.release(first, Instant::now());
        let replacement = allocator.allocate(ShardId::new(1), track, Instant::now());

        assert_eq!(first.publication_id, replacement.publication_id);
        assert_ne!(first.route, replacement.route);
    }

    #[test]
    fn removing_a_participant_removes_its_subscriptions_and_tracks() {
        let mut topology = TrackTopology::default();
        let owner = participant(1);
        let identity = topology
            .publish(track(TrackKind::Data, 1, 1, "data"))
            .unwrap();
        let _ = topology.subscribe(
            room(1),
            owner,
            TrackSelector::track(identity.publication_id),
            SelectionPolicy::All,
        );

        topology.remove_participant(EndpointKey {
            room: room(1),
            participant: owner,
        });

        assert!(!topology.contains(identity));
        assert_eq!(topology.matches(identity).count(), 0);
    }
}

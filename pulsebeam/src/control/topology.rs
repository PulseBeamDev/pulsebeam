use std::collections::{HashMap, HashSet};

use slotmap::{SlotMap, new_key_type};
use tokio::time::Instant;

use crate::{
    entity::{ParticipantId, RoomId, TrackId, TrackKind},
    id::ShardId,
    route::{PackedRoute, RouteError, RouteHandle, RouteId, SlotAllocator},
    track::{SelectionPolicy, Track, TrackSelector},
};

new_key_type! {
    pub(crate) struct SubscriptionId;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TrackIdentity {
    pub room_id: RoomId,
    pub publisher: ParticipantId,
    pub id: TrackId,
}

impl TrackIdentity {
    pub(crate) fn from_track(track: &Track) -> Self {
        Self {
            room_id: track.meta().room_id,
            publisher: track.meta().origin,
            id: track.id(),
        }
    }

    pub(crate) fn kind(self) -> TrackKind {
        self.id.kind()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Subscription {
    pub id: SubscriptionId,
    pub subscriber: ParticipantId,
    pub selector: TrackSelector,
    pub selection: SelectionPolicy,
}

#[derive(Debug)]
struct Publication {
    track: Track,
    label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SubscriptionAnchor {
    Track(TrackId),
    Publisher(ParticipantId),
    Label(String),
    Kind(TrackKind),
    Any,
}

#[derive(Debug, Default)]
struct RoomTrackTopology {
    tracks: HashMap<TrackId, Publication>,
    by_publisher: HashMap<ParticipantId, HashSet<TrackId>>,
    by_kind: HashMap<TrackKind, HashSet<TrackId>>,
    by_label: HashMap<String, HashSet<TrackId>>,
    subscriptions: SlotMap<SubscriptionId, Subscription>,
    by_track: HashMap<TrackId, HashSet<SubscriptionId>>,
    by_subscriber_publisher: HashMap<ParticipantId, HashSet<SubscriptionId>>,
    by_subscription_kind: HashMap<TrackKind, HashSet<SubscriptionId>>,
    by_subscription_label: HashMap<String, HashSet<SubscriptionId>>,
    unconstrained: HashSet<SubscriptionId>,
    candidate_reasons: HashMap<(ParticipantId, TrackId), HashSet<SubscriptionId>>,
    automatic_reasons: HashMap<(ParticipantId, TrackId), HashSet<SubscriptionId>>,
    allocated: HashSet<(ParticipantId, TrackId)>,
}

#[derive(Debug, Default)]
pub(crate) struct TrackTopology {
    rooms: HashMap<RoomId, RoomTrackTopology>,
}

impl TrackTopology {
    pub(crate) fn publish(&mut self, track: Track) -> Option<TrackIdentity> {
        let identity = TrackIdentity::from_track(&track);
        let label = track.publication_label();
        let room = self.rooms.entry(identity.room_id).or_default();
        if room.tracks.contains_key(&identity.id) {
            debug_assert!(false, "a track identity must be published once");
            return None;
        }
        room.index_track(identity, label.as_deref());
        room.tracks
            .insert(identity.id, Publication { track, label });
        room.add_publication_reasons(identity);
        Some(identity)
    }

    pub(crate) fn unpublish(&mut self, identity: TrackIdentity) -> Option<Track> {
        let room = self.rooms.get_mut(&identity.room_id)?;
        let publication = room.tracks.remove(&identity.id)?;
        debug_assert_eq!(TrackIdentity::from_track(&publication.track), identity);
        room.unindex_track(identity, publication.label.as_deref());
        room.candidate_reasons
            .retain(|(_, track), _| *track != identity.id);
        room.automatic_reasons
            .retain(|(_, track), _| *track != identity.id);
        room.allocated.retain(|(_, track)| *track != identity.id);
        Some(publication.track)
    }

    pub(crate) fn track(&self, identity: TrackIdentity) -> Option<&Track> {
        self.rooms
            .get(&identity.room_id)?
            .tracks
            .get(&identity.id)
            .map(|publication| &publication.track)
    }

    pub(crate) fn track_mut(&mut self, identity: TrackIdentity) -> Option<&mut Track> {
        self.rooms
            .get_mut(&identity.room_id)?
            .tracks
            .get_mut(&identity.id)
            .map(|publication| &mut publication.track)
    }

    pub(crate) fn subscribe(
        &mut self,
        room_id: RoomId,
        subscriber: ParticipantId,
        selector: TrackSelector,
        selection: SelectionPolicy,
    ) -> SubscriptionId {
        let room = self.rooms.entry(room_id).or_default();
        let id = room.subscriptions.insert_with_key(|id| Subscription {
            id,
            subscriber,
            selector: selector.clone(),
            selection,
        });
        room.index_subscription(&selector, id);
        room.add_subscription_reasons(id);
        id
    }

    pub(crate) fn remove_matching(
        &mut self,
        room_id: RoomId,
        subscriber: ParticipantId,
        selector: TrackSelector,
    ) -> Option<Subscription> {
        let room = self.rooms.get_mut(&room_id)?;
        let id = room.subscriptions.iter().find_map(|(id, subscription)| {
            (subscription.subscriber == subscriber && subscription.selector == selector)
                .then_some(id)
        });
        let id = id?;
        room.unsubscribe(id)
    }

    #[cfg(test)]
    pub(crate) fn matches(
        &self,
        identity: TrackIdentity,
    ) -> impl Iterator<Item = Subscription> + '_ {
        self.rooms
            .get(&identity.room_id)
            .into_iter()
            .flat_map(move |room| room.matching_subscriptions(identity))
    }

    pub(crate) fn candidate_subscribers(
        &self,
        identity: TrackIdentity,
    ) -> impl Iterator<Item = ParticipantId> + '_ {
        self.rooms
            .get(&identity.room_id)
            .into_iter()
            .flat_map(move |room| room.candidate_subscribers(identity.id))
    }

    pub(crate) fn active_subscribers(
        &self,
        identity: TrackIdentity,
    ) -> impl Iterator<Item = ParticipantId> + '_ {
        self.rooms
            .get(&identity.room_id)
            .into_iter()
            .flat_map(move |room| room.active_subscribers(identity.id))
    }

    pub(crate) fn activate(&mut self, identity: TrackIdentity, subscriber: ParticipantId) -> bool {
        let Some(room) = self.rooms.get_mut(&identity.room_id) else {
            debug_assert!(false, "activation must target a live room catalog");
            return false;
        };
        room.activate(identity.id, subscriber)
    }

    pub(crate) fn deactivate(
        &mut self,
        identity: TrackIdentity,
        subscriber: ParticipantId,
    ) -> bool {
        self.rooms
            .get_mut(&identity.room_id)
            .is_some_and(|room| room.allocated.remove(&(subscriber, identity.id)))
    }

    pub(crate) fn contains(&self, identity: TrackIdentity) -> bool {
        self.rooms
            .get(&identity.room_id)
            .is_some_and(|room| room.tracks.contains_key(&identity.id))
    }

    pub(crate) fn matching_tracks(
        &self,
        room_id: RoomId,
        selector: &TrackSelector,
    ) -> Vec<TrackIdentity> {
        self.rooms.get(&room_id).map_or_else(Vec::new, |room| {
            room.selector_tracks(selector)
                .into_iter()
                .filter_map(|track| room.identity(room_id, track))
                .collect()
        })
    }

    pub(crate) fn identities(&self) -> impl Iterator<Item = TrackIdentity> + '_ {
        self.rooms.iter().flat_map(|(room_id, room)| {
            room.tracks
                .keys()
                .filter_map(move |track| room.identity(*room_id, *track))
        })
    }

    pub(crate) fn remove_participant(&mut self, participant: ParticipantId) {
        for room in self.rooms.values_mut() {
            room.remove_subscriber(participant);
            let published: Vec<_> = room
                .tracks
                .iter()
                .filter_map(|(id, publication)| {
                    (publication.track.meta().origin == participant).then_some(*id)
                })
                .collect();
            for track in published {
                room.remove_track(track);
            }
        }
    }
}

impl RoomTrackTopology {
    fn identity(&self, room_id: RoomId, track: TrackId) -> Option<TrackIdentity> {
        let publication = self.tracks.get(&track)?;
        Some(TrackIdentity {
            room_id,
            publisher: publication.track.meta().origin,
            id: track,
        })
    }

    fn index_track(&mut self, identity: TrackIdentity, label: Option<&str>) {
        debug_assert!(!self.tracks.contains_key(&identity.id));
        let publisher_inserted = self
            .by_publisher
            .entry(identity.publisher)
            .or_default()
            .insert(identity.id);
        debug_assert!(publisher_inserted);
        let kind_inserted = self
            .by_kind
            .entry(identity.kind())
            .or_default()
            .insert(identity.id);
        debug_assert!(kind_inserted);
        if let Some(label) = label {
            let label_inserted = self
                .by_label
                .entry(label.to_owned())
                .or_default()
                .insert(identity.id);
            debug_assert!(label_inserted);
        }
    }

    fn unindex_track(&mut self, identity: TrackIdentity, label: Option<&str>) {
        let publisher_removed =
            remove_indexed(&mut self.by_publisher, identity.publisher, identity.id);
        debug_assert!(publisher_removed);
        let kind_removed = remove_indexed(&mut self.by_kind, identity.kind(), identity.id);
        debug_assert!(kind_removed);
        if let Some(label) = label {
            let label_removed = remove_indexed(&mut self.by_label, label.to_owned(), identity.id);
            debug_assert!(label_removed);
        }
    }

    fn selector_tracks(&self, selector: &TrackSelector) -> Vec<TrackId> {
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

    fn index_subscription(&mut self, selector: &TrackSelector, id: SubscriptionId) {
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

    fn unindex_subscription(&mut self, selector: &TrackSelector, id: SubscriptionId) {
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

    fn matching_subscription_ids(&self, identity: TrackIdentity) -> HashSet<SubscriptionId> {
        let Some(publication) = self.tracks.get(&identity.id) else {
            return HashSet::new();
        };
        debug_assert_eq!(TrackIdentity::from_track(&publication.track), identity);
        let mut candidates = self.unconstrained.clone();
        for ids in [
            self.by_track.get(&identity.id),
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
        identity: TrackIdentity,
    ) -> impl Iterator<Item = Subscription> + '_ {
        self.matching_subscription_ids(identity)
            .into_iter()
            .filter_map(|id| self.subscriptions.get(id).cloned())
    }

    fn add_publication_reasons(&mut self, identity: TrackIdentity) {
        for id in self.matching_subscription_ids(identity) {
            self.add_reason(identity.id, id);
        }
    }

    fn add_subscription_reasons(&mut self, id: SubscriptionId) {
        let Some(subscription) = self.subscriptions.get(id).cloned() else {
            debug_assert!(false, "new subscription must be live");
            return;
        };
        for track in self.selector_tracks(&subscription.selector) {
            self.add_reason(track, id);
        }
    }

    fn add_reason(&mut self, track: TrackId, id: SubscriptionId) {
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

    fn unsubscribe(&mut self, id: SubscriptionId) -> Option<Subscription> {
        let subscription = self.subscriptions.remove(id)?;
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

    fn candidate_subscribers(&self, track: TrackId) -> impl Iterator<Item = ParticipantId> + '_ {
        self.candidate_reasons
            .keys()
            .filter_map(move |(subscriber, candidate)| (*candidate == track).then_some(*subscriber))
    }

    fn active_subscribers(&self, track: TrackId) -> impl Iterator<Item = ParticipantId> + '_ {
        self.candidate_subscribers(track).filter(move |subscriber| {
            self.automatic_reasons.contains_key(&(*subscriber, track))
                || self.allocated.contains(&(*subscriber, track))
        })
    }

    fn activate(&mut self, track: TrackId, subscriber: ParticipantId) -> bool {
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

    fn remove_subscriber(&mut self, participant: ParticipantId) {
        let subscriptions: Vec<_> = self
            .subscriptions
            .iter()
            .filter_map(|(id, subscription)| (subscription.subscriber == participant).then_some(id))
            .collect();
        for id in subscriptions {
            let removed = self.unsubscribe(id);
            debug_assert!(removed.is_some());
        }
        self.allocated
            .retain(|(subscriber, _)| *subscriber != participant);
    }

    fn remove_track(&mut self, track: TrackId) {
        let Some(publication) = self.tracks.remove(&track) else {
            debug_assert!(false, "participant publication must remain indexed");
            return;
        };
        let identity = TrackIdentity::from_track(&publication.track);
        self.unindex_track(identity, publication.label.as_deref());
        self.candidate_reasons
            .retain(|(_, candidate), _| *candidate != track);
        self.automatic_reasons
            .retain(|(_, candidate), _| *candidate != track);
        self.allocated.retain(|(_, candidate)| *candidate != track);
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
    reasons: &mut HashMap<(ParticipantId, TrackId), HashSet<SubscriptionId>>,
    key: (ParticipantId, TrackId),
    id: SubscriptionId,
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
pub(crate) struct TrackAllocation {
    pub key: crate::keys::TrackKey,
    pub route: RouteHandle,
}

#[derive(Debug)]
pub(crate) struct TrackAllocator {
    keys: SlotMap<crate::keys::TrackKey, TrackIdentity>,
    routes: Vec<SlotAllocator>,
}

impl TrackAllocator {
    pub(crate) fn new(shard_count: usize) -> Self {
        Self {
            keys: SlotMap::with_key(),
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

    pub(crate) fn allocate(
        &mut self,
        shard: ShardId,
        identity: TrackIdentity,
        now: Instant,
    ) -> Result<TrackAllocation, RouteError> {
        let Some(allocator) = self.routes.get_mut(shard.index()) else {
            debug_assert!(false, "track allocation targeted an unknown shard");
            return Err(RouteError::Exhausted { max_slots: 0 });
        };
        let (slot, epoch) = allocator.allocate(now)?;
        let key = self.keys.insert(identity);
        Ok(TrackAllocation {
            key,
            route: RouteHandle::new(RouteId::new(shard, slot), epoch),
        })
    }

    pub(crate) fn release(&mut self, allocation: TrackAllocation, now: Instant) {
        let removed = self.keys.remove(allocation.key);
        debug_assert!(
            removed.is_some(),
            "track allocation must be live at release"
        );
        let Some(allocator) = self.routes.get_mut(allocation.route.shard().index()) else {
            debug_assert!(false, "track release targeted an unknown shard");
            return;
        };
        allocator.retire(allocation.route.route.slot(), now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::ExternalRoomId;

    fn room(seed: u8) -> RoomId {
        let name = match seed {
            1 => "topology-1",
            2 => "topology-2",
            _ => "topology-other",
        };
        RoomId::from_external(&ExternalRoomId::new(name).unwrap())
    }

    fn participant(seed: u8) -> ParticipantId {
        ParticipantId::from_bytes([seed; 16])
    }

    fn track(kind: TrackKind, publisher: u8, room_seed: u8, label: &str) -> Track {
        let publisher = participant(publisher);
        let meta = crate::track::TrackMeta {
            room_id: room(room_seed),
            shard_id: ShardId::new(0),
            id: publisher.derive_track_id(kind, label),
            origin: publisher,
        };
        match kind {
            TrackKind::Data => {
                let (lane, topic) = label
                    .strip_prefix("rel:")
                    .map_or((crate::track::DataLane::Realtime, label), |topic| {
                        (crate::track::DataLane::Reliable, topic)
                    });
                Track::data(meta, crate::track::Topic::for_test(topic), lane, None)
            }
            TrackKind::Audio => Track::audio(meta, None),
            TrackKind::Video => Track::video(meta, Vec::new(), None),
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
        let selector = TrackSelector::data_topic(Some(participant(1)), "v1/rt/chat".to_string());
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
        let exact = TrackSelector::track(identity.id);
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
                .remove_matching(room(1), subscriber, exact)
                .is_none()
        );
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
        assert!(topology.activate(identity, subscriber));
        assert_eq!(topology.active_subscribers(identity).count(), 1);
        assert!(!topology.activate(identity, subscriber));
        assert!(topology.deactivate(identity, subscriber));
        assert_eq!(topology.active_subscribers(identity).count(), 0);
    }

    #[test]
    fn a_retired_destination_gets_a_fresh_track_route() {
        let mut allocator = TrackAllocator::new(2);
        let track = TrackIdentity::from_track(&track(TrackKind::Audio, 1, 1, "audio"));
        let first = allocator
            .allocate(ShardId::new(1), track, Instant::now())
            .expect("first destination allocation");
        allocator.release(first, Instant::now());
        let replacement = allocator
            .allocate(ShardId::new(1), track, Instant::now())
            .expect("replacement destination allocation");

        assert_ne!(first.key, replacement.key);
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
            TrackSelector::track(identity.id),
            SelectionPolicy::All,
        );

        topology.remove_participant(owner);

        assert!(!topology.contains(identity));
        assert_eq!(topology.matches(identity).count(), 0);
    }
}

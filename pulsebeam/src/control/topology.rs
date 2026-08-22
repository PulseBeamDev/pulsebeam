use std::collections::{HashMap, HashSet};

use slotmap::{SlotMap, new_key_type};
use tokio::time::Instant;

use crate::{
    entity::{ParticipantId, RoomId, TrackId, TrackKind},
    id::ShardId,
    route::{PackedRoute, RouteError, RouteHandle, RouteId, SlotAllocator},
    track::Track,
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum TrackSelector {
    Exact(TrackIdentity),
    RoomKind {
        room_id: RoomId,
        kind: TrackKind,
    },
    DataTopic {
        room_id: RoomId,
        publisher: Option<ParticipantId>,
        label: String,
    },
}

impl TrackSelector {
    fn matches(&self, track: TrackIdentity, data_label: Option<&str>) -> bool {
        match self {
            Self::Exact(identity) => *identity == track,
            Self::RoomKind { room_id, kind } => track.room_id == *room_id && track.kind() == *kind,
            Self::DataTopic {
                room_id,
                publisher,
                label,
            } => {
                track.room_id == *room_id
                    && track.kind() == TrackKind::Data
                    && publisher.is_none_or(|expected| track.publisher == expected)
                    && data_label == Some(label.as_str())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Subscription {
    pub id: SubscriptionId,
    pub subscriber: ParticipantId,
    pub selector: TrackSelector,
    pub data: Option<DataSubscription>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DataSubscription {
    pub channel: str0m::channel::ChannelId,
    pub lane: crate::track::DataLane,
}

#[derive(Debug, Default)]
pub(crate) struct TrackTopology {
    tracks: HashMap<TrackIdentity, Track>,
    data_labels: HashMap<TrackIdentity, String>,
    data_publications: HashMap<TrackIdentity, (crate::track::Topic, crate::track::DataLane)>,
    subscriptions: SlotMap<SubscriptionId, Subscription>,
    by_room_kind: HashMap<(RoomId, TrackKind), HashSet<SubscriptionId>>,
    by_identity: HashMap<TrackIdentity, HashSet<SubscriptionId>>,
    by_data_label: HashMap<(RoomId, String), HashSet<SubscriptionId>>,
}

impl TrackTopology {
    pub(crate) fn publish(&mut self, track: Track) -> Option<TrackIdentity> {
        self.publish_with_label(track, None)
    }

    pub(crate) fn publish_with_label(
        &mut self,
        track: Track,
        data_label: Option<String>,
    ) -> Option<TrackIdentity> {
        let identity = TrackIdentity::from_track(&track);
        if self.tracks.insert(identity, track).is_some() {
            debug_assert!(false, "a track identity must be published once");
            return None;
        }
        if let Some(label) = data_label {
            self.data_labels.insert(identity, label);
        }
        Some(identity)
    }

    pub(crate) fn publish_data(
        &mut self,
        track: Track,
        topic: crate::track::Topic,
        lane: crate::track::DataLane,
    ) -> Option<TrackIdentity> {
        let label = crate::track::publication_label(lane, &topic);
        let identity = self.publish_with_label(track, Some(label))?;
        self.data_publications.insert(identity, (topic, lane));
        Some(identity)
    }

    pub(crate) fn unpublish(&mut self, identity: TrackIdentity) -> Option<Track> {
        self.data_labels.remove(&identity);
        self.data_publications.remove(&identity);
        self.tracks.remove(&identity)
    }

    pub(crate) fn track(&self, identity: TrackIdentity) -> Option<&Track> {
        self.tracks.get(&identity)
    }

    pub(crate) fn track_mut(&mut self, identity: TrackIdentity) -> Option<&mut Track> {
        self.tracks.get_mut(&identity)
    }

    pub(crate) fn data_publication(
        &self,
        identity: TrackIdentity,
    ) -> Option<&(crate::track::Topic, crate::track::DataLane)> {
        self.data_publications.get(&identity)
    }

    pub(crate) fn subscribe(
        &mut self,
        subscriber: ParticipantId,
        selector: TrackSelector,
    ) -> SubscriptionId {
        self.subscribe_with_data(subscriber, selector, None)
    }

    pub(crate) fn subscribe_data(
        &mut self,
        subscriber: ParticipantId,
        selector: TrackSelector,
        channel: str0m::channel::ChannelId,
        lane: crate::track::DataLane,
    ) -> SubscriptionId {
        self.subscribe_with_data(
            subscriber,
            selector,
            Some(DataSubscription { channel, lane }),
        )
    }

    fn subscribe_with_data(
        &mut self,
        subscriber: ParticipantId,
        selector: TrackSelector,
        data: Option<DataSubscription>,
    ) -> SubscriptionId {
        let id = self.subscriptions.insert_with_key(|id| Subscription {
            id,
            subscriber,
            selector: selector.clone(),
            data,
        });
        match &selector {
            TrackSelector::Exact(identity) => {
                let inserted = self.by_identity.entry(*identity).or_default().insert(id);
                debug_assert!(inserted);
            }
            TrackSelector::RoomKind { room_id, kind } => {
                let inserted = self
                    .by_room_kind
                    .entry((*room_id, *kind))
                    .or_default()
                    .insert(id);
                debug_assert!(inserted);
            }
            TrackSelector::DataTopic { room_id, label, .. } => {
                let inserted = self
                    .by_data_label
                    .entry((*room_id, label.clone()))
                    .or_default()
                    .insert(id);
                debug_assert!(inserted);
            }
        }
        id
    }

    pub(crate) fn unsubscribe_matching(
        &mut self,
        subscriber: ParticipantId,
        selector: TrackSelector,
    ) -> bool {
        self.remove_matching(subscriber, selector).is_some()
    }

    pub(crate) fn remove_matching(
        &mut self,
        subscriber: ParticipantId,
        selector: TrackSelector,
    ) -> Option<Subscription> {
        let id = self.subscriptions.iter().find_map(|(id, subscription)| {
            (subscription.subscriber == subscriber && subscription.selector == selector)
                .then_some(id)
        });
        let id = id?;
        let subscription = self.subscriptions.remove(id)?;
        self.unindex(&subscription.selector, id);
        Some(subscription)
    }

    pub(crate) fn unsubscribe(&mut self, id: SubscriptionId) -> Option<Subscription> {
        let subscription = self.subscriptions.remove(id)?;
        self.unindex(&subscription.selector, id);
        Some(subscription)
    }

    fn unindex(&mut self, selector: &TrackSelector, id: SubscriptionId) {
        let removed = match selector {
            TrackSelector::Exact(identity) => {
                Self::remove_indexed(&mut self.by_identity, *identity, id)
            }
            TrackSelector::RoomKind { room_id, kind } => {
                Self::remove_indexed(&mut self.by_room_kind, (*room_id, *kind), id)
            }
            TrackSelector::DataTopic { room_id, label, .. } => {
                Self::remove_indexed(&mut self.by_data_label, (*room_id, label.clone()), id)
            }
        };
        debug_assert!(
            removed,
            "subscription index must contain every subscription"
        );
    }

    fn remove_indexed<K: Eq + std::hash::Hash>(
        index: &mut HashMap<K, HashSet<SubscriptionId>>,
        key: K,
        id: SubscriptionId,
    ) -> bool {
        let Some(ids) = index.get_mut(&key) else {
            return false;
        };
        let removed = ids.remove(&id);
        if ids.is_empty() {
            index.remove(&key);
        }
        removed
    }

    pub(crate) fn matches(
        &self,
        identity: TrackIdentity,
    ) -> impl Iterator<Item = Subscription> + '_ {
        let mut candidates = HashSet::new();
        if let Some(ids) = self.by_room_kind.get(&(identity.room_id, identity.kind())) {
            candidates.extend(ids.iter().copied());
        }
        if let Some(ids) = self.by_identity.get(&identity) {
            candidates.extend(ids.iter().copied());
        }
        if let Some(label) = self.data_labels.get(&identity)
            && let Some(ids) = self.by_data_label.get(&(identity.room_id, label.clone()))
        {
            candidates.extend(ids.iter().copied());
        }
        let data_label = self.data_labels.get(&identity).map(String::as_str);
        candidates
            .into_iter()
            .filter_map(|id| self.subscriptions.get(id))
            .filter(move |&subscription| subscription.selector.matches(identity, data_label))
            .cloned()
    }

    pub(crate) fn contains(&self, identity: TrackIdentity) -> bool {
        self.tracks.contains_key(&identity)
    }

    pub(crate) fn tracks_in_room(
        &self,
        room_id: RoomId,
        kind: TrackKind,
    ) -> impl Iterator<Item = TrackIdentity> + '_ {
        self.tracks
            .keys()
            .filter(move |identity| identity.room_id == room_id && identity.kind() == kind)
            .copied()
    }

    pub(crate) fn identities(&self) -> impl Iterator<Item = TrackIdentity> + '_ {
        self.tracks.keys().copied()
    }

    pub(crate) fn remove_participant(&mut self, participant: ParticipantId) {
        let subscriptions: Vec<_> = self
            .subscriptions
            .iter()
            .filter_map(|(id, subscription)| (subscription.subscriber == participant).then_some(id))
            .collect();
        for id in subscriptions {
            let _ = self.unsubscribe(id);
        }
        self.tracks
            .retain(|identity, _| identity.publisher != participant);
        self.data_labels
            .retain(|identity, _| identity.publisher != participant);
        self.data_publications
            .retain(|identity, _| identity.publisher != participant);
    }
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

    fn room() -> RoomId {
        RoomId::from_external(&ExternalRoomId::new("topology").unwrap())
    }

    fn participant(seed: u8) -> ParticipantId {
        ParticipantId::from_bytes([seed; 16])
    }

    fn identity(kind: TrackKind, publisher: u8) -> TrackIdentity {
        let publisher = participant(publisher);
        TrackIdentity {
            room_id: room(),
            publisher,
            id: publisher.derive_track_id(kind, "same-label"),
        }
    }

    #[test]
    fn audio_wildcard_does_not_match_data_tracks() {
        let mut topology = TrackTopology::default();
        let subscriber = participant(3);
        let _ = topology.subscribe(
            subscriber,
            TrackSelector::RoomKind {
                room_id: room(),
                kind: TrackKind::Audio,
            },
        );

        assert_eq!(topology.matches(identity(TrackKind::Audio, 1)).count(), 1);
        assert_eq!(topology.matches(identity(TrackKind::Data, 2)).count(), 0);
    }

    #[test]
    fn data_topic_selectors_preserve_topic_and_lane_identity() {
        let mut topology = TrackTopology::default();
        let publisher = participant(1);
        let realtime = Track::data(
            crate::track::TrackMeta {
                room_id: room(),
                shard_id: ShardId::new(0),
                id: publisher.derive_track_id(TrackKind::Data, "v1/rt/chat"),
                origin: publisher,
            },
            None,
        );
        let reliable = Track::data(
            crate::track::TrackMeta {
                room_id: room(),
                shard_id: ShardId::new(0),
                id: publisher.derive_track_id(TrackKind::Data, "v1/rel/chat"),
                origin: publisher,
            },
            None,
        );
        let realtime_identity = topology
            .publish_with_label(realtime, Some("v1/rt/chat".to_string()))
            .unwrap();
        let reliable_identity = topology
            .publish_with_label(reliable, Some("v1/rel/chat".to_string()))
            .unwrap();
        let selector = TrackSelector::DataTopic {
            room_id: room(),
            publisher: None,
            label: "v1/rt/chat".to_string(),
        };
        let _ = topology.subscribe(participant(3), selector);

        assert_eq!(topology.matches(realtime_identity).count(), 1);
        assert_eq!(topology.matches(reliable_identity).count(), 0);
    }

    #[test]
    fn unsubscribe_removes_only_the_matching_subscription() {
        let mut topology = TrackTopology::default();
        let track = identity(TrackKind::Audio, 1);
        let subscriber = participant(3);
        let selector = TrackSelector::Exact(track);
        let _ = topology.subscribe(subscriber, selector.clone());
        let _ = topology.subscribe(participant(4), selector.clone());

        assert!(topology.unsubscribe_matching(subscriber, selector.clone()));
        assert_eq!(topology.matches(track).count(), 1);
        assert!(!topology.unsubscribe_matching(subscriber, selector));
    }

    #[test]
    fn a_retired_destination_gets_a_fresh_track_route() {
        let mut allocator = TrackAllocator::new(2);
        let track = identity(TrackKind::Audio, 1);
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
        let track = identity(TrackKind::Data, 1);
        let sub = topology.subscribe(owner, TrackSelector::Exact(track));
        assert!(topology.subscriptions.contains_key(sub));
        assert!(
            topology
                .publish(Track::data(
                    crate::track::TrackMeta {
                        room_id: room(),
                        shard_id: ShardId::new(0),
                        id: track.id,
                        origin: owner,
                    },
                    None,
                ))
                .is_some()
        );

        topology.remove_participant(owner);

        assert!(!topology.subscriptions.contains_key(sub));
        assert!(!topology.contains(track));
    }
}
